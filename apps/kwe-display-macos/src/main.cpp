// SPDX-License-Identifier: GPL-3.0-or-later
// kwe-display-macos: the macOS desktop display agent (MP-4). Thin by
// contract: it opens validated frame files, never parses wallpaper content,
// and forwards passive pointer positions. It follows the daemon entirely:
// `wallpaper.outputs` says which screens to cover, `renderer.status` says
// which frame file to show.
#include "desktopsurface.h"
#include "outputswatcher.h"
#include "platform.h"

#include <QCommandLineParser>
#include <QGuiApplication>
#include <QHash>
#include <QQmlEngine>
#include <QScreen>
#include <QStandardPaths>
#include <QTimer>

namespace {
QString defaultSocketPath() {
  const QString runtime = QStandardPaths::writableLocation(QStandardPaths::RuntimeLocation);
  return runtime.isEmpty() ? QString() : runtime + QStringLiteral("/kwe/daemon-v1.sock");
}
} // namespace

int main(int argc, char *argv[]) {
  QGuiApplication application(argc, argv);
  application.setApplicationName(QStringLiteral("kwe-display-macos"));
  application.setQuitOnLastWindowClosed(false);

  QCommandLineParser parser;
  parser.setApplicationDescription(
      QStringLiteral("KWE desktop display agent: shows the wallpaper daemon's validated frames "
                     "under the desktop icons of every assigned screen."));
  parser.addHelpOption();
  QCommandLineOption socketOption(QStringLiteral("daemon-socket"),
                                  QStringLiteral("Daemon socket (default: the platform runtime "
                                                 "location + /kwe/daemon-v1.sock)"),
                                  QStringLiteral("path"), defaultSocketPath());
  QCommandLineOption windowedOption(QStringLiteral("windowed"),
                                    QStringLiteral("Ordinary windows instead of desktop-level "
                                                   "windows (debugging; default on Linux)"));
  QCommandLineOption pollOption(QStringLiteral("poll-ms"),
                                QStringLiteral("wallpaper.outputs poll interval"),
                                QStringLiteral("milliseconds"), QStringLiteral("2000"));
  QCommandLineOption forceCoverOption(
      QStringLiteral("cover-all"),
      QStringLiteral("Cover every screen regardless of the daemon's output assignment (smoke)"));
  QCommandLineOption exitAfterOption(QStringLiteral("exit-after-ms"),
                                     QStringLiteral("Quit after this many milliseconds (smoke)"),
                                     QStringLiteral("milliseconds"));
  parser.addOption(socketOption);
  parser.addOption(windowedOption);
  parser.addOption(pollOption);
  parser.addOption(forceCoverOption);
  QCommandLineOption screenshotOption(
      QStringLiteral("screenshot"),
      QStringLiteral("At --exit-after-ms, save the first covering surface as PNG (smoke)"),
      QStringLiteral("path"));
  QCommandLineOption expectFrameOption(
      QStringLiteral("expect-frame"),
      QStringLiteral("At --exit-after-ms, exit 3 unless a covering surface shows a frame (smoke)"));
  parser.addOption(exitAfterOption);
  parser.addOption(screenshotOption);
  parser.addOption(expectFrameOption);
  parser.process(application);

#if defined(Q_OS_MACOS)
  const bool desktopLevel = !parser.isSet(windowedOption);
#else
  const bool desktopLevel = false;
#endif
  const QString socketPath = parser.value(socketOption);
  if (socketPath.isEmpty()) {
    qCritical("kwe-display: no daemon socket path; pass --daemon-socket");
    return 2;
  }
  bool validPoll = false;
  int pollMilliseconds = parser.value(pollOption).toInt(&validPoll);
  if (!validPoll || pollMilliseconds < 250 || pollMilliseconds > 60000)
    pollMilliseconds = 2000;
  const bool coverAll = parser.isSet(forceCoverOption);

  if (desktopLevel)
    platform::configureAgentProcess();

  QQmlEngine engine;
  OutputsWatcher watcher(socketPath, pollMilliseconds);
  QHash<QScreen *, DesktopSurface *> surfaces;

  auto reapply = [&] {
    QList<OutputRecord> outputs = watcher.outputs();
    bool available = watcher.available();
    if (coverAll) {
      // Smoke/debug: synthesize one kwe-assigned output per screen.
      outputs.clear();
      int index = 0;
      for (QScreen *screen : QGuiApplication::screens()) {
        OutputRecord record;
        // Offscreen/headless screens have empty names; outputs need one.
        record.name = screen->name().isEmpty() ? QStringLiteral("screen-%1").arg(index)
                                               : screen->name();
        ++index;
        record.geometry = screen->geometry();
        record.hasGeometry = true;
        record.wallpaperPlugin = QStringLiteral("org.kde.kwe.wallpaper");
        outputs.append(record);
      }
      available = true;
    }
    for (auto it = surfaces.begin(); it != surfaces.end(); ++it) {
      if (it.value()->applyOutputs(outputs, available)) {
        qInfo("kwe-display: screen %s output=%s covering=%d", qPrintable(it.key()->name()),
              qPrintable(it.value()->outputName()), it.value()->covering() ? 1 : 0);
      }
    }
  };
  auto addScreen = [&](QScreen *screen) {
    if (surfaces.contains(screen))
      return;
    auto *surface = new DesktopSurface(&engine, screen, socketPath, desktopLevel);
    surfaces.insert(screen, surface);
    QObject::connect(screen, &QScreen::geometryChanged, &application, [&, screen] { reapply(); });
    reapply();
  };
  auto removeScreen = [&](QScreen *screen) {
    if (DesktopSurface *surface = surfaces.take(screen)) {
      surface->hide();
      surface->deleteLater();
    }
  };

  for (QScreen *screen : QGuiApplication::screens())
    addScreen(screen);
  QObject::connect(&application, &QGuiApplication::screenAdded, &application, addScreen);
  QObject::connect(&application, &QGuiApplication::screenRemoved, &application, removeScreen);
  QObject::connect(&watcher, &OutputsWatcher::outputsChanged, &application, [&] { reapply(); });
  QObject::connect(&watcher, &OutputsWatcher::availabilityChanged, &application,
                   [&] { reapply(); });

  if (desktopLevel) {
    platform::startPointerMonitor([&](QPointF global) {
      for (DesktopSurface *surface : std::as_const(surfaces))
        surface->forwardGlobalPointer(global);
    });
  }

  bool validExit = false;
  const int exitAfter = parser.value(exitAfterOption).toInt(&validExit);
  const QString screenshotPath = parser.value(screenshotOption);
  const bool expectFrame = parser.isSet(expectFrameOption);
  if (parser.isSet(exitAfterOption) && validExit && exitAfter > 0) {
    QTimer::singleShot(exitAfter, &application, [&] {
      DesktopSurface *covering = nullptr;
      for (DesktopSurface *surface : std::as_const(surfaces))
        if (surface->covering() && covering == nullptr)
          covering = surface;
      int code = 0;
      if (covering == nullptr) {
        qWarning("kwe-display: smoke: no covering surface at exit");
        code = expectFrame ? 3 : 0;
      } else {
        const bool hasFrame = covering->hasFrame();
        qInfo("kwe-display: smoke: covering=%s hasFrame=%d sequence=%llu",
              qPrintable(covering->outputName()), hasFrame ? 1 : 0, covering->frameSequence());
        if (!screenshotPath.isEmpty() && !covering->grabWindow().save(screenshotPath))
          qWarning("kwe-display: smoke: screenshot to %s failed", qPrintable(screenshotPath));
        if (expectFrame && !hasFrame)
          code = 3;
      }
      application.exit(code);
    });
  }

  const int code = application.exec();
  platform::stopPointerMonitor();
  qDeleteAll(surfaces);
  return code;
}
