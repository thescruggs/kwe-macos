// SPDX-License-Identifier: GPL-3.0-or-later
#include "daemonactivator.h"

#include <QLocalSocket>
#include <QProcessEnvironment>

namespace {
// Liveness probe, not an existence check: a socket file left behind by a
// hard-killed daemon (StartLimitExceeded, kill -9) reads as "present" but
// accepts nothing, and must not skip activation. On Unix, connect(2) to a
// stale socket fails immediately with ECONNREFUSED; a live daemon accepts
// the connection and tolerates the probe closing without a request. Any
// connect failure counts as absent — a redundant activation is idempotent
// (systemctl start of a running unit is a no-op) and bounded.
bool socketIsLive(const QString &socketPath) {
    QLocalSocket socket;
    socket.connectToServer(socketPath);
    if (!socket.waitForConnected(50)) {
        return false;
    }
    socket.disconnectFromServer();
    return true;
}
}

DaemonActivator::DaemonActivator(QString socketPath, QString commandProgram,
                                 QStringList commandArguments, QObject *parent)
    : QObject(parent),
      m_socketPath(std::move(socketPath)),
      m_commandProgram(std::move(commandProgram)),
      m_commandArguments(std::move(commandArguments)) {
    // Bounded one-shot timers: an attempt times out, the socket probe polls
    // for a short window after a successful command, and retries back off.
    m_timeoutTimer.setSingleShot(true);
    m_probeTimer.setSingleShot(true);
    m_retryTimer.setSingleShot(true);
    connect(&m_timeoutTimer, &QTimer::timeout, this, &DaemonActivator::handleTimeout);
    connect(&m_probeTimer, &QTimer::timeout, this, &DaemonActivator::probeSocket);
    connect(&m_retryTimer, &QTimer::timeout, this, &DaemonActivator::runAttempt);
}

void DaemonActivator::setAttemptTimeoutMilliseconds(int milliseconds) {
    m_attemptTimeoutMilliseconds = milliseconds;
}

void DaemonActivator::setBackoffMilliseconds(int milliseconds) {
    m_backoffMilliseconds = milliseconds;
    m_initialBackoffMilliseconds = milliseconds;
}

void DaemonActivator::setMaxAttempts(int attempts) { m_maxAttempts = attempts; }

void DaemonActivator::setSocketProbeMilliseconds(int interval, int window) {
    m_probeIntervalMilliseconds = interval;
    m_probeWindowMilliseconds = window;
}

void DaemonActivator::activate() {
    // Probe first; the common case (daemon already up) never spawns a
    // process. A duplicate call while a run is in flight is ignored so a
    // manual retry cannot stack activations.
    if (m_state == Activating)
        return;
    m_attempts = 0;
    m_backoffMilliseconds = m_initialBackoffMilliseconds;
    if (socketIsLive(m_socketPath)) {
        setState(Running, {});
        return;
    }
    runAttempt();
}

void DaemonActivator::runAttempt() {
    ++m_attempts;
    // A timed-out or failed predecessor may still be dying; kill and drop it
    // so its late signals cannot disturb the new attempt.
    dropCurrentProcess();
    auto *process = new QProcess(this);
    m_process = process;
    auto environment = QProcessEnvironment::systemEnvironment();
    // The command contract: the spawned program inherits the manager
    // environment plus the exact socket the manager will connect to, so a
    // stub can start a daemon on the right path.
    environment.insert(QStringLiteral("KWE_DAEMON_SOCKET"), m_socketPath);
    process->setProcessEnvironment(environment);
    // Daemon/service diagnostics reach the manager's stderr, never a buffer
    // that could fill and stall the child.
    process->setProcessChannelMode(QProcess::ForwardedErrorChannel);
    connect(process, &QProcess::finished, this, &DaemonActivator::handleFinished);
    connect(process, &QProcess::errorOccurred, this, [this](QProcess::ProcessError error) {
        if (error == QProcess::FailedToStart)
            handleStartFailure();
    });
    m_timeoutTimer.start(m_attemptTimeoutMilliseconds);
    setState(Activating, tr("Starting the wallpaper service…"));
    process->start(m_commandProgram, m_commandArguments);
}

void DaemonActivator::dropCurrentProcess() {
    if (m_process == nullptr)
        return;
    m_process->disconnect(this);
    m_process->kill();
    m_process->deleteLater();
    m_process = nullptr;
}

void DaemonActivator::handleFinished(int exitCode, QProcess::ExitStatus status) {
    // Only the current attempt may transition the state; a timed-out attempt
    // was already detached and counted by handleTimeout().
    if (m_state != Activating || m_process == nullptr)
        return;
    m_timeoutTimer.stop();
    if (exitCode == 0 && status == QProcess::NormalExit)
        startProbing();
    else
        finishAttempt(AttemptResult::Failed);
}

void DaemonActivator::handleStartFailure() {
    if (m_state != Activating || m_process == nullptr)
        return;
    m_timeoutTimer.stop();
    dropCurrentProcess();
    finishAttempt(AttemptResult::Failed);
}

void DaemonActivator::handleTimeout() {
    m_timeoutTimer.stop();
    // Detach the attempt before counting it, so a late finished() from the
    // dying child cannot double-count. Bound the kill: SIGTERM first, then
    // SIGKILL after a grace period, so systemctl gets a chance to unwind.
    QPointer<QProcess> straggler = m_process;
    m_process = nullptr;
    if (straggler != nullptr) {
        straggler->disconnect(this);
        straggler->terminate();
        QTimer::singleShot(m_killGraceMilliseconds, straggler, &QProcess::kill);
    }
    finishAttempt(AttemptResult::TimedOut);
}

void DaemonActivator::startProbing() {
    m_probeElapsedMilliseconds = 0;
    probeSocket();
}

void DaemonActivator::probeSocket() {
    if (socketIsLive(m_socketPath)) {
        setState(Running, {});
        emit activated();
        return;
    }
    m_probeElapsedMilliseconds += m_probeIntervalMilliseconds;
    if (m_probeElapsedMilliseconds >= m_probeWindowMilliseconds) {
        // The command claimed success but the daemon never appeared; treat
        // it like a failed attempt so the bounded retry loop still applies.
        finishAttempt(AttemptResult::Failed);
        return;
    }
    m_probeTimer.start(m_probeIntervalMilliseconds);
}

void DaemonActivator::finishAttempt(AttemptResult result) {
    m_timeoutTimer.stop();
    if (m_state != Activating)
        return;
    if (result == AttemptResult::Succeeded) {
        startProbing();
        return;
    }
    if (m_attempts < m_maxAttempts) {
        m_retryTimer.start(m_backoffMilliseconds);
        m_backoffMilliseconds = qMin(m_backoffMilliseconds * 2, 4000);
        return;
    }
    setState(Failed,
             tr("The background service is not running. Run `systemctl --user start kwe-daemon`."));
}

void DaemonActivator::setState(State state, const QString &message) {
    if (m_state == state && m_message == message)
        return;
    m_state = state;
    m_message = message;
    emit stateChanged();
}
