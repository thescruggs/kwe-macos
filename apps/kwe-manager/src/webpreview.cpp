// SPDX-License-Identifier: Apache-2.0
#include "webpreview.h"
#include <QFileInfo>

WebPreview::WebPreview(QObject *parent) : QObject(parent) {
    connect(&m_process, &QProcess::stateChanged, this, [this] { emit runningChanged(); });
    connect(&m_process, &QProcess::errorOccurred, this, [this](QProcess::ProcessError) {
        setError(tr("Web preview could not start: %1").arg(m_process.errorString()));
    });
}

void WebPreview::play(const QUrl &url) {
    if (!url.isLocalFile()) { setError(tr("Only local web wallpapers can be previewed.")); return; }
    const QFileInfo entry(url.toLocalFile());
    if (!entry.isFile() || entry.fileName() != QStringLiteral("index.html")) { setError(tr("Web preview requires a local index.html.")); return; }
    const auto root = entry.absolutePath();
    m_process.kill();
    m_process.setProgram(QStringLiteral("bwrap"));
    m_process.setArguments({QStringLiteral("--unshare-net"), QStringLiteral("--die-with-parent"), QStringLiteral("--new-session"),
                            QStringLiteral("--ro-bind"), root, QStringLiteral("/wallpaper"), QStringLiteral("--proc"), QStringLiteral("/proc"),
                            QStringLiteral("--dev"), QStringLiteral("/dev"), QStringLiteral("--tmpfs"), QStringLiteral("/tmp"),
                            QStringLiteral("--chdir"), QStringLiteral("/wallpaper"), QStringLiteral("--"), QStringLiteral("chromium"),
                            QStringLiteral("--no-first-run"), QStringLiteral("--no-default-browser-check"), QStringLiteral("--disable-extensions"),
                            QStringLiteral("--user-data-dir=/tmp/kwe-chromium"), QStringLiteral("file:///wallpaper/index.html")});
    setError({});
    m_process.start();
}

void WebPreview::stop() { if (m_process.state() != QProcess::NotRunning) m_process.terminate(); }
void WebPreview::setError(const QString &message) { if (m_errorMessage == message) return; m_errorMessage = message; emit errorMessageChanged(); }
