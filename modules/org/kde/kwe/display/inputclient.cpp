// SPDX-License-Identifier: Apache-2.0
#include "inputclient.h"

#include <QJsonDocument>
#include <QJsonObject>
#include <QTimer>

#include <cmath>

namespace {
constexpr qsizetype MaxResponseBytes = 64 * 1024;
constexpr int RequestTimeoutMilliseconds = 1000;
constexpr int RetryCooldownMilliseconds = 1000;
} // namespace

InputClient::InputClient(QObject *parent) : QObject(parent) {
  m_requestTimeout.setSingleShot(true);
  m_requestTimeout.setInterval(RequestTimeoutMilliseconds);
  connect(&m_requestTimeout, &QTimer::timeout, this, [this] {
    fail(tr("The wallpaper service did not accept pointer input in time."));
  });
  m_retryCooldown.setSingleShot(true);
  m_retryCooldown.setInterval(RetryCooldownMilliseconds);
  connect(&m_retryCooldown, &QTimer::timeout, this, [this] {
    if (!enabled()) {
      setState(Disabled);
      return;
    }
    const Event next = m_pending;
    m_pending = {};
    setState(Ready);
    if (next.valid)
      begin(next);
  });
  connect(&m_socket, &QLocalSocket::connected, this, &InputClient::sendRequest);
  connect(&m_socket, &QLocalSocket::readyRead, this,
          &InputClient::consumeResponse);
  connect(&m_socket, &QLocalSocket::errorOccurred, this,
          [this](QLocalSocket::LocalSocketError error) {
            if (m_state == Sending && error != QLocalSocket::PeerClosedError) {
              fail(tr("Could not forward pointer position: %1")
                       .arg(m_socket.errorString()));
            }
          });
  connect(&m_socket, &QLocalSocket::disconnected, this, [this] {
    if (m_state == Sending && !m_disconnectExpected) {
      QTimer::singleShot(0, this, [this] {
        if (m_state == Sending)
          fail(tr("The wallpaper service closed the pointer request before a "
                  "complete response."));
      });
    }
  });
}

QString InputClient::stateText() const {
  switch (m_state) {
  case Disabled:
    return tr("Pointer position disabled");
  case Ready:
    return tr("Pointer position active");
  case Sending:
    return tr("Forwarding pointer position");
  case Error:
    return tr("Pointer forwarding unavailable");
  }
  return {};
}

void InputClient::setSocketPath(const QString &socketPath) {
  if (m_socketPath == socketPath)
    return;
  m_socketPath = socketPath;
  resetForConfiguration();
}

void InputClient::setDisplayGeneration(qulonglong displayGeneration) {
  if (m_displayGeneration == displayGeneration)
    return;
  m_displayGeneration = displayGeneration;
  resetForConfiguration();
}

void InputClient::resetForConfiguration() {
  m_disconnectExpected = true;
  m_socket.abort();
  m_disconnectExpected = false;
  m_buffer.clear();
  m_requestTimeout.stop();
  m_retryCooldown.stop();
  m_current = {};
  m_pending = {};
  setState(enabled() ? Ready : Disabled);
  emit configurationChanged();
}

void InputClient::sendPointer(const QString &phase, qreal x, qreal y) {
  if (!enabled())
    return;
  if ((phase != QStringLiteral("enter") && phase != QStringLiteral("move") &&
       phase != QStringLiteral("leave")) ||
      !std::isfinite(x) || !std::isfinite(y) || x < 0.0 || x > 1.0 || y < 0.0 ||
      y > 1.0) {
    fail(tr("The display surface produced an invalid normalized pointer "
            "position."));
    return;
  }
  Event event{phase, x, y, true};
  if (m_state == Error) {
    if (m_pending.valid)
      ++m_coalescedEvents;
    m_pending = event;
    emit coalescedEventsChanged();
    return;
  }
  if (m_state == Sending) {
    if (m_pending.valid)
      ++m_coalescedEvents;
    m_pending = event;
    emit coalescedEventsChanged();
    return;
  }
  begin(event);
}

void InputClient::begin(Event event) {
  m_current = std::move(event);
  m_buffer.clear();
  m_disconnectExpected = true;
  m_socket.abort();
  m_disconnectExpected = false;
  setState(Sending);
  m_requestTimeout.start();
  m_socket.connectToServer(m_socketPath, QIODevice::ReadWrite);
}

void InputClient::sendRequest() {
  if (m_requestId >= qulonglong(std::numeric_limits<qint64>::max())) {
    fail(tr("The pointer request sequence was exhausted."));
    return;
  }
  ++m_requestId;
  const QJsonObject params{
      {QStringLiteral("generation"), qint64(m_displayGeneration)},
      {QStringLiteral("phase"), m_current.phase},
      {QStringLiteral("x"), m_current.x},
      {QStringLiteral("y"), m_current.y},
  };
  const auto request =
      QJsonDocument(
          QJsonObject{
              {QStringLiteral("version"), 1},
              {QStringLiteral("id"), qint64(m_requestId)},
              {QStringLiteral("method"), QStringLiteral("renderer.input")},
              {QStringLiteral("params"), params},
          })
          .toJson(QJsonDocument::Compact) +
      '\n';
  if (m_socket.write(request) != request.size()) {
    fail(tr("The pointer request could not be queued."));
  }
}

void InputClient::consumeResponse() {
  m_buffer += m_socket.readAll();
  if (m_buffer.size() > MaxResponseBytes) {
    fail(tr("The wallpaper service returned an oversized pointer response."));
    return;
  }
  const auto newline = m_buffer.indexOf('\n');
  if (newline < 0)
    return;
  QJsonParseError parseError;
  const auto document =
      QJsonDocument::fromJson(m_buffer.left(newline), &parseError);
  if (parseError.error != QJsonParseError::NoError || !document.isObject()) {
    fail(tr("The wallpaper service returned an invalid pointer response."));
    return;
  }
  const auto response = document.object();
  const qint64 responseId = response.value(QStringLiteral("id")).toInteger(-1);
  if (response.value(QStringLiteral("version")).toInt() != 1 ||
      responseId != qint64(m_requestId)) {
    fail(tr("The wallpaper service returned a mismatched pointer response."));
    return;
  }
  if (!response.value(QStringLiteral("ok")).toBool()) {
    fail(tr("The wallpaper service rejected pointer input for this display "
            "generation."));
    return;
  }
  const qint64 sequence = response.value(QStringLiteral("result"))
                              .toObject()
                              .value(QStringLiteral("input_sequence"))
                              .toInteger(-1);
  if (sequence <= 0) {
    fail(tr("The wallpaper service omitted the accepted pointer sequence."));
    return;
  }
  if (qulonglong(sequence) > m_lastAcceptedSequence) {
    m_lastAcceptedSequence = qulonglong(sequence);
    emit lastAcceptedSequenceChanged();
  }
  completeAndContinue();
}

void InputClient::fail(const QString &message) {
  m_requestTimeout.stop();
  m_current = {};
  setState(Error, message);
  m_disconnectExpected = true;
  m_socket.abort();
  m_disconnectExpected = false;
  m_retryCooldown.start();
}

void InputClient::completeAndContinue() {
  m_requestTimeout.stop();
  m_retryCooldown.stop();
  m_current = {};
  const Event next = m_pending;
  m_pending = {};
  setState(Ready);
  m_disconnectExpected = true;
  m_socket.abort();
  m_disconnectExpected = false;
  if (next.valid)
    QTimer::singleShot(0, this, [this, next] { begin(next); });
}

void InputClient::setState(State state, const QString &error) {
  const bool stateChanged = m_state != state;
  const bool errorChanged = m_errorMessage != error;
  m_state = state;
  m_errorMessage = error;
  if (stateChanged)
    emit this->stateChanged();
  if (errorChanged)
    emit errorMessageChanged();
}
