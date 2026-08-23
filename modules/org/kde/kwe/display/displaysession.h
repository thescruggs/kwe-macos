// SPDX-License-Identifier: Apache-2.0
#pragma once

#include <QByteArray>
#include <QLocalSocket>
#include <QObject>
#include <QString>
#include <QTimer>
#include <qqmlintegration.h>

class DisplaySession : public QObject {
  Q_OBJECT
  QML_ELEMENT
  Q_PROPERTY(QString socketPath READ socketPath WRITE setSocketPath NOTIFY
                 socketPathChanged)
  Q_PROPERTY(State state READ state NOTIFY stateChanged)
  Q_PROPERTY(QString stateText READ stateText NOTIFY stateChanged)
  Q_PROPERTY(QString errorMessage READ errorMessage NOTIFY errorMessageChanged)
  Q_PROPERTY(QString frameFile READ frameFile NOTIFY sourceChanged)
  Q_PROPERTY(
      qulonglong displayGeneration READ displayGeneration NOTIFY sourceChanged)
  Q_PROPERTY(bool active READ active NOTIFY activeChanged)
  Q_PROPERTY(QString phase READ phase NOTIFY phaseChanged)
  /// F1: the live renderer's scaling mode ("aspect" | "fill" | "stretch"),
  /// from renderer.status; "aspect" when the daemon predates the field.
  Q_PROPERTY(QString scaling READ scaling NOTIFY scalingChanged)
  Q_PROPERTY(bool awaitingDisplayAck READ awaitingDisplayAck NOTIFY
                 awaitingDisplayAckChanged)

public:
  enum State { Waiting, Connecting, Ready, Degraded };
  Q_ENUM(State)

  explicit DisplaySession(QObject *parent = nullptr);

  QString socketPath() const { return m_socketPath; }
  State state() const { return m_state; }
  QString stateText() const;
  QString errorMessage() const { return m_errorMessage; }
  QString frameFile() const { return m_frameFile; }
  qulonglong displayGeneration() const { return m_displayGeneration; }
  bool active() const { return m_active; }
  QString phase() const { return m_phase; }
  QString scaling() const { return m_scaling; }
  bool awaitingDisplayAck() const { return m_awaitingDisplayAck; }

  void setSocketPath(const QString &socketPath);

public slots:
  void refresh();
  void acknowledgeFrameFile(const QString &path);

signals:
  void socketPathChanged();
  void stateChanged();
  void errorMessageChanged();
  void sourceChanged();
  void activeChanged();
  void phaseChanged();
  void scalingChanged();
  void awaitingDisplayAckChanged();

private:
  enum class RequestKind { None, Status, Acknowledge };

  static QString defaultSocketPath();
  void startRequest(RequestKind kind);
  void sendRequest();
  void consumeResponse();
  bool applyStatus(const QJsonObject &status, QString *error);
  void finishRequest();
  void failRequest(const QString &message);
  void setState(State state, const QString &error = {});
  void setPhase(const QString &phase);
  void setScaling(const QString &scaling);
  void setAwaitingDisplayAck(bool awaiting);
  void setActive(bool active);
  bool validatedSourceIsCurrent() const;

  QString m_socketPath;
  QString m_frameFile;
  QString m_phase = QStringLiteral("idle");
  QString m_scaling = QStringLiteral("aspect");
  QString m_errorMessage;
  QString m_validatedFrameFile;
  qulonglong m_displayGeneration = 0;
  qulonglong m_validatedGeneration = 0;
  qulonglong m_requestId = 0;
  State m_state = Waiting;
  bool m_active = false;
  bool m_awaitingDisplayAck = false;
  bool m_ackPending = false;
  bool m_inFlight = false;
  bool m_disconnectExpected = false;
  RequestKind m_requestKind = RequestKind::None;
  QLocalSocket m_socket;
  QByteArray m_buffer;
  QTimer m_pollTimer;
  QTimer m_requestTimeout;
};
