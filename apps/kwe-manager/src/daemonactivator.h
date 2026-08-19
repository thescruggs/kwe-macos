// SPDX-License-Identifier: Apache-2.0
#pragma once

#include <QQmlEngine>
#include <QObject>
#include <QPointer>
#include <QProcess>
#include <QTimer>

// Bounded daemon activation for the gallery: when the daemon socket is
// absent, start the user service through a configurable command (the systemd
// user unit by default) instead of connecting blind. The daemon lifecycle
// stays with systemd; this class never spawns kwe-daemon itself, so a
// manager-spawned daemon cannot die with the manager.
//
// Decision flow: probe the daemon socket first — a live connect attempt, not
// a file existence check, so a stale socket file left behind by a
// hard-killed daemon (StartLimitExceeded) does not read as running. If live,
// state is Running and no command runs. If absent, run the activation
// command with a bounded timeout and up to MaxAttempts retries with backoff;
// success means the socket becomes reachable within a bounded probe window.
// Exhausting the attempts yields a Failed state with an actionable message
// instead of an error storm. One activation cycle per activate() call; the
// QML retry action calls it again.
//
// The spawned command inherits the manager environment plus KWE_DAEMON_SOCKET
// set to the exact socket path, so stubs (smoke tests) can start a daemon on
// the path the manager will connect to.
class DaemonActivator final : public QObject {
    Q_OBJECT
    // Constructed in C++ and exposed as a context property; QML still needs
    // the registered type to reach the State enum values.
    QML_UNCREATABLE("DaemonActivator is created by the manager")
    Q_PROPERTY(State state READ state NOTIFY stateChanged)
    Q_PROPERTY(QString message READ message NOTIFY stateChanged)

public:
    enum State { Unknown, Running, Activating, Failed };
    Q_ENUM(State)

    explicit DaemonActivator(QString socketPath,
                             QString commandProgram,
                             QStringList commandArguments,
                             QObject *parent = nullptr);

    Q_INVOKABLE void activate();
    State state() const { return m_state; }
    QString message() const { return m_message; }

    // Test hooks: shrink the bounded timings so unit tests stay fast. The
    // defaults are generous because a real systemctl start may take seconds.
    void setAttemptTimeoutMilliseconds(int milliseconds);
    void setBackoffMilliseconds(int milliseconds);
    void setMaxAttempts(int attempts);
    void setSocketProbeMilliseconds(int interval, int window);

signals:
    void stateChanged();
    // Emitted once the daemon socket is observed after an activation attempt;
    // main.cpp refreshes the catalog immediately instead of waiting for the
    // client's exponential retry backoff.
    void activated();

private:
    enum class AttemptResult { Succeeded, Failed, TimedOut };
    void runAttempt();
    void dropCurrentProcess();
    void handleFinished(int exitCode, QProcess::ExitStatus status);
    void handleStartFailure();
    void handleTimeout();
    void startProbing();
    void probeSocket();
    void finishAttempt(AttemptResult result);
    void setState(State state, const QString &message);

    QString m_socketPath;
    QString m_commandProgram;
    QStringList m_commandArguments;
    QProcess *m_process = nullptr;
    QTimer m_timeoutTimer;
    QTimer m_probeTimer;
    QTimer m_retryTimer;
    int m_attempts = 0;
    int m_maxAttempts = 3;
    int m_attemptTimeoutMilliseconds = 10000;
    int m_backoffMilliseconds = 1000;
    int m_initialBackoffMilliseconds = 1000;
    int m_probeIntervalMilliseconds = 200;
    int m_probeWindowMilliseconds = 2000;
    int m_probeElapsedMilliseconds = 0;
    int m_killGraceMilliseconds = 1000;
    State m_state = Unknown;
    QString m_message;
};
