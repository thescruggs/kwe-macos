// SPDX-License-Identifier: Apache-2.0
#pragma once

#include <QByteArray>
#include <QJsonObject>
#include <QList>
#include <QLocalSocket>
#include <QObject>
#include <QQmlEngine>
#include <QString>
#include <QStringList>
#include <QTimer>
#include <QUrl>
#include <QVariantMap>
#include <functional>

// Daemon-backed live apply lane (BETA_M4b). Mirrors PermissionsClient's
// QLocalSocket request pattern: one request per connection, newline-delimited
// JSON, bounded responses; requests queue (bounded) and retry with backoff so
// an apply/restore survives a daemon restart. The lane is strictly
// serialized — the daemon rejects a concurrent apply with `apply_busy` and
// restore takes no transaction lock, so this client never issues a second
// wallpaper.* operation while one is in flight. The daemon contract is
// docs/SUPERVISOR_API_V1.md (wallpaper.outputs/apply/restore/assignments).
class ApplyClient final : public QObject {
    Q_OBJECT
    // Instances are constructed in C++ and exposed as a context property; QML
    // still needs the registered type to reach the State enum values.
    QML_ELEMENT
    QML_UNCREATABLE("ApplyClient is created by the manager")
    Q_PROPERTY(State state READ state NOTIFY stateChanged)
    Q_PROPERTY(QStringList outputs READ outputs NOTIFY outputsChanged)
    /// True once the daemon has answered an enumeration, whatever it
    /// contained. Without it an empty picker cannot be told apart from a
    /// picker that was never filled, and the UI has nothing truthful to
    /// say about either.
    Q_PROPERTY(bool outputsListed READ outputsListed NOTIFY outputsChanged)
    Q_PROPERTY(QString errorMessage READ errorMessage NOTIFY errorMessageChanged)
    Q_PROPERTY(bool busy READ busy NOTIFY busyChanged)
    Q_PROPERTY(QString appliedWallpaperId READ appliedWallpaperId NOTIFY resultChanged)
    Q_PROPERTY(QString appliedOutput READ appliedOutput NOTIFY resultChanged)
    Q_PROPERTY(QString restoredOutput READ restoredOutput NOTIFY resultChanged)
    Q_PROPERTY(QString restoreMode READ restoreMode NOTIFY resultChanged)
    /// "apply" or "restore" when the last user-facing operation failed
    /// (the UI's retry affordance targets exactly that operation); empty
    /// when nothing failed or when the failed operation was an enumeration
    /// (no recorded target to replay — the UI hides Try Again).
    Q_PROPERTY(QString failedMethod READ failedMethod NOTIFY resultChanged)
    Q_PROPERTY(QVariantMap assignments READ assignments NOTIFY assignmentsChanged)

public:
    enum State { Idle, ListingOutputs, Applying, Restoring, Applied, Failed };
    Q_ENUM(State)

    explicit ApplyClient(QString socketPath, QObject *parent = nullptr);
    State state() const { return m_state; }
    QStringList outputs() const { return m_outputs; }
    bool outputsListed() const { return m_outputsListed; }
    QString errorMessage() const { return m_errorMessage; }
    /// True while any apply-lane operation is queued or in flight; the UI
    /// disables the picker and both actions while it holds.
    bool busy() const { return m_current.method != None || !m_queue.isEmpty(); }
    QString appliedWallpaperId() const { return m_appliedWallpaperId; }
    QString appliedOutput() const { return m_appliedOutput; }
    QString restoredOutput() const { return m_restoredOutput; }
    /// Restore result mode from the daemon: "assignment" (saved previous
    /// config reverted) or "stock" (stock image wallpaper fallback).
    QString restoreMode() const { return m_restoreMode; }
    QString failedMethod() const { return m_failedMethod; }
    QVariantMap assignments() const { return m_assignments; }

    /// Refreshes the live output enumeration (wallpaper.outputs). The daemon
    /// caches it 5 s, so the page can call this on every appearance.
    Q_INVOKABLE void listOutputs();
    /// Applies one catalog wallpaper to an output (wallpaper.apply). Content
    /// is the catalog content path as a URL: the entry file for video/scene,
    /// the content root for web; an empty URL is omitted and the daemon
    /// starts its own catalog content.
    Q_INVOKABLE void applyWallpaper(const QString &output, const QString &wallpaperId,
                                    const QString &kind, const QUrl &content);
    /// Safe-mode restore (wallpaper.restore): reverts the saved previous
    /// wallpaper config on the output, or restores the stock image wallpaper
    /// when no assignment exists.
    Q_INVOKABLE void restoreWallpaper(const QString &output);
    /// Refreshes the mirrored assignment store (wallpaper.assignments).
    Q_INVOKABLE void refreshAssignments();
    /// Clears the last result (confirmation/error) without touching the
    /// output enumeration; called when the selection changes so a result for
    /// one wallpaper never reads as the next one's.
    Q_INVOKABLE void resetStatus();
    /// Re-runs the operation that just failed (apply, restore, or the output
    /// enumeration), for the UI's retry affordance. No-op when nothing
    /// failed.
    Q_INVOKABLE void retry();

signals:
    void stateChanged();
    void outputsChanged();
    void errorMessageChanged();
    void busyChanged();
    void resultChanged();
    void assignmentsChanged();

private:
    enum Method { None, ListOutputs, Apply, Restore, Assignments };

    struct Pending {
        Method method = None;
        QString output;
        QString wallpaperId;
        QString kind;
        QString content;
        std::function<void(bool ok, const QJsonObject &result, const QString &error)> callback;
    };

    void send(Pending pending);
    void begin(Pending pending);
    void writeRequest();
    void consumeResponse();
    /// Fails the in-flight operation. `requeue` false abandons it instead of
    /// replaying it automatically — used for a deadline miss, where the
    /// daemon may still be running the transaction it never answered.
    void failCurrent(const QString &error, bool requeue = true);
    /// Records the failed operation so the UI's Try Again replays exactly
    /// it, and nothing else.
    void recordFailure(const Pending &pending);
    int requestTimeoutMilliseconds(Method method) const;
    void drainQueue();
    void retryLater();
    void setState(State state);
    void setErrorMessage(const QString &message);
    void clearResult();
    void finish(bool ok, const QJsonObject &result, const QString &errorCode,
                const QString &detail);
    void applyOutputs(const QJsonObject &result);
    void applyAssignments(const QJsonObject &result);
    /// Maps the daemon's wire error codes to actionable user-facing text.
    /// `detail` is the daemon's bounded failure detail.
    static QString mapError(const QString &code, const QString &detail);
    static bool validIdentity(const QString &value);
    static bool validKind(const QString &kind);

    QString m_socketPath;
    QLocalSocket m_socket;
    QByteArray m_buffer;
    State m_state = Idle;
    QString m_errorMessage;
    QList<Pending> m_queue;
    Pending m_current;
    QTimer m_retryTimer;
    // Bounds every request: a daemon that accepts the connection and never
    // answers used to leave this client busy forever, with the picker
    // disabled and no error anywhere.
    QTimer m_requestTimer;
    int m_retryDelayMilliseconds = 5000;
    int m_requestSerial = 0;
    QStringList m_outputs;
    bool m_outputsListed = false;
    QString m_appliedWallpaperId;
    QString m_appliedOutput;
    QString m_restoredOutput;
    QString m_restoreMode;
    QString m_failedMethod;
    Method m_lastFailedMethod = None;
    QString m_lastFailedOutput;
    QString m_lastFailedWallpaperId;
    QString m_lastFailedKind;
    QString m_lastFailedContent;
    QVariantMap m_assignments;
};
