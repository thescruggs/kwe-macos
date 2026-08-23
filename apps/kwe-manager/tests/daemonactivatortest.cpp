// SPDX-License-Identifier: GPL-3.0-or-later
#include "../src/daemonactivator.h"

#include <QLocalServer>
#include <QFile>
#include <QSignalSpy>
#include <QTemporaryDir>
#include <QtTest>

#include <errno.h>
#include <signal.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <unistd.h>

namespace {
constexpr int ShortTimeoutMilliseconds = 3000;

QString readFileText(const QString &path) {
    QFile file(path);
    if (!file.open(QIODevice::ReadOnly))
        return {};
    return QString::fromUtf8(file.readAll());
}

void writeStub(const QString &path, const QString &body) {
    QFile file(path);
    QVERIFY2(file.open(QIODevice::WriteOnly), qPrintable(file.errorString()));
    file.write(body.toUtf8());
    file.close();
    QVERIFY2(QFile::setPermissions(path, QFile::ExeOwner | QFile::ExeGroup | QFile::ExeOther |
                                              QFile::ReadOwner | QFile::WriteOwner),
             qPrintable(QStringLiteral("could not chmod %1").arg(path)));
}

bool processIsGone(pid_t pid) { return kill(pid, 0) != 0 && errno == ESRCH; }

// Leaves a genuine Unix socket file behind with no listener — exactly what a
// hard-killed daemon (kill -9 / StartLimitExceeded) leaves in the runtime
// directory: bind() without unlink on close.
void leaveStaleSocket(const QString &path) {
    const QByteArray pathBytes = path.toLocal8Bit();
    QVERIFY2(pathBytes.size() < static_cast<int>(sizeof(sockaddr_un::sun_path)),
             qPrintable(QStringLiteral("socket path too long: %1").arg(path)));
    const int fd = ::socket(AF_UNIX, SOCK_STREAM, 0);
    QVERIFY2(fd >= 0, "socket() failed");
    sockaddr_un address;
    std::memset(&address, 0, sizeof(address));
    address.sun_family = AF_UNIX;
    std::strncpy(address.sun_path, pathBytes.constData(), sizeof(address.sun_path) - 1);
    QVERIFY2(::bind(fd, reinterpret_cast<sockaddr *>(&address), sizeof(address)) == 0,
             "bind() failed");
    ::close(fd);
}
}

// The activation decision logic: probe the daemon socket with a live connect
// (a stale socket file must not read as running), activate only when it is
// absent, bound the attempts and the command timeout, and reach an actionable
// failure state instead of an error storm. Stub scripts stand in for
// systemctl, so the tests need no systemd.
class DaemonActivatorTest final : public QObject {
    Q_OBJECT

private slots:
    void activateWhenAbsent();
    void noActivateWhenPresent();
    void staleSocketActivates();
    void staleSocketDuringProbeRetries();
    void probeExpiryRetries();
    void boundedAttemptsThenFailure();
    void timeoutKillsProcess();
    void duplicateActivateWhileInFlight();
    void manualRetryAfterFailure();
};

void DaemonActivatorTest::activateWhenAbsent() {
    QTemporaryDir dir;
    QVERIFY(dir.isValid());
    const QString socketPath = dir.filePath(QStringLiteral("daemon.sock"));
    const QString runsPath = dir.filePath(QStringLiteral("runs"));
    const QString sawSocketPath = dir.filePath(QStringLiteral("saw-socket"));
    const QString script = dir.filePath(QStringLiteral("activate.sh"));
    // The stub plays systemctl against a stand-in daemon: a short-lived
    // Python listener on the manager's socket (bound via the KWE_DAEMON_SOCKET
    // contract), so the liveness probe can actually connect.
    writeStub(script,
              QStringLiteral("#!/usr/bin/env bash\n"
                             "set -euo pipefail\n"
                             "echo run >> %1\n"
                             "echo \"$KWE_DAEMON_SOCKET\" > %2\n"
                             "python3 -c \"import socket,time; s=socket.socket(socket.AF_UNIX);"
                             " s.bind('$KWE_DAEMON_SOCKET'); s.listen(1); time.sleep(2)\" &\n")
                  .arg(runsPath, sawSocketPath));

    DaemonActivator activator(socketPath, script, {});
    activator.setBackoffMilliseconds(10);
    activator.setSocketProbeMilliseconds(10, 1000);
    QSignalSpy activatedSpy(&activator, &DaemonActivator::activated);

    activator.activate();

    QVERIFY(activatedSpy.wait(ShortTimeoutMilliseconds));
    QCOMPARE(activator.state(), DaemonActivator::Running);
    // The command ran exactly once and received the manager's socket path.
    QCOMPARE(readFileText(runsPath).count(QLatin1String("run")), 1);
    QCOMPARE(readFileText(sawSocketPath).trimmed(), socketPath);
}

void DaemonActivatorTest::noActivateWhenPresent() {
    QTemporaryDir dir;
    QVERIFY(dir.isValid());
    const QString socketPath = dir.filePath(QStringLiteral("daemon.sock"));
    // A live daemon: the probe must connect, see the listener, and skip the
    // activation command entirely.
    QLocalServer server;
    QVERIFY(server.listen(socketPath));
    const QString runsPath = dir.filePath(QStringLiteral("runs"));
    const QString script = dir.filePath(QStringLiteral("activate.sh"));
    writeStub(script, QStringLiteral("#!/usr/bin/env bash\nset -euo pipefail\necho run >> %1\n")
                          .arg(runsPath));

    DaemonActivator activator(socketPath, script, {});
    activator.activate();

    QCOMPARE(activator.state(), DaemonActivator::Running);
    QTest::qWait(300);
    QCOMPARE(activator.state(), DaemonActivator::Running);
    // The daemon was already live, so the command must never have run.
    QVERIFY(!QFile::exists(runsPath));
}

void DaemonActivatorTest::staleSocketActivates() {
    QTemporaryDir dir;
    QVERIFY(dir.isValid());
    const QString socketPath = dir.filePath(QStringLiteral("daemon.sock"));
    // A hard-killed daemon leaves its socket file behind; the probe must
    // treat "file present, nothing listening" as absent and run activation.
    leaveStaleSocket(socketPath);
    QVERIFY(QFile::exists(socketPath));
    const QString runsPath = dir.filePath(QStringLiteral("runs"));
    const QString script = dir.filePath(QStringLiteral("activate.sh"));
    writeStub(script, QStringLiteral("#!/usr/bin/env bash\nset -euo pipefail\necho run >> %1\nexit 1\n")
                          .arg(runsPath));

    DaemonActivator activator(socketPath, script, {});
    activator.setBackoffMilliseconds(10);
    activator.setMaxAttempts(1);

    activator.activate();

    QTRY_VERIFY_WITH_TIMEOUT(activator.state() == DaemonActivator::Failed,
                             ShortTimeoutMilliseconds);
    QCOMPARE(readFileText(runsPath).count(QLatin1String("run")), 1);
}

void DaemonActivatorTest::staleSocketDuringProbeRetries() {
    QTemporaryDir dir;
    QVERIFY(dir.isValid());
    const QString socketPath = dir.filePath(QStringLiteral("daemon.sock"));
    const QString runsPath = dir.filePath(QStringLiteral("runs"));
    const QString script = dir.filePath(QStringLiteral("activate.sh"));
    // The activation command "succeeds" but the daemon it spawned is dead:
    // only a socket file remains, nothing listening. The liveness probe must
    // not accept it, and the bounded retry loop must exhaust.
    writeStub(script,
              QStringLiteral("#!/usr/bin/env bash\n"
                             "set -euo pipefail\n"
                             "echo run >> %1\n"
                             "mkdir -p \"$(dirname %2)\"\n"
                             "touch %2\n"
                             "exit 0\n")
                  .arg(runsPath, socketPath));

    DaemonActivator activator(socketPath, script, {});
    activator.setBackoffMilliseconds(10);
    activator.setSocketProbeMilliseconds(10, 50);
    activator.setMaxAttempts(3);

    activator.activate();

    QTRY_VERIFY_WITH_TIMEOUT(activator.state() == DaemonActivator::Failed,
                             ShortTimeoutMilliseconds);
    QCOMPARE(readFileText(runsPath).count(QLatin1String("run")), 3);
}

void DaemonActivatorTest::probeExpiryRetries() {
    QTemporaryDir dir;
    QVERIFY(dir.isValid());
    const QString runsPath = dir.filePath(QStringLiteral("runs"));
    const QString script = dir.filePath(QStringLiteral("activate.sh"));
    // The command claims success but the daemon never appears; the bounded
    // probe window must count as a failed attempt and retry.
    writeStub(script, QStringLiteral("#!/usr/bin/env bash\nset -euo pipefail\necho run >> %1\nexit 0\n")
                          .arg(runsPath));

    DaemonActivator activator(dir.filePath(QStringLiteral("daemon.sock")), script, {});
    activator.setBackoffMilliseconds(10);
    activator.setSocketProbeMilliseconds(10, 50);
    activator.setMaxAttempts(3);

    activator.activate();

    QTRY_VERIFY_WITH_TIMEOUT(activator.state() == DaemonActivator::Failed,
                             ShortTimeoutMilliseconds);
    QCOMPARE(readFileText(runsPath).count(QLatin1String("run")), 3);
}

void DaemonActivatorTest::boundedAttemptsThenFailure() {
    QTemporaryDir dir;
    QVERIFY(dir.isValid());
    const QString runsPath = dir.filePath(QStringLiteral("runs"));
    const QString script = dir.filePath(QStringLiteral("fail.sh"));
    writeStub(script, QStringLiteral("#!/usr/bin/env bash\nset -euo pipefail\necho run >> %1\nexit 1\n")
                          .arg(runsPath));

    DaemonActivator activator(dir.filePath(QStringLiteral("daemon.sock")), script, {});
    activator.setBackoffMilliseconds(10);
    activator.setMaxAttempts(3);

    activator.activate();

    QTRY_VERIFY_WITH_TIMEOUT(activator.state() == DaemonActivator::Failed,
                             ShortTimeoutMilliseconds);
    // Exactly three bounded attempts, then one actionable failure message.
    QCOMPARE(readFileText(runsPath).count(QLatin1String("run")), 3);
    QVERIFY(activator.message().contains(QStringLiteral("systemctl --user start kwe-daemon")));
}

void DaemonActivatorTest::timeoutKillsProcess() {
    QTemporaryDir dir;
    QVERIFY(dir.isValid());
    const QString pidPath = dir.filePath(QStringLiteral("pid"));
    const QString script = dir.filePath(QStringLiteral("hang.sh"));
    // Ignore SIGTERM so the grace-period SIGKILL path is exercised; the pid
    // lets the test prove the child is actually dead.
    writeStub(script, QStringLiteral("#!/usr/bin/env bash\n"
                                     "set -euo pipefail\n"
                                     "echo $$ > %1\n"
                                     "trap '' TERM\n"
                                     "while true; do sleep 1; done\n")
                          .arg(pidPath));

    DaemonActivator activator(dir.filePath(QStringLiteral("daemon.sock")), script, {});
    activator.setAttemptTimeoutMilliseconds(200);
    activator.setBackoffMilliseconds(10);
    activator.setMaxAttempts(1);

    activator.activate();

    QTRY_VERIFY_WITH_TIMEOUT(activator.state() == DaemonActivator::Failed,
                             ShortTimeoutMilliseconds);
    bool pidOk = false;
    const pid_t childPid = readFileText(pidPath).trimmed().toInt(&pidOk);
    QVERIFY(pidOk);
    QVERIFY(childPid > 0);
    // The bounded timeout killed the hung activation command.
    QTRY_VERIFY_WITH_TIMEOUT(processIsGone(childPid), ShortTimeoutMilliseconds);
}

void DaemonActivatorTest::duplicateActivateWhileInFlight() {
    QTemporaryDir dir;
    QVERIFY(dir.isValid());
    const QString runsPath = dir.filePath(QStringLiteral("runs"));
    const QString script = dir.filePath(QStringLiteral("hang.sh"));
    writeStub(script, QStringLiteral("#!/usr/bin/env bash\n"
                                     "set -euo pipefail\n"
                                     "echo run >> %1\n"
                                     "while true; do sleep 1; done\n")
                          .arg(runsPath));

    DaemonActivator activator(dir.filePath(QStringLiteral("daemon.sock")), script, {});
    activator.setAttemptTimeoutMilliseconds(200);
    activator.setBackoffMilliseconds(10);
    activator.setMaxAttempts(1);

    activator.activate();
    QCOMPARE(activator.state(), DaemonActivator::Activating);
    // A second call while a run is in flight must not stack activations.
    activator.activate();
    QCOMPARE(activator.state(), DaemonActivator::Activating);

    QTRY_VERIFY_WITH_TIMEOUT(activator.state() == DaemonActivator::Failed,
                             ShortTimeoutMilliseconds);
    QCOMPARE(readFileText(runsPath).count(QLatin1String("run")), 1);
}

void DaemonActivatorTest::manualRetryAfterFailure() {
    QTemporaryDir dir;
    QVERIFY(dir.isValid());
    const QString socketPath = dir.filePath(QStringLiteral("daemon.sock"));
    const QString runsPath = dir.filePath(QStringLiteral("runs"));
    const QString script = dir.filePath(QStringLiteral("activate.sh"));
    writeStub(script, QStringLiteral("#!/usr/bin/env bash\nset -euo pipefail\necho run >> %1\nexit 1\n")
                          .arg(runsPath));

    DaemonActivator activator(socketPath, script, {});
    activator.setBackoffMilliseconds(10);
    activator.setMaxAttempts(2);

    activator.activate();
    QTRY_VERIFY_WITH_TIMEOUT(activator.state() == DaemonActivator::Failed,
                             ShortTimeoutMilliseconds);
    QCOMPARE(readFileText(runsPath).count(QLatin1String("run")), 2);

    // The QML retry action calls activate() again; the budget resets and a
    // now-working command can still succeed (the stand-in daemon is a
    // short-lived Python listener, as in activateWhenAbsent).
    writeStub(script,
              QStringLiteral("#!/usr/bin/env bash\n"
                             "set -euo pipefail\n"
                             "echo run >> %1\n"
                             "python3 -c \"import socket,time; s=socket.socket(socket.AF_UNIX);"
                             " s.bind('$KWE_DAEMON_SOCKET'); s.listen(1); time.sleep(2)\" &\n")
                  .arg(runsPath));
    QSignalSpy activatedSpy(&activator, &DaemonActivator::activated);

    activator.activate();

    QVERIFY(activatedSpy.wait(ShortTimeoutMilliseconds));
    QCOMPARE(activator.state(), DaemonActivator::Running);
    QCOMPARE(readFileText(runsPath).count(QLatin1String("run")), 3);
}

QTEST_GUILESS_MAIN(DaemonActivatorTest)
#include "daemonactivatortest.moc"
