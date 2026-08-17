// SPDX-License-Identifier: Apache-2.0
#pragma once

#include <QElapsedTimer>
#include <QFile>
#include <QHoverEvent>
#include <QImage>
#include <QQuickPaintedItem>
#include <QString>
#include <QTimer>
#include <qqmlintegration.h>

class FrameItem : public QQuickPaintedItem {
  Q_OBJECT
  QML_NAMED_ELEMENT(FrameSurface)
  Q_PROPERTY(QString frameFile READ frameFile WRITE setFrameFile NOTIFY
                 frameFileChanged)
  Q_PROPERTY(Status status READ status NOTIFY statusChanged)
  Q_PROPERTY(QString statusText READ statusText NOTIFY statusChanged)
  Q_PROPERTY(QString errorMessage READ errorMessage NOTIFY errorMessageChanged)
  Q_PROPERTY(qulonglong sequence READ sequence NOTIFY sequenceChanged)
  Q_PROPERTY(bool hasFrame READ hasFrame NOTIFY hasFrameChanged)
  Q_PROPERTY(QSize frameSize READ frameSize NOTIFY frameSizeChanged)

public:
  enum Status { Waiting, Live, Frozen, Invalid, Stopped };
  Q_ENUM(Status)

  explicit FrameItem(QQuickItem *parent = nullptr);
  ~FrameItem() override;

  QString frameFile() const { return m_frameFile; }
  Status status() const { return m_status; }
  QString statusText() const;
  QString errorMessage() const { return m_errorMessage; }
  qulonglong sequence() const { return m_sequence; }
  bool hasFrame() const { return !m_image.isNull(); }
  QSize frameSize() const { return m_image.size(); }

  void setFrameFile(const QString &path);
  Q_INVOKABLE bool openFrameFile(const QString &path);
  void paint(QPainter *painter) override;

protected:
  void hoverEnterEvent(QHoverEvent *event) override;
  void hoverMoveEvent(QHoverEvent *event) override;
  void hoverLeaveEvent(QHoverEvent *event) override;

signals:
  void frameFileChanged();
  void frameFileOpened(const QString &path);
  void statusChanged();
  void errorMessageChanged();
  void sequenceChanged();
  void hasFrameChanged();
  void frameSizeChanged();
  void pointerPosition(const QString &phase, qreal x, qreal y);

private:
  struct Layout {
    quint32 width = 0;
    quint32 height = 0;
    quint32 stride = 0;
    quint64 slotBytes = 0;
    quint64 fileBytes = 0;
  };

  void closeFrameFile();
  void pollFrame();
  bool validateHeader(Layout *layout, QString *error) const;
  bool readExact(qsizetype offset, void *destination, qsizetype bytes) const;
  void setStatus(Status status, const QString &error = {});
  QRectF imageDestination() const;
  void updatePointer(const QPointF &position, bool rateLimited);
  void leavePointer();
  bool loadGeneration(quint64 *generation) const;
  bool load32(qsizetype offset, quint32 *value) const;

  QString m_frameFile;
  QFile m_file;
  qsizetype m_fileBytes = 0;
  Layout m_layout;
  QImage m_image;
  QTimer m_pollTimer;
  QTimer m_reopenTimer;
  QElapsedTimer m_frameAge;
  Status m_status = Waiting;
  QString m_errorMessage;
  quint64 m_sequence = 0;
  bool m_receivedSequence = false;
  bool m_frameFileReadySignaled = false;
  QElapsedTimer m_pointerRate;
  bool m_pointerInsideImage = false;
  qreal m_pointerX = 0.0;
  qreal m_pointerY = 0.0;
};
