// SPDX-License-Identifier: Apache-2.0
#include "../src/playlistcontroller.h"

#include <QCoreApplication>
#include <QJsonArray>
#include <QJsonDocument>
#include <QJsonObject>
#include <QSettings>
#include <QTemporaryDir>
#include <QtTest>

// The daemon owns playlist persistence (exercised by the daemon smoke
// suites); these tests cover the controller's local, synchronous behavior:
// validation, optimistic state, and legacy-blob migration wiring. The
// socket path below never exists, so persistence attempts queue offline.
class PlaylistControllerTest final : public QObject {
    Q_OBJECT

private slots:
    void initTestCase() {
        QVERIFY(m_settingsRoot.isValid());
        QCoreApplication::setOrganizationName(QStringLiteral("KDEWallpaperEngineTests"));
        QCoreApplication::setApplicationName(QStringLiteral("PlaylistController"));
        QSettings::setDefaultFormat(QSettings::IniFormat);
        QSettings::setPath(QSettings::IniFormat, QSettings::UserScope, m_settingsRoot.path());
        m_socketPath = m_settingsRoot.path() + QStringLiteral("/missing-daemon.sock");
    }

    void init() {
        QSettings settings;
        settings.clear();
        settings.sync();
        QCOMPARE(settings.status(), QSettings::NoError);
    }

    void createAndEditLocalState() {
        PlaylistController controller(m_socketPath);
        controller.create(QStringLiteral("Morning"));
        QCOMPARE(controller.names(), QStringList{QStringLiteral("Morning")});
        QCOMPARE(controller.durationSeconds(QStringLiteral("Morning")), 300);
        QCOMPARE(controller.transition(QStringLiteral("Morning")), QStringLiteral("none"));
        QCOMPARE(controller.transitionSeconds(QStringLiteral("Morning")), 0);

        controller.setDurationSeconds(QStringLiteral("Morning"), 900);
        controller.setTransition(QStringLiteral("Morning"), QStringLiteral("crossfade"));
        controller.setTransitionSeconds(QStringLiteral("Morning"), 4);
        controller.setShuffle(QStringLiteral("Morning"), true);
        controller.setRepeat(QStringLiteral("Morning"), false);
        controller.add(QStringLiteral("Morning"), QStringLiteral("123"));
        controller.add(QStringLiteral("Morning"), QStringLiteral("123")); // dedup
        QCOMPARE(controller.entries(QStringLiteral("Morning")), QStringList{QStringLiteral("123")});
        QCOMPARE(controller.durationSeconds(QStringLiteral("Morning")), 900);
        QCOMPARE(controller.transition(QStringLiteral("Morning")), QStringLiteral("crossfade"));
        QCOMPARE(controller.transitionSeconds(QStringLiteral("Morning")), 4);

        controller.removeEntry(QStringLiteral("Morning"), QStringLiteral("123"));
        QVERIFY(controller.entries(QStringLiteral("Morning")).isEmpty());
    }

    void rejectsInvalidInputWithoutChangingState() {
        PlaylistController controller(m_socketPath);
        controller.create(QStringLiteral("Evening"));
        controller.create(QStringLiteral("Evening"));
        QCOMPARE(controller.names(), QStringList{QStringLiteral("Evening")});
        QVERIFY(!controller.errorMessage().isEmpty());

        controller.setDurationSeconds(QStringLiteral("Evening"), 9);
        QCOMPARE(controller.durationSeconds(QStringLiteral("Evening")), 300);
        QVERIFY(!controller.errorMessage().isEmpty());

        controller.setTransition(QStringLiteral("Evening"), QStringLiteral("crossfade"));
        controller.setTransitionSeconds(QStringLiteral("Evening"), 11);
        QCOMPARE(controller.transitionSeconds(QStringLiteral("Evening")), 0);
        QVERIFY(!controller.errorMessage().isEmpty());

        controller.add(QStringLiteral("Evening"), QString{});
        QVERIFY(!controller.errorMessage().isEmpty());
    }

    void removeDropsLocalState() {
        PlaylistController controller(m_socketPath);
        controller.create(QStringLiteral("Morning"));
        controller.create(QStringLiteral("Night"));
        controller.remove(QStringLiteral("Morning"));
        QCOMPARE(controller.names(), QStringList{QStringLiteral("Night")});
        controller.remove(QStringLiteral("Morning"));
        QCOMPARE(controller.names(), QStringList{QStringLiteral("Night")});
    }

    void malformedLegacyBlobIsNotReadIntoLocalState() {
        QSettings settings;
        settings.setValue(QStringLiteral("playlists/data"), QByteArrayLiteral("not-json"));
        settings.sync();
        PlaylistController controller(m_socketPath);
        // The legacy blob is only a migration source; nothing may appear
        // locally without a successful daemon round trip.
        QVERIFY(controller.names().isEmpty());
    }

private:
    QTemporaryDir m_settingsRoot;
    QString m_socketPath;
};

QTEST_GUILESS_MAIN(PlaylistControllerTest)
#include "playlistcontrollertest.moc"
