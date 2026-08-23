// SPDX-License-Identifier: Apache-2.0
#include "../src/applyclient.h"

#include <QCoreApplication>
#include <QJsonArray>
#include <QJsonDocument>
#include <QJsonObject>
#include <QLocalServer>
#include <QLocalSocket>
#include <QSet>
#include <QTemporaryDir>
#include <QtTest>

// Minimal daemon stand-in: answers the wallpaper.* wire protocol with the
// documented daemon semantics (docs/SUPERVISOR_API_V1.md) so the tests
// exercise the real client request path. The daemon-side transaction,
// validation, and persistence are covered by the daemon unit tests and the
// smoke suites.
class StubDaemon final : public QObject {
    Q_OBJECT
public:
    struct Fail {
        QString error;
        QString detail;
    };
    StubDaemon() {
        connect(&m_server, &QLocalServer::newConnection, this, &StubDaemon::onConnection);
    }
    bool listen(const QString &path) { return m_server.listen(path); }
    void reset() {
        receivedMethods.clear();
        receivedParams.clear();
        failByMethod.clear();
        holdResponses = false;
        holdByMethod.clear();
        restoreMode = QStringLiteral("stock");
        outputs = defaultOutputs();
    }
    static QJsonArray defaultOutputs() {
        return QJsonArray{
            QJsonObject{{QStringLiteral("name"), QStringLiteral("DP-1")},
                        {QStringLiteral("screen"), 0},
                        {QStringLiteral("desktop_id"), 111},
                        {QStringLiteral("desktop_index"), 1},
                        {QStringLiteral("enabled"), true},
                        {QStringLiteral("connected"), true}},
            QJsonObject{{QStringLiteral("name"), QStringLiteral("HDMI-A-1")},
                        {QStringLiteral("screen"), 1},
                        {QStringLiteral("desktop_id"), 112},
                        {QStringLiteral("desktop_index"), 0},
                        {QStringLiteral("enabled"), true},
                        {QStringLiteral("connected"), true}},
        };
    }
    /// Records requests but never answers them, so a client stays busy with
    /// its queue filling.
    bool holdResponses = false;
    /// Holds requests of one method unanswered, so a client stays busy on
    /// exactly that operation while the others complete.
    QSet<QString> holdByMethod;
    /// Fails one method with the given wire error code/detail.
    QHash<QString, Fail> failByMethod;
    QList<QString> receivedMethods;
    QList<QJsonObject> receivedParams;
    QJsonArray outputs = defaultOutputs();
    /// The wallpaper.restore success mode.
    QString restoreMode = QStringLiteral("stock");
    /// The wallpaper.assignments store payload.
    QJsonObject assignmentsStore;
    /// Closes every accepted connection, simulating a daemon going away while
    /// a request is in flight.
    void dropClients() {
        for (auto *socket : std::as_const(m_clients))
            socket->close();
        m_clients.clear();
    }

private:
    QLocalServer m_server;
    QList<QLocalSocket *> m_clients;

    void onConnection() {
        auto *socket = m_server.nextPendingConnection();
        m_clients.append(socket);
        connect(socket, &QLocalSocket::readyRead, this, [this, socket] {
            for (;;) {
                const auto newline = socket->peek(64 * 1024 + 1024).indexOf('\n');
                if (newline < 0)
                    return;
                const auto line = socket->read(newline + 1);
                const auto request = QJsonDocument::fromJson(line).object();
                const auto method = request.value(QStringLiteral("method")).toString();
                const auto params = request.value(QStringLiteral("params")).toObject();
                receivedMethods.push_back(method);
                receivedParams.push_back(params);
                if (holdResponses || holdByMethod.contains(method))
                    return;
                QJsonObject result;
                bool ok = true;
                const auto fail = failByMethod.value(method);
                if (!fail.error.isEmpty()) {
                    ok = false;
                    result.insert(QStringLiteral("error"), fail.error);
                    if (!fail.detail.isEmpty())
                        result.insert(QStringLiteral("detail"), fail.detail);
                } else if (method == QStringLiteral("wallpaper.outputs")) {
                    result.insert(QStringLiteral("outputs"), outputs);
                } else if (method == QStringLiteral("wallpaper.apply")) {
                    result.insert(QStringLiteral("output"), params.value(QStringLiteral("output")));
                    result.insert(QStringLiteral("applied"),
                                  QJsonObject{
                                      {QStringLiteral("wallpaper_id"),
                                       params.value(QStringLiteral("wallpaper_id"))},
                                      {QStringLiteral("kind"), params.value(QStringLiteral("kind"))},
                                      {QStringLiteral("content"),
                                       params.value(QStringLiteral("content"))},
                                      {QStringLiteral("applied_at_unix_seconds"), 1787188000},
                                  });
                } else if (method == QStringLiteral("wallpaper.restore")) {
                    result.insert(QStringLiteral("output"), params.value(QStringLiteral("output")));
                    result.insert(QStringLiteral("mode"), restoreMode);
                    result.insert(QStringLiteral("restored"),
                                  QJsonObject{
                                      {QStringLiteral("wallpaper_plugin"),
                                       QStringLiteral("org.kde.image")},
                                      {QStringLiteral("config_group"),
                                       QJsonArray{QStringLiteral("Wallpaper"),
                                                  QStringLiteral("org.kde.image"),
                                                  QStringLiteral("General")}},
                                      {QStringLiteral("image"),
                                       QStringLiteral("file:///usr/share/wallpapers/fallback.png")},
                                  });
                    result.insert(QStringLiteral("stock_image"),
                                  QStringLiteral("file:///usr/share/wallpapers/fallback.png"));
                } else if (method == QStringLiteral("wallpaper.assignments")) {
                    result.insert(QStringLiteral("outputs"), assignmentsStore);
                }
                const auto response = QJsonDocument(QJsonObject{
                    {QStringLiteral("version"), 1},
                    {QStringLiteral("id"), request.value(QStringLiteral("id"))},
                    {QStringLiteral("ok"), ok},
                    {QStringLiteral("result"), result},
                }).toJson(QJsonDocument::Compact) + '\n';
                socket->write(response);
                socket->flush();
            }
        });
    }
};

class ApplyClientTest final : public QObject {
    Q_OBJECT

private slots:
    void initTestCase() {
        QVERIFY(m_settingsRoot.isValid());
        QVERIFY(m_daemon.listen(m_settingsRoot.path() + QStringLiteral("/daemon.sock")));
        m_socketPath = m_settingsRoot.path() + QStringLiteral("/daemon.sock");
    }

    void init() { m_daemon.reset(); }

    void listOutputsRoundTrip() {
        const QStringList expected{QStringLiteral("DP-1"), QStringLiteral("HDMI-A-1")};
        ApplyClient client(m_socketPath);
        client.listOutputs();
        QTRY_VERIFY_WITH_TIMEOUT(client.outputs() == expected, 5000);
        QCOMPARE(client.state(), ApplyClient::Idle);
        QVERIFY(client.errorMessage().isEmpty());
        QVERIFY(!client.busy());
        QCOMPARE(m_daemon.receivedMethods.size(), 1);
        QCOMPARE(m_daemon.receivedMethods.first(), QStringLiteral("wallpaper.outputs"));
        QVERIFY(m_daemon.receivedParams.first().isEmpty());
    }

    void applySuccessStateMachine() {
        ApplyClient client(m_socketPath);
        client.applyWallpaper(QStringLiteral("DP-1"), QStringLiteral("431960-123"),
                              QStringLiteral("video"),
                              QUrl::fromLocalFile(QStringLiteral("/steam/431960/video.mp4")));
        QCOMPARE(client.state(), ApplyClient::Applying);
        QVERIFY(client.busy());
        QTRY_VERIFY_WITH_TIMEOUT(client.state() == ApplyClient::Applied, 5000);
        QVERIFY(!client.busy());
        QVERIFY(client.errorMessage().isEmpty());
        QCOMPARE(client.appliedWallpaperId(), QStringLiteral("431960-123"));
        QCOMPARE(client.appliedOutput(), QStringLiteral("DP-1"));
        QCOMPARE(m_daemon.receivedMethods.first(), QStringLiteral("wallpaper.apply"));
        const auto params = m_daemon.receivedParams.first();
        QCOMPARE(params.value(QStringLiteral("output")).toString(), QStringLiteral("DP-1"));
        QCOMPARE(params.value(QStringLiteral("wallpaper_id")).toString(),
                 QStringLiteral("431960-123"));
        QCOMPARE(params.value(QStringLiteral("kind")).toString(), QStringLiteral("video"));
        QCOMPARE(params.value(QStringLiteral("content")).toString(),
                 QStringLiteral("/steam/431960/video.mp4"));
        // A successful apply auto-refreshes the assignment mirror.
        QTRY_VERIFY_WITH_TIMEOUT(m_daemon.receivedMethods.size() == 2, 5000);
        QCOMPARE(m_daemon.receivedMethods.at(1), QStringLiteral("wallpaper.assignments"));
    }

    void webApplySendsTheContentRootAsContent() {
        ApplyClient client(m_socketPath);
        client.applyWallpaper(QStringLiteral("DP-1"), QStringLiteral("431960-web"),
                              QStringLiteral("web"),
                              QUrl::fromLocalFile(QStringLiteral("/steam/431960-web")));
        QTRY_VERIFY_WITH_TIMEOUT(client.state() == ApplyClient::Applied, 5000);
        QCOMPARE(m_daemon.receivedParams.first().value(QStringLiteral("kind")).toString(),
                 QStringLiteral("web"));
        QCOMPARE(m_daemon.receivedParams.first().value(QStringLiteral("content")).toString(),
                 QStringLiteral("/steam/431960-web"));
    }

    void applyFailureSurfacesTheDaemonDetail() {
        m_daemon.failByMethod.insert(QStringLiteral("wallpaper.apply"),
                                     {QStringLiteral("apply_failed"),
                                      QStringLiteral("renderer exited with code 73")});
        ApplyClient client(m_socketPath);
        client.applyWallpaper(QStringLiteral("DP-1"), QStringLiteral("1"), QStringLiteral("scene"),
                              QUrl::fromLocalFile(QStringLiteral("/tmp/scene.json")));
        QTRY_VERIFY_WITH_TIMEOUT(client.state() == ApplyClient::Failed, 5000);
        QVERIFY(client.errorMessage().contains(QStringLiteral("renderer exited with code 73")));
        QVERIFY(client.errorMessage().contains(QStringLiteral("Applying failed")));
        QCOMPARE(client.failedMethod(), QStringLiteral("apply"));
        QVERIFY(!client.busy());
        QVERIFY(client.appliedOutput().isEmpty());
        QVERIFY(client.appliedWallpaperId().isEmpty());
    }

    void restoreRoundTrip() {
        ApplyClient client(m_socketPath);
        client.restoreWallpaper(QStringLiteral("DP-1"));
        QCOMPARE(client.state(), ApplyClient::Restoring);
        QTRY_VERIFY_WITH_TIMEOUT(m_daemon.receivedMethods.size() >= 1, 5000);
        QCOMPARE(m_daemon.receivedMethods.first(), QStringLiteral("wallpaper.restore"));
        QCOMPARE(m_daemon.receivedParams.first().value(QStringLiteral("output")).toString(),
                 QStringLiteral("DP-1"));
        QTRY_VERIFY_WITH_TIMEOUT(client.restoredOutput() == QStringLiteral("DP-1"), 5000);
        QCOMPARE(client.restoreMode(), QStringLiteral("stock"));
        QCOMPARE(client.state(), ApplyClient::Idle);
        QVERIFY(client.errorMessage().isEmpty());
        QVERIFY(!client.busy());
        // A successful restore auto-refreshes the assignment mirror.
        QTRY_VERIFY_WITH_TIMEOUT(m_daemon.receivedMethods.size() == 2, 5000);
        QCOMPARE(m_daemon.receivedMethods.at(1), QStringLiteral("wallpaper.assignments"));
    }

    void restoreReportsTheAssignmentModeWhenRevertingASavedAssignment() {
        m_daemon.restoreMode = QStringLiteral("assignment");
        ApplyClient client(m_socketPath);
        client.restoreWallpaper(QStringLiteral("DP-1"));
        QTRY_VERIFY_WITH_TIMEOUT(client.restoreMode() == QStringLiteral("assignment"), 5000);
        QCOMPARE(client.restoredOutput(), QStringLiteral("DP-1"));
        QCOMPARE(client.state(), ApplyClient::Idle);
    }

    void errorCodesMapToActionableMessages() {
        const auto check = [this](const QString &method, const QString &code,
                                  const QString &detail, const QString &expectContains) {
            m_daemon.failByMethod.insert(method, {code, detail});
            ApplyClient client(m_socketPath);
            if (method == QStringLiteral("wallpaper.apply")) {
                client.applyWallpaper(QStringLiteral("DP-1"), QStringLiteral("1"),
                                      QStringLiteral("video"),
                                      QUrl::fromLocalFile(QStringLiteral("/x.mp4")));
            } else {
                client.restoreWallpaper(QStringLiteral("DP-1"));
            }
            QTRY_VERIFY_WITH_TIMEOUT(client.state() == ApplyClient::Failed, 5000);
            QVERIFY2(client.errorMessage().contains(expectContains),
                     qPrintable(QStringLiteral("error=%1 expected substring=%2")
                                    .arg(client.errorMessage(), expectContains)));
            m_daemon.failByMethod.clear();
        };
        check(QStringLiteral("wallpaper.apply"), QStringLiteral("output_missing"),
              QStringLiteral("DP-2"), QStringLiteral("Output not found: DP-2"));
        check(QStringLiteral("wallpaper.apply"), QStringLiteral("apply_busy"), QString(),
              QStringLiteral("Another apply is in progress"));
        check(QStringLiteral("wallpaper.apply"), QStringLiteral("apply_unknown_wallpaper"),
              QStringLiteral("42"), QStringLiteral("not available to apply"));
        check(QStringLiteral("wallpaper.apply"), QStringLiteral("apply_incompatible"),
              QStringLiteral("kind mismatch"), QStringLiteral("cannot be applied"));
        check(QStringLiteral("wallpaper.apply"), QStringLiteral("shell_unreachable"), QString(),
              QStringLiteral("could not be reached"));
        check(QStringLiteral("wallpaper.apply"), QStringLiteral("apply_unavailable"), QString(),
              QStringLiteral("does not support applying"));
        check(QStringLiteral("wallpaper.apply"), QStringLiteral("invalid_params"),
              QStringLiteral("kind: unexpected"), QStringLiteral("rejected the request"));
        // B2: a scene refused because nothing in it can be drawn reads as a
        // feature gap, not as a rejected request, and says the desktop was
        // left alone.
        check(QStringLiteral("wallpaper.apply"), QStringLiteral("invalid_params"),
              QStringLiteral("scene preflight rejected /w/scene.pkg: scene draws nothing in "
                             "this build: 5 model layer(s) need scene3d, which this build does "
                             "not render yet"),
              QStringLiteral("needs features this version cannot render yet"));
        // B4: a quarantined record names the reason and what Try Again does;
        // a stale service names the restart.
        check(QStringLiteral("wallpaper.apply"), QStringLiteral("apply_quarantined"),
              QStringLiteral("disabled after 3 failures under this build; last failure: "
                             "exit_code_73 stderr=[backend_reject]"),
              QStringLiteral("disabled after repeated failures (disabled after 3 failures"));
        check(QStringLiteral("wallpaper.apply"), QStringLiteral("service_stale"),
              QStringLiteral("binary replaced"),
              QStringLiteral("systemctl --user restart kwe-daemon"));
        check(QStringLiteral("wallpaper.restore"), QStringLiteral("output_missing"),
              QStringLiteral("DP-2"), QStringLiteral("Output not found: DP-2"));
        check(QStringLiteral("wallpaper.restore"), QStringLiteral("restore_failed"),
              QStringLiteral("script error: rejected"), QStringLiteral("Restoring the previous wallpaper failed"));
    }

    void queuedOperationsWaitForTheInFlightOne() {
        m_daemon.holdResponses = true;
        ApplyClient client(m_socketPath);
        client.applyWallpaper(QStringLiteral("DP-1"), QStringLiteral("hold-1"),
                              QStringLiteral("video"), QUrl::fromLocalFile(QStringLiteral("/x.mp4")));
        QTRY_VERIFY_WITH_TIMEOUT(client.busy(), 5000);
        QCOMPARE(client.state(), ApplyClient::Applying);
        // Wait for the in-flight apply to reach the wire.
        QTRY_VERIFY_WITH_TIMEOUT(m_daemon.receivedMethods.size() == 1, 5000);
        // A second operation queues instead of touching the wire.
        client.restoreWallpaper(QStringLiteral("DP-1"));
        QVERIFY(client.busy());
        QTRY_VERIFY_WITH_TIMEOUT(m_daemon.receivedMethods.size() == 1, 500);
        QCOMPARE(m_daemon.receivedMethods.size(), 1);
        // Daemon loss re-queues the in-flight apply at the front (the restore
        // stays queued behind it); the bounded retry re-sends both in order.
        m_daemon.holdResponses = false;
        m_daemon.dropClients();
        QTRY_VERIFY_WITH_TIMEOUT(!client.busy(), 15000);
        // Both operations ran, in order: the retried apply, then the queued
        // restore, then each success's automatic assignment-mirror refresh.
        QCOMPARE(m_daemon.receivedMethods.at(0), QStringLiteral("wallpaper.apply"));
        QCOMPARE(m_daemon.receivedMethods.at(1), QStringLiteral("wallpaper.apply"));
        QCOMPARE(m_daemon.receivedMethods.at(2), QStringLiteral("wallpaper.restore"));
        QCOMPARE(m_daemon.receivedMethods.at(3), QStringLiteral("wallpaper.assignments"));
        QCOMPARE(m_daemon.receivedMethods.at(4), QStringLiteral("wallpaper.assignments"));
        // The apply confirmation is cleared when the restore begins (a new
        // user-facing operation invalidates the previous result); the
        // restore's own result survives.
        QVERIFY(client.appliedOutput().isEmpty());
        QCOMPARE(client.restoredOutput(), QStringLiteral("DP-1"));
    }

    void fullQueueRejectsImmediatelyAndDropsOnDaemonLoss() {
        m_daemon.holdResponses = true;
        ApplyClient client(m_socketPath);
        client.applyWallpaper(QStringLiteral("DP-1"), QStringLiteral("hold-1"),
                              QStringLiteral("video"), QUrl::fromLocalFile(QStringLiteral("/x.mp4")));
        for (int i = 2; i <= 65; ++i) {
            client.applyWallpaper(QStringLiteral("DP-1"), QStringLiteral("hold-%1").arg(i),
                                  QStringLiteral("video"),
                                  QUrl::fromLocalFile(QStringLiteral("/x.mp4")));
        }
        QTRY_VERIFY_WITH_TIMEOUT(m_daemon.receivedMethods.size() == 1, 5000);
        // 64 queued + one in flight: the 66th exceeds the bound and fails
        // immediately with a surfaced error (the queue is untouched).
        client.applyWallpaper(QStringLiteral("DP-1"), QStringLiteral("hold-66"),
                              QStringLiteral("video"), QUrl::fromLocalFile(QStringLiteral("/x.mp4")));
        QVERIFY(client.errorMessage().contains(QStringLiteral("too many")));
        QCOMPARE(client.state(), ApplyClient::Failed);
        QCOMPARE(m_daemon.receivedMethods.size(), 1);
        // The daemon goes away with a request in flight and the queue at the
        // bound: failCurrent re-queues the failed operation at the front by
        // dropping the least urgent queued one, and the retry timer (5 s)
        // re-sends until every queued operation drains.
        m_daemon.holdResponses = false;
        m_daemon.dropClients();
        QTRY_VERIFY_WITH_TIMEOUT(client.errorMessage().contains(QStringLiteral("wallpaper service")),
                                 5000);
        QTRY_VERIFY_WITH_TIMEOUT(!client.busy(), 15000);
    }

    void assignmentsRoundTrip() {
        m_daemon.assignmentsStore = QJsonObject{
            {QStringLiteral("DP-1"),
             QJsonObject{{QStringLiteral("wallpaper_id"), QStringLiteral("1")},
                         {QStringLiteral("kind"), QStringLiteral("video")}}}};
        ApplyClient client(m_socketPath);
        client.refreshAssignments();
        QTRY_VERIFY_WITH_TIMEOUT(client.assignments().contains(QStringLiteral("DP-1")), 5000);
        QCOMPARE(client.state(), ApplyClient::Idle);
        QVERIFY(client.errorMessage().isEmpty());
        QCOMPARE(m_daemon.receivedMethods.first(), QStringLiteral("wallpaper.assignments"));
    }

    void backgroundAssignmentsFailureLeavesTheUserFacingStateAlone() {
        m_daemon.failByMethod.insert(QStringLiteral("wallpaper.assignments"),
                                     {QStringLiteral("apply_failed"), QStringLiteral("store hiccup")});
        ApplyClient client(m_socketPath);
        client.refreshAssignments();
        QTRY_VERIFY_WITH_TIMEOUT(!client.busy(), 5000);
        QCOMPARE(client.state(), ApplyClient::Idle);
        QVERIFY(client.errorMessage().isEmpty());
        QVERIFY(client.assignments().isEmpty());
    }

    void assignmentsSocketFailureLeavesTheUserFacingStateAlone() {
        // Hold the success auto-refresh in flight, then kill the daemon: the
        // mirror fails over the socket (not the daemon-answered path) and
        // must requeue in the background without clobbering the confirmed
        // apply result or the state.
        m_daemon.holdByMethod.insert(QStringLiteral("wallpaper.assignments"));
        ApplyClient client(m_socketPath);
        client.applyWallpaper(QStringLiteral("DP-1"), QStringLiteral("1"), QStringLiteral("video"),
                              QUrl::fromLocalFile(QStringLiteral("/x.mp4")));
        QTRY_VERIFY_WITH_TIMEOUT(client.state() == ApplyClient::Applied, 5000);
        QTRY_VERIFY_WITH_TIMEOUT(m_daemon.receivedMethods.size() == 2, 5000);
        QCOMPARE(m_daemon.receivedMethods.at(1), QStringLiteral("wallpaper.assignments"));
        m_daemon.dropClients();
        // The mirror requeues at the front and retries (the daemon accepts
        // again within the backoff window); the apply confirmation survives
        // the whole detour.
        QTRY_VERIFY_WITH_TIMEOUT(m_daemon.receivedMethods.size() == 3, 15000);
        QCOMPARE(client.state(), ApplyClient::Applied);
        QCOMPARE(client.appliedOutput(), QStringLiteral("DP-1"));
        QCOMPARE(client.appliedWallpaperId(), QStringLiteral("1"));
        QVERIFY(client.errorMessage().isEmpty());
        QVERIFY(client.failedMethod().isEmpty());
    }

    void failedEnumerationIsNotLabeledRestoreAndRetries() {
        m_daemon.failByMethod.insert(QStringLiteral("wallpaper.outputs"),
                                     {QStringLiteral("shell_unreachable"),
                                      QStringLiteral("plasma crashed")});
        ApplyClient client(m_socketPath);
        client.listOutputs();
        QTRY_VERIFY_WITH_TIMEOUT(client.state() == ApplyClient::Failed, 5000);
        QVERIFY(client.errorMessage().contains(QStringLiteral("plasma crashed")));
        QVERIFY(client.errorMessage().contains(QStringLiteral("could not be reached")));
        // An enumeration failure has no recorded target to replay: the UI
        // hides Try Again, so it must not be mislabeled "restore".
        QVERIFY(client.failedMethod().isEmpty());
        QVERIFY(!client.busy());
        // retry() re-runs the listing itself.
        m_daemon.failByMethod.clear();
        client.retry();
        const QStringList expected{QStringLiteral("DP-1"), QStringLiteral("HDMI-A-1")};
        QTRY_VERIFY_WITH_TIMEOUT(client.outputs() == expected, 10000);
        QCOMPARE(client.state(), ApplyClient::Idle);
        QVERIFY(client.errorMessage().isEmpty());
        QCOMPARE(m_daemon.receivedMethods.size(), 2);
        QCOMPARE(m_daemon.receivedMethods.first(), QStringLiteral("wallpaper.outputs"));
        QCOMPARE(m_daemon.receivedMethods.at(1), QStringLiteral("wallpaper.outputs"));
    }

    void retryRerunsTheExactFailedApply() {
        m_daemon.failByMethod.insert(QStringLiteral("wallpaper.apply"),
                                     {QStringLiteral("apply_failed"), QStringLiteral("first attempt")});
        ApplyClient client(m_socketPath);
        client.applyWallpaper(QStringLiteral("DP-1"), QStringLiteral("1"), QStringLiteral("video"),
                              QUrl::fromLocalFile(QStringLiteral("/x.mp4")));
        QTRY_VERIFY_WITH_TIMEOUT(client.state() == ApplyClient::Failed, 5000);
        QCOMPARE(client.failedMethod(), QStringLiteral("apply"));
        m_daemon.failByMethod.clear();
        client.retry();
        QTRY_VERIFY_WITH_TIMEOUT(client.state() == ApplyClient::Applied, 5000);
        QCOMPARE(client.appliedOutput(), QStringLiteral("DP-1"));
        QCOMPARE(m_daemon.receivedMethods.size(), 3); // apply, apply, assignments
        QCOMPARE(m_daemon.receivedParams.at(1).value(QStringLiteral("content")).toString(),
                 QStringLiteral("/x.mp4"));
    }

    void retryAfterQuarantineSendsTheClearFlagOnce() {
        // B4: the first apply never carries `retry`; after apply_quarantined
        // the replay does; a later fresh apply does not inherit it.
        m_daemon.failByMethod.insert(QStringLiteral("wallpaper.apply"),
                                     {QStringLiteral("apply_quarantined"),
                                      QStringLiteral("disabled after 3 failures")});
        ApplyClient client(m_socketPath);
        client.applyWallpaper(QStringLiteral("DP-1"), QStringLiteral("1"), QStringLiteral("web"),
                              QUrl::fromLocalFile(QStringLiteral("/w")));
        QTRY_VERIFY_WITH_TIMEOUT(client.state() == ApplyClient::Failed, 5000);
        QVERIFY(client.errorMessage().contains(QStringLiteral("disabled after repeated failures")));
        QVERIFY(!m_daemon.receivedParams.first().contains(QStringLiteral("retry")));
        m_daemon.failByMethod.clear();
        client.retry();
        QTRY_VERIFY_WITH_TIMEOUT(client.state() == ApplyClient::Applied, 5000);
        QCOMPARE(m_daemon.receivedParams.at(1).value(QStringLiteral("retry")).toBool(), true);
        client.applyWallpaper(QStringLiteral("DP-1"), QStringLiteral("2"), QStringLiteral("web"),
                              QUrl::fromLocalFile(QStringLiteral("/w2")));
        QTRY_VERIFY_WITH_TIMEOUT(m_daemon.receivedMethods.size() >= 4, 5000);
        const auto fresh = m_daemon.receivedParams.at(3);
        QCOMPARE(fresh.value(QStringLiteral("wallpaper_id")).toString(), QStringLiteral("2"));
        QVERIFY(!fresh.contains(QStringLiteral("retry")));
    }

    void scalingModeTravelsOnlyWhenNotDefault() {
        // F1: the default apply carries no `scaling`; fill/stretch do; the
        // retry replays the mode that failed.
        ApplyClient client(m_socketPath);
        client.applyWallpaper(QStringLiteral("DP-1"), QStringLiteral("1"), QStringLiteral("video"),
                              QUrl::fromLocalFile(QStringLiteral("/x.mp4")));
        QTRY_VERIFY_WITH_TIMEOUT(client.state() == ApplyClient::Applied, 5000);
        QVERIFY(!m_daemon.receivedParams.first().contains(QStringLiteral("scaling")));
        m_daemon.failByMethod.insert(QStringLiteral("wallpaper.apply"),
                                     {QStringLiteral("apply_failed"), QStringLiteral("boom")});
        client.applyWallpaper(QStringLiteral("DP-1"), QStringLiteral("1"), QStringLiteral("video"),
                              QUrl::fromLocalFile(QStringLiteral("/x.mp4")),
                              QStringLiteral("fill"));
        QTRY_VERIFY_WITH_TIMEOUT(client.state() == ApplyClient::Failed, 5000);
        const int failedIndex = m_daemon.receivedParams.size() - 1;
        QCOMPARE(m_daemon.receivedParams.at(failedIndex).value(QStringLiteral("scaling")).toString(),
                 QStringLiteral("fill"));
        m_daemon.failByMethod.clear();
        client.retry();
        QTRY_VERIFY_WITH_TIMEOUT(client.state() == ApplyClient::Applied, 5000);
        QCOMPARE(m_daemon.receivedParams.at(failedIndex + 1).value(QStringLiteral("scaling")).toString(),
                 QStringLiteral("fill"));
        // An invalid mode never reaches the wire.
        const int before = m_daemon.receivedMethods.size();
        client.applyWallpaper(QStringLiteral("DP-1"), QStringLiteral("1"), QStringLiteral("video"),
                              QUrl::fromLocalFile(QStringLiteral("/x.mp4")),
                              QStringLiteral("tile"));
        QCOMPARE(client.state(), ApplyClient::Failed);
        QCOMPARE(m_daemon.receivedMethods.size(), before);
    }

    void retryRerunsAFailedRestore() {
        m_daemon.failByMethod.insert(QStringLiteral("wallpaper.restore"),
                                     {QStringLiteral("restore_failed"), QStringLiteral("boom")});
        ApplyClient client(m_socketPath);
        client.restoreWallpaper(QStringLiteral("DP-1"));
        QTRY_VERIFY_WITH_TIMEOUT(client.state() == ApplyClient::Failed, 5000);
        QCOMPARE(client.failedMethod(), QStringLiteral("restore"));
        m_daemon.failByMethod.clear();
        client.retry();
        QTRY_VERIFY_WITH_TIMEOUT(client.restoredOutput() == QStringLiteral("DP-1"), 5000);
        QCOMPARE(client.restoreMode(), QStringLiteral("stock"));
        QCOMPARE(client.state(), ApplyClient::Idle);
    }

    void resetStatusClearsResultsButKeepsOutputs() {
        ApplyClient client(m_socketPath);
        client.applyWallpaper(QStringLiteral("DP-1"), QStringLiteral("1"), QStringLiteral("video"),
                              QUrl::fromLocalFile(QStringLiteral("/x.mp4")));
        QTRY_VERIFY_WITH_TIMEOUT(client.state() == ApplyClient::Applied, 5000);
        client.resetStatus();
        QCOMPARE(client.state(), ApplyClient::Idle);
        QVERIFY(client.appliedOutput().isEmpty());
        QVERIFY(client.appliedWallpaperId().isEmpty());
        QVERIFY(client.errorMessage().isEmpty());
        // The output enumeration survives a status reset.
        client.listOutputs();
        QTRY_VERIFY_WITH_TIMEOUT(client.outputs().size() > 0, 5000);
        QCOMPARE(client.state(), ApplyClient::Idle);
    }

    void emptyEnumerationIsAnAnswerNotSilence() {
        // A daemon that truthfully reports zero outputs used to leave the
        // picker mutely empty: applyOutputs() returned early on the unchanged
        // list, so nothing signalled and the UI could not tell "asked and got
        // none" apart from "never asked".
        m_daemon.outputs = QJsonArray{};
        ApplyClient client(m_socketPath);
        QVERIFY(!client.outputsListed());
        client.listOutputs();
        QTRY_VERIFY_WITH_TIMEOUT(client.outputsListed(), 5000);
        QVERIFY(client.outputs().isEmpty());
        QCOMPARE(client.state(), ApplyClient::Idle);
        QVERIFY(!client.errorMessage().isEmpty());
        QCOMPARE(m_daemon.receivedMethods.size(), 1);
    }

    void resetStatusKeepsTheErrorOfAnOperationStillPending() {
        // Clearing unconditionally erased the message of a failure that was
        // still queued for retry, leaving Failed with empty text.
        m_daemon.holdByMethod.insert(QStringLiteral("wallpaper.apply"));
        ApplyClient client(m_socketPath);
        client.applyWallpaper(QStringLiteral("DP-1"), QStringLiteral("1"), QStringLiteral("video"),
                              QUrl::fromLocalFile(QStringLiteral("/x.mp4")));
        QTRY_VERIFY_WITH_TIMEOUT(m_daemon.receivedMethods.size() == 1, 5000);
        m_daemon.dropClients();
        QTRY_VERIFY_WITH_TIMEOUT(client.state() == ApplyClient::Failed, 5000);
        const auto failure = client.errorMessage();
        QVERIFY(!failure.isEmpty());
        QVERIFY(client.busy());
        client.resetStatus();
        QCOMPARE(client.errorMessage(), failure);
        QCOMPARE(client.state(), ApplyClient::Failed);
    }

    void unansweredRequestFailsAtItsDeadline() {
        // A daemon that accepts the connection and never answers used to leave
        // the client busy forever: picker disabled, no error, no retry.
        m_daemon.holdResponses = true;
        ApplyClient client(m_socketPath);
        client.listOutputs();
        QTRY_VERIFY_WITH_TIMEOUT(m_daemon.receivedMethods.size() == 1, 5000);
        QCOMPARE(client.state(), ApplyClient::ListingOutputs);
        QTRY_VERIFY_WITH_TIMEOUT(client.state() == ApplyClient::Failed, 15000);
        QVERIFY(!client.errorMessage().isEmpty());
        QVERIFY(!client.busy());
        QVERIFY(!client.outputsListed());
        // A deadline miss is never replayed on its own: the daemon may still be
        // running the transaction it never answered.
        QCOMPARE(m_daemon.receivedMethods.size(), 1);
    }

    void invalidInputIsRejectedWithoutTraffic() {
        ApplyClient client(m_socketPath);
        client.applyWallpaper(QStringLiteral("DP-1"), QStringLiteral("1"), QStringLiteral("test"),
                              QUrl::fromLocalFile(QStringLiteral("/x")));
        QVERIFY(!client.errorMessage().isEmpty());
        QCOMPARE(client.state(), ApplyClient::Failed);
        client.applyWallpaper(QStringLiteral("../escape"), QStringLiteral("1"),
                              QStringLiteral("video"), QUrl::fromLocalFile(QStringLiteral("/x")));
        QVERIFY(!client.errorMessage().isEmpty());
        client.applyWallpaper(QStringLiteral("DP-1"), QString(130, QLatin1Char('x')),
                              QStringLiteral("video"), QUrl::fromLocalFile(QStringLiteral("/x")));
        QVERIFY(!client.errorMessage().isEmpty());
        client.restoreWallpaper(QString());
        QVERIFY(!client.errorMessage().isEmpty());
        QCOMPARE(m_daemon.receivedMethods.size(), 0);
        // A valid request still works afterwards.
        client.listOutputs();
        QTRY_VERIFY_WITH_TIMEOUT(client.outputs().size() > 0, 5000);
        QCOMPARE(m_daemon.receivedMethods.size(), 1);
    }

private:
    QTemporaryDir m_settingsRoot;
    StubDaemon m_daemon;
    QString m_socketPath;
};

QTEST_GUILESS_MAIN(ApplyClientTest)
#include "applyclienttest.moc"
