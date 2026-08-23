// SPDX-License-Identifier: GPL-3.0-or-later
#include "../src/permissionsclient.h"

#include <QCoreApplication>
#include <QJsonDocument>
#include <QJsonObject>
#include <QLocalServer>
#include <QLocalSocket>
#include <QTemporaryDir>
#include <QtTest>

// Minimal daemon stand-in: answers the permissions.* wire protocol with the
// documented daemon semantics (defaults for records that do not exist, patch
// semantics on set) so the tests exercise the real client request path. The
// daemon-side validation and persistence are covered by the daemon unit
// tests and the smoke suites.
class StubDaemon final : public QObject {
    Q_OBJECT
public:
    StubDaemon() {
        connect(&m_server, &QLocalServer::newConnection, this, &StubDaemon::onConnection);
    }
    bool listen(const QString &path) { return m_server.listen(path); }
    void reset() {
        records.clear();
        receivedMethods.clear();
        receivedParams.clear();
        failGet = false;
        holdResponses = false;
    }
    /// Records requests but never answers them, so a client stays in the
    /// loading state with its queue filling.
    bool holdResponses = false;
    /// Closes every accepted connection, simulating a daemon going away while
    /// a request is in flight.
    void dropClients() {
        for (auto *socket : std::as_const(m_clients))
            socket->close();
        m_clients.clear();
    }
    QHash<QString, QJsonObject> records;
    QList<QString> receivedMethods;
    QList<QJsonObject> receivedParams;
    bool failGet = false;

private:
    QLocalServer m_server;
    QList<QLocalSocket *> m_clients;

    void onConnection() {
        auto *socket = m_server.nextPendingConnection();
        m_clients.append(socket);
        connect(socket, &QLocalSocket::readyRead, this, [this, socket] {
            for (;;) {
                const auto newline = socket->peek(64 * 1024 + 1024).indexOf('\n');
                if (newline < 0)
                    return;
                const auto line = socket->read(newline + 1);
                const auto request = QJsonDocument::fromJson(line).object();
                const auto method = request.value(QStringLiteral("method")).toString();
                const auto params = request.value(QStringLiteral("params")).toObject();
                receivedMethods.push_back(method);
                receivedParams.push_back(params);
                if (holdResponses)
                    return;
                const auto defaults = QJsonObject{
                    {QStringLiteral("network"), false},
                    {QStringLiteral("audio"), false},
                    {QStringLiteral("pointer"), true},
                };
                QJsonObject result;
                bool ok = true;
                if (failGet) {
                    ok = false;
                    result.insert(QStringLiteral("error"), QStringLiteral("permissions_failed"));
                } else if (method == QStringLiteral("permissions.get")) {
                    const auto id = params.value(QStringLiteral("wallpaper_id")).toString();
                    result.insert(QStringLiteral("granted"), records.value(id, defaults));
                } else if (method == QStringLiteral("permissions.set")) {
                    // Mirrors the daemon's patch semantics: provided fields
                    // replace their values, omitted fields keep them, and a
                    // record without one starts from the documented defaults.
                    const auto id = params.value(QStringLiteral("wallpaper_id")).toString();
                    auto record = records.value(id, defaults);
                    for (auto it = params.constBegin(); it != params.constEnd(); ++it) {
                        if (it.key() != QStringLiteral("wallpaper_id"))
                            record.insert(it.key(), it.value());
                    }
                    records.insert(id, record);
                    result.insert(QStringLiteral("granted"), record);
                }
                const auto response = QJsonDocument(QJsonObject{
                    {QStringLiteral("version"), 1},
                    {QStringLiteral("id"), request.value(QStringLiteral("id"))},
                    {QStringLiteral("ok"), ok},
                    {QStringLiteral("result"), result},
                }).toJson(QJsonDocument::Compact) + '\n';
                socket->write(response);
                socket->flush();
            }
        });
    }
};

class PermissionsClientTest final : public QObject {
    Q_OBJECT

private slots:
    void initTestCase() {
        QVERIFY(m_settingsRoot.isValid());
        QVERIFY(m_daemon.listen(m_settingsRoot.path() + QStringLiteral("/daemon.sock")));
        m_socketPath = m_settingsRoot.path() + QStringLiteral("/daemon.sock");
    }

    void init() { m_daemon.reset(); }

    void requestPermissionsReadsTheEffectiveRecord() {
        m_daemon.records.insert(QStringLiteral("431960-123"),
                                QJsonObject{{QStringLiteral("network"), true},
                                            {QStringLiteral("audio"), false},
                                            {QStringLiteral("pointer"), true}});
        PermissionsClient client(m_socketPath);
        QSignalSpy changedSpy(&client, &PermissionsClient::grantedChanged);
        client.requestPermissions(QStringLiteral("431960-123"));
        // network and pointer both flip from the client-side unknown state.
        QTRY_VERIFY_WITH_TIMEOUT(changedSpy.count() == 2, 5000);
        const auto network = changedSpy.at(0);
        QCOMPARE(network.at(0).toString(), QStringLiteral("431960-123"));
        QCOMPARE(network.at(1).toString(), QStringLiteral("network"));
        QCOMPARE(network.at(2).toBool(), true);
        const auto pointer = changedSpy.at(1);
        QCOMPARE(pointer.at(0).toString(), QStringLiteral("431960-123"));
        QCOMPARE(pointer.at(1).toString(), QStringLiteral("pointer"));
        QCOMPARE(pointer.at(2).toBool(), true);
        QVERIFY(client.isGranted(QStringLiteral("431960-123"), QStringLiteral("network")));
        QVERIFY(!client.isGranted(QStringLiteral("431960-123"), QStringLiteral("audio")));
        QVERIFY(client.isGranted(QStringLiteral("431960-123"), QStringLiteral("pointer")));
        QVERIFY(!client.isPending(QStringLiteral("431960-123")));
    }

    void requestPermissionsReturnsTheDocumentedDefaultsWithoutARecord() {
        PermissionsClient client(m_socketPath);
        QSignalSpy pendingSpy(&client, &PermissionsClient::pendingChanged);
        client.requestPermissions(QStringLiteral("fresh"));
        // begin() emits once for the busy state; the response emits again
        // when the request completes.
        QCOMPARE(pendingSpy.count(), 1);
        QVERIFY(client.isPending(QStringLiteral("fresh")));
        QTRY_VERIFY_WITH_TIMEOUT(pendingSpy.count() == 2, 5000);
        QVERIFY(!client.isPending(QStringLiteral("fresh")));
        QVERIFY(!client.isGranted(QStringLiteral("fresh"), QStringLiteral("network")));
        QVERIFY(!client.isGranted(QStringLiteral("fresh"), QStringLiteral("audio")));
        QVERIFY(client.isGranted(QStringLiteral("fresh"), QStringLiteral("pointer")));
    }

    void setPermissionReachesTheDaemonWithPatchSemantics() {
        PermissionsClient client(m_socketPath);
        client.setPermission(QStringLiteral("431960-123"), QStringLiteral("network"), true);
        QTRY_VERIFY_WITH_TIMEOUT(client.isGranted(QStringLiteral("431960-123"),
                                                  QStringLiteral("network")),
                                 5000);
        QTRY_VERIFY(!client.isPending(QStringLiteral("431960-123")));
        QCOMPARE(m_daemon.receivedMethods.size(), 1);
        QCOMPARE(m_daemon.receivedMethods.first(), QStringLiteral("permissions.set"));
        const auto params = m_daemon.receivedParams.first();
        QCOMPARE(params.value(QStringLiteral("wallpaper_id")).toString(),
                 QStringLiteral("431960-123"));
        QCOMPARE(params.value(QStringLiteral("network")).toBool(), true);
        QVERIFY(!params.contains(QStringLiteral("audio")));
        QVERIFY(!params.contains(QStringLiteral("pointer")));
        // The stub mirrors the daemon's patch semantics: setting audio keeps
        // the stored network grant and the pointer default.
        client.setPermission(QStringLiteral("431960-123"), QStringLiteral("audio"), true);
        QTRY_VERIFY_WITH_TIMEOUT(client.isGranted(QStringLiteral("431960-123"),
                                                  QStringLiteral("audio")),
                                 5000);
        QVERIFY(client.isGranted(QStringLiteral("431960-123"), QStringLiteral("network")));
        QVERIFY(client.isGranted(QStringLiteral("431960-123"), QStringLiteral("pointer")));
        QCOMPARE(m_daemon.receivedMethods.size(), 2);
    }

    void errorsSurfaceAsMessageAndClearPending() {
        m_daemon.failGet = true;
        PermissionsClient client(m_socketPath);
        client.requestPermissions(QStringLiteral("431960-123"));
        QTRY_VERIFY_WITH_TIMEOUT(!client.errorMessage().isEmpty(), 5000);
        QVERIFY(client.errorMessage().contains(QStringLiteral("permissions_failed")));
        QVERIFY(!client.isPending(QStringLiteral("431960-123")));
        QVERIFY(!client.isGranted(QStringLiteral("431960-123"), QStringLiteral("network")));
    }

    void invalidInputIsRejectedWithoutTraffic() {
        PermissionsClient client(m_socketPath);
        client.setPermission(QStringLiteral("431960-123"), QStringLiteral("chat"), true);
        QVERIFY(!client.errorMessage().isEmpty());
        client.setPermission(QString(), QStringLiteral("network"), true);
        QVERIFY(!client.errorMessage().isEmpty());
        client.setPermission(QStringLiteral("bad id!"), QStringLiteral("network"), true);
        QVERIFY(!client.errorMessage().isEmpty());
        client.setPermission(QString(130, QLatin1Char('x')), QStringLiteral("network"), true);
        QVERIFY(!client.errorMessage().isEmpty());
        client.requestPermissions(QStringLiteral("../escape"));
        QCOMPARE(m_daemon.receivedMethods.size(), 0);
        // A valid request still works afterwards.
        client.requestPermissions(QStringLiteral("431960-123"));
        QTRY_VERIFY_WITH_TIMEOUT(!client.isPending(QStringLiteral("431960-123")), 5000);
        QCOMPARE(m_daemon.receivedMethods.size(), 1);
    }

    void queuedRequestsApplyInOrder() {
        PermissionsClient client(m_socketPath);
        client.requestPermissions(QStringLiteral("a"));
        // The first request is already in flight, so this one queues.
        client.setPermission(QStringLiteral("a"), QStringLiteral("network"), true);
        QTRY_VERIFY_WITH_TIMEOUT(m_daemon.receivedMethods.size() == 2, 5000);
        QCOMPARE(m_daemon.receivedMethods.at(0), QStringLiteral("permissions.get"));
        QCOMPARE(m_daemon.receivedMethods.at(1), QStringLiteral("permissions.set"));
        QTRY_VERIFY_WITH_TIMEOUT(client.isGranted(QStringLiteral("a"), QStringLiteral("network")),
                                 5000);
        QTRY_VERIFY(!client.isPending(QStringLiteral("a")));
    }

    void fullQueueDropRunsTheDroppedCallbackAndSurfacesTheError() {
        m_daemon.holdResponses = true;
        PermissionsClient client(m_socketPath);
        // The first call goes in flight; the rest queue while it stays
        // unanswered (only the in-flight request ever reaches the wire).
        client.setPermission(QStringLiteral("hold-1"), QStringLiteral("network"), true);
        for (int i = 2; i <= 65; ++i)
            client.setPermission(QStringLiteral("hold-%1").arg(i), QStringLiteral("network"), true);
        QTRY_VERIFY_WITH_TIMEOUT(m_daemon.receivedMethods.size() == 1, 5000);
        // 64 queued + one in flight: the 66th exceeds the bound and fails
        // immediately with a surfaced error (the queue is untouched), and the
        // queued operations show the busy state from enqueue time.
        client.setPermission(QStringLiteral("hold-66"), QStringLiteral("network"), true);
        QVERIFY(client.errorMessage().contains(QStringLiteral("too many")));
        QVERIFY(!client.isPending(QStringLiteral("hold-66")));
        QVERIFY(client.isPending(QStringLiteral("hold-2")));
        // The daemon goes away with a request in flight and the queue at the
        // bound: failCurrent re-queues the failed operation by dropping the
        // least urgent queued one — and the drop must run its callback so the
        // user's change surfaces and the pending flag is released, instead of
        // silently vanishing.
        m_daemon.dropClients();
        QTRY_VERIFY_WITH_TIMEOUT(!client.isPending(QStringLiteral("hold-65")), 5000);
        QVERIFY(client.isPending(QStringLiteral("hold-2")));
        // The re-queued operation's own flag was released too (a permanently
        // unreachable daemon must not leave the toggle busy forever).
        QVERIFY(!client.isPending(QStringLiteral("hold-1")));
        QVERIFY(client.errorMessage().contains(QStringLiteral("wallpaper service")));
    }

private:
    QTemporaryDir m_settingsRoot;
    StubDaemon m_daemon;
    QString m_socketPath;
};

QTEST_GUILESS_MAIN(PermissionsClientTest)
#include "permissionsclienttest.moc"
