// SPDX-License-Identifier: Apache-2.0
#pragma once

#include <QObject>
#include <QProcess>
#include <QString>
#include <QUrl>

class VideoPreview final : public QObject {
    Q_OBJECT
    Q_PROPERTY(bool running READ running NOTIFY runningChanged)
    Q_PROPERTY(QString errorMessage READ errorMessage NOTIFY errorMessageChanged)

public:
    explicit VideoPreview(QObject *parent = nullptr);
    bool running() const { return m_process.state() != QProcess::NotRunning; }
    QString errorMessage() const { return m_errorMessage; }

    Q_INVOKABLE void play(const QUrl &url);
    Q_INVOKABLE void stop();

signals:
    void runningChanged();
    void errorMessageChanged();

private:
    void setError(const QString &message);
    QProcess m_process;
    QString m_errorMessage;
};
