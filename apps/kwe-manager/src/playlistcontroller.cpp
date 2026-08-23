// SPDX-License-Identifier: GPL-3.0-or-later
#include "playlistcontroller.h"

#include <QJsonArray>
#include <QJsonDocument>
#include <QJsonObject>
#include <QJsonParseError>
#include <QJsonValue>
#include <QSet>
#include <QSettings>

namespace {
constexpr qsizetype MaxStoredBytes = 4 * 1024 * 1024;
constexpr qsizetype MaxPlaylists = 256;
constexpr qsizetype MaxEntries = 1024;
constexpr qsizetype MaxNameLength = 128; // daemon playlist id bound
constexpr int MinDurationSeconds = 10;
constexpr int MaxDurationSeconds = 24 * 60 * 60;
constexpr int DefaultDurationSeconds = 5 * 60;
constexpr int MaxTransitionSeconds = 10;
const QString MigrationFlag = QStringLiteral("playlists/migrated");

bool isTransition(const QString &value) {
    return value == QStringLiteral("none") || value == QStringLiteral("crossfade");
}
}

PlaylistController::PlaylistController(QString socketPath, QObject *parent)
    : QObject(parent), m_client(std::move(socketPath)) {
    connect(&m_client, &PlaylistClient::playlistsReceived, this, &PlaylistController::onPlaylistsReceived);
    connect(&m_client, &PlaylistClient::importFinished, this,
            [this](bool ok, int, int rejected, const QString &error) {
                if (ok) {
                    QSettings settings;
                    settings.setValue(MigrationFlag, true);
                    settings.sync();
                    if (rejected > 0)
                        m_error = tr("%1 stored playlist(s) could not be migrated and remain in the settings backup.").arg(rejected);
                } else if (error.contains(QStringLiteral("playlist_import_blocked"))) {
                    // The daemon store filled up concurrently; the list
                    // refresh below is authoritative and nothing is lost.
                    QSettings settings;
                    settings.setValue(MigrationFlag, true);
                    settings.sync();
                } else {
                    m_error = error;
                    emit changed();
                    return;
                }
                refresh();
            });
    connect(&m_client, &PlaylistClient::putFinished, this, [this](bool ok, const QString &error) {
        --m_pendingEdits;
        if (ok)
            m_error.clear();
        else
            m_error = error;
        if (m_pendingEdits == 0)
            refresh(); // re-sync after the queue drained
        emit changed();
    });
    connect(&m_client, &PlaylistClient::removeFinished, this, [this](bool ok, const QString &error) {
        --m_pendingEdits;
        if (ok)
            m_error.clear();
        else
            m_error = error;
        if (m_pendingEdits == 0)
            refresh(); // re-sync after the queue drained
        emit changed();
    });
    connect(&m_client, &PlaylistClient::stateChanged, this, [this] {
        if (m_client.state() == PlaylistClient::Error)
            m_error = m_pendingEdits > 0
                ? tr("The wallpaper service is not running; playlist changes will be saved when it returns.")
                : m_client.errorMessage();
        else if (m_client.state() == PlaylistClient::Ready && m_pendingEdits == 0)
            m_error.clear();
        emit changed();
    });
    refresh();
}

// Reads the pre-M5k QSettings blob with the historical validation rules so a
// corrupt blob is reported rather than silently migrated. The blob itself is
// never modified.
QJsonArray PlaylistController::legacyBlob() {
    QSettings settings;
    const auto bytes = settings.value(QStringLiteral("playlists/data")).toByteArray();
    if (bytes.isEmpty())
        return {};
    if (bytes.size() > MaxStoredBytes) {
        m_error = tr("Stored playlists exceed the safety limit and were not migrated.");
        return {};
    }
    QJsonParseError parseError;
    const auto document = QJsonDocument::fromJson(bytes, &parseError);
    if (parseError.error != QJsonParseError::NoError || !document.isArray()
        || document.array().size() > MaxPlaylists) {
        m_error = tr("Stored playlists are invalid and were not migrated.");
        return {};
    }
    QSet<QString> names;
    for (const auto &value : document.array()) {
        if (!value.isObject()) {
            m_error = tr("Stored playlists are invalid and were not migrated.");
            return {};
        }
        const auto object = value.toObject();
        const auto name = object.value(QStringLiteral("title")).toString().trimmed();
        const auto entries = object.value(QStringLiteral("entries")).toArray();
        const int duration = object.value(QStringLiteral("duration_seconds")).toInt(DefaultDurationSeconds);
        const QString transition = object.value(QStringLiteral("transition")).toString(QStringLiteral("none"));
        const int transitionSeconds = object.value(QStringLiteral("transition_seconds")).toInt(0);
        if (name.isEmpty() || name.size() > 256 || names.contains(name)
            || entries.size() > MaxEntries
            || duration < MinDurationSeconds || duration > MaxDurationSeconds
            || !isTransition(transition)
            || transitionSeconds < 0 || transitionSeconds > MaxTransitionSeconds
            || (transition == QStringLiteral("none") && transitionSeconds != 0)) {
            m_error = tr("Stored playlists are invalid and were not migrated.");
            return {};
        }
        names.insert(name);
        QSet<QString> seen;
        for (const auto &entryValue : entries) {
            const QString entry = entryValue.toString();
            if (!entryValue.isString() || entry.isEmpty() || entry.size() > 128 || seen.contains(entry)) {
                m_error = tr("Stored playlists are invalid and were not migrated.");
                return {};
            }
            seen.insert(entry);
        }
    }
    return document.array();
}

void PlaylistController::refresh() { m_client.refresh(); }

void PlaylistController::onPlaylistsReceived(const QJsonArray &playlists) {
    QSettings settings;
    if (!settings.value(MigrationFlag).toBool()) {
        const auto legacy = legacyBlob();
        if (!legacy.isEmpty()) {
            m_client.importLegacy(legacy);
            return;
        }
        // Nothing to migrate; never ask again.
        settings.setValue(MigrationFlag, true);
        settings.sync();
    }
    // A stale list response must not wipe optimistic edits that are queued
    // behind it; the drain-triggered refresh applies the authoritative list.
    if (m_pendingEdits > 0)
        return;
    applyList(playlists);
}

void PlaylistController::applyList(const QJsonArray &playlists) {
    QStringList names;
    QHash<QString, QString> ids;
    QHash<QString, QStringList> allEntries;
    QHash<QString, bool> allShuffle;
    QHash<QString, bool> allRepeat;
    QHash<QString, int> allDurations;
    QHash<QString, QString> allTransitions;
    QHash<QString, int> allTransitionSeconds;
    bool hadInvalid = false;
    for (const auto &value : playlists) {
        if (!value.isObject()) {
            hadInvalid = true;
            continue;
        }
        const auto object = value.toObject();
        const auto title = object.value(QStringLiteral("title")).toString().trimmed();
        const auto id = object.value(QStringLiteral("id")).toString();
        // Deduplicate by title, not id: every map below is keyed by title, so
        // two same-titled playlists (possible after legacy import, where the
        // daemon suffixes ids) would otherwise collide and corrupt each other.
        // The duplicate is dropped and reported via hadInvalid.
        if (title.isEmpty() || id.isEmpty() || names.contains(title)) {
            hadInvalid = true;
            continue;
        }
        QStringList playlistEntries;
        QSet<QString> seen;
        for (const auto &entryValue : object.value(QStringLiteral("entries")).toArray()) {
            const QString entry = entryValue.toString();
            if (!entryValue.isString() || entry.isEmpty() || entry.size() > 128 || seen.contains(entry)) {
                hadInvalid = true;
                continue;
            }
            seen.insert(entry);
            playlistEntries.push_back(entry);
        }
        names.push_back(title);
        ids.insert(title, id);
        allEntries.insert(title, playlistEntries);
        allShuffle.insert(title, object.value(QStringLiteral("shuffle")).toBool(false));
        allRepeat.insert(title, object.value(QStringLiteral("repeat")).toBool(true));
        allDurations.insert(title, object.value(QStringLiteral("duration_seconds")).toInt(DefaultDurationSeconds));
        allTransitions.insert(title, object.value(QStringLiteral("transition")).toString(QStringLiteral("none")));
        allTransitionSeconds.insert(title, object.value(QStringLiteral("transition_seconds")).toInt(0));
    }
    m_names = names;
    m_ids = ids;
    m_entries = allEntries;
    m_shuffle = allShuffle;
    m_repeat = allRepeat;
    m_durationSeconds = allDurations;
    m_transition = allTransitions;
    m_transitionSeconds = allTransitionSeconds;
    m_loaded = true;
    if (hadInvalid)
        m_error = tr("Some playlists could not be shown because they are invalid.");
    emit changed();
}

QJsonObject PlaylistController::playlistObject(const QString &name) const {
    // m_names and m_ids move in lockstep (applyList, create, remove); a
    // missing id would mean a bookkeeping bug, not a legitimate fallback.
    Q_ASSERT(m_ids.contains(name));
    QJsonArray entries;
    for (const auto &entry : m_entries.value(name))
        entries.push_back(entry);
    return QJsonObject{
        {QStringLiteral("id"), m_ids.value(name, name.left(MaxNameLength))},
        {QStringLiteral("title"), name},
        {QStringLiteral("entries"), entries},
        {QStringLiteral("shuffle"), m_shuffle.value(name, false)},
        {QStringLiteral("repeat"), m_repeat.value(name, true)},
        {QStringLiteral("duration_seconds"), m_durationSeconds.value(name, DefaultDurationSeconds)},
        {QStringLiteral("transition"), m_transition.value(name, QStringLiteral("none"))},
        {QStringLiteral("transition_seconds"), m_transitionSeconds.value(name, 0)},
    };
}

void PlaylistController::create(const QString &name) {
    if (!m_loaded) {
        // Until the first list round trip the daemon store may already
        // contain playlists this controller has not seen; a blind upsert
        // could overwrite them.
        m_error = tr("Playlists are still loading; try again in a moment.");
        emit changed();
        return;
    }
    const auto clean = name.trimmed().left(MaxNameLength);
    if (clean.isEmpty() || m_names.contains(clean)) {
        m_error = tr("Playlist name is empty or already exists.");
        emit changed();
        return;
    }
    if (m_names.size() >= MaxPlaylists) {
        m_error = tr("Playlist limit reached.");
        emit changed();
        return;
    }
    m_names.push_back(clean);
    m_ids.insert(clean, clean.left(MaxNameLength));
    m_durationSeconds[clean] = DefaultDurationSeconds;
    m_transition[clean] = QStringLiteral("none");
    m_transitionSeconds[clean] = 0;
    m_error.clear();
    ++m_pendingEdits;
    m_client.putPlaylist(playlistObject(clean));
    emit changed();
}

void PlaylistController::remove(const QString &name) {
    const QString id = m_ids.value(name);
    if (!m_names.removeOne(name))
        return;
    m_ids.remove(name);
    m_entries.remove(name);
    m_shuffle.remove(name);
    m_repeat.remove(name);
    m_durationSeconds.remove(name);
    m_transition.remove(name);
    m_transitionSeconds.remove(name);
    m_error.clear();
    ++m_pendingEdits;
    m_client.removePlaylist(id);
    emit changed();
}

void PlaylistController::add(const QString &name, const QString &workshopId) {
    if (!m_names.contains(name) || workshopId.isEmpty() || workshopId.size() > 128) {
        m_error = tr("Select a valid playlist and wallpaper first.");
        emit changed();
        return;
    }
    auto &entries = m_entries[name];
    if (!entries.contains(workshopId) && entries.size() >= MaxEntries) {
        m_error = tr("Playlist entry limit reached.");
        emit changed();
        return;
    }
    if (!entries.contains(workshopId))
        entries.push_back(workshopId);
    m_error.clear();
    ++m_pendingEdits;
    m_client.putPlaylist(playlistObject(name));
    emit changed();
}

void PlaylistController::removeEntry(const QString &name, const QString &workshopId) {
    if (m_entries.contains(name) && m_entries[name].removeOne(workshopId)) {
        ++m_pendingEdits;
        m_client.putPlaylist(playlistObject(name));
        emit changed();
    }
}

void PlaylistController::setShuffle(const QString &name, bool value) {
    if (m_names.contains(name)) {
        m_shuffle[name] = value;
        ++m_pendingEdits;
        m_client.putPlaylist(playlistObject(name));
        emit changed();
    }
}

void PlaylistController::setRepeat(const QString &name, bool value) {
    if (m_names.contains(name)) {
        m_repeat[name] = value;
        ++m_pendingEdits;
        m_client.putPlaylist(playlistObject(name));
        emit changed();
    }
}

void PlaylistController::setDurationSeconds(const QString &name, int value) {
    if (!m_names.contains(name) || value < MinDurationSeconds || value > MaxDurationSeconds) {
        m_error = tr("Playlist duration must be between 10 seconds and 24 hours.");
        emit changed();
        return;
    }
    m_durationSeconds[name] = value;
    m_error.clear();
    ++m_pendingEdits;
    m_client.putPlaylist(playlistObject(name));
    emit changed();
}

void PlaylistController::setTransition(const QString &name, const QString &value) {
    if (!m_names.contains(name) || !isTransition(value)) {
        m_error = tr("Playlist transition is invalid.");
        emit changed();
        return;
    }
    m_transition[name] = value;
    if (value == QStringLiteral("none"))
        m_transitionSeconds[name] = 0;
    m_error.clear();
    ++m_pendingEdits;
    m_client.putPlaylist(playlistObject(name));
    emit changed();
}

void PlaylistController::setTransitionSeconds(const QString &name, int value) {
    if (!m_names.contains(name) || value < 0 || value > MaxTransitionSeconds
        || (m_transition.value(name, QStringLiteral("none")) == QStringLiteral("none") && value != 0)) {
        m_error = tr("Transition duration must be between 0 and 10 seconds.");
        emit changed();
        return;
    }
    m_transitionSeconds[name] = value;
    m_error.clear();
    ++m_pendingEdits;
    m_client.putPlaylist(playlistObject(name));
    emit changed();
}
