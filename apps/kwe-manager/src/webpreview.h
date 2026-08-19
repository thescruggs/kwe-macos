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
};
