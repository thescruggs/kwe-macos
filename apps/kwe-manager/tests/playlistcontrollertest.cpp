// SPDX-License-Identifier: Apache-2.0
#include "../src/playlistcontroller.h"

#include <QCoreApplication>
#include <QJsonArray>
#include <QJsonDocument>
#include <QJsonObject>
#include <QSettings>
#include <QTemporaryDir>
#include <QtTest>

class PlaylistControllerTest final : public QObject {
    Q_OBJECT

private slots:
    void initTestCase() {
        QVERIFY(m_settingsRoot.isValid());
        QCoreApplication::setOrganizationName(QStringLiteral("KDEWallpaperEngineTests"));
        QCoreApplication::setApplicationName(QStringLiteral("PlaylistController"));
        QSettings::setDefaultFormat(QSettings::IniFormat);
        QSettings::setPath(QSettings::IniFormat, QSettings::UserScope, m_settingsRoot.path());
    }

    void init() {
        QSettings settings;
        settings.clear();
        settings.sync();
        QCOMPARE(settings.status(), QSettings::NoError);
    }

    void defaultsAndTimingRoundTrip() {
        PlaylistController controller;
        controller.create(QStringLiteral("Morning"));
        QCOMPARE(controller.durationSeconds(QStringLiteral("Morning")), 300);
        QCOMPARE(controller.transition(QStringLiteral("Morning")), QStringLiteral("none"));
        QCOMPARE(controller.transitionSeconds(QStringLiteral("Morning")), 0);

        controller.setDurationSeconds(QStringLiteral("Morning"), 900);
        controller.setTransition(QStringLiteral("Morning"), QStringLiteral("crossfade"));
        controller.setTransitionSeconds(QStringLiteral("Morning"), 4);
        QVERIFY(controller.errorMessage().isEmpty());

        PlaylistController reloaded;
        QCOMPARE(reloaded.names(), QStringList{QStringLiteral("Morning")});
        QCOMPARE(reloaded.durationSeconds(QStringLiteral("Morning")), 900);
        QCOMPARE(reloaded.transition(QStringLiteral("Morning")), QStringLiteral("crossfade"));
        QCOMPARE(reloaded.transitionSeconds(QStringLiteral("Morning")), 4);
    }

    void migratesM5fSettingsWithSafeDefaults() {
        const QJsonArray stored{QJsonObject{
            {QStringLiteral("title"), QStringLiteral("Legacy")},
            {QStringLiteral("entries"), QJsonArray{QStringLiteral("123")}},
            {QStringLiteral("shuffle"), true},
            {QStringLiteral("repeat"), false},
        }};
        QSettings settings;
        settings.setValue(QStringLiteral("playlists/data"), QJsonDocument(stored).toJson(QJsonDocument::Compact));
        settings.sync();

        PlaylistController controller;
        QCOMPARE(controller.names(), QStringList{QStringLiteral("Legacy")});
        QCOMPARE(controller.durationSeconds(QStringLiteral("Legacy")), 300);
        QCOMPARE(controller.transition(QStringLiteral("Legacy")), QStringLiteral("none"));
        QCOMPARE(controller.transitionSeconds(QStringLiteral("Legacy")), 0);
    }

    void rejectsInvalidTimingWithoutChangingValidState() {
        PlaylistController controller;
        controller.create(QStringLiteral("Evening"));
        controller.setDurationSeconds(QStringLiteral("Evening"), 9);
        QCOMPARE(controller.durationSeconds(QStringLiteral("Evening")), 300);
        QVERIFY(!controller.errorMessage().isEmpty());

        controller.setTransition(QStringLiteral("Evening"), QStringLiteral("crossfade"));
        controller.setTransitionSeconds(QStringLiteral("Evening"), 11);
        QCOMPARE(controller.transitionSeconds(QStringLiteral("Evening")), 0);
        QVERIFY(!controller.errorMessage().isEmpty());
    }

    void malformedOrDuplicateSettingsFailClosed() {
        QSettings settings;
        settings.setValue(QStringLiteral("playlists/data"), QByteArrayLiteral("not-json"));
        settings.sync();
        PlaylistController malformed;
        QVERIFY(malformed.names().isEmpty());
        QVERIFY(!malformed.errorMessage().isEmpty());

        const QJsonArray duplicateEntries{QStringLiteral("123"), QStringLiteral("123")};
        const QJsonArray stored{QJsonObject{
            {QStringLiteral("title"), QStringLiteral("Broken")},
            {QStringLiteral("entries"), duplicateEntries},
            {QStringLiteral("duration_seconds"), 300},
            {QStringLiteral("transition"), QStringLiteral("none")},
            {QStringLiteral("transition_seconds"), 0},
        }};
        settings.setValue(QStringLiteral("playlists/data"), QJsonDocument(stored).toJson(QJsonDocument::Compact));
        settings.sync();
        PlaylistController duplicate;
        QVERIFY(duplicate.names().isEmpty());
        QVERIFY(!duplicate.errorMessage().isEmpty());
    }

private:
    QTemporaryDir m_settingsRoot;
};

QTEST_GUILESS_MAIN(PlaylistControllerTest)
#include "playlistcontrollertest.moc"
