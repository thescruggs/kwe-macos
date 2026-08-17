// SPDX-License-Identifier: Apache-2.0
#pragma once

#include <QByteArray>
#include <QJsonArray>
#include <QJsonObject>
#include <QList>
#include <QLocalSocket>
#include <QObject>
#include <QTimer>
#include <functional>

// Single daemon connection for playlist operations. Requests that fail while
// the service is unreachable are queued (bounded) and retried with backoff so
// local edits survive a daemon restart or a transient outage.
class PlaylistClient final : public QObject {
    Q_OBJECT
    Q_PROPERTY(State state READ state NOTIFY stateChanged)
    Q_PROPERTY(QString errorMessage READ errorMessage NOTIFY errorMessageChanged)

public:
    enum State { Disconnected, Loading, Ready, Error };
    Q_ENUM(State)

    explicit PlaylistClient(QString socketPath, QObject *parent = nullptr);
    State state() const { return m_state; }
    QString errorMessage() const { return m_errorMessage; }

    void refresh();
    void importLegacy(const QJsonArray &playlists);
    void putPlaylist(const QJsonObject &playlist);
    void removePlaylist(const QString &id);

signals:
    void stateChanged();
    void errorMessageChanged();
    void playlistsReceived(const QJsonArray &playlists);
    void importFinished(bool ok, int imported, int rejected, const QString &error);
    void putFinished(bool ok, const QString &error);
    void removeFinished(bool ok, const QString &error);

private:
    struct Pending {
        QString method;
        QJsonObject params;
        std::function<void(bool ok, const QJsonObject &result, const QString &error)> callback;
    };

    void send(Pending pending);
    void begin(Pending pending);
    void writeRequest();
    void consumeResponse();
    void failCurrent(const QString &error);
    void drainQueue();
    void retryLater();
    void setState(State state, const QString &error = {});

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
};
