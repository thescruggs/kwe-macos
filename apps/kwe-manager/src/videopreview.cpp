// SPDX-License-Identifier: Apache-2.0
#include "videopreview.h"

#include <QFileInfo>

VideoPreview::VideoPreview(QObject *parent) : QObject(parent) {
    connect(&m_process, &QProcess::stateChanged, this, [this] { emit runningChanged(); });
    connect(&m_process, &QProcess::errorOccurred, this, [this](QProcess::ProcessError) {
        setError(tr("Video preview could not start: %1").arg(m_process.errorString()));
    });
    connect(&m_process, qOverload<int, QProcess::ExitStatus>(&QProcess::finished), this,
            [this](int code, QProcess::ExitStatus status) {
                if (status == QProcess::CrashExit || code != 0)
                    setError(tr("Video preview exited unexpectedly (code %1).").arg(code));
            });
}

void VideoPreview::play(const QUrl &url) {
    if (!url.isLocalFile()) {
        setError(tr("Only local video files can be previewed."));
        return;
    }
    const QFileInfo file(url.toLocalFile());
    if (!file.isFile() || !file.isReadable()) {
        setError(tr("The selected video is not readable."));
        return;
    }
    m_process.kill();
    m_process.setProgram(QStringLiteral("mpv"));
    m_process.setArguments({QStringLiteral("--no-config"), QStringLiteral("--hwdec=auto-safe"),
                            QStringLiteral("--keep-open=yes"), QStringLiteral("--force-window=yes"),
                            QStringLiteral("--"), file.absoluteFilePath()});
    setError({});
    m_process.start();
}

void VideoPreview::stop() {
    if (m_process.state() != QProcess::NotRunning)
        m_process.terminate();
}

void VideoPreview::setError(const QString &message) {
    if (m_errorMessage == message) return;
    m_errorMessage = message;
    emit errorMessageChanged();
}
