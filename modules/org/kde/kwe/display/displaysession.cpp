// SPDX-License-Identifier: Apache-2.0
#include "displaysession.h"

#include <QJsonDocument>
#include <QJsonObject>
#include <QJsonValue>
#include <QStandardPaths>

#include <limits>

namespace {
constexpr qsizetype MaxResponseBytes = 64 * 1024;
constexpr int PollIntervalMilliseconds = 500;
constexpr int RequestTimeoutMilliseconds = 1000;

bool knownPhase(const QString &phase) {
  return phase == QStringLiteral("idle") ||
         phase == QStringLiteral("starting") ||
         phase == QStringLiteral("canary") || phase == QStringLiteral("live") ||
         phase == QStringLiteral("restarting") ||
         phase == QStringLiteral("awaiting_ack") ||
         phase == QStringLiteral("rolled_back") ||
         phase == QStringLiteral("stopped") ||
         phase == QStringLiteral("quarantined");
}
} // namespace

DisplaySession::DisplaySession(QObject *parent)
    : QObject(parent), m_socketPath(defaultSocketPath()) {
  m_pollTimer.setInterval(PollIntervalMilliseconds);
  m_pollTimer.setTimerType(Qt::CoarseTimer);
  connect(&m_pollTimer, &QTimer::timeout, this, &DisplaySession::refresh);

  m_requestTimeout.setSingleShot(true);
  m_requestTimeout.setInterval(RequestTimeoutMilliseconds);
  connect(&m_requestTimeout, &QTimer::timeout, this, [this] {
    failRequest(tr("The wallpaper service did not respond in time."));
  });

  connect(&m_socket, &QLocalSocket::connected, this,
          &DisplaySession::sendRequest);
  connect(&m_socket, &QLocalSocket::readyRead, this,
          &DisplaySession::consumeResponse);
  connect(&m_socket, &QLocalSocket::errorOccurred, this,
          [this](QLocalSocket::LocalSocketError error) {
            if (m_inFlight && error != QLocalSocket::PeerClosedError) {
              failRequest(tr("Could not reach the wallpaper service: %1")
                              .arg(m_socket.errorString()));
            }
          });
  connect(&m_socket, &QLocalSocket::disconnected, this, [this] {
    if (m_inFlight && !m_disconnectExpected) {
      QTimer::singleShot(0, this, [this] {
        if (m_inFlight)
          failRequest(tr("The wallpaper service closed an incomplete reply."));
      });
    }
  });

  m_pollTimer.start();
  QTimer::singleShot(0, this, &DisplaySession::refresh);
}

QString DisplaySession::defaultSocketPath() {
  const QString runtime =
      QStandardPaths::writableLocation(QStandardPaths::RuntimeLocation);
  return runtime.isEmpty() ? QString()
                           : runtime + QStringLiteral("/kwe/daemon-v1.sock");
}

QString DisplaySession::stateText() const {
  if (m_state == Connecting)
    return tr("Connecting to wallpaper service");
  if (m_state == Degraded)
    return tr("Wallpaper service unavailable — showing last good frame");
  if (m_state == Waiting)
    return tr("Waiting for wallpaper service");
  if (m_phase == QStringLiteral("starting") ||
      m_phase == QStringLiteral("canary"))
    return tr("Testing wallpaper safely");
  if (m_phase == QStringLiteral("restarting"))
    return tr("Recovering wallpaper renderer");
  if (m_phase == QStringLiteral("awaiting_ack"))
    return tr("Switching to the validated wallpaper");
  if (m_phase == QStringLiteral("rolled_back"))
    return tr("Wallpaper failed — previous frame restored");
  if (m_phase == QStringLiteral("quarantined"))
    return tr("Wallpaper quarantined — showing last good frame");
  if (m_phase == QStringLiteral("stopped"))
    return tr("Wallpaper stopped — showing last good frame");
  if (!m_active)
    return tr("No active wallpaper — showing safe fallback");
  return tr("Wallpaper active");
}

void DisplaySession::setSocketPath(const QString &socketPath) {
  if (m_socketPath == socketPath)
    return;
  m_socketPath = socketPath;
  emit socketPathChanged();
  m_disconnectExpected = true;
  m_socket.abort();
  m_disconnectExpected = false;
  m_requestTimeout.stop();
  m_buffer.clear();
  m_inFlight = false;
  m_requestKind = RequestKind::None;
  m_ackPending = false;
  setActive(false);
  setState(Waiting);
  refresh();
}

void DisplaySession::refresh() {
  if (m_inFlight)
    return;
  if (m_ackPending && validatedSourceIsCurrent() && m_awaitingDisplayAck) {
    startRequest(RequestKind::Acknowledge);
    return;
  }
  startRequest(RequestKind::Status);
}

void DisplaySession::acknowledgeFrameFile(const QString &path) {
  if (path.isEmpty() || path != m_frameFile || m_displayGeneration == 0)
    return;
  m_validatedFrameFile = path;
  m_validatedGeneration = m_displayGeneration;
  if (!m_awaitingDisplayAck)
    return;
  m_ackPending = true;
  if (!m_inFlight)
    startRequest(RequestKind::Acknowledge);
}

void DisplaySession::startRequest(RequestKind kind) {
  if (m_inFlight)
    return;
  if (m_socketPath.isEmpty()) {
    failRequest(
        tr("No runtime socket is available for the wallpaper service."));
    return;
  }
  m_requestKind = kind;
  m_inFlight = true;
  m_buffer.clear();
  m_disconnectExpected = true;
  m_socket.abort();
  m_disconnectExpected = false;
  if (m_state == Waiting)
    setState(Connecting);
  m_requestTimeout.start();
  m_socket.connectToServer(m_socketPath, QIODevice::ReadWrite);
}

void DisplaySession::sendRequest() {
  if (m_requestId >= qulonglong(std::numeric_limits<qint64>::max())) {
    failRequest(tr("The display request sequence was exhausted."));
    return;
  }
  ++m_requestId;
  QJsonObject params;
  QString method = QStringLiteral("renderer.status");
  if (m_requestKind == RequestKind::Acknowledge) {
    method = QStringLiteral("renderer.ack");
    params.insert(QStringLiteral("generation"), qint64(m_validatedGeneration));
    m_ackPending = false;
  }
  const QByteArray request =
      QJsonDocument(QJsonObject{
                        {QStringLiteral("version"), 1},
                        {QStringLiteral("id"), qint64(m_requestId)},
                        {QStringLiteral("method"), method},
                        {QStringLiteral("params"), params},
                    })
          .toJson(QJsonDocument::Compact) +
      '\n';
  if (m_socket.write(request) != request.size())
    failRequest(tr("The wallpaper service request could not be queued."));
}

void DisplaySession::consumeResponse() {
  m_buffer += m_socket.readAll();
  if (m_buffer.size() > MaxResponseBytes) {
    failRequest(tr("The wallpaper service returned an oversized reply."));
    return;
  }
  const qsizetype newline = m_buffer.indexOf('\n');
  if (newline < 0)
    return;

  QJsonParseError parseError;
  const QJsonDocument document =
      QJsonDocument::fromJson(m_buffer.left(newline), &parseError);
  if (parseError.error != QJsonParseError::NoError || !document.isObject()) {
    failRequest(tr("The wallpaper service returned invalid JSON."));
    return;
  }
  const QJsonObject response = document.object();
  const qint64 responseId = response.value(QStringLiteral("id")).toInteger(-1);
  if (response.value(QStringLiteral("version")).toInt(-1) != 1 ||
      responseId != qint64(m_requestId)) {
    failRequest(tr("The wallpaper service returned a mismatched reply."));
    return;
  }
  if (!response.value(QStringLiteral("ok")).toBool(false) ||
      !response.value(QStringLiteral("result")).isObject()) {
    failRequest(tr("The wallpaper service rejected the display request."));
    return;
  }
  QString error;
  if (!applyStatus(response.value(QStringLiteral("result")).toObject(),
                   &error)) {
    failRequest(error);
    return;
  }
  finishRequest();
}

bool DisplaySession::applyStatus(const QJsonObject &status, QString *error) {
  const QJsonValue phaseValue = status.value(QStringLiteral("phase"));
  if (!phaseValue.isString() || !knownPhase(phaseValue.toString())) {
    *error = tr("The wallpaper service returned an unknown renderer state.");
    return false;
  }
  const QJsonValue frameValue = status.value(QStringLiteral("frame_file"));
  if (!frameValue.isString() && !frameValue.isNull()) {
    *error = tr("The wallpaper service returned an invalid frame path.");
    return false;
  }
  const qint64 generationValue =
      status.value(QStringLiteral("display_generation")).toInteger(-1);
  if (generationValue < 0) {
    *error =
        tr("The wallpaper service returned an invalid display generation.");
    return false;
  }
  const qulonglong generation = qulonglong(generationValue);
  const QJsonValue awaitingValue =
      status.value(QStringLiteral("awaiting_display_ack"));
  if (!awaitingValue.isBool()) {
    *error = tr("The wallpaper service omitted handoff state.");
    return false;
  }

  const QString frameFile = frameValue.toString();
  const bool hasSource = !frameFile.isEmpty() && generation != 0;
  if (!frameFile.isEmpty() && generation == 0) {
    *error = tr("The wallpaper service returned an incomplete display source.");
    return false;
  }

  setPhase(phaseValue.toString());
  setAwaitingDisplayAck(awaitingValue.toBool());
  setActive(hasSource);
  if (hasSource &&
      (m_frameFile != frameFile || m_displayGeneration != generation)) {
    m_frameFile = frameFile;
    m_displayGeneration = generation;
    emit sourceChanged();
  }
  if (m_awaitingDisplayAck && validatedSourceIsCurrent())
    m_ackPending = true;
  return true;
}

void DisplaySession::finishRequest() {
  m_requestTimeout.stop();
  m_inFlight = false;
  m_requestKind = RequestKind::None;
  m_disconnectExpected = true;
  m_socket.abort();
  m_disconnectExpected = false;
  setState(Ready);
  if (m_ackPending && validatedSourceIsCurrent() && m_awaitingDisplayAck)
    QTimer::singleShot(0, this, &DisplaySession::refresh);
}

void DisplaySession::failRequest(const QString &message) {
  m_requestTimeout.stop();
  m_inFlight = false;
  m_requestKind = RequestKind::None;
  m_disconnectExpected = true;
  m_socket.abort();
  m_disconnectExpected = false;
  setActive(false);
  setState(Degraded, message);
}

void DisplaySession::setState(State state, const QString &error) {
  const bool stateChanged = m_state != state;
  const bool errorChanged = m_errorMessage != error;
  m_state = state;
  m_errorMessage = error;
  if (stateChanged)
    emit this->stateChanged();
  if (errorChanged)
    emit errorMessageChanged();
}

void DisplaySession::setPhase(const QString &phase) {
  if (m_phase == phase)
    return;
  m_phase = phase;
  emit phaseChanged();
  emit stateChanged();
}

void DisplaySession::setAwaitingDisplayAck(bool awaiting) {
  if (m_awaitingDisplayAck == awaiting)
    return;
  m_awaitingDisplayAck = awaiting;
  if (!awaiting)
    m_ackPending = false;
  emit awaitingDisplayAckChanged();
}

void DisplaySession::setActive(bool active) {
  if (m_active == active)
    return;
  m_active = active;
  emit activeChanged();
  emit stateChanged();
}

bool DisplaySession::validatedSourceIsCurrent() const {
  return !m_validatedFrameFile.isEmpty() &&
         m_validatedFrameFile == m_frameFile &&
         m_validatedGeneration == m_displayGeneration;
}
