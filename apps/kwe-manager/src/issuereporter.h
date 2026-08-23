// SPDX-License-Identifier: GPL-3.0-or-later
#pragma once

#include <QObject>
#include <QString>
#include <QStringList>

// F4: "Report rendering issue" — a local, opt-in diagnostic bundle the
// maintainer records by hand right after seeing a rendering problem: their
// note plus the daemon's current renderer/assignment/health state, its
// recent journal, and the last frame the renderer actually produced. Nothing
// is uploaded anywhere; the bundle sits on disk under
// ~/.local/share/kwe/reports/ for the next debugging session. Every
// subprocess is bounded (5 s, capped output) and a failed artefact is
// recorded inside report.md instead of aborting the whole report — a
// maintainer capturing evidence for a broken renderer should never be
// blocked by one more thing being broken too.
class IssueReporter final : public QObject {
    Q_OBJECT
    Q_PROPERTY(bool busy READ busy NOTIFY busyChanged)
    Q_PROPERTY(QString lastReportPath READ lastReportPath NOTIFY lastReportPathChanged)
    Q_PROPERTY(QString errorMessage READ errorMessage NOTIFY errorMessageChanged)

public:
    /// `cliPath` overrides the `kwe` binary used for `daemon-call`
    /// (otherwise the KWE_CLI_PATH environment variable, then the bare
    /// "kwe" resolved on PATH); the test suite points it at a stub script
    /// so no real daemon or package database is needed.
    explicit IssueReporter(QString socketPath, QString cliPath = {}, QObject *parent = nullptr);

    bool busy() const { return m_busy; }
    QString lastReportPath() const { return m_lastReportPath; }
    QString errorMessage() const { return m_errorMessage; }

    /// Writes one report bundle under
    /// ~/.local/share/kwe/reports/<YYYYMMDD-HHMMSS>-<wallpaperId>/.
    /// Bounded and best-effort per artefact: a failed daemon call, a
    /// missing frame, or an unavailable journal never aborts the whole
    /// report — the failure is recorded inside report.md instead.
    /// `note` is written verbatim, truncated to 4 KiB. Emits recorded() on
    /// success; errorMessage is set only when the report directory itself,
    /// or report.md within it, could not be written.
    Q_INVOKABLE void record(const QString &wallpaperId, const QString &title,
                             const QString &kind, const QString &note);

signals:
    void busyChanged();
    void lastReportPathChanged();
    void errorMessageChanged();
    void recorded(QString path);

private:
    struct DaemonCallResult {
        bool ok = false;
        QString error;
        QByteArray stdoutBytes;
    };

    DaemonCallResult callDaemon(const QString &method) const;
    QString capturePackageVersion() const;
    QString applicationVersionFallback() const;
    QStringList renderingDiagnosticLines(const QByteArray &statusJson) const;
    static QString newestLastGoodFrame();
    void setBusy(bool busy);
    void setErrorMessage(const QString &message);
    void setLastReportPath(const QString &path);

    QString m_socketPath;
    QString m_cliPath;
    bool m_busy = false;
    QString m_lastReportPath;
    QString m_errorMessage;
};
