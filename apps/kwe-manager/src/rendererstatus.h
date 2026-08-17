// SPDX-License-Identifier: Apache-2.0
#pragma once

#include <QLocalSocket>
#include <QObject>
#include <QTimer>

class RendererStatus final : public QObject {
    Q_OBJECT
    Q_PROPERTY(QString phase READ phase NOTIFY statusChanged)
    Q_PROPERTY(QString wallpaperId READ wallpaperId NOTIFY statusChanged)
    Q_PROPERTY(QString detail READ detail NOTIFY statusChanged)
    Q_PROPERTY(bool quarantined READ quarantined NOTIFY statusChanged)

public:
    explicit RendererStatus(QString socketPath, QObject *parent = nullptr);
    QString phase() const { return m_phase; }
    QString wallpaperId() const { return m_wallpaperId; }
    QString detail() const { return m_detail; }
    bool quarantined() const { return m_phase == QStringLiteral("quarantined"); }

signals:
    void statusChanged();

private:
    void poll();
    QString m_socketPath;
    QLocalSocket m_socket;
    QTimer m_timer;
    QString m_phase = QStringLiteral("unknown");
    QString m_wallpaperId;
    QString m_detail;
};
