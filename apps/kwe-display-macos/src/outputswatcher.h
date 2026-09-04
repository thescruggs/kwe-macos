// SPDX-License-Identifier: GPL-3.0-or-later
#pragma once

#include <QByteArray>
#include <QList>
#include <QLocalSocket>
#include <QObject>
#include <QRect>
#include <QString>
#include <QTimer>

// One entry of the daemon's `wallpaper.outputs` reply that the agent cares
// about: the output name (on macOS the CoreGraphics display UUID), its
// geometry, and which wallpaper plugin the daemon believes is active on it.
struct OutputRecord {
  QString name;
  QRect geometry;
  bool hasGeometry = false;
  QString wallpaperPlugin;
};

// Bounded poller of `wallpaper.outputs`: one short-lived connection per
// poll, 64 KiB reply cap, request timeout, and an `outputsChanged` signal
// only when the assignment picture actually changes. Connection loss is
// reported as an empty list after `unavailable` so screens go dark instead
// of showing stale state forever.
class OutputsWatcher : public QObject {
  Q_OBJECT
public:
  explicit OutputsWatcher(QString socketPath, int pollMilliseconds, QObject *parent = nullptr);

  QList<OutputRecord> outputs() const { return m_outputs; }
  bool available() const { return m_available; }

public slots:
  void refresh();

signals:
  void outputsChanged(const QList<OutputRecord> &outputs);
  void availabilityChanged(bool available);

private:
  void finish(bool ok, const QList<OutputRecord> &outputs, const QString &error);
  void consume();

  QString m_socketPath;
  QTimer m_pollTimer;
  QTimer m_requestTimeout;
  QLocalSocket m_socket;
  QByteArray m_buffer;
  qint64 m_requestId = 0;
  bool m_inFlight = false;
  bool m_available = false;
  QList<OutputRecord> m_outputs;
  QString m_lastError;
};
