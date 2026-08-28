// SPDX-License-Identifier: GPL-3.0-or-later
#pragma once

#include <QLocalSocket>
#include <QObject>
#include <QStringList>
#include <QTimer>

class RendererStatus final : public QObject {
    Q_OBJECT
    Q_PROPERTY(QString phase READ phase NOTIFY statusChanged)
    Q_PROPERTY(QString wallpaperId READ wallpaperId NOTIFY statusChanged)
    Q_PROPERTY(QString detail READ detail NOTIFY statusChanged)
    Q_PROPERTY(bool quarantined READ quarantined NOTIFY statusChanged)
    /// SR-1c/SR-1e: capability ids the apply gate tolerated as missing on
    /// the currently active/requested scene (empty for every non-scene
    /// wallpaper and every scene with nothing tolerated-missing). Transient
    /// like every other field here — SR-1c's recorded open risk is that this
    /// is not persisted into the assignment, so it reads empty again after a
    /// daemon restart until the scene is re-applied.
    Q_PROPERTY(QStringList capabilityLimitations READ capabilityLimitations NOTIFY statusChanged)

public:
    explicit RendererStatus(QString socketPath, QObject *parent = nullptr);
    QString phase() const { return m_phase; }
    QString wallpaperId() const { return m_wallpaperId; }
    QString detail() const { return m_detail; }
    bool quarantined() const { return m_phase == QStringLiteral("quarantined"); }
    QStringList capabilityLimitations() const { return m_capabilityLimitations; }

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
    QStringList m_capabilityLimitations;
};
