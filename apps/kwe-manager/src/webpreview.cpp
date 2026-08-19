// SPDX-License-Identifier: Apache-2.0
#include "webpreview.h"
#include "permissionsclient.h"

#include <QDir>
#include <QFileInfo>

WebPreview::WebPreview(PermissionsClient *permissions, QObject *parent)
    : QObject(parent), m_permissions(permissions) {
    connect(&m_process, &QProcess::stateChanged, this, [this](QProcess::ProcessState state) {
        emit runningChanged();
        // A grant-driven relaunch is deferred until the old instance is
        // actually dead: kill() is async and QProcess::start() refuses a
        // non-NotRunning process — starting early silently drops the
        // corrected launch and leaves the wrong network flag running
        // forever (measured bug in the first M2d form).
        if (state == QProcess::NotRunning && m_pendingRelaunch)
            launch();
    });
    connect(&m_process, &QProcess::errorOccurred, this, [this](QProcess::ProcessError) {
        setError(tr("Web preview could not start: %1").arg(m_process.errorString()));
    });
    if (m_permissions) {
        connect(m_permissions, &PermissionsClient::grantedChanged, this,
                [this](const QString &wallpaperId, const QString &permission, bool granted) {
                    // A grant change for the previewed wallpaper relaunches
                    // the sandbox with the new network decision. Bounded:
                    // one relaunch per change — launch() re-reads the grant,
                    // so a second toggle before the restart simply updates
                    // the value the restart starts with.
                    if (!wantsGrantRelaunch(permission, wallpaperId, m_wallpaperId, running(),
                                            granted, m_networkAllowed))
                        return;
                    m_pendingRelaunch = true;
                    m_process.kill();
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
    m_pendingRelaunch = false;
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
              << QStringLiteral("--ro-bind") << root << QStringLiteral("/wallpaper");
    // Display sockets: the namespace shadows /tmp with an empty tmpfs and
    // leaves /run unbound, so the inherited DISPLAY/WAYLAND_DISPLAY would
    // point at sockets that do not exist inside the sandbox. Bind only the
    // socket files selected by displayBinds, dropping any whose source is
    // absent (bwrap refuses to start on a missing source; an offscreen run
    // with no display at all binds nothing).
    const auto binds = displayBinds(qEnvironmentVariable("DISPLAY"),
                                    qEnvironmentVariable("WAYLAND_DISPLAY"),
                                    qEnvironmentVariable("XDG_RUNTIME_DIR"));
    for (int i = 0; i + 2 < binds.size(); i += 3) {
        if (binds.at(i) == QStringLiteral("--ro-bind") && QFileInfo::exists(binds.at(i + 1))) {
            arguments << binds.at(i) << binds.at(i + 1) << binds.at(i + 2);
        }
    }
    arguments << QStringLiteral("--proc") << QStringLiteral("/proc") << QStringLiteral("--dev")
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

QStringList WebPreview::displayBinds(const QString &display, const QString &waylandDisplay,
                                     const QString &xdgRuntimeDir) {
    QStringList binds;
    // A local (socket) X11 display is `:N` or `:N.S`; a hostname-prefixed
    // DISPLAY reaches a remote server and has no local socket file to bind.
    const bool localX11 = display.startsWith(QLatin1Char(':')) && display.size() > 1;
    if (localX11) {
        int i = 1;
        while (i < display.size() && display.at(i).isDigit())
            ++i;
        const bool wellFormed =
            i > 1 && (i == display.size() || display.at(i) == QLatin1Char('.'));
        if (wellFormed) {
            binds << QStringLiteral("--ro-bind") << QStringLiteral("/tmp/.X11-unix")
                  << QStringLiteral("/tmp/.X11-unix");
        }
    }
    if (!waylandDisplay.isEmpty() && waylandDisplay != QStringLiteral("none")
        && !xdgRuntimeDir.isEmpty()) {
        QString runtime = xdgRuntimeDir;
        while (runtime.endsWith(QLatin1Char('/')))
            runtime.chop(1);
        // Only the socket FILE is bound — never the runtime dir itself,
        // which carries the user's kwallet/pipewire/ssh sockets.
        binds << QStringLiteral("--ro-bind") << runtime + QLatin1Char('/') + waylandDisplay
              << runtime + QLatin1Char('/') + waylandDisplay;
    }
    return binds;
}

bool WebPreview::wantsGrantRelaunch(const QString &permission, const QString &wallpaperId,
                                    const QString &previewId, bool running, bool granted,
                                    bool networkAllowed) {
    return permission == QStringLiteral("network") && wallpaperId == previewId && running
        && granted != networkAllowed;
}

void WebPreview::launch() {
    m_pendingRelaunch = false;
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
