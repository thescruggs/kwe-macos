// SPDX-License-Identifier: Apache-2.0
#include "../src/daemonactivator.h"

#include <QFile>
#include <QSignalSpy>
#include <QTemporaryDir>
#include <QtTest>

#include <errno.h>
#include <signal.h>
#include <unistd.h>

namespace {
constexpr int ShortTimeoutMilliseconds = 2000;

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
}

// The activation decision logic: probe the socket, activate only when it is
// absent, bound the attempts and the command timeout, and reach an actionable
// failure state instead of an error storm. Stub scripts stand in for
// systemctl, so the tests need no systemd.
class DaemonActivatorTest final : public QObject {
    Q_OBJECT

private slots:
    void activateWhenAbsent();
    void noActivateWhenPresent();
    void boundedAttemptsThenFailure();
    void timeoutKillsProcess();
    void manualRetryAfterFailure();
};

void DaemonActivatorTest::activateWhenAbsent() {
    QTemporaryDir dir;
    QVERIFY(dir.isValid());
    const QString socketPath = dir.filePath(QStringLiteral("daemon.sock"));
    const QString runsPath = dir.filePath(QStringLiteral("runs"));
    const QString sawSocketPath = dir.filePath(QStringLiteral("saw-socket"));
    const QString script = dir.filePath(QStringLiteral("activate.sh"));
    writeStub(script,
              QStringLiteral("#!/usr/bin/env bash\n"
                             "set -euo pipefail\n"
                             "echo run >> %1\n"
                             "echo \"$KWE_DAEMON_SOCKET\" > %2\n"
                             "touch \"$KWE_DAEMON_SOCKET\"\n")
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
    // The probe only checks existence; a plain file stands in for the socket.
    QFile socketFile(socketPath);
    QVERIFY(socketFile.open(QIODevice::WriteOnly));
    socketFile.close();
    const QString runsPath = dir.filePath(QStringLiteral("runs"));
    const QString script = dir.filePath(QStringLiteral("activate.sh"));
    writeStub(script, QStringLiteral("#!/usr/bin/env bash\nset -euo pipefail\necho run >> %1\n")
                          .arg(runsPath));

    DaemonActivator activator(socketPath, script, {});
    activator.activate();

    QCOMPARE(activator.state(), DaemonActivator::Running);
    QTest::qWait(300);
    QCOMPARE(activator.state(), DaemonActivator::Running);
    // The socket was already there, so the command must never have run.
    QVERIFY(!QFile::exists(runsPath));
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
    // now-working command can still succeed.
    writeStub(script,
              QStringLiteral("#!/usr/bin/env bash\n"
                             "set -euo pipefail\n"
                             "echo run >> %1\n"
                             "touch %2\n")
                  .arg(runsPath, socketPath));
    QSignalSpy activatedSpy(&activator, &DaemonActivator::activated);

    activator.activate();

    QVERIFY(activatedSpy.wait(ShortTimeoutMilliseconds));
    QCOMPARE(activator.state(), DaemonActivator::Running);
    QCOMPARE(readFileText(runsPath).count(QLatin1String("run")), 3);
}

QTEST_GUILESS_MAIN(DaemonActivatorTest)
#include "daemonactivatortest.moc"
