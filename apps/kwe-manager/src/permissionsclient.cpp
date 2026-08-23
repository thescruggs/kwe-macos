// SPDX-License-Identifier: GPL-3.0-or-later
#include "permissionsclient.h"

#include <QFileInfo>
#include <QJsonDocument>
#include <QJsonParseError>

namespace {
constexpr int InitialRetryMilliseconds = 5000;
constexpr int MaximumRetryMilliseconds = 30000;
constexpr qsizetype MaxResponseBytes = 64 * 1024 + 16 * 1024;
constexpr qsizetype MaxQueuedOperations = 64;
constexpr int MaxWallpaperIdBytes = 128;
}

PermissionsClient::PermissionsClient(QString socketPath, QObject *parent)
    : QObject(parent), m_socketPath(std::move(socketPath)) {
    connect(&m_socket, &QLocalSocket::connected, this, &PermissionsClient::writeRequest);
    connect(&m_socket, &QLocalSocket::readyRead, this, &PermissionsClient::consumeResponse);
    connect(&m_socket, &QLocalSocket::errorOccurred, this,
            [this](QLocalSocket::LocalSocketError) {
                const QString message =
                    tr("Could not reach the wallpaper service at %1: %2")
                        .arg(m_socketPath, m_socket.errorString());
                const QString hint = QFileInfo::exists(m_socketPath)
                    ? QString()
                    : tr(". Start the service with: systemctl --user enable --now "
                         "kwe-daemon.service");
                failCurrent(message + hint);
            });
    m_retryTimer.setSingleShot(true);
    connect(&m_retryTimer, &QTimer::timeout, this, [this] {
        // Never clobber an in-flight operation: its own failure path re-arms
        // this timer.
        if (m_state == Loading)
            return;
        if (!m_queue.isEmpty())
            begin(m_queue.takeFirst());
    });
}

bool PermissionsClient::isGranted(const QString &wallpaperId,
                                  const QString &permission) const {
    return m_grants.value(wallpaperId).value(permission, false);
}

bool PermissionsClient::isPending(const QString &wallpaperId) const {
    return m_pendingIds.contains(wallpaperId);
}

void PermissionsClient::requestPermissions(const QString &wallpaperId) {
    if (!validWallpaperId(wallpaperId))
        return;
    send(Pending{
        QStringLiteral("permissions.get"),
        QJsonObject{{QStringLiteral("wallpaper_id"), wallpaperId}},
        wallpaperId,
        [this, wallpaperId](bool ok, const QJsonObject &result, const QString &error) {
            if (ok) {
                applyGranted(wallpaperId, result.value(QStringLiteral("granted")).toObject());
            } else {
                finishPending(wallpaperId);
                setState(Error, error);
            }
        }});
}

void PermissionsClient::setPermission(const QString &wallpaperId, const QString &permission,
                                      bool granted) {
    if (!validWallpaperId(wallpaperId)) {
        setState(Error, tr("The wallpaper id is invalid for a permission change."));
        return;
    }
    if (!validPermission(permission)) {
        setState(Error, tr("Unknown permission '%1'.").arg(permission));
        return;
    }
    // Patch semantics: only the changed permission is sent; the daemon keeps
    // the other permissions' current values and answers with the new
    // effective record.
    QJsonObject params{{QStringLiteral("wallpaper_id"), wallpaperId},
                       {permission, granted}};
    send(Pending{
        QStringLiteral("permissions.set"),
        params,
        wallpaperId,
        [this, wallpaperId](bool ok, const QJsonObject &result, const QString &error) {
            if (ok) {
                applyGranted(wallpaperId, result.value(QStringLiteral("granted")).toObject());
            } else {
                finishPending(wallpaperId);
                setState(Error, error);
            }
        }});
}

void PermissionsClient::send(Pending pending) {
    // Mark pending at enqueue time, not only when the request goes out: a
    // queued operation must already show the busy toggle (which disables
    // further toggles, so duplicates cannot stack), and it must release the
    // flag again if it fails before leaving the queue.
    markPending(pending.wallpaperId);
    if (m_state == Loading) {
        if (m_queue.size() >= MaxQueuedOperations) {
            pending.callback(false, {},
                             tr("The wallpaper service is unreachable and too many permission "
                                "changes are pending."));
            return;
        }
        m_queue.push_back(std::move(pending));
        return;
    }
    begin(std::move(pending));
}

void PermissionsClient::begin(Pending pending) {
    m_current = std::move(pending);
    markPending(m_current.wallpaperId);
    setState(Loading);
    m_socket.abort();
    m_buffer.clear();
    m_socket.connectToServer(m_socketPath, QIODevice::ReadWrite);
}

void PermissionsClient::writeRequest() {
    const auto request = QJsonDocument(QJsonObject{
        {QStringLiteral("version"), 1},
        {QStringLiteral("id"), ++m_requestSerial},
        {QStringLiteral("method"), m_current.method},
        {QStringLiteral("params"), m_current.params},
    }).toJson(QJsonDocument::Compact) + '\n';
    m_socket.write(request);
}

void PermissionsClient::consumeResponse() {
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
        failCurrent(tr("The wallpaper service returned an invalid response: %1")
                        .arg(parseError.errorString()));
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
    callback(ok, result,
             ok ? QString{} : (detail.isEmpty() ? error : tr("%1: %2").arg(error, detail)));
    drainQueue();
}

void PermissionsClient::failCurrent(const QString &error) {
    m_socket.abort();
    if (m_current.method.isEmpty()) {
        // A retry-timer connection attempt failed before writing; fall
        // through and let the timer fire again.
        setState(Error, error);
        retryLater();
        return;
    }
    // Re-queue the failed operation at the front so no toggle is lost, but
    // respect the capacity bound: if the queue is already full the least
    // urgent queued operation is dropped — never silently. Its callback runs
    // with the queue-full error, so the user's permission change surfaces
    // (and its pending flag is released) instead of vanishing. Release the
    // current operation's pending flag too: a permanently unreachable daemon
    // must not leave the toggle stuck in a busy state forever (the re-queued
    // operation re-arms it on retry).
    const auto wallpaperId = m_current.wallpaperId;
    if (m_queue.size() >= MaxQueuedOperations) {
        const auto dropped = m_queue.takeLast();
        if (dropped.callback)
            dropped.callback(false, {},
                             tr("The wallpaper service is unreachable and too many permission "
                                "changes are pending."));
    }
    m_queue.push_front(std::move(m_current));
    m_current = {};
    finishPending(wallpaperId);
    setState(Error, error);
    retryLater();
}

void PermissionsClient::drainQueue() {
    if (m_queue.isEmpty())
        return;
    const auto next = m_queue.takeFirst();
    begin(next);
}

void PermissionsClient::retryLater() {
    if (m_retryTimer.isActive())
        return;
    m_retryTimer.start(m_retryDelayMilliseconds);
    m_retryDelayMilliseconds = qMin(m_retryDelayMilliseconds * 2, MaximumRetryMilliseconds);
}

void PermissionsClient::setState(State state, const QString &error) {
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

void PermissionsClient::applyGranted(const QString &wallpaperId, const QJsonObject &granted) {
    const QStringList names{QStringLiteral("network"), QStringLiteral("audio"),
                            QStringLiteral("pointer")};
    QHash<QString, bool> record;
    for (const auto &name : names)
        record.insert(name, granted.value(name).toBool());
    const auto previous = m_grants.value(wallpaperId);
    if (previous != record) {
        m_grants.insert(wallpaperId, record);
        for (const auto &name : names) {
            const bool before = previous.value(name, false);
            const bool after = record.value(name, false);
            if (before != after)
                emit grantedChanged(wallpaperId, name, after);
        }
    }
    finishPending(wallpaperId);
}

void PermissionsClient::markPending(const QString &wallpaperId) {
    if (wallpaperId.isEmpty() || m_pendingIds.contains(wallpaperId))
        return;
    m_pendingIds.insert(wallpaperId);
    emit pendingChanged();
}

void PermissionsClient::finishPending(const QString &wallpaperId) {
    if (m_pendingIds.remove(wallpaperId))
        emit pendingChanged();
}

bool PermissionsClient::validPermission(const QString &permission) {
    return permission == QStringLiteral("network") || permission == QStringLiteral("audio") ||
           permission == QStringLiteral("pointer");
}

bool PermissionsClient::validWallpaperId(const QString &wallpaperId) {
    if (wallpaperId.isEmpty() || wallpaperId.size() > MaxWallpaperIdBytes)
        return false;
    const auto bytes = wallpaperId.toLatin1();
    for (const char byte : bytes) {
        const auto c = static_cast<unsigned char>(byte);
        const bool alnum = (c >= 'a' && c <= 'z') || (c >= 'A' && c <= 'Z') ||
                           (c >= '0' && c <= '9');
        if (!alnum && byte != '.' && byte != '_' && byte != '-')
            return false;
    }
    return true;
}
