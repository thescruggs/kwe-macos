// SPDX-License-Identifier: GPL-3.0-or-later
#include "rendererstatus.h"

#include <QJsonArray>
#include <QJsonDocument>
#include <QJsonObject>

RendererStatus::RendererStatus(QString socketPath, QObject *parent)
    : QObject(parent), m_socketPath(std::move(socketPath)) {
    connect(&m_socket, &QLocalSocket::connected, this, [this] {
        const auto request = QJsonDocument(QJsonObject{
            {QStringLiteral("version"), 1}, {QStringLiteral("id"), QStringLiteral("manager-status")},
            {QStringLiteral("method"), QStringLiteral("renderer.status")},
        }).toJson(QJsonDocument::Compact) + '\n';
        m_socket.write(request);
    });
    connect(&m_socket, &QLocalSocket::readyRead, this, [this] {
        const auto line = m_socket.readLine(1024 * 1024);
        const auto object = QJsonDocument::fromJson(line).object();
        if (!object.value(QStringLiteral("ok")).toBool()) return;
        const auto status = object.value(QStringLiteral("result")).toObject();
        const auto phase = status.value(QStringLiteral("phase")).toString();
        const auto id = status.value(QStringLiteral("wallpaper_id")).toString();
        const auto detail = status.value(QStringLiteral("last_failure_detail")).toString();
        // SR-1c: capability_limitations, empty by default (absent on the
        // wire, a non-scene wallpaper, or a scene with nothing tolerated).
        QStringList limitations;
        for (const auto &value : status.value(QStringLiteral("capability_limitations")).toArray())
            limitations.push_back(value.toString());
        if (phase == m_phase && id == m_wallpaperId && detail == m_detail
            && limitations == m_capabilityLimitations)
            return;
        m_phase = phase;
        m_wallpaperId = id;
        m_detail = detail;
        m_capabilityLimitations = limitations;
        emit statusChanged();
        m_socket.disconnectFromServer();
    });
    m_timer.setInterval(5000);
    connect(&m_timer, &QTimer::timeout, this, &RendererStatus::poll);
    m_timer.start();
    poll();
}

void RendererStatus::poll() {
    m_socket.abort();
    m_socket.connectToServer(m_socketPath);
}
