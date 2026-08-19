// SPDX-License-Identifier: Apache-2.0
#include "webpreview.h"
#include "permissionsclient.h"

#include <QFileInfo>

WebPreview::WebPreview(PermissionsClient *permissions, QObject *parent)
    : QObject(parent), m_permissions(permissions) {
    connect(&m_process, &QProcess::stateChanged, this, [this] { emit runningChanged(); });
    connect(&m_process, &QProcess::errorOccurred, this, [this](QProcess::ProcessError) {
        setError(tr("Web preview could not start: %1").arg(m_process.errorString()));
    });
    if (m_permissions) {
        connect(m_permissions, &PermissionsClient::grantedChanged, this,
                [this](const QString &wallpaperId, const QString &permission, bool granted) {
                    // A grant change for the previewed wallpaper relaunches
                    // the sandbox with the new network decision. Bounded: one
                    // relaunch from the new state, never a mid-flight change.
                    if (permission != QStringLiteral("network") || wallpaperId != m_wallpaperId)
                        return;
                    if (running() && granted != m_networkAllowed)
                        launch();
                });
    }
}

void WebPreview::play(const QUrl &url, const QString &wallpaperId) {
    if (!url.isLocalFile()) {
        setError(tr("Only local web wallpapers can be previewed."));
        return;
    }
    const QFileInfo entry(url.toLocalFile());
    if (!entry.isFile() || entry.fileName() != QStringLiteral("index.html")) {
        setError(tr("Web preview requires a local index.html."));
        return;
    }
    m_url = url;
    m_wallpaperId = wallpaperId;
    launch();
    // The mirrored grant may be stale (a wallpaper without a record reads
    // as network off until the record loads): fetch the effective record;
    // if the network decision differs, the grantedChanged handler relaunches
    // the sandbox once.
    if (m_permissions && !wallpaperId.isEmpty())
        m_permissions->requestPermissions(wallpaperId);
}

void WebPreview::stop() {
    if (m_process.state() != QProcess::NotRunning)
        m_process.terminate();
}

QStringList WebPreview::argumentsFor(const QString &root, bool networkAllowed) {
    // Same bind set and flags as kwe_core::websandbox::web_preview_command
    // (pinned on both sides by unit tests): the M2b isolation with a
    // WINDOWED chromium — no --headless, no CDP pipe — and the throwaway
    // preview profile. --no-sandbox is required: bwrap is the sandbox.
    QStringList arguments;
    if (!networkAllowed)
        arguments << QStringLiteral("--unshare-net");
    arguments << QStringLiteral("--die-with-parent") << QStringLiteral("--new-session")
              << QStringLiteral("--ro-bind") << QStringLiteral("/usr") << QStringLiteral("/usr")
              << QStringLiteral("--ro-bind") << QStringLiteral("/etc") << QStringLiteral("/etc")
              << QStringLiteral("--ro-bind") << QStringLiteral("/lib") << QStringLiteral("/lib")
              << QStringLiteral("--ro-bind") << QStringLiteral("/lib64") << QStringLiteral("/lib64")
              << QStringLiteral("--ro-bind") << QStringLiteral("/bin") << QStringLiteral("/bin")
              << QStringLiteral("--ro-bind") << QStringLiteral("/sbin") << QStringLiteral("/sbin")
              << QStringLiteral("--ro-bind") << root << QStringLiteral("/wallpaper")
              << QStringLiteral("--proc") << QStringLiteral("/proc") << QStringLiteral("--dev")
              << QStringLiteral("/dev") << QStringLiteral("--tmpfs") << QStringLiteral("/tmp")
              << QStringLiteral("--chdir") << QStringLiteral("/wallpaper") << QStringLiteral("--")
              << QStringLiteral("chromium") << QStringLiteral("--no-sandbox")
              << QStringLiteral("--disable-dev-shm-usage") << QStringLiteral("--no-first-run")
              << QStringLiteral("--no-default-browser-check")
              << QStringLiteral("--disable-extensions")
              << QStringLiteral("--user-data-dir=/tmp/kwe-preview-profile")
              << QStringLiteral("file:///wallpaper/index.html");
    return arguments;
}

void WebPreview::launch() {
    const QFileInfo entry(m_url.toLocalFile());
    m_networkAllowed =
        m_permissions && m_permissions->isGranted(m_wallpaperId, QStringLiteral("network"));
    m_process.kill();
    m_process.setProgram(QStringLiteral("bwrap"));
    m_process.setArguments(argumentsFor(entry.absolutePath(), m_networkAllowed));
    setError({});
    m_process.start();
}

void WebPreview::setError(const QString &message) {
    if (m_errorMessage == message) return;
    m_errorMessage = message;
    emit errorMessageChanged();
}
