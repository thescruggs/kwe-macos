// SPDX-License-Identifier: GPL-3.0-or-later
#include "../src/playlistcontroller.h"

#include <QCoreApplication>
#include <QJsonArray>
#include <QJsonDocument>
#include <QJsonObject>
#include <QLocalServer>
#include <QLocalSocket>
#include <QSettings>
#include <QTemporaryDir>
#include <QtTest>

// Minimal daemon stand-in: answers the playlist.* wire protocol so the tests
// exercise the real controller/client request path. The daemon-side
// validation semantics are covered by the daemon unit tests and smoke suites.
class StubDaemon final : public QObject {
    Q_OBJECT
public:
    StubDaemon() {
        connect(&m_server, &QLocalServer::newConnection, this, &StubDaemon::onConnection);
    }
    bool listen(const QString &path) { return m_server.listen(path); }
    QList<QJsonObject> stored; // playlist.put payloads received
    QJsonArray imported; // playlist.import payloads received
    bool failList = false;

private:
    QLocalServer m_server;

    void onConnection() {
        auto *socket = m_server.nextPendingConnection();
        connect(socket, &QLocalSocket::readyRead, this, [this, socket] {
            for (;;) {
                const auto newline = socket->peek(64 * 1024 + 1024).indexOf('\n');
                if (newline < 0)
                    return;
                const auto line = socket->read(newline + 1);
                const auto request = QJsonDocument::fromJson(line).object();
                const auto method = request.value(QStringLiteral("method")).toString();
                const auto params = request.value(QStringLiteral("params")).toObject();
                QJsonObject result;
                bool ok = true;
                if (failList) {
                    ok = false;
                    result.insert(QStringLiteral("error"), QStringLiteral("playlist_store_unavailable"));
                } else if (method == QStringLiteral("playlist.list")) {
                    QJsonArray playlists;
                    for (const auto &playlist : stored)
                        playlists.push_back(playlist);
                    result.insert(QStringLiteral("playlists"), playlists);
                } else if (method == QStringLiteral("playlist.put")) {
                    const auto playlist = params.value(QStringLiteral("playlist")).toObject();
                    const auto id = playlist.value(QStringLiteral("id")).toString();
                    bool replaced = false;
                    for (auto &existing : stored) {
                        if (existing.value(QStringLiteral("id")).toString() == id) {
                            existing = playlist;
                            replaced = true;
                            break;
                        }
                    }
                    if (!replaced)
                        stored.push_back(playlist);
                    result = playlist;
                } else if (method == QStringLiteral("playlist.remove")) {
                    const auto id = params.value(QStringLiteral("id")).toString();
                    stored.removeIf([&id](const QJsonObject &playlist) {
                        return playlist.value(QStringLiteral("id")).toString() == id;
                    });
                    result.insert(QStringLiteral("removed"), id);
                } else if (method == QStringLiteral("playlist.import")) {
                    imported = params.value(QStringLiteral("playlists")).toArray();
                    result.insert(QStringLiteral("imported"), imported.size());
                    result.insert(QStringLiteral("rejected"), 0);
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

class PlaylistControllerTest final : public QObject {
    Q_OBJECT

private slots:
    void initTestCase() {
        QVERIFY(m_settingsRoot.isValid());
        QCoreApplication::setOrganizationName(QStringLiteral("KDEWallpaperEngineTests"));
        QCoreApplication::setApplicationName(QStringLiteral("PlaylistController"));
        QSettings::setDefaultFormat(QSettings::IniFormat);
        QSettings::setPath(QSettings::IniFormat, QSettings::UserScope, m_settingsRoot.path());
        QVERIFY(m_daemon.listen(m_settingsRoot.path() + QStringLiteral("/daemon.sock")));
        m_socketPath = m_settingsRoot.path() + QStringLiteral("/daemon.sock");
    }

    void init() {
        QSettings settings;
        settings.clear();
        settings.sync();
        QCOMPARE(settings.status(), QSettings::NoError);
        m_daemon.stored.clear();
        m_daemon.imported = {};
        m_daemon.failList = false;
    }

    void createAndEditReachTheDaemon() {
        PlaylistController controller(m_socketPath);
        QSignalSpy changedSpy(&controller, &PlaylistController::changed);
        QTRY_VERIFY(changedSpy.count() >= 1); // first list round trip finished
        controller.create(QStringLiteral("Morning"));
        QCOMPARE(controller.names(), QStringList{QStringLiteral("Morning")});
        QCOMPARE(controller.durationSeconds(QStringLiteral("Morning")), 300);

        controller.setDurationSeconds(QStringLiteral("Morning"), 900);
        controller.setTransition(QStringLiteral("Morning"), QStringLiteral("crossfade"));
        controller.setTransitionSeconds(QStringLiteral("Morning"), 4);
        controller.setShuffle(QStringLiteral("Morning"), true);
        controller.add(QStringLiteral("Morning"), QStringLiteral("123"));
        controller.add(QStringLiteral("Morning"), QStringLiteral("123")); // dedup
        QCOMPARE(controller.entries(QStringLiteral("Morning")), QStringList{QStringLiteral("123")});

        // Whole-object upserts: the same playlist is put repeatedly, so the
        // stub stores exactly one definition with the latest values. Wait
        // for the final edit, not just the first put.
        QTRY_VERIFY(m_daemon.stored.size() == 1
                    && m_daemon.stored.first().value(QStringLiteral("duration_seconds")).toInt() == 900);
        const auto stored = m_daemon.stored.first();
        QCOMPARE(stored.value(QStringLiteral("id")).toString(), QStringLiteral("Morning"));
        QCOMPARE(stored.value(QStringLiteral("duration_seconds")).toInt(), 900);
        QCOMPARE(stored.value(QStringLiteral("transition")).toString(), QStringLiteral("crossfade"));
        QCOMPARE(stored.value(QStringLiteral("shuffle")).toBool(), true);
        QCOMPARE(stored.value(QStringLiteral("entries")).toArray().size(), 1);

        controller.removeEntry(QStringLiteral("Morning"), QStringLiteral("123"));
        QVERIFY(controller.entries(QStringLiteral("Morning")).isEmpty());
        controller.remove(QStringLiteral("Morning"));
        QCOMPARE(controller.names(), QStringList{});
        QTRY_VERIFY(m_daemon.stored.isEmpty());
    }

    void rejectsInvalidInputWithoutChangingState() {
        PlaylistController controller(m_socketPath);
        QSignalSpy changedSpy(&controller, &PlaylistController::changed);
        QTRY_VERIFY(changedSpy.count() >= 1); // first list round trip finished
        controller.create(QStringLiteral("Evening"));
        controller.create(QStringLiteral("Evening"));
        QCOMPARE(controller.names(), QStringList{QStringLiteral("Evening")});
        QVERIFY(!controller.errorMessage().isEmpty());

        controller.setDurationSeconds(QStringLiteral("Evening"), 9);
        QCOMPARE(controller.durationSeconds(QStringLiteral("Evening")), 300);

        controller.setTransition(QStringLiteral("Evening"), QStringLiteral("crossfade"));
        controller.setTransitionSeconds(QStringLiteral("Evening"), 11);
        QCOMPARE(controller.transitionSeconds(QStringLiteral("Evening")), 0);

        controller.add(QStringLiteral("Evening"), QString{});
        QVERIFY(!controller.errorMessage().isEmpty());
    }

    void migratesLegacyBlobOnce() {
        const QJsonArray legacy{QJsonObject{
            {QStringLiteral("title"), QStringLiteral("Legacy")},
            {QStringLiteral("entries"), QJsonArray{QStringLiteral("123")}},
            {QStringLiteral("shuffle"), true},
            {QStringLiteral("repeat"), false},
        }};
        QSettings settings;
        settings.setValue(QStringLiteral("playlists/data"),
                          QJsonDocument(legacy).toJson(QJsonDocument::Compact));
        settings.sync();

        PlaylistController controller(m_socketPath);
        QTRY_VERIFY(!m_daemon.imported.isEmpty());
        QCOMPARE(m_daemon.imported.size(), 1);
        QTRY_VERIFY(settings.value(QStringLiteral("playlists/migrated")).toBool());

        // The blob remains untouched as a backup.
        QCOMPARE(settings.value(QStringLiteral("playlists/data")).toByteArray(),
                 QJsonDocument(legacy).toJson(QJsonDocument::Compact));
    }

    void malformedLegacyBlobIsNotMigrated() {
        QSettings settings;
        settings.setValue(QStringLiteral("playlists/data"), QByteArrayLiteral("not-json"));
        settings.sync();
        PlaylistController controller(m_socketPath);
        QTRY_VERIFY(settings.value(QStringLiteral("playlists/migrated")).toBool());
        QVERIFY(m_daemon.imported.isEmpty());
        QVERIFY(!controller.errorMessage().isEmpty());
    }

    void corruptStoreErrorReachesTheController() {
        m_daemon.failList = true;
        PlaylistController controller(m_socketPath);
        QTRY_VERIFY_WITH_TIMEOUT(controller.errorMessage().contains(QStringLiteral("playlist_store_unavailable")), 10000);
    }

private:
    QTemporaryDir m_settingsRoot;
    StubDaemon m_daemon;
    QString m_socketPath;
};

QTEST_GUILESS_MAIN(PlaylistControllerTest)
#include "playlistcontrollertest.moc"
