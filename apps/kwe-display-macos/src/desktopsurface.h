// SPDX-License-Identifier: GPL-3.0-or-later
#pragma once

#include "outputswatcher.h"

#include <QList>
#include <QPointer>
#include <QQuickView>
#include <QScreen>
#include <QString>

// One desktop window per screen. Stays hidden until the daemon reports the
// kwe plugin on the output whose geometry matches this screen; the QML
// inside (Desktop.qml) is the same DisplaySession + FrameSurface +
// InputClient triple the Plasma package uses.
class DesktopSurface : public QQuickView {
  Q_OBJECT
public:
  DesktopSurface(QQmlEngine *engine, QScreen *screen, QString socketPath, bool desktopLevel);

  QScreen *targetScreen() const { return m_screen; }
  QString outputName() const { return m_outputName; }
  bool covering() const { return m_covering; }
  // Smoke helpers: whether the FrameSurface holds a validated frame, and
  // its sequence number.
  bool hasFrame() const;
  qulonglong frameSequence() const;

  // Re-evaluates which daemon output this screen is, and whether it should
  // be covered. Returns true when the visible state changed.
  bool applyOutputs(const QList<OutputRecord> &outputs, bool available);

  // Re-assert desktop level and back ordering (Finder redraws its own
  // desktop window at the same level after wake, relaunch, Space changes).
  void reassertDesktopLevel();

  // Pointer position in global top-left logical coordinates; forwarded as
  // a normalized position when it falls inside this surface.
  void forwardGlobalPointer(const QPointF &global);
  void pointerLeft();

private:
  void syncGeometry();
  void forwardPointer(const QString &phase, qreal x, qreal y);

  QPointer<QScreen> m_screen;
  QString m_socketPath;
  bool m_desktopLevel;
  bool m_desktopApplied = false;
  bool m_covering = false;
  bool m_pointerInside = false;
  QString m_outputName;
};

// Geometry match between a QScreen and the daemon's outputs: exact rect
// first, then a unique same-size output, then the only output for the
// only screen. Empty when nothing matches.
QString matchOutput(const QRect &screenGeometry, int screenCount, const QList<OutputRecord> &outputs);
