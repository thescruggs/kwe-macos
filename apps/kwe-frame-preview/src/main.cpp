// SPDX-License-Identifier: Apache-2.0
#include "frameitem.h"
#include "inputclient.h"

#include <QCommandLineParser>
#include <QGuiApplication>
#include <QHash>
#include <QQmlApplicationEngine>
#include <QQuickStyle>
#include <QQuickWindow>
#include <QTimer>

#include <limits>

int main(int argc, char *argv[]) {
  if (qEnvironmentVariableIsEmpty("QT_QUICK_CONTROLS_STYLE")) {
    QQuickStyle::setStyle(QStringLiteral("org.kde.desktop"));
  }
  QGuiApplication application(argc, argv);
  application.setApplicationName(QStringLiteral("KWE Frame Preview"));

  QCommandLineParser parser;
  parser.addHelpOption();
  parser.addVersionOption();
  QCommandLineOption frameOption(QStringLiteral("frame-file"),
                                 QStringLiteral("Mapped frame file"),
                                 QStringLiteral("path"));
  QCommandLineOption smokeOption(QStringLiteral("smoke-test-ms"),
                                 QStringLiteral("Exit after checking a frame"),
                                 QStringLiteral("milliseconds"));
  QCommandLineOption screenshotOption(
      QStringLiteral("screenshot"), QStringLiteral("Save a visual QA snapshot"),
      QStringLiteral("path"));
  QCommandLineOption expectedStatusOption(
      QStringLiteral("expect-status"),
      QStringLiteral(
          "Require live, frozen, invalid, or stopped status in smoke mode"),
      QStringLiteral("status"));
  QCommandLineOption daemonSocketOption(
      QStringLiteral("daemon-socket"),
      QStringLiteral("Daemon socket used for passive pointer forwarding"),
      QStringLiteral("path"));
  QCommandLineOption displayGenerationOption(
      QStringLiteral("display-generation"),
      QStringLiteral("Promoted display generation used to reject stale input"),
      QStringLiteral("generation"));
  QCommandLineOption pointerSmokeOption(
      QStringLiteral("smoke-pointer"),
      QStringLiteral("Send one normalized pointer event in smoke mode"));
  QCommandLineOption followDaemonOption(
      QStringLiteral("follow-daemon"),
      QStringLiteral(
          "Discover and acknowledge the active frame through the daemon"));
  parser.addOption(frameOption);
  parser.addOption(smokeOption);
  parser.addOption(screenshotOption);
  parser.addOption(expectedStatusOption);
  parser.addOption(daemonSocketOption);
  parser.addOption(displayGenerationOption);
  parser.addOption(pointerSmokeOption);
  parser.addOption(followDaemonOption);
  parser.process(application);
  const bool followDaemon = parser.isSet(followDaemonOption);
  if (!parser.isSet(frameOption) && !followDaemon)
    parser.showHelp(2);
  if (followDaemon && !parser.isSet(daemonSocketOption)) {
    qCritical("--follow-daemon requires --daemon-socket");
    return 2;
  }
  if (!followDaemon && parser.isSet(daemonSocketOption) !=
                           parser.isSet(displayGenerationOption)) {
    qCritical(
        "--daemon-socket and --display-generation must be supplied together");
    return 2;
  }
  bool validGeneration = false;
  const qulonglong displayGeneration =
      parser.value(displayGenerationOption).toULongLong(&validGeneration);
  if (parser.isSet(displayGenerationOption) &&
      (!validGeneration || displayGeneration == 0 ||
       displayGeneration > qulonglong(std::numeric_limits<qint64>::max()))) {
    qCritical("--display-generation must be a non-zero integer");
    return 2;
  }

  QQmlApplicationEngine engine;
  engine.setInitialProperties(QVariantMap{
      {QStringLiteral("framePathOption"), parser.value(frameOption)},
      {QStringLiteral("daemonSocketOption"), parser.value(daemonSocketOption)},
      {QStringLiteral("displayGenerationOption"),
       QVariant::fromValue(displayGeneration)},
      {QStringLiteral("followDaemonOption"), followDaemon},
  });
  engine.load(QUrl(
      QStringLiteral("qrc:/qt/qml/org/kde/kwe/framepreview/qml/Preview.qml")));
  if (engine.rootObjects().isEmpty())
    return 1;
  auto *surface = engine.rootObjects().constFirst()->findChild<FrameItem *>(
      QStringLiteral("frameSurface"));
  auto *inputClient =
      engine.rootObjects().constFirst()->findChild<InputClient *>(
          QStringLiteral("inputClient"));
  if (surface == nullptr || inputClient == nullptr)
    return 1;
  if (parser.isSet(pointerSmokeOption)) {
    if (!followDaemon && !inputClient->enabled()) {
      qCritical("--smoke-pointer requires daemon input options");
      return 2;
    }
    QTimer::singleShot(500, inputClient, [inputClient] {
      inputClient->sendPointer(QStringLiteral("move"), 0.5, 0.5);
    });
  }

  bool validTimeout = false;
  int timeout = parser.value(smokeOption).toInt(&validTimeout);
  const QString screenshotPath = parser.value(screenshotOption);
  const QString expectedStatus = parser.value(expectedStatusOption).toLower();
  const bool expectPointer = parser.isSet(pointerSmokeOption);
  if (!screenshotPath.isEmpty() && (!validTimeout || timeout <= 0)) {
    timeout = 2000;
    validTimeout = true;
  }
  if (validTimeout && timeout > 0) {
    QTimer::singleShot(
        timeout, &application,
        [&application, &engine, inputClient, screenshotPath, expectedStatus,
         expectPointer] {
          auto *surface =
              engine.rootObjects().constFirst()->findChild<FrameItem *>(
                  QStringLiteral("frameSurface"));
          if (surface == nullptr || !surface->hasFrame()) {
            qCritical(
                "KWE frame preview smoke test did not receive a valid frame");
            application.exit(2);
            return;
          }
          const QHash<QString, FrameItem::Status> statuses{
              {QStringLiteral("live"), FrameItem::Live},
              {QStringLiteral("frozen"), FrameItem::Frozen},
              {QStringLiteral("invalid"), FrameItem::Invalid},
              {QStringLiteral("stopped"), FrameItem::Stopped},
          };
          if (!expectedStatus.isEmpty() &&
              (!statuses.contains(expectedStatus) ||
               surface->status() != statuses.value(expectedStatus))) {
            qCritical(
                "KWE frame preview expected status '%s' but observed '%s'",
                qPrintable(expectedStatus), qPrintable(surface->statusText()));
            application.exit(4);
            return;
          }
          if (expectPointer && inputClient->lastAcceptedSequence() == 0) {
            qCritical(
                "KWE frame preview pointer smoke request was not accepted");
            application.exit(5);
            return;
          }
          if (!screenshotPath.isEmpty()) {
            auto *window =
                qobject_cast<QQuickWindow *>(engine.rootObjects().constFirst());
            if (window == nullptr ||
                !window->grabWindow().save(screenshotPath)) {
              qCritical("KWE frame preview screenshot failed");
              application.exit(3);
              return;
            }
          }
          application.exit(0);
        });
  }
  return application.exec();
}
