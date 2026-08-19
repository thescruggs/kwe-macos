// SPDX-License-Identifier: Apache-2.0
#include "../src/webpreview.h"

#include <QFile>
#include <QTemporaryDir>
#include <QUrl>
#include <QtTest>

// The WebPreview command shape and its validation gates are exercised
// WITHOUT spawning any process (bwrap/chromium are never started here); the
// running sandbox itself is covered by scripts/smoke-web-compromise.sh and
// the kwe-core builder tests that pin the same command.
class WebPreviewTest final : public QObject {
    Q_OBJECT
private slots:
    void isolationByDefault();
    void networkGrantDropsUnshareNet();
    void windowedChromiumFlags();
    void rejectsNonLocalUrlWithoutSpawning();
    void rejectsNonIndexFileWithoutSpawning();
};

namespace {
bool hasPair(const QStringList &list, const QString &first, const QString &second) {
    for (int i = 0; i + 1 < list.size(); ++i) {
        if (list.at(i) == first && list.at(i + 1) == second)
            return true;
    }
    return false;
}
} // namespace

void WebPreviewTest::isolationByDefault() {
    const auto arguments = WebPreview::argumentsFor(QStringLiteral("/tmp/wallpaper"), false);
    QVERIFY(arguments.contains(QStringLiteral("--unshare-net")));
    // The M2b bind set: the browser's system paths read-only, then the
    // content root overlaid at /wallpaper.
    const QStringList sources{QStringLiteral("/usr"), QStringLiteral("/etc"), QStringLiteral("/lib"),
                              QStringLiteral("/lib64"), QStringLiteral("/bin"),
                              QStringLiteral("/sbin")};
    for (const auto &source : sources) {
        QVERIFY2(hasPair(arguments, QStringLiteral("--ro-bind"), source),
                 qPrintable(QStringLiteral("missing ro-bind for %1").arg(source)));
    }
    QVERIFY(hasPair(arguments, QStringLiteral("--ro-bind"), QStringLiteral("/tmp/wallpaper")));
    QVERIFY(hasPair(arguments, QStringLiteral("/tmp/wallpaper"), QStringLiteral("/wallpaper")));
}

void WebPreviewTest::networkGrantDropsUnshareNet() {
    const auto granted = WebPreview::argumentsFor(QStringLiteral("/tmp/wallpaper"), true);
    QVERIFY(!granted.contains(QStringLiteral("--unshare-net")));
    const auto denied = WebPreview::argumentsFor(QStringLiteral("/tmp/wallpaper"), false);
    QVERIFY(denied.contains(QStringLiteral("--unshare-net")));
}

void WebPreviewTest::windowedChromiumFlags() {
    const auto denied = WebPreview::argumentsFor(QStringLiteral("/tmp/wallpaper"), false);
    const QStringList expected{QStringLiteral("--no-sandbox"),
                               QStringLiteral("--disable-dev-shm-usage"),
                               QStringLiteral("--no-first-run"),
                               QStringLiteral("--no-default-browser-check"),
                               QStringLiteral("--disable-extensions"),
                               QStringLiteral("--user-data-dir=/tmp/kwe-preview-profile"),
                               QStringLiteral("file:///wallpaper/index.html")};
    for (const auto &flag : expected) {
        QVERIFY2(denied.contains(flag), qPrintable(QStringLiteral("missing flag %1").arg(flag)));
    }
    // Windowed, not headless: no CDP pipe, no screencast viewport.
    for (const auto &args : {WebPreview::argumentsFor(QStringLiteral("/tmp/wallpaper"), false),
                             WebPreview::argumentsFor(QStringLiteral("/tmp/wallpaper"), true)}) {
        QVERIFY(!args.contains(QStringLiteral("--headless=new")));
        QVERIFY(!args.contains(QStringLiteral("--remote-debugging-pipe")));
        QVERIFY(args.contains(QStringLiteral("--")));
        QVERIFY(args.contains(QStringLiteral("chromium")));
    }
}

void WebPreviewTest::rejectsNonLocalUrlWithoutSpawning() {
    WebPreview preview;
    preview.play(QUrl(QStringLiteral("https://example.com/wallpaper/index.html")),
                 QStringLiteral("web-x"));
    QVERIFY(!preview.running());
    QVERIFY(!preview.errorMessage().isEmpty());
}

void WebPreviewTest::rejectsNonIndexFileWithoutSpawning() {
    QTemporaryDir dir;
    QVERIFY(dir.isValid());
    QFile file(dir.filePath(QStringLiteral("scene.json")));
    QVERIFY(file.open(QIODevice::WriteOnly));
    file.write("{}");
    file.close();
    WebPreview preview;
    preview.play(QUrl::fromLocalFile(file.fileName()), QStringLiteral("web-x"));
    QVERIFY(!preview.running());
    QVERIFY(!preview.errorMessage().isEmpty());
}

QTEST_GUILESS_MAIN(WebPreviewTest)
#include "webpreviewtest.moc"
