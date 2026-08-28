// SPDX-License-Identifier: GPL-3.0-or-later
#include "../src/rendererstatus.h"

#include <QJsonArray>
#include <QJsonDocument>
#include <QJsonObject>
#include <QLocalServer>
#include <QLocalSocket>
#include <QTemporaryDir>
#include <QtTest>

// Minimal daemon stand-in for `renderer.status` (docs/SUPERVISOR_API_V1.md):
// answers whatever `result` object is currently configured, so a test can
// change it between polls. RendererStatus polls on a timer and once eagerly
// from its constructor; the tests below only need that first poll.
class StubStatusDaemon final : public QObject {
    Q_OBJECT
public:
    StubStatusDaemon() {
        connect(&m_server, &QLocalServer::newConnection, this, &StubStatusDaemon::onConnection);
    }
    bool listen(const QString &path) { return m_server.listen(path); }
    QJsonObject result = QJsonObject{{QStringLiteral("phase"), QStringLiteral("unknown")}};

private:
    QLocalServer m_server;

    void onConnection() {
        auto *socket = m_server.nextPendingConnection();
        connect(socket, &QLocalSocket::readyRead, this, [this, socket] {
            const auto newline = socket->peek(64 * 1024).indexOf('\n');
            if (newline < 0)
                return;
            socket->read(newline + 1);
            const auto response = QJsonDocument(QJsonObject{
                {QStringLiteral("version"), 1},
                {QStringLiteral("ok"), true},
                {QStringLiteral("result"), result},
            }).toJson(QJsonDocument::Compact) + '\n';
            socket->write(response);
            socket->flush();
        });
        connect(socket, &QLocalSocket::disconnected, socket, &QObject::deleteLater);
    }
};

// SR-1e: capability_limitations (SR-1c) is the newest field in the same
// daemon-json -> C++ member -> notify -> QML-property flow phase/
// wallpaperId/detail already use; these tests mirror that existing
// status-field flow for the new one.
class RendererStatusTest final : public QObject {
    Q_OBJECT

private slots:
    void initTestCase() {
        QVERIFY(m_settingsRoot.isValid());
        QVERIFY(m_daemon.listen(m_settingsRoot.path() + QStringLiteral("/daemon.sock")));
        m_socketPath = m_settingsRoot.path() + QStringLiteral("/daemon.sock");
    }

    void capabilityLimitationsRoundTripsFromTheStatusJson() {
        m_daemon.result = QJsonObject{
            {QStringLiteral("phase"), QStringLiteral("live")},
            {QStringLiteral("wallpaper_id"), QStringLiteral("1")},
            {QStringLiteral("capability_limitations"),
             QJsonArray{QStringLiteral("scene.layer.sound"), QStringLiteral("scene.lighting")}},
        };
        RendererStatus status(m_socketPath);
        const QStringList expected{QStringLiteral("scene.layer.sound"),
                                    QStringLiteral("scene.lighting")};
        QTRY_VERIFY_WITH_TIMEOUT(status.capabilityLimitations() == expected, 5000);
        QCOMPARE(status.phase(), QStringLiteral("live"));
    }

    void capabilityLimitationsIsEmptyWhenTheFieldIsAbsent() {
        m_daemon.result = QJsonObject{
            {QStringLiteral("phase"), QStringLiteral("idle")},
            {QStringLiteral("wallpaper_id"), QStringLiteral("2")},
        };
        RendererStatus status(m_socketPath);
        QTRY_VERIFY_WITH_TIMEOUT(status.wallpaperId() == QStringLiteral("2"), 5000);
        QVERIFY(status.capabilityLimitations().isEmpty());
    }

private:
    QTemporaryDir m_settingsRoot;
    StubStatusDaemon m_daemon;
    QString m_socketPath;
};

QTEST_GUILESS_MAIN(RendererStatusTest)
#include "rendererstatustest.moc"
