// SPDX-License-Identifier: Apache-2.0
#include "applyclient.h"

#include <QFileInfo>
#include <QJsonArray>
#include <QJsonDocument>
#include <QJsonParseError>

namespace {
constexpr int InitialRetryMilliseconds = 5000;
constexpr int MaximumRetryMilliseconds = 30000;
constexpr qsizetype MaxResponseBytes = 64 * 1024 + 16 * 1024;
constexpr qsizetype MaxQueuedOperations = 64;
constexpr int MaxIdentityBytes = 128;
constexpr int MaxContentBytes = 4096;
constexpr int MaxOutputs = 64;
// Per-request deadlines. The daemon bounds its own probes (5 s) and the
// promotion wait (15 s), so these sit above the daemon's worst case and
// only fire when it has stopped answering altogether.
constexpr int RequestTimeoutMilliseconds = 10000;
constexpr int ApplyRequestTimeoutMilliseconds = 30000;
}

ApplyClient::ApplyClient(QString socketPath, QObject *parent)
    : QObject(parent), m_socketPath(std::move(socketPath)) {
    connect(&m_socket, &QLocalSocket::connected, this, &ApplyClient::writeRequest);
    connect(&m_socket, &QLocalSocket::readyRead, this, &ApplyClient::consumeResponse);
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
    m_requestTimer.setSingleShot(true);
    connect(&m_requestTimer, &QTimer::timeout, this, [this] {
        if (m_current.method == None)
            return;
        // Never replayed automatically: the daemon may still be running the
        // transaction it never answered, and a silent re-apply would race it.
        // Try Again re-runs it deliberately.
        failCurrent(tr("The wallpaper service did not answer within %1 seconds.")
                        .arg(requestTimeoutMilliseconds(m_current.method) / 1000),
                    false);
    });
    m_retryTimer.setSingleShot(true);
    connect(&m_retryTimer, &QTimer::timeout, this, [this] {
        // Never clobber an in-flight operation: its own failure path re-arms
        // this timer.
        if (m_current.method != None)
            return;
        if (!m_queue.isEmpty())
            begin(m_queue.takeFirst());
    });
}

void ApplyClient::listOutputs() {
    // Failures surface through finish() as a mapped, actionable error.
    send(Pending{ListOutputs, {}, {}, {}, {}, {}});
}

void ApplyClient::applyWallpaper(const QString &output, const QString &wallpaperId,
                                 const QString &kind, const QUrl &content) {
    if (!validIdentity(output) || !validIdentity(wallpaperId)) {
        setErrorMessage(tr("The output or wallpaper id is invalid for applying."));
        setState(Failed);
        return;
    }
    if (!validKind(kind)) {
        setErrorMessage(tr("This wallpaper type cannot be applied."));
        setState(Failed);
        return;
    }
    // The daemon's catalog content is authoritative; a path the client cannot
    // resolve to a local file is omitted so the daemon starts its own catalog
    // content (the daemon re-validates and preflights everything either way).
    QString contentPath = content.toLocalFile();
    if (contentPath.size() > MaxContentBytes || !contentPath.startsWith(QLatin1Char('/')))
        contentPath.clear();
    send(Pending{Apply, output, wallpaperId, kind, contentPath,
                 [this](bool ok, const QJsonObject &, const QString &) {
                     if (ok)
                         refreshAssignments();
                 }});
}

void ApplyClient::restoreWallpaper(const QString &output) {
    if (!validIdentity(output)) {
        setErrorMessage(tr("The output name is invalid for restoring."));
        setState(Failed);
        return;
    }
    send(Pending{Restore, output, {}, {}, {},
                 [this](bool ok, const QJsonObject &, const QString &) {
                     if (ok)
                         refreshAssignments();
                 }});
}

void ApplyClient::refreshAssignments() {
    send(Pending{Assignments, {}, {}, {}, {}, {}});
}

void ApplyClient::resetStatus() {
    clearResult();
    // Only a settled lane is safe to clear. Clearing unconditionally erased
    // the message of a failure that was still queued or in flight, leaving
    // Failed with empty text and nothing for the user to act on.
    if (m_current.method == None && m_queue.isEmpty()) {
        setErrorMessage({});
        setState(Idle);
    }
}

void ApplyClient::retry() {
    if (m_lastFailedMethod == Apply) {
        applyWallpaper(m_lastFailedOutput, m_lastFailedWallpaperId, m_lastFailedKind,
                       QUrl::fromLocalFile(m_lastFailedContent));
    } else if (m_lastFailedMethod == Restore) {
        restoreWallpaper(m_lastFailedOutput);
    } else if (m_lastFailedMethod == ListOutputs) {
        // An enumeration failure has no recorded target to replay; re-run the
        // listing itself.
        listOutputs();
    }
}

void ApplyClient::send(Pending pending) {
    if (!busy()) {
        begin(std::move(pending));
        return;
    }
    if (pending.method == Assignments) {
        // The assignment mirror is a background lane; when the queue is full
        // the daemon is unreachable and the user-facing lane already shows
        // that. The mirror stays stale until the next successful refresh.
        if (m_queue.size() >= MaxQueuedOperations)
            return;
        m_queue.push_back(std::move(pending));
        return;
    }
    if (m_queue.size() >= MaxQueuedOperations) {
        const QString message =
            tr("The wallpaper service is unreachable and too many apply "
               "operations are pending.");
        if (pending.callback)
            pending.callback(false, {}, message);
        setErrorMessage(message);
        setState(Failed);
        return;
    }
    m_queue.push_back(std::move(pending));
}

void ApplyClient::begin(Pending pending) {
    m_current = std::move(pending);
    // A new user-facing operation invalidates the previous result; a
    // background assignment refresh leaves the state alone.
    if (m_current.method != Assignments)
        clearResult();
    switch (m_current.method) {
    case ListOutputs:
        setState(ListingOutputs);
        break;
    case Apply:
        setState(Applying);
        break;
    case Restore:
        setState(Restoring);
        break;
    case Assignments:
    case None:
        break;
    }
    emit busyChanged();
    m_socket.abort();
    m_buffer.clear();
    m_requestTimer.start(requestTimeoutMilliseconds(m_current.method));
    m_socket.connectToServer(m_socketPath, QIODevice::ReadWrite);
}

void ApplyClient::writeRequest() {
    QJsonObject params;
    switch (m_current.method) {
    case Apply:
        params.insert(QStringLiteral("output"), m_current.output);
        params.insert(QStringLiteral("wallpaper_id"), m_current.wallpaperId);
        params.insert(QStringLiteral("kind"), m_current.kind);
        if (!m_current.content.isEmpty())
            params.insert(QStringLiteral("content"), m_current.content);
        break;
    case Restore:
        params.insert(QStringLiteral("output"), m_current.output);
        break;
    case ListOutputs:
    case Assignments:
    case None:
        break;
    }
    QString method = QStringLiteral("wallpaper.assignments");
    switch (m_current.method) {
    case ListOutputs:
        method = QStringLiteral("wallpaper.outputs");
        break;
    case Apply:
        method = QStringLiteral("wallpaper.apply");
        break;
    case Restore:
        method = QStringLiteral("wallpaper.restore");
        break;
    case Assignments:
    case None:
        break;
    }
    const auto request = QJsonDocument(QJsonObject{
        {QStringLiteral("version"), 1},
        {QStringLiteral("id"), ++m_requestSerial},
        {QStringLiteral("method"), method},
        {QStringLiteral("params"), params},
    }).toJson(QJsonDocument::Compact) + '\n';
    m_socket.write(request);
}

void ApplyClient::consumeResponse() {
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
    m_retryDelayMilliseconds = InitialRetryMilliseconds;
    finish(ok, result, result.value(QStringLiteral("error")).toString(),
           result.value(QStringLiteral("detail")).toString());
}

void ApplyClient::finish(bool ok, const QJsonObject &result, const QString &errorCode,
                         const QString &detail) {
    m_requestTimer.stop();
    const auto method = m_current.method;
    const auto output = m_current.output;
    const auto wallpaperId = m_current.wallpaperId;
    const auto kind = m_current.kind;
    const auto content = m_current.content;
    const auto callback = std::move(m_current.callback);
    m_current = {};
    emit busyChanged();
    if (!ok) {
        if (method == Assignments) {
            // The assignment mirror must not clobber the user-facing state or
            // a just-confirmed result; it stays stale until the next refresh.
        } else {
            // Remember exactly what failed so the UI's retry re-runs the same
            // operation (apply or restore), never a different one.
            m_lastFailedMethod = method;
            m_lastFailedOutput = output;
            m_lastFailedWallpaperId = wallpaperId;
            m_lastFailedKind = kind;
            m_lastFailedContent = content;
            clearResult();
            // The UI's Try Again only makes sense for operations with a
            // recorded target to replay (apply/restore). An enumeration
            // failure has no payload, so failedMethod stays empty and the
            // affordance stays hidden (the picker re-lists on its own).
            m_failedMethod = method == Apply ? QStringLiteral("apply")
                                             : method == Restore ? QStringLiteral("restore")
                                                                 : QString();
            // clearResult() above emitted with failedMethod still empty; without
            // this the Try Again affordance never re-evaluates its binding.
            if (!m_failedMethod.isEmpty())
                emit resultChanged();
            setErrorMessage(mapError(errorCode, detail));
            setState(Failed);
        }
    } else {
        setErrorMessage({});
        switch (method) {
        case ListOutputs:
            applyOutputs(result);
            // A successful enumeration that names nothing is a real answer, not
            // a no-op: say so instead of leaving the picker mutely empty.
            if (m_outputs.isEmpty()) {
                setErrorMessage(tr("The wallpaper service reports no display outputs. "
                                   "Check that a display is connected and enabled."));
            }
            setState(Idle);
            break;
        case Apply:
            m_appliedWallpaperId = wallpaperId;
            m_appliedOutput = output;
            emit resultChanged();
            setState(Applied);
            break;
        case Restore:
            m_restoredOutput = output;
            m_restoreMode = result.value(QStringLiteral("mode")).toString();
            emit resultChanged();
            setState(Idle);
            break;
        case Assignments:
            applyAssignments(result);
            break;
        case None:
            break;
        }
    }
    if (callback)
        callback(ok, result, ok ? QString{} : mapError(errorCode, detail));
    drainQueue();
}

void ApplyClient::failCurrent(const QString &error, bool requeue) {
    m_socket.abort();
    m_requestTimer.stop();
    if (m_current.method == None) {
        // A retry-timer connection attempt failed before writing; fall
        // through and let the timer fire again.
        setErrorMessage(error);
        retryLater();
        return;
    }
    if (requeue && m_queue.size() >= MaxQueuedOperations) {
        // Re-queue the failed operation at the front, but respect the
        // capacity bound: the least urgent queued operation is dropped —
        // never silently. Its callback runs with the queue-full error.
        const auto dropped = m_queue.takeLast();
        if (dropped.callback) {
            dropped.callback(false, {},
                             tr("The wallpaper service is unreachable and too many apply "
                                "operations are pending."));
        }
    }
    const bool backgroundMirror = m_current.method == Assignments;
    const auto abandoned = m_current;
    if (requeue) {
        m_queue.push_front(std::move(m_current));
    } else if (abandoned.callback) {
        abandoned.callback(false, {}, error);
    }
    m_current = {};
    emit busyChanged();
    if (backgroundMirror) {
        // The assignment mirror must not clobber the user-facing state or a
        // just-confirmed result on a socket failure either — the same
        // isolation as the daemon-answered failure path in finish(). It
        // re-queues at the front and retries in the background; an abandoned
        // one has nothing to retry, so the queue simply drains.
        if (requeue)
            retryLater();
        else
            drainQueue();
        return;
    }
    clearResult();
    if (!requeue)
        recordFailure(abandoned);
    setErrorMessage(error);
    setState(Failed);
    if (requeue)
        retryLater();
    else
        drainQueue();
}

void ApplyClient::recordFailure(const Pending &pending) {
    m_lastFailedMethod = pending.method;
    m_lastFailedOutput = pending.output;
    m_lastFailedWallpaperId = pending.wallpaperId;
    m_lastFailedKind = pending.kind;
    m_lastFailedContent = pending.content;
    m_failedMethod = pending.method == Apply
        ? QStringLiteral("apply")
        : pending.method == Restore ? QStringLiteral("restore") : QString();
    if (!m_failedMethod.isEmpty())
        emit resultChanged();
}

int ApplyClient::requestTimeoutMilliseconds(Method method) const {
    // An apply waits on the daemon's bounded promotion wait; everything else
    // is a probe or a state read.
    return method == Apply ? ApplyRequestTimeoutMilliseconds : RequestTimeoutMilliseconds;
}

void ApplyClient::drainQueue() {
    if (m_queue.isEmpty())
        return;
    const auto next = m_queue.takeFirst();
    begin(next);
}

void ApplyClient::retryLater() {
    if (m_retryTimer.isActive())
        return;
    m_retryTimer.start(m_retryDelayMilliseconds);
    m_retryDelayMilliseconds = qMin(m_retryDelayMilliseconds * 2, MaximumRetryMilliseconds);
}

void ApplyClient::setState(State state) {
    if (m_state == state)
        return;
    m_state = state;
    emit stateChanged();
}

void ApplyClient::setErrorMessage(const QString &message) {
    if (m_errorMessage == message)
        return;
    m_errorMessage = message;
    emit errorMessageChanged();
}

void ApplyClient::clearResult() {
    const bool changed = !m_appliedWallpaperId.isEmpty() || !m_appliedOutput.isEmpty()
        || !m_restoredOutput.isEmpty() || !m_restoreMode.isEmpty() || !m_failedMethod.isEmpty();
    m_appliedWallpaperId.clear();
    m_appliedOutput.clear();
    m_restoredOutput.clear();
    m_restoreMode.clear();
    m_failedMethod.clear();
    if (changed)
        emit resultChanged();
}

void ApplyClient::applyOutputs(const QJsonObject &result) {
    QStringList names;
    for (const auto &value : result.value(QStringLiteral("outputs")).toArray()) {
        const auto name = value.toObject().value(QStringLiteral("name")).toString();
        if (!name.isEmpty() && !names.contains(name)) {
            names.push_back(name);
            if (names.size() >= MaxOutputs)
                break;
        }
    }
    const bool firstAnswer = !m_outputsListed;
    m_outputsListed = true;
    if (names == m_outputs) {
        if (firstAnswer)
            emit outputsChanged();
        return;
    }
    m_outputs = names;
    emit outputsChanged();
}

void ApplyClient::applyAssignments(const QJsonObject &result) {
    const auto records = result.value(QStringLiteral("outputs")).toObject().toVariantMap();
    if (records == m_assignments)
        return;
    m_assignments = records;
    emit assignmentsChanged();
}

QString ApplyClient::mapError(const QString &code, const QString &detail) {
    // B2: the daemon refuses a scene whose every layer needs a feature this
    // build does not have yet. The raw detail is a preflight sentence with a
    // file path in it; the user needs the "why", not the path, and they need
    // to know their current wallpaper is untouched. The matched phrase is a
    // contract with kwe-core preflight (no_drawable_content_reasons) — the
    // refusal has no error code of its own, it arrives as invalid_params.
    if (detail.contains(QStringLiteral("draws nothing in this build"))) {
        const auto because = detail.section(QStringLiteral("draws nothing in this build:"), 1)
                                 .trimmed();
        return because.isEmpty()
            ? tr("This wallpaper needs features this version cannot render yet, so it "
                 "was not applied. Your current wallpaper is unchanged.")
            : tr("This wallpaper needs features this version cannot render yet, so it "
                 "was not applied (%1). Your current wallpaper is unchanged.")
                  .arg(because);
    }
    if (code == QStringLiteral("output_missing"))
        return detail.isEmpty() ? tr("Output not found")
                                : tr("Output not found: %1").arg(detail);
    if (code == QStringLiteral("apply_busy"))
        return tr("Another apply is in progress; wait for it to finish.");
    if (code == QStringLiteral("apply_unknown_wallpaper"))
        return detail.isEmpty() ? tr("This wallpaper is not available to apply")
                                : tr("This wallpaper is not available to apply: %1").arg(detail);
    if (code == QStringLiteral("apply_incompatible"))
        return detail.isEmpty()
            ? tr("This wallpaper cannot be applied in its current form")
            : tr("This wallpaper cannot be applied in its current form: %1").arg(detail);
    if (code == QStringLiteral("display_unavailable"))
        return tr("The wallpaper service started before the desktop session and "
                  "cannot see your displays. Restart it with `systemctl --user "
                  "restart kwe-daemon`, then try again.");
    if (code == QStringLiteral("shell_unreachable"))
        return detail.isEmpty()
            ? tr("The Plasma desktop could not be reached; nothing was changed.")
            : tr("The Plasma desktop could not be reached: %1; nothing was changed.")
                  .arg(detail);
    if (code == QStringLiteral("apply_failed"))
        return detail.isEmpty()
            ? tr("Applying failed; the previous wallpaper was restored.")
            : tr("Applying failed: %1").arg(detail);
    if (code == QStringLiteral("restore_failed"))
        return detail.isEmpty() ? tr("Restoring the previous wallpaper failed.")
                                : tr("Restoring the previous wallpaper failed: %1").arg(detail);
    if (code == QStringLiteral("invalid_params"))
        return detail.isEmpty() ? tr("The wallpaper service rejected the request.")
                                : tr("The wallpaper service rejected the request: %1").arg(detail);
    if (code == QStringLiteral("apply_unavailable"))
        return tr("The wallpaper service does not support applying wallpapers.");
    return detail.isEmpty() ? code : tr("%1: %2").arg(code, detail);
}

bool ApplyClient::validIdentity(const QString &value) {
    if (value.isEmpty() || value.size() > MaxIdentityBytes)
        return false;
    const auto bytes = value.toLatin1();
    for (const char byte : bytes) {
        const auto c = static_cast<unsigned char>(byte);
        const bool alnum = (c >= 'a' && c <= 'z') || (c >= 'A' && c <= 'Z') ||
                           (c >= '0' && c <= '9');
        if (!alnum && byte != '.' && byte != '_' && byte != '-')
            return false;
    }
    return true;
}

bool ApplyClient::validKind(const QString &kind) {
    return kind == QStringLiteral("video") || kind == QStringLiteral("web") ||
           kind == QStringLiteral("scene");
}
