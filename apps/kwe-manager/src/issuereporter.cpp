// SPDX-License-Identifier: GPL-3.0-or-later
#include "issuereporter.h"

#include <QCoreApplication>
#include <QDateTime>
#include <QDir>
#include <QFile>
#include <QFileInfo>
#include <QImage>
#include <QJsonArray>
#include <QJsonDocument>
#include <QJsonObject>
#include <QJsonValue>
#include <QProcess>
#include <QRegularExpression>
#include <QStandardPaths>
#include <QTextStream>

namespace {
// Every subprocess this class runs is a single bounded diagnostic query —
// never a long-running renderer — so one deadline covers all of them.
constexpr int SubprocessTimeoutMilliseconds = 5000;
constexpr qint64 MaxCapturedBytes = 1024 * 1024; // 1 MiB, per captured artefact.
constexpr qint64 MaxNoteBytes = 4096; // 4 KiB, per the F4 contract.
constexpr int MaxDiagnosticLines = 60;
constexpr int MaxFrameWidthPixels = 1280;
constexpr int MaxWallpaperIdBytes = 128;

struct ProcessRun {
    bool started = false;
    bool finished = false;
    int exitCode = -1;
    QByteArray stdoutBytes;
    QByteArray stderrBytes;
};

// Runs one subprocess to completion or kills it at `timeoutMs`. Captured
// output is capped at `maxBytes` so a runaway or malicious child can never
// grow the report past a bounded size (AGENTS.md: bound subprocess waits
// and log volume).
ProcessRun runBounded(const QString &program, const QStringList &arguments, int timeoutMs,
                      qint64 maxBytes) {
    ProcessRun run;
    QProcess process;
    process.start(program, arguments);
    run.started = process.waitForStarted(timeoutMs);
    if (!run.started) {
        run.stderrBytes = QStringLiteral("could not start %1: %2")
                               .arg(program, process.errorString())
                               .toUtf8();
        return run;
    }
    run.finished = process.waitForFinished(timeoutMs);
    if (!run.finished) {
        process.kill();
        process.waitForFinished(1000);
        run.stderrBytes = QStringLiteral("timed out after %1 ms").arg(timeoutMs).toUtf8();
        return run;
    }
    run.exitCode = process.exitCode();
    run.stdoutBytes = process.readAllStandardOutput();
    run.stderrBytes = process.readAllStandardError();
    if (run.stdoutBytes.size() > maxBytes)
        run.stdoutBytes.truncate(maxBytes);
    if (run.stderrBytes.size() > maxBytes)
        run.stderrBytes.truncate(maxBytes);
    return run;
}
}

IssueReporter::IssueReporter(QString socketPath, QString cliPath, QObject *parent)
    : QObject(parent), m_socketPath(std::move(socketPath)) {
    if (!cliPath.isEmpty()) {
        m_cliPath = std::move(cliPath);
    } else {
        const QString envPath = qEnvironmentVariable("KWE_CLI_PATH");
        m_cliPath = envPath.isEmpty() ? QStringLiteral("kwe") : envPath;
    }
}

IssueReporter::DaemonCallResult IssueReporter::callDaemon(const QString &method) const {
    DaemonCallResult result;
    const auto run = runBounded(
        m_cliPath,
        {QStringLiteral("daemon-call"), QStringLiteral("--socket"), m_socketPath,
         QStringLiteral("--method"), method},
        SubprocessTimeoutMilliseconds, MaxCapturedBytes);
    if (!run.started) {
        result.error = QString::fromUtf8(run.stderrBytes);
        return result;
    }
    if (!run.finished) {
        result.error = QString::fromUtf8(run.stderrBytes);
        return result;
    }
    // `kwe daemon-call` exits 0 when the daemon answered ok:true and 2 when
    // it answered ok:false; either way it already printed the JSON body,
    // which is exactly the evidence this report wants. Any other exit code
    // means the CLI itself failed before printing a usable response.
    if (run.exitCode != 0 && run.exitCode != 2) {
        result.error = QStringLiteral("kwe daemon-call exited with code %1: %2")
                            .arg(run.exitCode)
                            .arg(QString::fromUtf8(run.stderrBytes).trimmed());
        return result;
    }
    result.stdoutBytes = run.stdoutBytes;
    result.ok = true;
    return result;
}

QString IssueReporter::capturePackageVersion() const {
    const auto run = runBounded(QStringLiteral("pacman"),
                                {QStringLiteral("-Q"), QStringLiteral("kde-wallpaper-engine")},
                                SubprocessTimeoutMilliseconds, 4096);
    if (!run.started || !run.finished || run.exitCode != 0)
        return {};
    // pacman -Q prints "<name> <version>-<pkgrel>".
    const QString text = QString::fromUtf8(run.stdoutBytes).trimmed();
    const auto spaceIndex = text.indexOf(QLatin1Char(' '));
    return spaceIndex < 0 ? text : text.mid(spaceIndex + 1);
}

QString IssueReporter::applicationVersionFallback() const {
    const QString version = QCoreApplication::applicationVersion();
    return version.isEmpty() ? tr("unknown") : version;
}

QStringList IssueReporter::renderingDiagnosticLines(const QByteArray &statusJson) const {
    QStringList matched;
    const auto document = QJsonDocument::fromJson(statusJson);
    if (!document.isObject())
        return matched;
    const auto result = document.object().value(QStringLiteral("result")).toObject();
    const auto tailValue = result.value(QStringLiteral("stderr_tail"));
    QStringList lines;
    if (tailValue.isArray()) {
        for (const auto &entry : tailValue.toArray())
            lines << entry.toString();
    } else {
        lines = tailValue.toString().split(QLatin1Char('\n'));
    }
    // B2/S1-era renderer diagnostics: shader fallbacks, effect counts, model
    // skips. Only these three event families sit next to the note — the
    // rest of stderr_tail is already captured verbatim in
    // renderer-status.json for anyone who needs more.
    static const QStringList markers = {
        QStringLiteral("event=renderer.scene."),
        QStringLiteral("event=renderer.web."),
        QStringLiteral("event=renderer.video."),
    };
    for (const auto &line : lines) {
        const auto trimmed = line.trimmed();
        if (trimmed.isEmpty())
            continue;
        for (const auto &marker : markers) {
            if (trimmed.contains(marker)) {
                matched << trimmed;
                break;
            }
        }
        if (matched.size() >= MaxDiagnosticLines)
            break;
    }
    return matched;
}

QString IssueReporter::newestLastGoodFrame() {
    QString stateHome = qEnvironmentVariable("XDG_STATE_HOME");
#if defined(Q_OS_MACOS)
    // Matches kwe_platform::state_dir(): ~/Library/Application Support/kwe/state.
    if (stateHome.isEmpty())
        stateHome = QDir::homePath() + QStringLiteral("/Library/Application Support/kwe");
    QDir dir(stateHome + (qEnvironmentVariableIsEmpty("XDG_STATE_HOME")
                              ? QStringLiteral("/state") : QStringLiteral("/kwe")));
#else
    if (stateHome.isEmpty())
        stateHome = QDir::homePath() + QStringLiteral("/.local/state");
    QDir dir(stateHome + QStringLiteral("/kwe"));
#endif
    const auto entries =
        dir.entryInfoList({QStringLiteral("last-good-*.ppm")}, QDir::Files | QDir::NoDotAndDotDot);
    QString newestPath;
    QDateTime newestModified;
    for (const auto &entry : entries) {
        if (newestPath.isEmpty() || entry.lastModified() > newestModified) {
            newestPath = entry.absoluteFilePath();
            newestModified = entry.lastModified();
        }
    }
    return newestPath;
}

void IssueReporter::record(const QString &wallpaperId, const QString &title, const QString &kind,
                           const QString &note) {
    setBusy(true);
    setErrorMessage({});

    const QDateTime now = QDateTime::currentDateTime();

    // The wallpaper id becomes a path component; strip anything that is not
    // safe in a single directory name so a hostile or unexpected id can
    // never escape the reports directory (AGENTS.md: parse untrusted
    // metadata without letting it reach a filesystem path unchecked).
    QString safeId = wallpaperId;
    safeId.replace(QRegularExpression(QStringLiteral("[^A-Za-z0-9._-]")), QStringLiteral("_"));
    if (safeId.isEmpty())
        safeId = QStringLiteral("unknown");
    if (safeId.size() > MaxWallpaperIdBytes)
        safeId.truncate(MaxWallpaperIdBytes);

    const QString reportsRoot =
        QStandardPaths::writableLocation(QStandardPaths::GenericDataLocation)
        + QStringLiteral("/kwe/reports");
    const QString dirName = now.toString(QStringLiteral("yyyyMMdd-HHmmss"))
        + QLatin1Char('-') + safeId;
    const QString reportDir = reportsRoot + QLatin1Char('/') + dirName;

    QDir directory;
    if (!directory.mkpath(reportDir)) {
        setErrorMessage(tr("Could not create the report directory: %1").arg(reportDir));
        setBusy(false);
        return;
    }
    QFile::setPermissions(reportDir, QFileDevice::ReadOwner | QFileDevice::WriteOwner
                                          | QFileDevice::ExeOwner);

    QString truncatedNote = note;
    bool noteTruncated = false;
    while (truncatedNote.toUtf8().size() > MaxNoteBytes) {
        truncatedNote.chop(1);
        noteTruncated = true;
    }

    QStringList artefactLines;

    const auto writeJsonArtefact = [&](const QString &fileName, const DaemonCallResult &call) {
        if (!call.ok) {
            artefactLines << QStringLiteral("- %1: %2").arg(fileName, call.error);
            return;
        }
        QFile file(reportDir + QLatin1Char('/') + fileName);
        if (file.open(QIODevice::WriteOnly)) {
            file.write(call.stdoutBytes);
            artefactLines << QStringLiteral("- %1: captured").arg(fileName);
        } else {
            artefactLines << QStringLiteral("- %1: failed to write file").arg(fileName);
        }
    };

    const auto rendererStatus = callDaemon(QStringLiteral("renderer.status"));
    writeJsonArtefact(QStringLiteral("renderer-status.json"), rendererStatus);
    writeJsonArtefact(QStringLiteral("assignments.json"),
                      callDaemon(QStringLiteral("wallpaper.assignments")));
    writeJsonArtefact(QStringLiteral("health.json"), callDaemon(QStringLiteral("health")));

    // journal.txt: the daemon's recent journal. Absence (no systemd
    // session, no matching unit, journalctl missing) is not an error — the
    // best available diagnostic goes into the file instead so the artefact
    // always exists.
    {
        const auto run = runBounded(QStringLiteral("journalctl"),
                                    {QStringLiteral("--user"), QStringLiteral("-u"),
                                     QStringLiteral("kwe-daemon"), QStringLiteral("-n"),
                                     QStringLiteral("400"), QStringLiteral("--no-pager")},
                                    SubprocessTimeoutMilliseconds, MaxCapturedBytes);
        QFile file(reportDir + QStringLiteral("/journal.txt"));
        const bool captured = run.started && run.finished && run.exitCode == 0
            && !run.stdoutBytes.isEmpty();
        if (file.open(QIODevice::WriteOnly)) {
            if (captured) {
                file.write(run.stdoutBytes);
                artefactLines << QStringLiteral("- journal.txt: captured (last 400 lines)");
            } else {
                const QByteArray reason = !run.stderrBytes.isEmpty()
                    ? run.stderrBytes
                    : QByteArrayLiteral(
                          "journalctl returned no entries for kwe-daemon in this session");
                file.write(reason);
                artefactLines << QStringLiteral("- journal.txt: unavailable (%1)")
                                     .arg(QString::fromUtf8(reason).trimmed());
            }
        } else {
            artefactLines << QStringLiteral("- journal.txt: failed to write file");
        }
    }

    // frame.png: the newest frame the renderer actually published, so the
    // maintainer's note ("black layer", "wrong colours"...) has a picture
    // next to it.
    {
        const QString framePath = newestLastGoodFrame();
        if (framePath.isEmpty()) {
            artefactLines << QStringLiteral(
                "- frame.png: skipped (no ~/.local/state/kwe/last-good-*.ppm found)");
        } else {
            QImage image(framePath);
            if (image.isNull()) {
                artefactLines << QStringLiteral("- frame.png: skipped (could not decode %1)")
                                     .arg(framePath);
            } else {
                if (image.width() > MaxFrameWidthPixels)
                    image = image.scaledToWidth(MaxFrameWidthPixels, Qt::SmoothTransformation);
                if (image.save(reportDir + QStringLiteral("/frame.png"), "PNG")) {
                    artefactLines << QStringLiteral("- frame.png: captured from %1")
                                         .arg(QFileInfo(framePath).fileName());
                } else {
                    artefactLines << QStringLiteral("- frame.png: failed to save PNG");
                }
            }
        }
    }

    QString packageVersion = capturePackageVersion();
    const bool versionFromPackage = !packageVersion.isEmpty();
    if (!versionFromPackage)
        packageVersion = applicationVersionFallback();

    const QStringList diagnosticLines =
        rendererStatus.ok ? renderingDiagnosticLines(rendererStatus.stdoutBytes) : QStringList{};

    QString markdown;
    QTextStream out(&markdown);
    out << "# Rendering issue report\n\n";
    out << "- Wallpaper ID: " << wallpaperId << "\n";
    out << "- Title: " << (title.isEmpty() ? tr("(unknown)") : title) << "\n";
    out << "- Kind: " << (kind.isEmpty() ? tr("(unknown)") : kind) << "\n";
    out << "- Recorded: " << now.toString(Qt::ISODate) << "\n";
    out << "- Package version: " << packageVersion
        << (versionFromPackage ? QString() : tr(" (app version; pacman lookup failed)")) << "\n";
    out << "\n## Note\n\n";
    out << truncatedNote << "\n";
    if (noteTruncated)
        out << "\n_(note truncated to 4 KiB)_\n";
    if (!diagnosticLines.isEmpty()) {
        out << "\n## Renderer diagnostics\n\n```\n";
        for (const auto &line : diagnosticLines)
            out << line << "\n";
        out << "```\n";
    }
    out << "\n## Artefacts\n\n";
    for (const auto &line : artefactLines)
        out << line << "\n";
    out.flush();

    QFile reportFile(reportDir + QStringLiteral("/report.md"));
    if (!reportFile.open(QIODevice::WriteOnly)) {
        setErrorMessage(tr("Could not write report.md in %1").arg(reportDir));
        setBusy(false);
        return;
    }
    reportFile.write(markdown.toUtf8());
    reportFile.close();

    setLastReportPath(reportDir);
    setBusy(false);
    emit recorded(reportDir);
}

void IssueReporter::setBusy(bool busy) {
    if (m_busy == busy)
        return;
    m_busy = busy;
    emit busyChanged();
}

void IssueReporter::setErrorMessage(const QString &message) {
    if (m_errorMessage == message)
        return;
    m_errorMessage = message;
    emit errorMessageChanged();
}

void IssueReporter::setLastReportPath(const QString &path) {
    if (m_lastReportPath == path)
        return;
    m_lastReportPath = path;
    emit lastReportPathChanged();
}
