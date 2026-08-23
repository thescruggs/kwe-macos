// SPDX-License-Identifier: GPL-3.0-or-later
#pragma once

#include "catalogmodel.h"

#include <QObject>
#include <QLocalSocket>
#include <QQmlEngine>
#include <QTimer>
#include <QStringList>
#include <QHash>

class CatalogClient final : public QObject {
    Q_OBJECT
    // Instances are constructed in C++ and exposed as a context property; QML
    // still needs the registered type to reach the State enum values.
    QML_ELEMENT
    QML_UNCREATABLE("CatalogClient is created by the manager")
    Q_PROPERTY(State state READ state NOTIFY stateChanged)
    Q_PROPERTY(QString errorMessage READ errorMessage NOTIFY errorMessageChanged)
    Q_PROPERTY(QString socketPath READ socketPath CONSTANT)
    Q_PROPERTY(CatalogModel *sourceModel READ sourceModel CONSTANT)
    Q_PROPERTY(QString changeMessage READ changeMessage NOTIFY changeMessageChanged)
    Q_PROPERTY(QStringList changeHistory READ changeHistory NOTIFY changeHistoryChanged)

public:
    enum State { Disconnected, Loading, Ready, Error };
    Q_ENUM(State)

    explicit CatalogClient(QString socketPath, QObject *parent = nullptr);
    State state() const { return m_state; }
    QString errorMessage() const { return m_errorMessage; }
    QString socketPath() const { return m_socketPath; }
    CatalogModel *sourceModel() { return &m_model; }
    QString changeMessage() const { return m_changeMessage; }
    QStringList changeHistory() const { return m_changeHistory; }

    Q_INVOKABLE void refresh();
    Q_INVOKABLE void rescan();
    Q_INVOKABLE void dismissChange();
    Q_INVOKABLE void clearHistory();

signals:
    void stateChanged();
    void errorMessageChanged();
    void changeMessageChanged();
    void changeHistoryChanged();

private:
    enum Operation { LoadCatalog, RescanCatalog };
    void begin(Operation operation);
    /// B3: a background auto-refresh keeps the Ready state (the gallery binds
    /// section visibility to it; flipping to Loading every few seconds made
    /// the grid hide/show and re-settle). Only the first load, an explicit
    /// refresh(), and rescan() show Loading.
    void beginSilent(Operation operation);
    void sendRequest();
    void consumeResponse();
    void setState(State state, const QString &error = {});
    void setChangeMessage(const QString &message);
    void recordChange(const QString &message);

    QString m_socketPath;
    QString m_errorMessage;
    QLocalSocket m_socket;
    QByteArray m_buffer;
    CatalogModel m_model;
    State m_state = Disconnected;
    Operation m_operation = LoadCatalog;
    QTimer m_autoRefreshTimer;
    QByteArray m_catalogSnapshot;
    QString m_changeMessage;
    bool m_haveCatalog = false;
    bool m_silent = false;
    QHash<QString, QString> m_workshopStates;
    QHash<QString, int> m_workshopProgress;
    QStringList m_changeHistory;
    int m_retryDelayMilliseconds = 5000;
};
