// SPDX-License-Identifier: GPL-3.0-or-later
#pragma once

#include <QByteArray>
#include <QLocalSocket>
#include <QObject>
#include <QString>
#include <QTimer>
#include <qqmlintegration.h>

#include <limits>

class InputClient : public QObject {
  Q_OBJECT
  QML_ELEMENT
  Q_PROPERTY(QString socketPath READ socketPath WRITE setSocketPath NOTIFY
                 configurationChanged)
  Q_PROPERTY(qulonglong displayGeneration READ displayGeneration WRITE
                 setDisplayGeneration NOTIFY configurationChanged)
  Q_PROPERTY(bool enabled READ enabled NOTIFY configurationChanged)
  Q_PROPERTY(State state READ state NOTIFY stateChanged)
  Q_PROPERTY(QString stateText READ stateText NOTIFY stateChanged)
  Q_PROPERTY(QString errorMessage READ errorMessage NOTIFY errorMessageChanged)
  Q_PROPERTY(qulonglong lastAcceptedSequence READ lastAcceptedSequence NOTIFY
                 lastAcceptedSequenceChanged)
  Q_PROPERTY(qulonglong coalescedEvents READ coalescedEvents NOTIFY
                 coalescedEventsChanged)

public:
  enum State { Disabled, Ready, Sending, Error };
  Q_ENUM(State)

  explicit InputClient(QObject *parent = nullptr);

  QString socketPath() const { return m_socketPath; }
  qulonglong displayGeneration() const { return m_displayGeneration; }
  bool enabled() const {
    return !m_socketPath.isEmpty() && m_displayGeneration != 0 &&
           m_displayGeneration <=
               qulonglong(std::numeric_limits<qint64>::max());
  }
  State state() const { return m_state; }
  QString stateText() const;
  QString errorMessage() const { return m_errorMessage; }
  qulonglong lastAcceptedSequence() const { return m_lastAcceptedSequence; }
  qulonglong coalescedEvents() const { return m_coalescedEvents; }

  void setSocketPath(const QString &socketPath);
  void setDisplayGeneration(qulonglong displayGeneration);

public slots:
  void sendPointer(const QString &phase, qreal x, qreal y);

signals:
  void configurationChanged();
  void stateChanged();
  void errorMessageChanged();
  void lastAcceptedSequenceChanged();
  void coalescedEventsChanged();

private:
  struct Event {
    QString phase;
    qreal x = 0.0;
    qreal y = 0.0;
    bool valid = false;
  };

  void resetForConfiguration();
  void begin(Event event);
  void sendRequest();
  void consumeResponse();
  void fail(const QString &message);
  void completeAndContinue();
  void setState(State state, const QString &error = {});

  QString m_socketPath;
  QString m_errorMessage;
  qulonglong m_displayGeneration = 0;
  QLocalSocket m_socket;
  QTimer m_requestTimeout;
  QTimer m_retryCooldown;
  QByteArray m_buffer;
  Event m_current;
  Event m_pending;
  State m_state = Disabled;
  qulonglong m_requestId = 0;
  qulonglong m_lastAcceptedSequence = 0;
  qulonglong m_coalescedEvents = 0;
  bool m_disconnectExpected = false;
};
