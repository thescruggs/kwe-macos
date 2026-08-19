// SPDX-License-Identifier: Apache-2.0
#include "../src/webpreview.h"

#include <QDir>
#include <QFile>
#include <QTemporaryDir>
#include <QUrl>
#include <QtTest>

#include <cstdlib>

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
    void displayBindsX11();
    void displayBindsWayland();
    void displayBindsNone();
    void argumentsForAddsPresentDisplayBinds();
    void grantRelaunchDecision();
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

void WebPreviewTest::displayBindsX11() {
    // A local display binds the /tmp/.X11-unix socket dir; the destination
    // is the same dir inside the namespace (the tmpfs /tmp would otherwise
    // shadow it with nothing).
    const auto binds = WebPreview::displayBinds(QStringLiteral(":0"), {}, {});
    QCOMPARE(binds.size(), 3);
    QCOMPARE(binds.at(0), QStringLiteral("--ro-bind"));
    QCOMPARE(binds.at(1), QStringLiteral("/tmp/.X11-unix"));
    QCOMPARE(binds.at(2), QStringLiteral("/tmp/.X11-unix"));
    // The screen-suffixed form is local too.
    QCOMPARE(WebPreview::displayBinds(QStringLiteral(":0.0"), {}, {}),
             WebPreview::displayBinds(QStringLiteral(":0"), {}, {}));
    // A hostname-prefixed DISPLAY reaches a remote server: no local socket
    // file exists to bind.
    QVERIFY(WebPreview::displayBinds(QStringLiteral("workstation:10.0"), {}, {}).isEmpty());
    // Garbage forms bind nothing.
    QVERIFY(WebPreview::displayBinds(QStringLiteral(":"), {}, {}).isEmpty());
    QVERIFY(WebPreview::displayBinds(QStringLiteral(":abc"), {}, {}).isEmpty());
    QVERIFY(WebPreview::displayBinds(QStringLiteral(":123abc"), {}, {}).isEmpty());
}

void WebPreviewTest::displayBindsWayland() {
    // Only the socket FILE is bound — the runtime dir itself would carry
    // the user's kwallet/pipewire/ssh sockets.
    const auto binds = WebPreview::displayBinds({}, QStringLiteral("wayland-0"),
                                                QStringLiteral("/run/user/1000"));
    QCOMPARE(binds.size(), 3);
    QCOMPARE(binds.at(0), QStringLiteral("--ro-bind"));
    QCOMPARE(binds.at(1), QStringLiteral("/run/user/1000/wayland-0"));
    QCOMPARE(binds.at(2), QStringLiteral("/run/user/1000/wayland-0"));
    // Trailing slashes on the runtime dir are tolerated.
    const auto trimmed = WebPreview::displayBinds({}, QStringLiteral("wayland-0"),
                                                  QStringLiteral("/run/user/1000/"));
    QCOMPARE(trimmed.at(1), QStringLiteral("/run/user/1000/wayland-0"));
    // A missing runtime dir means no socket path to bind; "none" is the
    // explicit offscreen sentinel some sessions export.
    QVERIFY(WebPreview::displayBinds({}, QStringLiteral("wayland-0"), {}).isEmpty());
    QVERIFY(WebPreview::displayBinds({}, QStringLiteral("none"), QStringLiteral("/run/user/1000"))
                .isEmpty());
}

void WebPreviewTest::displayBindsNone() {
    // Offscreen preview (neither display set): nothing is bound.
    QVERIFY(WebPreview::displayBinds({}, {}, {}).isEmpty());
    // X11 and Wayland can both be bound (X11 fallback sessions).
    QCOMPARE(WebPreview::displayBinds(QStringLiteral(":0"), QStringLiteral("wayland-0"),
                                      QStringLiteral("/run/user/1000"))
                 .size(),
             6);
}

void WebPreviewTest::argumentsForAddsPresentDisplayBinds() {
    // Environment-driven path: argumentsFor() reads DISPLAY etc. Only the
    // present sockets are bound (bwrap refuses a missing source). The pure
    // selection logic is covered by the displayBinds tests above.
    const QDir x11Dir(QStringLiteral("/tmp/.X11-unix"));
    const bool x11Present = x11Dir.exists();
    if (x11Present) {
        QVERIFY(setenv("DISPLAY", ":0", 1) == 0);
        const auto arguments = WebPreview::argumentsFor(QStringLiteral("/tmp/wallpaper"), false);
        QVERIFY(hasPair(arguments, QStringLiteral("--ro-bind"), QStringLiteral("/tmp/.X11-unix")));
        QVERIFY(unsetenv("DISPLAY") == 0);
    }
    QVERIFY(unsetenv("WAYLAND_DISPLAY") == 0);
    QVERIFY(unsetenv("XDG_RUNTIME_DIR") == 0);
    const auto offscreen = WebPreview::argumentsFor(QStringLiteral("/tmp/wallpaper"), false);
    QVERIFY(!hasPair(offscreen, QStringLiteral("--ro-bind"), QStringLiteral("/tmp/.X11-unix")));
}

void WebPreviewTest::grantRelaunchDecision() {
    // The relaunch predicate: only a network permission for the previewed
    // wallpaper, while it runs, with a value different from the launch.
    QVERIFY(WebPreview::wantsGrantRelaunch(QStringLiteral("network"), QStringLiteral("web-x"),
                                           QStringLiteral("web-x"), true, true, false));
    QVERIFY(WebPreview::wantsGrantRelaunch(QStringLiteral("network"), QStringLiteral("web-x"),
                                           QStringLiteral("web-x"), true, false, true));
    QVERIFY(!WebPreview::wantsGrantRelaunch(QStringLiteral("audio"), QStringLiteral("web-x"),
                                            QStringLiteral("web-x"), true, true, false));
    QVERIFY(!WebPreview::wantsGrantRelaunch(QStringLiteral("network"), QStringLiteral("other"),
                                            QStringLiteral("web-x"), true, true, false));
    QVERIFY(!WebPreview::wantsGrantRelaunch(QStringLiteral("network"), QStringLiteral("web-x"),
                                            QStringLiteral("web-x"), false, true, false));
    QVERIFY(!WebPreview::wantsGrantRelaunch(QStringLiteral("network"), QStringLiteral("web-x"),
                                            QStringLiteral("web-x"), true, true, true));
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
