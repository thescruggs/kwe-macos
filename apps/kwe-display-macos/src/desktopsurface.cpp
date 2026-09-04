// SPDX-License-Identifier: GPL-3.0-or-later
#include "desktopsurface.h"
#include "platform.h"

#include <QQmlEngine>
#include <QQuickItem>
#include <QVariant>

namespace {
const QString KwePlugin = QStringLiteral("org.kde.kwe.wallpaper");
}

QString matchOutput(const QRect &screenGeometry, int screenCount, const QList<OutputRecord> &outputs) {
  for (const OutputRecord &output : outputs)
    if (output.hasGeometry && output.geometry == screenGeometry)
      return output.name;
  QString sameSize;
  int sameSizeCount = 0;
  for (const OutputRecord &output : outputs) {
    if (output.hasGeometry && output.geometry.size() == screenGeometry.size()) {
      sameSize = output.name;
      ++sameSizeCount;
    }
  }
  if (sameSizeCount == 1)
    return sameSize;
  if (screenCount == 1 && outputs.size() == 1)
    return outputs.first().name;
  return QString();
}

DesktopSurface::DesktopSurface(QQmlEngine *engine, QScreen *screen, QString socketPath,
                               bool desktopLevel)
    : QQuickView(engine, nullptr), m_screen(screen), m_socketPath(std::move(socketPath)),
      m_desktopLevel(desktopLevel) {
  setObjectName(QStringLiteral("kwe-desktop-%1").arg(screen->name()));
  setTitle(QStringLiteral("KWE Desktop"));
  setColor(Qt::black);
  setResizeMode(QQuickView::SizeRootObjectToView);
  Qt::WindowFlags flags = Qt::Window | Qt::FramelessWindowHint | Qt::WindowDoesNotAcceptFocus;
  if (m_desktopLevel)
    flags |= Qt::WindowStaysOnBottomHint;
  setFlags(flags);
  setScreen(screen);
  setInitialProperties({{QStringLiteral("socketPath"), m_socketPath}});
  setSource(QUrl(QStringLiteral("qrc:/qt/qml/org/kde/kwe/displaymacos/qml/Desktop.qml")));
  if (status() == QQuickView::Error) {
    for (const QQmlError &error : errors())
      qCritical("kwe-display: %s", qPrintable(error.toString()));
  }
  connect(screen, &QScreen::geometryChanged, this, [this] { syncGeometry(); });
  syncGeometry();
}

void DesktopSurface::syncGeometry() {
  if (m_screen.isNull())
    return;
  const QRect geometry = m_screen->geometry();
  if (m_desktopLevel) {
    setGeometry(geometry);
  } else {
    // Development mode: a quarter-size normal window on that screen.
    setGeometry(QRect(geometry.topLeft() + QPoint(40, 40), geometry.size() / 2));
  }
}

bool DesktopSurface::applyOutputs(const QList<OutputRecord> &outputs, bool available) {
  const int screenCount = QGuiApplication::screens().size();
  const QRect geometry = m_screen.isNull() ? QRect() : m_screen->geometry();
  m_outputName = available ? matchOutput(geometry, screenCount, outputs) : QString();
  bool cover = false;
  for (const OutputRecord &output : outputs)
    if (!m_outputName.isEmpty() && output.name == m_outputName)
      cover = output.wallpaperPlugin == KwePlugin;
  if (cover == m_covering)
    return false;
  m_covering = cover;
  if (cover) {
    syncGeometry();
    show();
    if (m_desktopLevel && !m_desktopApplied) {
      platform::makeDesktopWindow(this);
      m_desktopApplied = true;
    } else if (m_desktopLevel) {
      // Re-assert the level after a hide/show cycle.
      platform::makeDesktopWindow(this);
    }
  } else {
    pointerLeft();
    hide();
  }
  return true;
}

void DesktopSurface::forwardGlobalPointer(const QPointF &global) {
  if (!m_covering || !isVisible())
    return;
  const QRectF geometry(this->geometry());
  if (!geometry.contains(global)) {
    pointerLeft();
    return;
  }
  const qreal x = (global.x() - geometry.x()) / geometry.width();
  const qreal y = (global.y() - geometry.y()) / geometry.height();
  forwardPointer(m_pointerInside ? QStringLiteral("move") : QStringLiteral("enter"), x, y);
  m_pointerInside = true;
}

void DesktopSurface::pointerLeft() {
  if (!m_pointerInside)
    return;
  m_pointerInside = false;
  forwardPointer(QStringLiteral("leave"), 0.5, 0.5);
}

void DesktopSurface::forwardPointer(const QString &phase, qreal x, qreal y) {
  QQuickItem *root = rootObject();
  if (root == nullptr)
    return;
  QMetaObject::invokeMethod(root, "forwardPointer", Q_ARG(QVariant, phase), Q_ARG(QVariant, x),
                            Q_ARG(QVariant, y));
}

bool DesktopSurface::hasFrame() const {
  QQuickItem *root = rootObject();
  if (root == nullptr)
    return false;
  QQuickItem *frame = root->findChild<QQuickItem *>(QStringLiteral("frameSurface"));
  return frame != nullptr && frame->property("hasFrame").toBool();
}

qulonglong DesktopSurface::frameSequence() const {
  QQuickItem *root = rootObject();
  if (root == nullptr)
    return 0;
  QQuickItem *frame = root->findChild<QQuickItem *>(QStringLiteral("frameSurface"));
  return frame == nullptr ? 0 : frame->property("sequence").toULongLong();
}
