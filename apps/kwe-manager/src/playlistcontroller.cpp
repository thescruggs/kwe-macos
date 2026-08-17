// SPDX-License-Identifier: Apache-2.0
#include "playlistcontroller.h"
#include <QJsonArray>
#include <QJsonDocument>
#include <QJsonObject>
#include <QJsonParseError>
#include <QSet>
#include <QSettings>

namespace {
constexpr qsizetype MaxStoredBytes = 4 * 1024 * 1024;
constexpr qsizetype MaxPlaylists = 256;
constexpr qsizetype MaxEntries = 1024;
constexpr int MinDurationSeconds = 10;
constexpr int MaxDurationSeconds = 24 * 60 * 60;
constexpr int DefaultDurationSeconds = 5 * 60;
constexpr int MaxTransitionSeconds = 10;

bool isTransition(const QString &value) {
    return value == QStringLiteral("none") || value == QStringLiteral("crossfade");
}
}

PlaylistController::PlaylistController(QObject *parent) : QObject(parent) { load(); }
void PlaylistController::load() {
    QSettings settings;
    const auto bytes = settings.value(QStringLiteral("playlists/data")).toByteArray();
    if (bytes.isEmpty()) return;
    if (bytes.size() > MaxStoredBytes) {
        m_error = tr("Stored playlists exceed the safety limit and were not loaded.");
        return;
    }
    QJsonParseError parseError;
    const auto document = QJsonDocument::fromJson(bytes, &parseError);
    if (parseError.error != QJsonParseError::NoError || !document.isArray()
        || document.array().size() > MaxPlaylists) {
        m_error = tr("Stored playlists are invalid and were not loaded.");
        return;
    }

    QStringList names;
    QHash<QString, QStringList> allEntries;
    QHash<QString, bool> allShuffle;
    QHash<QString, bool> allRepeat;
    QHash<QString, int> allDurations;
    QHash<QString, QString> allTransitions;
    QHash<QString, int> allTransitionSeconds;
    for (const auto &value : document.array()) {
        if (!value.isObject()) {
            m_error = tr("Stored playlists are invalid and were not loaded.");
            return;
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
            m_error = tr("Stored playlists are invalid and were not loaded.");
            return;
        }
        QStringList playlistEntries;
        QSet<QString> seen;
        for (const auto &entryValue : entries) {
            const QString entry = entryValue.toString();
            if (!entryValue.isString() || entry.isEmpty() || entry.size() > 128 || seen.contains(entry)) {
                m_error = tr("Stored playlists are invalid and were not loaded.");
                return;
            }
            seen.insert(entry);
            playlistEntries.push_back(entry);
        }
        names.push_back(name);
        allEntries.insert(name, playlistEntries);
        allShuffle.insert(name, object.value(QStringLiteral("shuffle")).toBool(false));
        allRepeat.insert(name, object.value(QStringLiteral("repeat")).toBool(true));
        allDurations.insert(name, duration);
        allTransitions.insert(name, transition);
        allTransitionSeconds.insert(name, transitionSeconds);
    }
    m_names = names;
    m_entries = allEntries;
    m_shuffle = allShuffle;
    m_repeat = allRepeat;
    m_durationSeconds = allDurations;
    m_transition = allTransitions;
    m_transitionSeconds = allTransitionSeconds;
    m_error.clear();
}
void PlaylistController::save() {
    QJsonArray array;
    for (const auto &name : m_names) {
        QJsonArray entries;
        for (const auto &entry : m_entries.value(name)) entries.push_back(entry);
        array.push_back(QJsonObject{
            {QStringLiteral("title"), name},
            {QStringLiteral("entries"), entries},
            {QStringLiteral("shuffle"), m_shuffle.value(name, false)},
            {QStringLiteral("repeat"), m_repeat.value(name, true)},
            {QStringLiteral("duration_seconds"), m_durationSeconds.value(name, DefaultDurationSeconds)},
            {QStringLiteral("transition"), m_transition.value(name, QStringLiteral("none"))},
            {QStringLiteral("transition_seconds"), m_transitionSeconds.value(name, 0)},
        });
    }
    const auto bytes = QJsonDocument(array).toJson(QJsonDocument::Compact);
    if (bytes.size() > MaxStoredBytes) {
        m_error = tr("Playlist data exceeds the safety limit and was not saved.");
        return;
    }
    QSettings settings;
    settings.setValue(QStringLiteral("playlists/data"), bytes);
    settings.sync();
    if (settings.status() != QSettings::NoError)
        m_error = tr("Playlist settings could not be saved.");
}
void PlaylistController::create(const QString &name) {
    const auto clean = name.trimmed().left(256);
    if (clean.isEmpty() || m_names.contains(clean)) { m_error = tr("Playlist name is empty or already exists."); emit changed(); return; }
    if (m_names.size() >= MaxPlaylists) { m_error = tr("Playlist limit reached."); emit changed(); return; }
    m_names.push_back(clean);
    m_durationSeconds[clean] = DefaultDurationSeconds;
    m_transition[clean] = QStringLiteral("none");
    m_transitionSeconds[clean] = 0;
    m_error.clear(); save(); emit changed();
}
void PlaylistController::remove(const QString &name) {
    if (m_names.removeOne(name)) {
        m_entries.remove(name);
        m_shuffle.remove(name);
        m_repeat.remove(name);
        m_durationSeconds.remove(name);
        m_transition.remove(name);
        m_transitionSeconds.remove(name);
        m_error.clear(); save(); emit changed();
    }
}
void PlaylistController::add(const QString &name, const QString &workshopId) {
    if (!m_names.contains(name) || workshopId.isEmpty() || workshopId.size() > 128) { m_error = tr("Select a valid playlist and wallpaper first."); emit changed(); return; }
    auto &entries = m_entries[name];
    if (!entries.contains(workshopId) && entries.size() >= MaxEntries) { m_error = tr("Playlist entry limit reached."); emit changed(); return; }
    if (!entries.contains(workshopId)) entries.push_back(workshopId);
    m_error.clear(); save(); emit changed();
}
void PlaylistController::removeEntry(const QString &name, const QString &workshopId) {
    if (m_entries.contains(name) && m_entries[name].removeOne(workshopId)) { save(); emit changed(); }
}
void PlaylistController::setShuffle(const QString &name, bool value) { if (m_names.contains(name)) { m_shuffle[name] = value; save(); emit changed(); } }
void PlaylistController::setRepeat(const QString &name, bool value) { if (m_names.contains(name)) { m_repeat[name] = value; save(); emit changed(); } }
void PlaylistController::setDurationSeconds(const QString &name, int value) {
    if (!m_names.contains(name) || value < MinDurationSeconds || value > MaxDurationSeconds) {
        m_error = tr("Playlist duration must be between 10 seconds and 24 hours.");
        emit changed();
        return;
    }
    m_durationSeconds[name] = value;
    m_error.clear(); save(); emit changed();
}
void PlaylistController::setTransition(const QString &name, const QString &value) {
    if (!m_names.contains(name) || !isTransition(value)) {
        m_error = tr("Playlist transition is invalid.");
        emit changed();
        return;
    }
    m_transition[name] = value;
    if (value == QStringLiteral("none")) m_transitionSeconds[name] = 0;
    m_error.clear(); save(); emit changed();
}
void PlaylistController::setTransitionSeconds(const QString &name, int value) {
    if (!m_names.contains(name) || value < 0 || value > MaxTransitionSeconds
        || (m_transition.value(name, QStringLiteral("none")) == QStringLiteral("none") && value != 0)) {
        m_error = tr("Transition duration must be between 0 and 10 seconds.");
        emit changed();
        return;
    }
    m_transitionSeconds[name] = value;
    m_error.clear(); save(); emit changed();
}
