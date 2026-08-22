// SPDX-License-Identifier: Apache-2.0
#pragma once

#include <QHash>
#include <QJsonObject>
#include <QList>
#include <QLocalSocket>
#include <QObject>
#include <QQmlEngine>
#include <QSet>
#include <QString>
#include <QTimer>
#include <functional>

// Daemon-backed per-wallpaper permission grants (BETA_M2c). Mirrors
// CatalogClient's QLocalSocket request pattern: one request per connection,
// newline-delimited JSON, bounded responses. Requests queue (bounded) and
// retry with backoff like PlaylistClient so a toggle survives a daemon
// restart. Grant state lives in the daemon's permissions-v1.json; this
// client mirrors only the effective records it has read, and the daemon's
// defaults (network off, audio off, pointer on) are the effective state for
// every wallpaper without a record.
class PermissionsClient final : public QObject {
    Q_OBJECT
    // Instances are constructed in C++ and exposed as a context property; QML
    // still needs the registered type to reach the State enum values.
    QML_ELEMENT
    QML_UNCREATABLE("PermissionsClient is created by the manager")
    Q_PROPERTY(State state READ state NOTIFY stateChanged)
    Q_PROPERTY(QString errorMessage READ errorMessage NOTIFY errorMessageChanged)

public:
    enum State { Disconnected, Loading, Ready, Error };
    Q_ENUM(State)

    explicit PermissionsClient(QString socketPath, QObject *parent = nullptr);
    State state() const { return m_state; }
    QString errorMessage() const { return m_errorMessage; }

    /// Effective grant state as mirrored from the daemon: false for a
    /// wallpaper/permission never read (or a wallpaper with no record whose
    /// default is off).
    Q_INVOKABLE bool isGranted(const QString &wallpaperId, const QString &permission) const;
    /// True while a get/set for this wallpaper is queued or in flight, so
    /// the UI can show a busy state instead of a stale toggle.
    Q_INVOKABLE bool isPending(const QString &wallpaperId) const;
    /// Fetches the effective record for one wallpaper (permissions.get).
    Q_INVOKABLE void requestPermissions(const QString &wallpaperId);
    /// Patches one permission (permissions.set); the daemon keeps the other
    /// permissions' current values.
    Q_INVOKABLE void setPermission(const QString &wallpaperId, const QString &permission,
                                   bool granted);

signals:
    void stateChanged();
    void errorMessageChanged();
    /// Emitted when the mirrored effective value of one permission changed.
    void grantedChanged(const QString &wallpaperId, const QString &permission, bool granted);
    void pendingChanged();

private:
    struct Pending {
        QString method;
        QJsonObject params;
        QString wallpaperId;
        std::function<void(bool ok, const QJsonObject &result, const QString &error)> callback;
    };

    void send(Pending pending);
    void begin(Pending pending);
    /// Marks a wallpaper busy from the moment its operation is enqueued (not
    /// only when it goes out), idempotent, so queued operations show a busy
    /// toggle and cannot stack duplicates.
    void markPending(const QString &wallpaperId);
    void writeRequest();
    void consumeResponse();
    void failCurrent(const QString &error);
    void drainQueue();
    void retryLater();
    void setState(State state, const QString &error = {});
    void applyGranted(const QString &wallpaperId, const QJsonObject &granted);
    void finishPending(const QString &wallpaperId);
    static bool validPermission(const QString &permission);
    static bool validWallpaperId(const QString &wallpaperId);

    QString m_socketPath;
    QLocalSocket m_socket;
    QByteArray m_buffer;
    State m_state = Disconnected;
    QString m_errorMessage;
    QList<Pending> m_queue;
    Pending m_current;
    QTimer m_retryTimer;
    int m_retryDelayMilliseconds = 5000;
    int m_requestSerial = 0;
    QHash<QString, QHash<QString, bool>> m_grants;
    QSet<QString> m_pendingIds;
};
