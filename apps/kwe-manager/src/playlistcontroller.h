// SPDX-License-Identifier: Apache-2.0
#pragma once

#include "playlistclient.h"

#include <QHash>
#include <QJsonArray>
#include <QObject>
#include <QStringList>

// Playlist model for the manager UI. The daemon owns the playlist store;
// this controller applies edits optimistically and persists them through
// PlaylistClient, so the QML surface stays unchanged. The pre-M5k QSettings
// blob is only a one-time migration source and is never written again.
class PlaylistController final : public QObject {
    Q_OBJECT
    Q_PROPERTY(QStringList names READ names NOTIFY changed)
    Q_PROPERTY(QString errorMessage READ errorMessage NOTIFY changed)
public:
    explicit PlaylistController(QString socketPath, QObject *parent = nullptr);
    QStringList names() const { return m_names; }
    QString errorMessage() const { return m_error; }
    Q_INVOKABLE void create(const QString &name);
    Q_INVOKABLE void remove(const QString &name);
    Q_INVOKABLE void add(const QString &name, const QString &workshopId);
    Q_INVOKABLE void removeEntry(const QString &name, const QString &workshopId);
    Q_INVOKABLE QStringList entries(const QString &name) const { return m_entries.value(name); }
    Q_INVOKABLE bool shuffle(const QString &name) const { return m_shuffle.value(name, false); }
    Q_INVOKABLE bool repeat(const QString &name) const { return m_repeat.value(name, true); }
    Q_INVOKABLE int durationSeconds(const QString &name) const { return m_durationSeconds.value(name, 300); }
    Q_INVOKABLE QString transition(const QString &name) const { return m_transition.value(name, QStringLiteral("none")); }
    Q_INVOKABLE int transitionSeconds(const QString &name) const { return m_transitionSeconds.value(name, 0); }
    Q_INVOKABLE void setShuffle(const QString &name, bool value);
    Q_INVOKABLE void setRepeat(const QString &name, bool value);
    Q_INVOKABLE void setDurationSeconds(const QString &name, int value);
    Q_INVOKABLE void setTransition(const QString &name, const QString &value);
    Q_INVOKABLE void setTransitionSeconds(const QString &name, int value);
signals:
    void changed();

private:
    QJsonArray legacyBlob();
    void onPlaylistsReceived(const QJsonArray &playlists);
    void applyList(const QJsonArray &playlists);
    QJsonObject playlistObject(const QString &name) const;
    void refresh();

    PlaylistClient m_client;
    QStringList m_names;
    QHash<QString, QString> m_ids; // title -> daemon id
    QHash<QString, QStringList> m_entries;
    QHash<QString, bool> m_shuffle;
    QHash<QString, bool> m_repeat;
    QHash<QString, int> m_durationSeconds;
    QHash<QString, QString> m_transition;
    QHash<QString, int> m_transitionSeconds;
    QString m_error;
    int m_pendingEdits = 0;
};
