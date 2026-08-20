// SPDX-License-Identifier: Apache-2.0
#include "applyclient.h"
#include "catalogclient.h"
#include "catalogmodel.h"
#include "daemonactivator.h"
#include "packageinstaller.h"
#include "permissionsclient.h"
#include "workshopclient.h"
#include "videopreview.h"
#include "rendererstatus.h"
#include "webpreview.h"
#include "playlistcontroller.h"

#include <QCommandLineParser>
#include <QGuiApplication>
#include <QQmlApplicationEngine>
#include <QQmlContext>
#include <QQuickWindow>
#include <QQuickStyle>
#include <QStandardPaths>
#include <QTimer>

int main(int argc, char *argv[]) {
    if (qEnvironmentVariableIsEmpty("QT_QUICK_CONTROLS_STYLE")) {
        QQuickStyle::setStyle(QStringLiteral("org.kde.desktop"));
    }
    QGuiApplication application(argc, argv);
    application.setApplicationName(QStringLiteral("KDE Wallpaper Engine"));
    application.setOrganizationDomain(QStringLiteral("org.kde"));
    application.setDesktopFileName(QStringLiteral("org.kde.kwe"));

    QCommandLineParser parser;
    parser.setApplicationDescription(QStringLiteral("KDE Wallpaper Engine Alpha gallery"));
    parser.addHelpOption();
    parser.addVersionOption();
    QCommandLineOption socketOption(QStringLiteral("socket"), QStringLiteral("Daemon Unix socket path"), QStringLiteral("path"));
    parser.addOption(socketOption);
    QCommandLineOption smokeOption(
        QStringLiteral("smoke-test-ms"),
        QStringLiteral("Exit after validating the daemon catalog (development only)"),
        QStringLiteral("milliseconds"));
    parser.addOption(smokeOption);
    QCommandLineOption screenshotOption(
        QStringLiteral("screenshot"),
        QStringLiteral("Save an offscreen UI snapshot after loading (development only)"),
        QStringLiteral("path"));
    parser.addOption(screenshotOption);
    QCommandLineOption packageSourceOption(
        QStringLiteral("package-source"),
        QStringLiteral("Validated Plasma package source directory (development only)"),
        QStringLiteral("path"));
    parser.addOption(packageSourceOption);
    QCommandLineOption packageRootOption(
        QStringLiteral("package-root"),
        QStringLiteral("User-local Plasma package root (development only)"),
        QStringLiteral("path"));
    parser.addOption(packageRootOption);
    QCommandLineOption safeModeOption(
        QStringLiteral("safe-mode"),
        QStringLiteral("Disable the user-local KWE Plasma package before starting"));
    parser.addOption(safeModeOption);
    QCommandLineOption leaveSafeModeOption(
        QStringLiteral("leave-safe-mode"),
        QStringLiteral("Re-enable the user-local KWE Plasma package before starting"));
    parser.addOption(leaveSafeModeOption);
    QCommandLineOption daemonActivationOption(
        QStringLiteral("daemon-activation-command"),
        QStringLiteral("Command that starts the daemon user service when its socket is absent (development only)"),
        QStringLiteral("path"));
    parser.addOption(daemonActivationOption);
    parser.process(application);

    QString socketPath = parser.value(socketOption);
    if (socketPath.isEmpty()) {
        socketPath = QStandardPaths::writableLocation(QStandardPaths::RuntimeLocation) + QStringLiteral("/kwe/daemon-v1.sock");
    }

    QString packageRoot = parser.value(packageRootOption);
    if (packageRoot.isEmpty()) {
        packageRoot = QStandardPaths::writableLocation(QStandardPaths::GenericDataLocation)
            + QStringLiteral("/plasma/wallpapers");
    }
    // The package shipped system-wide by the distro package (or, in dev runs,
    // the --package-source directory) counts as an installed display package.
    // QStandardPaths::locate prefers the user-local copy, so this only names
    // the system copy when no user-local package exists.
    QString packageSource = parser.value(packageSourceOption);
    if (packageSource.isEmpty()) {
        packageSource = QStandardPaths::locate(
            QStandardPaths::GenericDataLocation,
            QStringLiteral("plasma/wallpapers/org.kde.kwe.wallpaper"),
            QStandardPaths::LocateDirectory);
    }
    PackageInstaller packageInstaller(packageRoot + QStringLiteral("/org.kde.kwe.wallpaper"),
                                      packageSource);
    WorkshopClient workshopClient;
    VideoPreview videoPreview;
    RendererStatus rendererStatus(socketPath);
    // BETA_M2c: daemon-owned per-wallpaper permission grants. Grant state
    // comes from the daemon (permissions-v1.json), never from local settings.
    PermissionsClient permissionsClient(socketPath);
    // BETA_M4b: the live apply lane (wallpaper.outputs/apply/restore/
    // assignments). All requests are serialized and bounded; the daemon owns
    // the switch transaction and its rollback.
    ApplyClient applyClient(socketPath);
    // BETA_M2d: the web preview asks the grant client for the wallpaper's
    // network decision and launches the windowed sandbox accordingly
    // (relaunching once if the loaded record differs).
    WebPreview webPreview(&permissionsClient);
    PlaylistController playlistController(socketPath);
    // When the daemon socket is absent, start the user service before the
    // catalog begins. Defaults to the systemd user unit; the smoke suite
    // injects a stub command instead of touching the user's real unit.
    QString activationProgram = QStringLiteral("systemctl");
    QStringList activationArguments{QStringLiteral("--user"), QStringLiteral("start"),
                                    QStringLiteral("kwe-daemon")};
    const QString activationCommand = parser.value(daemonActivationOption);
    if (!activationCommand.isEmpty()) {
        activationProgram = activationCommand;
        activationArguments.clear();
    }
    DaemonActivator daemonActivator(socketPath, activationProgram, activationArguments);
    // The probe is synchronous; QML then sees a truthful initial state.
    daemonActivator.activate();
    if (parser.isSet(safeModeOption)) {
        if (!packageInstaller.enterSafeMode())
            qWarning("Warning: --safe-mode could not be activated (%s)",
                     qPrintable(packageInstaller.message()));
    }
    if (parser.isSet(leaveSafeModeOption))
        packageInstaller.leaveSafeMode();

    CatalogClient client(socketPath);
    WallpaperFilterModel filtered;
    filtered.setSourceModel(client.sourceModel());
    WallpaperFilterModel workshopFiltered;
    workshopFiltered.setSourceModel(client.sourceModel());
    workshopFiltered.setWorkshopView(true);
    QQmlApplicationEngine engine;
    // Once the activated daemon socket appears, refresh the catalog right
    // away instead of waiting for the client's exponential retry backoff.
    QObject::connect(&daemonActivator, &DaemonActivator::activated, &client, &CatalogClient::refresh);
    engine.rootContext()->setContextProperty(QStringLiteral("daemonActivator"), &daemonActivator);
    engine.rootContext()->setContextProperty(QStringLiteral("catalogClient"), &client);
    engine.rootContext()->setContextProperty(QStringLiteral("wallpaperModel"), &filtered);
    engine.rootContext()->setContextProperty(QStringLiteral("workshopModel"), &workshopFiltered);
    engine.rootContext()->setContextProperty(QStringLiteral("catalogStats"), client.sourceModel());
    engine.rootContext()->setContextProperty(QStringLiteral("permissionsClient"), &permissionsClient);
    engine.rootContext()->setContextProperty(QStringLiteral("applyClient"), &applyClient);
    engine.rootContext()->setContextProperty(QStringLiteral("packageInstaller"), &packageInstaller);
    engine.rootContext()->setContextProperty(QStringLiteral("packageSource"), packageSource);
    engine.rootContext()->setContextProperty(QStringLiteral("workshopClient"), &workshopClient);
    engine.rootContext()->setContextProperty(QStringLiteral("videoPreview"), &videoPreview);
    engine.rootContext()->setContextProperty(QStringLiteral("rendererStatus"), &rendererStatus);
    engine.rootContext()->setContextProperty(QStringLiteral("webPreview"), &webPreview);
    engine.rootContext()->setContextProperty(QStringLiteral("playlistController"), &playlistController);
    engine.load(QUrl(QStringLiteral("qrc:/qt/qml/org/kde/kwe/qml/Main.qml")));
    if (engine.rootObjects().isEmpty()) return 1;
    client.refresh();
    bool smokeValueValid = false;
    int smokeMilliseconds = parser.value(smokeOption).toInt(&smokeValueValid);
    const QString screenshotPath = parser.value(screenshotOption);
    if (!screenshotPath.isEmpty() && (!smokeValueValid || smokeMilliseconds <= 0)) {
        smokeMilliseconds = 3000;
        smokeValueValid = true;
    }
    if (smokeValueValid && smokeMilliseconds > 0) {
        QTimer::singleShot(smokeMilliseconds, &application, [&application, &client, &engine, screenshotPath] {
            if (client.state() == CatalogClient::Ready && client.sourceModel()->totalCount() > 0) {
                if (!screenshotPath.isEmpty()) {
                    auto *window = qobject_cast<QQuickWindow *>(engine.rootObjects().constFirst());
                    if (window == nullptr || !window->grabWindow().save(screenshotPath)) {
                        qCritical("KWE UI screenshot failed: %s", qPrintable(screenshotPath));
                        application.exit(3);
                        return;
                    }
                }
                qInfo("KWE UI smoke test ready: %d catalog items", client.sourceModel()->totalCount());
                application.exit(0);
            } else {
                qCritical("KWE UI smoke test failed: state=%d error=%s",
                          static_cast<int>(client.state()),
                          qPrintable(client.errorMessage()));
                application.exit(2);
            }
        });
    }
    return application.exec();
}
