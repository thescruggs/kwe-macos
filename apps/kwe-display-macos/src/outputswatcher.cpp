// SPDX-License-Identifier: GPL-3.0-or-later
#include "outputswatcher.h"

#include <QJsonArray>
#include <QJsonDocument>
#include <QJsonObject>
#include <limits>

namespace {
constexpr qsizetype MaxReplyBytes = 64 * 1024;
constexpr int RequestTimeoutMilliseconds = 2000;
} // namespace

OutputsWatcher::OutputsWatcher(QString socketPath, int pollMilliseconds, QObject *parent)
    : QObject(parent), m_socketPath(std::move(socketPath)) {
  m_pollTimer.setInterval(pollMilliseconds);
  m_pollTimer.setTimerType(Qt::CoarseTimer);
  connect(&m_pollTimer, &QTimer::timeout, this, &OutputsWatcher::refresh);
  m_requestTimeout.setSingleShot(true);
  m_requestTimeout.setInterval(RequestTimeoutMilliseconds);
  connect(&m_requestTimeout, &QTimer::timeout, this, [this] {
    finish(false, {}, QStringLiteral("wallpaper service did not answer in time"));
  });
  connect(&m_socket, &QLocalSocket::connected, this, [this] {
    if (m_requestId >= std::numeric_limits<qint64>::max()) {
      finish(false, {}, QStringLiteral("request sequence exhausted"));
      return;
    }
    ++m_requestId;
    const QByteArray request =
        QJsonDocument(QJsonObject{{QStringLiteral("version"), 1},
                                  {QStringLiteral("id"), m_requestId},
                                  {QStringLiteral("method"), QStringLiteral("wallpaper.outputs")},
                                  {QStringLiteral("params"), QJsonObject{}}})
            .toJson(QJsonDocument::Compact) +
        '\n';
    if (m_socket.write(request) != request.size())
      finish(false, {}, QStringLiteral("request could not be queued"));
  });
  connect(&m_socket, &QLocalSocket::readyRead, this, &OutputsWatcher::consume);
  connect(&m_socket, &QLocalSocket::errorOccurred, this, [this](QLocalSocket::LocalSocketError) {
    if (m_inFlight)
      finish(false, {}, m_socket.errorString());
  });
  m_pollTimer.start();
  QTimer::singleShot(0, this, &OutputsWatcher::refresh);
}

void OutputsWatcher::refresh() {
  if (m_inFlight || m_socketPath.isEmpty())
    return;
  m_inFlight = true;
  m_buffer.clear();
  m_requestTimeout.start();
  m_socket.connectToServer(m_socketPath, QIODevice::ReadWrite);
}

void OutputsWatcher::consume() {
  m_buffer += m_socket.readAll();
  if (m_buffer.size() > MaxReplyBytes) {
    finish(false, {}, QStringLiteral("oversized reply"));
    return;
  }
  const qsizetype newline = m_buffer.indexOf('\n');
  if (newline < 0)
    return;
  QJsonParseError parseError;
  const QJsonDocument document = QJsonDocument::fromJson(m_buffer.left(newline), &parseError);
  if (parseError.error != QJsonParseError::NoError || !document.isObject()) {
    finish(false, {}, QStringLiteral("invalid JSON reply"));
    return;
  }
  const QJsonObject response = document.object();
  if (response.value(QStringLiteral("version")).toInt(-1) != 1 ||
      response.value(QStringLiteral("id")).toInteger(-1) != m_requestId) {
    finish(false, {}, QStringLiteral("mismatched reply"));
    return;
  }
  if (!response.value(QStringLiteral("ok")).toBool(false)) {
    // The daemon answered but cannot enumerate (no display session, shell
    // unreachable): treat as no outputs, keep polling.
    const QJsonObject error = response.value(QStringLiteral("error")).toObject();
    finish(true, {}, error.value(QStringLiteral("message")).toString());
    return;
  }
  QList<OutputRecord> outputs;
  const QJsonArray array =
      response.value(QStringLiteral("result")).toObject().value(QStringLiteral("outputs")).toArray();
  for (const QJsonValue &value : array) {
    const QJsonObject object = value.toObject();
    OutputRecord record;
    record.name = object.value(QStringLiteral("name")).toString();
    if (record.name.isEmpty())
      continue;
    const QJsonArray geometry = object.value(QStringLiteral("geometry")).toArray();
    if (geometry.size() == 4) {
      record.geometry = QRect(geometry.at(0).toInt(), geometry.at(1).toInt(),
                              geometry.at(2).toInt(), geometry.at(3).toInt());
      record.hasGeometry = record.geometry.width() > 0 && record.geometry.height() > 0;
    }
    record.wallpaperPlugin = object.value(QStringLiteral("wallpaper_plugin")).toString();
    outputs.append(record);
    if (outputs.size() >= 32)
      break;
  }
  finish(true, outputs, QString());
}

void OutputsWatcher::finish(bool ok, const QList<OutputRecord> &outputs, const QString &error) {
  m_requestTimeout.stop();
  m_inFlight = false;
  m_socket.abort();
  if (!error.isEmpty() && error != m_lastError)
    qWarning("kwe-display: outputs poll: %s", qPrintable(error));
  m_lastError = error;
  if (ok != m_available) {
    m_available = ok;
    emit availabilityChanged(ok);
  }
  bool changed = outputs.size() != m_outputs.size();
  for (qsizetype index = 0; !changed && index < outputs.size(); ++index) {
    const OutputRecord &next = outputs.at(index);
    const OutputRecord &previous = m_outputs.at(index);
    changed = next.name != previous.name || next.geometry != previous.geometry ||
              next.hasGeometry != previous.hasGeometry ||
              next.wallpaperPlugin != previous.wallpaperPlugin;
  }
  if (changed) {
    m_outputs = outputs;
    emit outputsChanged(m_outputs);
  }
}
