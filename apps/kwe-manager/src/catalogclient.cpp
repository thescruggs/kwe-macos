// SPDX-License-Identifier: GPL-3.0-or-later
#include "catalogclient.h"

#include <QFileInfo>
#include <QJsonDocument>
#include <QJsonObject>
#include <QJsonArray>

namespace {
constexpr int AutomaticRefreshMilliseconds = 5000;
constexpr int MaximumRetryMilliseconds = 30000;
}

namespace {
constexpr qsizetype MaxResponseBytes = 32 * 1024 * 1024;
}

CatalogClient::CatalogClient(QString socketPath, QObject *parent)
    : QObject(parent), m_socketPath(std::move(socketPath)), m_model(this) {
    connect(&m_socket, &QLocalSocket::connected, this, &CatalogClient::sendRequest);
    connect(&m_socket, &QLocalSocket::readyRead, this, &CatalogClient::consumeResponse);
    connect(&m_socket, &QLocalSocket::errorOccurred, this, [this](QLocalSocket::LocalSocketError) {
        const QString message = tr("Could not connect to the wallpaper service at %1: %2")
                                    .arg(m_socketPath, m_socket.errorString());
        // The manager activates the user service itself when the socket is
        // absent; DaemonActivator owns the actionable recovery guidance.
        const QString hint = QFileInfo::exists(m_socketPath) ? QString()
            : tr(". The wallpaper service is not running.");
        setState(Error, message + hint);
    });
    m_autoRefreshTimer.setInterval(AutomaticRefreshMilliseconds);
    m_autoRefreshTimer.setTimerType(Qt::CoarseTimer);
    connect(&m_autoRefreshTimer, &QTimer::timeout, this, [this] {
        // B3: poll silently while a catalog is on screen; a request already
        // in flight (socket not idle) is left alone.
        if (m_state == Loading || m_socket.state() != QLocalSocket::UnconnectedState)
            return;
        if (m_haveCatalog)
            beginSilent(LoadCatalog);
        else
            refresh();
    });
    m_autoRefreshTimer.start();
}

void CatalogClient::refresh() { begin(LoadCatalog); }
void CatalogClient::rescan() { begin(RescanCatalog); }

void CatalogClient::dismissChange() { setChangeMessage({}); }

void CatalogClient::clearHistory() {
    if (m_changeHistory.isEmpty())
        return;
    m_changeHistory.clear();
    emit changeHistoryChanged();
}

void CatalogClient::begin(Operation operation) {
    m_silent = false;
    m_socket.abort();
    m_buffer.clear();
    m_operation = operation;
    setState(Loading);
    m_socket.connectToServer(m_socketPath, QIODevice::ReadWrite);
}

void CatalogClient::beginSilent(Operation operation) {
    m_silent = true;
    m_socket.abort();
    m_buffer.clear();
    m_operation = operation;
    // State stays Ready: the gallery keeps its layout while the poll runs.
    m_socket.connectToServer(m_socketPath, QIODevice::ReadWrite);
}

void CatalogClient::sendRequest() {
    const auto method = m_operation == RescanCatalog ? "rescan" : "catalog";
    const auto request = QJsonDocument(QJsonObject {
        {QStringLiteral("version"), 1},
        {QStringLiteral("id"), 1},
        {QStringLiteral("method"), QString::fromLatin1(method)},
    }).toJson(QJsonDocument::Compact) + '\n';
    m_socket.write(request);
}

void CatalogClient::consumeResponse() {
    m_buffer += m_socket.readAll();
    if (m_buffer.size() > MaxResponseBytes) {
        m_socket.abort();
        setState(Error, tr("The wallpaper service returned more than the 32 MiB safety limit."));
        return;
    }
    const auto newline = m_buffer.indexOf('\n');
    if (newline < 0) return;
    QJsonParseError parseError;
    const auto document = QJsonDocument::fromJson(m_buffer.left(newline), &parseError);
    m_socket.disconnectFromServer();
    if (parseError.error != QJsonParseError::NoError || !document.isObject()) {
        setState(Error, tr("The wallpaper service returned an invalid response: %1").arg(parseError.errorString()));
        return;
    }
    const auto response = document.object();
    if (!response.value(QStringLiteral("ok")).toBool()) {
        setState(Error, tr("The wallpaper service rejected the request."));
        return;
    }
    if (m_operation == RescanCatalog) {
        begin(LoadCatalog);
        return;
    }
    const auto catalog = response.value(QStringLiteral("result")).toObject();
    // The daemon stamps every response with the scan time. Exclude that
    // volatile field so the five-second poll only reports meaningful changes.
    auto snapshotObject = catalog;
    snapshotObject.remove(QStringLiteral("generated_unix_ms"));
    const auto snapshot = QJsonDocument(snapshotObject).toJson(QJsonDocument::Compact);
    if (m_haveCatalog && snapshot != m_catalogSnapshot) {
        const auto stats = catalog.value(QStringLiteral("stats")).toObject();
        const int missing = stats.value(QStringLiteral("missing")).toInt();
        QHash<QString, QString> states;
        QHash<QString, int> progress;
        for (const auto &value : catalog.value(QStringLiteral("items")).toArray()) {
            const auto item = value.toObject();
            const auto id = item.value(QStringLiteral("workshop_id")).toString();
            if (id.isEmpty())
                continue;
            states.insert(id, item.value(QStringLiteral("workshop_state")).toString());
            progress.insert(id, item.value(QStringLiteral("workshop_progress")).toInt(-1));
        }
        int completed = 0;
        int changed = 0;
        for (auto it = states.cbegin(); it != states.cend(); ++it) {
            const auto oldState = m_workshopStates.value(it.key());
            if (oldState != it.value()) {
                ++changed;
                if (oldState == QStringLiteral("downloading") &&
                    it.value() == QStringLiteral("subscribed_installed"))
                    ++completed;
            } else if (m_workshopProgress.value(it.key(), -1) != progress.value(it.key(), -1)) {
                ++changed;
            }
        }
        const auto message = completed > 0
            ? tr("Workshop update: %1 item(s) finished downloading.").arg(completed)
            : missing > 0
                ? tr("Workshop state updated; %1 item(s) are awaiting download.").arg(missing)
                : tr("Workshop state updated: %1 item(s) changed.").arg(changed);
        setChangeMessage(message);
        recordChange(message);
        m_workshopStates = states;
        m_workshopProgress = progress;
    }
    m_catalogSnapshot = snapshot;
    m_haveCatalog = true;
    // Keep state available even when the first response arrives after a
    // daemon restart; later snapshots then produce meaningful transitions.
    m_workshopStates.clear();
    m_workshopProgress.clear();
    for (const auto &value : catalog.value(QStringLiteral("items")).toArray()) {
        const auto item = value.toObject();
        const auto id = item.value(QStringLiteral("workshop_id")).toString();
        m_workshopStates.insert(id, item.value(QStringLiteral("workshop_state")).toString());
        m_workshopProgress.insert(id, item.value(QStringLiteral("workshop_progress")).toInt(-1));
    }
    m_model.replaceFromCatalog(catalog);
    setState(Ready);
}

void CatalogClient::setState(State state, const QString &error) {
    const bool stateChanged = m_state != state;
    const bool errorChanged = m_errorMessage != error;
    m_state = state;
    m_errorMessage = error;
    if (state == Error) {
        m_retryDelayMilliseconds = qMin(m_retryDelayMilliseconds * 2, MaximumRetryMilliseconds);
        m_autoRefreshTimer.setInterval(m_retryDelayMilliseconds);
    } else if (state == Ready) {
        m_retryDelayMilliseconds = AutomaticRefreshMilliseconds;
        m_autoRefreshTimer.setInterval(m_retryDelayMilliseconds);
    }
    if (stateChanged) emit this->stateChanged();
    if (errorChanged) emit errorMessageChanged();
}

void CatalogClient::setChangeMessage(const QString &message) {
    if (m_changeMessage == message)
        return;
    m_changeMessage = message;
    emit changeMessageChanged();
}

void CatalogClient::recordChange(const QString &message) {
    m_changeHistory.prepend(message);
    while (m_changeHistory.size() > 10)
        m_changeHistory.removeLast();
    emit changeHistoryChanged();
}
