// SPDX-License-Identifier: GPL-3.0-or-later
#include "../src/issuereporter.h"

#include <QDateTime>
#include <QDir>
#include <QDirIterator>
#include <QFile>
#include <QFileInfo>
#include <QImage>
#include <QSet>
#include <QSignalSpy>
#include <QStandardPaths>
#include <QTemporaryDir>
#include <QtTest>

namespace {
// A shell stub standing in for `kwe daemon-call --socket ... --method ...`
// (BETA F4 test contract: the CLI binary is overridable so no real daemon
// or package database is needed). Mirrors the real CLI's exit-code
// contract: 0 for ok:true, 2 for ok:false, always with a JSON body on
// stdout.
const char *const StubKweScript = R"SCRIPT(#!/bin/sh
set -eu
method=""
prev=""
for arg in "$@"; do
    if [ "$prev" = "--method" ]; then
        method="$arg"
    fi
    prev="$arg"
done
case "$method" in
    renderer.status)
        printf '%s\n' '{"ok":true,"result":{"phase":"running","wallpaper_id":"431960-1","stderr_tail":"event=renderer.scene.shader_fallback layer=2 reason=unsupported_blend\nsome unrelated noise line\nevent=renderer.web.effect_count count=3\nevent=renderer.video.model_skip reason=unsupported_format\n"}}'
        exit 0
        ;;
    wallpaper.assignments)
        printf '%s\n' '{"ok":true,"result":{"outputs":{"DP-1":{"wallpaper_id":"431960-1"}}}}'
        exit 0
        ;;
    health)
        printf '%s\n' '{"ok":true,"result":{"status":"ok"}}'
        exit 0
        ;;
    *)
        printf '%s\n' '{"ok":false,"result":{"error":"unknown_method"}}'
        exit 2
        ;;
esac
)SCRIPT";

// A minimal valid binary PPM (P6), small enough that no downscale kicks in.
QByteArray tinyPpm() {
    QByteArray ppm = "P6\n2 2\n255\n";
    for (int i = 0; i < 2 * 2; ++i)
        ppm += QByteArray("\xff\x00\x00", 3);
    return ppm;
}
}

class IssueReporterTest final : public QObject {
    Q_OBJECT

private slots:
    void init() {
        // A fresh isolated HOME/XDG tree per test: record() must never
        // touch anything outside it, and stray state (e.g. a frame written
        // by an earlier test) must never leak into the next one.
        m_home.reset(new QTemporaryDir);
        QVERIFY(m_home->isValid());
        m_dataHome = m_home->filePath(QStringLiteral("share"));
        m_stateHome = m_home->filePath(QStringLiteral("state"));
        QVERIFY(QDir().mkpath(m_dataHome));
        QVERIFY(QDir().mkpath(m_stateHome));
        qputenv("HOME", m_home->path().toUtf8());
        qputenv("XDG_DATA_HOME", m_dataHome.toUtf8());
        qputenv("XDG_STATE_HOME", m_stateHome.toUtf8());
        qunsetenv("KWE_CLI_PATH");

        m_stubKwePath = m_home->filePath(QStringLiteral("kwe-stub.sh"));
        QFile script(m_stubKwePath);
        QVERIFY(script.open(QIODevice::WriteOnly));
        QCOMPARE(script.write(StubKweScript), qint64(qstrlen(StubKweScript)));
        script.close();
        QVERIFY(QFile::setPermissions(m_stubKwePath,
            QFileDevice::ReadOwner | QFileDevice::WriteOwner | QFileDevice::ExeOwner));
    }

    void recordWritesTheFullBundle() {
        QDir(m_stateHome).mkpath(QStringLiteral("kwe"));
        const QString framePath = m_stateHome + QStringLiteral("/kwe/last-good-DP-1.ppm");
        QFile frame(framePath);
        QVERIFY(frame.open(QIODevice::WriteOnly));
        frame.write(tinyPpm());
        frame.close();

        IssueReporter reporter(QStringLiteral("/nonexistent.sock"), m_stubKwePath);
        QSignalSpy recordedSpy(&reporter, &IssueReporter::recorded);
        QVERIFY(reporter.errorMessage().isEmpty());

        reporter.record(QStringLiteral("431960-1"), QStringLiteral("Test Wallpaper"),
                        QStringLiteral("scene"), QStringLiteral("black layer over the whole scene"));

        QCOMPARE(recordedSpy.count(), 1);
        QVERIFY(!reporter.busy());
        QVERIFY(reporter.errorMessage().isEmpty());
        const QString reportDir = reporter.lastReportPath();
        QVERIFY(!reportDir.isEmpty());
        QVERIFY(reportDir.startsWith(m_dataHome + QStringLiteral("/kwe/reports/")));
        QVERIFY(QFileInfo(reportDir).isDir());

        const QString reportMd = reportDir + QStringLiteral("/report.md");
        QVERIFY(QFileInfo::exists(reportMd));
        QFile reportFile(reportMd);
        QVERIFY(reportFile.open(QIODevice::ReadOnly));
        const QString markdown = QString::fromUtf8(reportFile.readAll());
        QVERIFY(markdown.contains(QStringLiteral("431960-1")));
        QVERIFY(markdown.contains(QStringLiteral("Test Wallpaper")));
        QVERIFY(markdown.contains(QStringLiteral("black layer over the whole scene")));
        // The Renderer diagnostics section pulls only the three tagged
        // event families out of stderr_tail, dropping the unrelated line.
        QVERIFY(markdown.contains(QStringLiteral("Renderer diagnostics")));
        QVERIFY(markdown.contains(QStringLiteral("event=renderer.scene.shader_fallback")));
        QVERIFY(markdown.contains(QStringLiteral("event=renderer.web.effect_count")));
        QVERIFY(markdown.contains(QStringLiteral("event=renderer.video.model_skip")));
        QVERIFY(!markdown.contains(QStringLiteral("unrelated noise line")));

        QVERIFY(QFileInfo::exists(reportDir + QStringLiteral("/renderer-status.json")));
        QVERIFY(QFileInfo::exists(reportDir + QStringLiteral("/assignments.json")));
        QVERIFY(QFileInfo::exists(reportDir + QStringLiteral("/health.json")));
        QVERIFY(QFileInfo::exists(reportDir + QStringLiteral("/journal.txt")));
        QVERIFY(QFileInfo(reportDir + QStringLiteral("/journal.txt")).size() > 0);

        const QString framePng = reportDir + QStringLiteral("/frame.png");
        QVERIFY(QFileInfo::exists(framePng));
        QImage decoded(framePng);
        QVERIFY(!decoded.isNull());
        QCOMPARE(decoded.width(), 2);
    }

    void noteIsTruncatedAtFourKibibytes() {
        IssueReporter reporter(QStringLiteral("/nonexistent.sock"), m_stubKwePath);
        const QString hugeNote = QString(8192, QLatin1Char('a'));
        reporter.record(QStringLiteral("431960-2"), QStringLiteral("Big Note"),
                        QStringLiteral("video"), hugeNote);
        QVERIFY(reporter.errorMessage().isEmpty());
        QFile reportFile(reporter.lastReportPath() + QStringLiteral("/report.md"));
        QVERIFY(reportFile.open(QIODevice::ReadOnly));
        const QByteArray markdown = reportFile.readAll();
        QVERIFY(markdown.contains("note truncated to 4 KiB"));
        // The note body itself never exceeds the 4 KiB bound, whatever
        // headers and section text surround it.
        const auto noteStart = markdown.indexOf("## Note\n\n") + 9;
        const auto noteEnd = markdown.indexOf("\n\n_(note truncated", noteStart);
        QVERIFY(noteStart > 8);
        QVERIFY(noteEnd > noteStart);
        QVERIFY(noteEnd - noteStart <= 4096);
    }

    void missingFrameIsTolerated() {
        // No ~/.local/state/kwe/last-good-*.ppm exists in this test's XDG
        // tree at all: record() must still produce a full report, just
        // without frame.png.
        IssueReporter reporter(QStringLiteral("/nonexistent.sock"), m_stubKwePath);
        QSignalSpy recordedSpy(&reporter, &IssueReporter::recorded);
        reporter.record(QStringLiteral("431960-3"), QStringLiteral("No Frame"),
                        QStringLiteral("web"), QStringLiteral("nothing renders"));
        QCOMPARE(recordedSpy.count(), 1);
        QVERIFY(reporter.errorMessage().isEmpty());
        const QString reportDir = reporter.lastReportPath();
        QVERIFY(!QFileInfo::exists(reportDir + QStringLiteral("/frame.png")));
        QFile reportFile(reportDir + QStringLiteral("/report.md"));
        QVERIFY(reportFile.open(QIODevice::ReadOnly));
        QVERIFY(QString::fromUtf8(reportFile.readAll()).contains(QStringLiteral("frame.png: skipped")));
    }

    void neverWritesOutsideTheReportsDirectory() {
        QDir(m_stateHome).mkpath(QStringLiteral("kwe"));
        const QString framePath = m_stateHome + QStringLiteral("/kwe/last-good-DP-1.ppm");
        QFile frame(framePath);
        QVERIFY(frame.open(QIODevice::WriteOnly));
        frame.write(tinyPpm());
        frame.close();

        // Files only: record() creating the reports/<dir> directories
        // themselves is expected, the question is only which *files* land
        // where.
        const auto snapshot = [](const QString &root) {
            QSet<QString> paths;
            QDirIterator it(root, QDir::Files, QDirIterator::Subdirectories);
            while (it.hasNext())
                paths.insert(it.next());
            return paths;
        };
        const auto before = snapshot(m_home->path());

        // A wallpaper id crafted to look like a path-traversal attempt: the
        // reports directory name must stay a single safe path component.
        IssueReporter reporter(QStringLiteral("/nonexistent.sock"), m_stubKwePath);
        reporter.record(QStringLiteral("../../etc/passwd"), QStringLiteral("Hostile Id"),
                        QStringLiteral("scene"), QStringLiteral("note"));
        QVERIFY(reporter.errorMessage().isEmpty());
        const QString reportDir = reporter.lastReportPath();
        QVERIFY(reportDir.startsWith(m_dataHome + QStringLiteral("/kwe/reports/")));

        const auto after = snapshot(m_home->path());
        const QString reportsRootPrefix = m_dataHome + QStringLiteral("/kwe/reports/");
        for (const auto &path : after) {
            if (before.contains(path))
                continue;
            // Every new file lands inside the one report directory —
            // nothing landed anywhere else (not next to the frame, not at
            // $HOME, not loose in the state dir).
            QVERIFY2(path.startsWith(reportsRootPrefix), qPrintable(path));
        }
        // The state dir (where the frame lives) gained nothing: record()
        // only ever reads last-good-*.ppm, never writes there.
        QCOMPARE(snapshot(m_stateHome), QSet<QString>{framePath});
    }

    void cliPathFallsBackToTheEnvironmentVariable() {
        qputenv("KWE_CLI_PATH", m_stubKwePath.toUtf8());
        IssueReporter reporter(QStringLiteral("/nonexistent.sock"));
        reporter.record(QStringLiteral("431960-4"), QStringLiteral("Env Path"),
                        QStringLiteral("video"), QStringLiteral("note"));
        QVERIFY(reporter.errorMessage().isEmpty());
        QFile reportFile(reporter.lastReportPath() + QStringLiteral("/report.md"));
        QVERIFY(reportFile.open(QIODevice::ReadOnly));
        // The stub answered (its renderer.status body reached the report),
        // proving KWE_CLI_PATH was honoured with no constructor override.
        QVERIFY(QString::fromUtf8(reportFile.readAll()).contains(QStringLiteral("renderer-status.json: captured")));
        qunsetenv("KWE_CLI_PATH");
    }

private:
    QScopedPointer<QTemporaryDir> m_home;
    QString m_dataHome;
    QString m_stateHome;
    QString m_stubKwePath;
};

QTEST_GUILESS_MAIN(IssueReporterTest)
#include "issuereportertest.moc"
