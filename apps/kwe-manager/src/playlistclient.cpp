// SPDX-License-Identifier: Apache-2.0
#include "playlistclient.h"

#include <QJsonDocument>
#include <QJsonParseError>

namespace {
constexpr int InitialRetryMilliseconds = 5000;
constexpr int MaximumRetryMilliseconds = 30000;
constexpr qsizetype MaxResponseBytes = 4 * 1024 * 1024 + 64 * 1024;
constexpr qsizetype MaxQueuedOperations = 64;
}

PlaylistClient::PlaylistClient(QString socketPath, QObject *parent)
    : QObject(parent), m_socketPath(std::move(socketPath)) {
    connect(&m_socket, &QLocalSocket::connected, this, &PlaylistClient::writeRequest);
    connect(&m_socket, &QLocalSocket::readyRead, this, &PlaylistClient::consumeResponse);
    connect(&m_socket, &QLocalSocket::errorOccurred, this, [this](QLocalSocket::LocalSocketError) {
        failCurrent(tr("Could not connect to the wallpaper service at %1: %2")
                        .arg(m_socketPath, m_socket.errorString()));
    });
    m_retryTimer.setSingleShot(true);
    connect(&m_retryTimer, &QTimer::timeout, this, [this] {
        if (!m_queue.isEmpty())
            begin(m_queue.takeFirst());
    });
}

void PlaylistClient::refresh() {
    send(Pending{QStringLiteral("playlist.list"), {},
                 [this](bool ok, const QJsonObject &result, const QString &error) {
                     if (ok)
                         emit playlistsReceived(result.value(QStringLiteral("playlists")).toArray());
                     else
                         setState(Error, error);
                 }});
}

void PlaylistClient::importLegacy(const QJsonArray &playlists) {
    send(Pending{
        QStringLiteral("playlist.import"),
        QJsonObject{{QStringLiteral("playlists"), playlists}},
        [this](bool ok, const QJsonObject &result, const QString &error) {
            emit importFinished(ok, ok ? result.value(QStringLiteral("imported")).toInt() : 0, error);
        }});
}

void PlaylistClient::putPlaylist(const QJsonObject &playlist) {
    send(Pending{
        QStringLiteral("playlist.put"),
        QJsonObject{{QStringLiteral("playlist"), playlist}},
        [this](bool ok, const QJsonObject &, const QString &error) { emit putFinished(ok, error); }});
}

void PlaylistClient::removePlaylist(const QString &id) {
    send(Pending{
        QStringLiteral("playlist.remove"),
        QJsonObject{{QStringLiteral("id"), id}},
        [this](bool ok, const QJsonObject &, const QString &error) { emit removeFinished(ok, error); }});
}

void PlaylistClient::send(Pending pending) {
    if (m_state == Loading) {
        if (m_queue.size() >= MaxQueuedOperations) {
            pending.callback(false, {}, tr("The wallpaper service is unreachable and too many playlist changes are pending."));
            return;
        }
        m_queue.push_back(std::move(pending));
        return;
    }
    begin(std::move(pending));
}

void PlaylistClient::begin(Pending pending) {
    m_current = std::move(pending);
    setState(Loading);
    m_socket.abort();
    m_buffer.clear();
    m_socket.connectToServer(m_socketPath, QIODevice::ReadWrite);
}

void PlaylistClient::writeRequest() {
    const auto request = QJsonDocument(QJsonObject{
        {QStringLiteral("version"), 1},
        {QStringLiteral("id"), ++m_requestSerial},
        {QStringLiteral("method"), m_current.method},
        {QStringLiteral("params"), m_current.params},
    }).toJson(QJsonDocument::Compact) + '\n';
    m_socket.write(request);
}

void PlaylistClient::consumeResponse() {
    m_buffer += m_socket.readAll();
    if (m_buffer.size() > MaxResponseBytes) {
        failCurrent(tr("The wallpaper service returned more than the safety limit."));
        return;
    }
    const auto newline = m_buffer.indexOf('\n');
    if (newline < 0)
        return;
    QJsonParseError parseError;
    const auto document = QJsonDocument::fromJson(m_buffer.left(newline), &parseError);
    m_socket.disconnectFromServer();
    if (parseError.error != QJsonParseError::NoError || !document.isObject()) {
        failCurrent(tr("The wallpaper service returned an invalid response: %1").arg(parseError.errorString()));
        return;
    }
    const auto response = document.object();
    const bool ok = response.value(QStringLiteral("ok")).toBool();
    const auto result = response.value(QStringLiteral("result")).toObject();
    const auto error = result.value(QStringLiteral("error")).toString();
    const auto detail = result.value(QStringLiteral("detail")).toString();
    const auto callback = std::move(m_current.callback);
    m_current = {};
    setState(Ready);
    callback(ok, result, ok ? QString{} : (detail.isEmpty() ? error : tr("%1: %2").arg(error, detail)));
    drainQueue();
}

void PlaylistClient::failCurrent(const QString &error) {
    m_socket.abort();
    if (m_current.method.isEmpty()) {
        // A retry-timer connection attempt failed before writing; fall
        // through and let the timer fire again.
        setState(Error, error);
        retryLater();
        return;
    }
    // Re-queue the failed operation at the front so no edit is lost.
    m_queue.push_front(std::move(m_current));
    m_current = {};
    setState(Error, error);
    retryLater();
}

void PlaylistClient::drainQueue() {
    if (m_queue.isEmpty())
        return;
    const auto next = m_queue.takeFirst();
    begin(next);
}

void PlaylistClient::retryLater() {
    if (m_retryTimer.isActive())
        return;
    m_retryTimer.start(m_retryDelayMilliseconds);
    m_retryDelayMilliseconds = qMin(m_retryDelayMilliseconds * 2, MaximumRetryMilliseconds);
}

void PlaylistClient::setState(State state, const QString &error) {
    if (state == Ready)
        m_retryDelayMilliseconds = InitialRetryMilliseconds;
    const bool stateChanged = m_state != state;
    const bool errorChanged = m_errorMessage != error;
    m_state = state;
    m_errorMessage = error;
    if (stateChanged)
        emit this->stateChanged();
    if (errorChanged)
        emit errorMessageChanged();
}
