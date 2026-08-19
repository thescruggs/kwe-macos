// SPDX-License-Identifier: Apache-2.0
#pragma once
#include <QObject>
#include <QProcess>
#include <QStringList>
#include <QUrl>

class PermissionsClient;

// User-visible preview of a web wallpaper (BETA_M2d). Launches chromium in
// a WINDOWED bwrap sandbox with the same M2b isolation as the supervised
// renderer (ro-binds for /usr /etc /lib /lib64 /bin /sbin, content root at
// /wallpaper, tmpfs profile) — the old M2a chromium_command form (empty
// bwrap root, no system binds, no --no-sandbox) could not exec chromium at
// all. DISPLAY is inherited from the manager's environment: the preview is
// the user-facing window, not the headless renderer.
//
// Display sockets: the namespace shadows /tmp with an empty tmpfs and
// leaves /run unbound, so inherited DISPLAY/WAYLAND_DISPLAY would point at
// sockets that do not exist inside the sandbox — the preview could never
// connect to any display without binds for them. Only the socket FILES are
// bound (never $XDG_RUNTIME_DIR as a whole, which would leak
// kwallet/pipewire/ssh sockets to wallpaper JS); a local X11 DISPLAY binds
// the /tmp/.X11-unix socket dir, a Wayland session binds its socket file,
// and an offscreen run (neither set) binds nothing.
//
// Network wiring (M2c grants): the effective network grant for the
// wallpaper is mirrored through PermissionsClient. play() launches with the
// currently mirrored value, requests the effective record, and relaunches
// once if the loaded decision differs (bounded: one relaunch, no mid-flight
// mutation). A grant toggle while previewing relaunches the sandbox with
// the new decision.
class WebPreview final : public QObject {
    Q_OBJECT
    Q_PROPERTY(bool running READ running NOTIFY runningChanged)
    Q_PROPERTY(QString errorMessage READ errorMessage NOTIFY errorMessageChanged)
public:
    explicit WebPreview(PermissionsClient *permissions = nullptr, QObject *parent = nullptr);
    bool running() const { return m_process.state() != QProcess::NotRunning; }
    QString errorMessage() const { return m_errorMessage; }
    Q_INVOKABLE void play(const QUrl &url, const QString &wallpaperId);
    Q_INVOKABLE void stop();
    /// The bwrap argv for one preview launch. Shared with the unit test (no
    /// process is spawned there); the kwe-core `web_preview_command` builder
    /// pins the same command shape on the Rust side.
    static QStringList argumentsFor(const QString &root, bool networkAllowed);
    /// Pure selection of the display-socket binds (mirrors
    /// kwe_core::websandbox::display_binds): flat `--ro-bind SOURCE DEST`
    /// triples. The caller drops a triple whose source does not exist
    /// (bwrap refuses to start on a missing source).
    static QStringList displayBinds(const QString &display, const QString &waylandDisplay,
                                    const QString &xdgRuntimeDir);
    /// True when a grant change should relaunch the preview: the permission
    /// is network, the ids match the previewed wallpaper, the preview is
    /// running, and the new value differs from the launched one.
    static bool wantsGrantRelaunch(const QString &permission, const QString &wallpaperId,
                                   const QString &previewId, bool running, bool granted,
                                   bool networkAllowed);
signals:
    void runningChanged();
    void errorMessageChanged();
private:
    void setError(const QString &message);
    void launch();
    PermissionsClient *m_permissions = nullptr;
    QProcess m_process;
    QString m_errorMessage;
    QUrl m_url;
    QString m_wallpaperId;
    bool m_networkAllowed = false;
    bool m_pendingRelaunch = false;
};
