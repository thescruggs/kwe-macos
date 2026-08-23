// SPDX-License-Identifier: Apache-2.0
#include "frameitem.h"

#include <QPainter>
#include <QtEndian>

#include <array>
#include <cerrno>
#include <cstring>
#include <fcntl.h>
#include <limits>
#include <sys/stat.h>
#include <unistd.h>

namespace {
constexpr char Magic[8] = {'K', 'W', 'E', 'F', 'R', 'M', '1', '\0'};
constexpr quint32 Version = 1;
constexpr qsizetype HeaderBytes = 64;
constexpr quint32 SlotCount = 2;
constexpr quint32 BytesPerPixel = 4;
constexpr quint32 PixelFormatBgraPremultiplied = 1;
constexpr quint32 MaxDimension = 8192;
constexpr quint64 MaxFrameFileBytes = 512ULL * 1024ULL * 1024ULL;
constexpr qsizetype OffsetVersion = 8;
constexpr qsizetype OffsetHeaderBytes = 12;
constexpr qsizetype OffsetFileBytes = 16;
constexpr qsizetype OffsetWidth = 24;
constexpr qsizetype OffsetHeight = 28;
constexpr qsizetype OffsetStride = 32;
constexpr qsizetype OffsetPixelFormat = 36;
constexpr qsizetype OffsetSlotCount = 40;
constexpr qsizetype OffsetGeneration = 48;
constexpr qsizetype OffsetActiveSlot = 56;
constexpr qsizetype OffsetProducerState = 60;
constexpr qint64 FrozenAfterMilliseconds = 1500;

static_assert(Q_BYTE_ORDER == Q_LITTLE_ENDIAN,
              "Frame protocol v1 requires little-endian Linux");

quint32 read32(const uchar *bytes, qsizetype offset) {
  return qFromLittleEndian<quint32>(bytes + offset);
}

quint64 read64(const uchar *bytes, qsizetype offset) {
  return qFromLittleEndian<quint64>(bytes + offset);
}
} // namespace

FrameItem::FrameItem(QQuickItem *parent) : QQuickPaintedItem(parent) {
  setAntialiasing(false);
  setAcceptHoverEvents(true);
  setAcceptedMouseButtons(Qt::NoButton);
  setAcceptTouchEvents(false);
  setActiveFocusOnTab(false);
  m_pointerRate.start();

  m_pollTimer.setInterval(33);
  m_pollTimer.setTimerType(Qt::PreciseTimer);
  connect(&m_pollTimer, &QTimer::timeout, this, &FrameItem::pollFrame);

  m_reopenTimer.setInterval(2000);
  m_reopenTimer.setTimerType(Qt::CoarseTimer);
  connect(&m_reopenTimer, &QTimer::timeout, this, [this] {
    if (!m_file.isOpen() && !m_frameFile.isEmpty())
      openFrameFile(m_frameFile);
  });
}

FrameItem::~FrameItem() { closeFrameFile(); }

QString FrameItem::statusText() const {
  switch (m_status) {
  case Waiting:
    return tr("Waiting for renderer");
  case Live:
    return tr("Live");
  case Frozen:
    return tr("Renderer stalled — showing last good frame");
  case Invalid:
    return tr("Invalid frame transport — showing last good frame");
  case Stopped:
    return tr("Renderer stopped — showing last good frame");
  }
  return {};
}

void FrameItem::setFrameFile(const QString &path) {
  if (m_frameFile == path)
    return;
  m_frameFile = path;
  emit frameFileChanged();
  if (path.isEmpty()) {
    m_reopenTimer.stop();
    closeFrameFile();
    setStatus(Waiting);
    return;
  }
  if (!openFrameFile(path))
    m_reopenTimer.start();
}

bool FrameItem::openFrameFile(const QString &path) {
  closeFrameFile();
  const QByteArray encodedPath = QFile::encodeName(path);
  const int descriptor =
      ::open(encodedPath.constData(), O_RDONLY | O_CLOEXEC | O_NOFOLLOW);
  if (descriptor < 0) {
    setStatus(Invalid, tr("Could not safely open the frame file: %1")
                           .arg(QString::fromLocal8Bit(std::strerror(errno))));
    return false;
  }

  struct stat statusBuffer{};
  if (::fstat(descriptor, &statusBuffer) != 0 ||
      !S_ISREG(statusBuffer.st_mode)) {
    ::close(descriptor);
    setStatus(Invalid, tr("Frame path must be a regular, non-symlink file."));
    return false;
  }
  if (statusBuffer.st_size < HeaderBytes ||
      quint64(statusBuffer.st_size) > MaxFrameFileBytes) {
    ::close(descriptor);
    setStatus(
        Invalid,
        tr("Frame file size is outside the 64-byte to 512 MiB safety range."));
    return false;
  }

  m_file.setFileName(path);
  if (!m_file.open(descriptor, QIODevice::ReadOnly,
                   QFileDevice::AutoCloseHandle)) {
    ::close(descriptor);
    setStatus(
        Invalid,
        tr("Could not open the frame file: %1").arg(m_file.errorString()));
    return false;
  }
  m_fileBytes = statusBuffer.st_size;

  QString error;
  if (!validateHeader(&m_layout, &error)) {
    setStatus(Invalid, error);
    closeFrameFile();
    return false;
  }
  if (m_frameFile != path) {
    m_frameFile = path;
    emit frameFileChanged();
  }
  setStatus(Waiting);
  m_reopenTimer.stop();
  m_pollTimer.start();
  pollFrame();
  return true;
}

void FrameItem::closeFrameFile() {
  m_pollTimer.stop();
  m_fileBytes = 0;
  m_file.close();
  m_receivedSequence = false;
  m_frameFileReadySignaled = false;
}

bool FrameItem::readExact(qsizetype offset, void *destination,
                          qsizetype bytes) const {
  if (!m_file.isOpen() || offset < 0 || bytes < 0 || offset > m_fileBytes ||
      bytes > m_fileBytes - offset)
    return false;

  auto *output = static_cast<uchar *>(destination);
  qsizetype completed = 0;
  while (completed < bytes) {
    const ssize_t result =
        ::pread(m_file.handle(), output + completed, size_t(bytes - completed),
                off_t(offset + completed));
    if (result > 0) {
      completed += result;
      continue;
    }
    if (result < 0 && errno == EINTR)
      continue;
    return false;
  }
  return true;
}

bool FrameItem::validateHeader(Layout *layout, QString *error) const {
  std::array<uchar, HeaderBytes> header{};
  if (!readExact(0, header.data(), HeaderBytes)) {
    *error = tr("Frame header is truncated.");
    return false;
  }
  if (std::memcmp(header.data(), Magic, sizeof(Magic)) != 0) {
    *error = tr("Frame protocol magic is invalid.");
    return false;
  }
  if (read32(header.data(), OffsetVersion) != Version ||
      read32(header.data(), OffsetHeaderBytes) != HeaderBytes ||
      read32(header.data(), OffsetPixelFormat) !=
          PixelFormatBgraPremultiplied ||
      read32(header.data(), OffsetSlotCount) != SlotCount) {
    *error = tr("Frame protocol version or format is unsupported.");
    return false;
  }

  const quint32 width = read32(header.data(), OffsetWidth);
  const quint32 height = read32(header.data(), OffsetHeight);
  if (width == 0 || height == 0 || width > MaxDimension ||
      height > MaxDimension) {
    *error = tr("Frame dimensions are outside the safety limit.");
    return false;
  }
  const quint64 stride64 = quint64(width) * BytesPerPixel;
  const quint64 slotBytes = stride64 * height;
  const quint64 expectedBytes = HeaderBytes + slotBytes * SlotCount;
  if (stride64 > std::numeric_limits<quint32>::max() ||
      read32(header.data(), OffsetStride) != stride64 ||
      read64(header.data(), OffsetFileBytes) != expectedBytes ||
      quint64(m_fileBytes) != expectedBytes ||
      expectedBytes > MaxFrameFileBytes) {
    *error = tr("Frame stride or file size is inconsistent.");
    return false;
  }
  *layout = Layout{width, height, quint32(stride64), slotBytes, expectedBytes};
  return true;
}

bool FrameItem::loadGeneration(quint64 *generation) const {
  std::array<uchar, sizeof(quint64)> bytes{};
  if (!readExact(OffsetGeneration, bytes.data(), qsizetype(bytes.size())))
    return false;
  *generation = qFromLittleEndian<quint64>(bytes.data());
  return true;
}

bool FrameItem::load32(qsizetype offset, quint32 *value) const {
  std::array<uchar, sizeof(quint32)> bytes{};
  if (!readExact(offset, bytes.data(), qsizetype(bytes.size())))
    return false;
  *value = qFromLittleEndian<quint32>(bytes.data());
  return true;
}

void FrameItem::pollFrame() {
  if (!m_file.isOpen())
    return;

  struct stat statusBuffer {};
  if (::fstat(m_file.handle(), &statusBuffer) != 0 ||
      statusBuffer.st_size != m_fileBytes) {
    setStatus(Invalid, tr("Frame file size changed unexpectedly."));
    return;
  }

  QString error;
  Layout current;
  if (!validateHeader(&current, &error) ||
      current.fileBytes != m_layout.fileBytes) {
    setStatus(Invalid, error.isEmpty()
                           ? tr("Frame layout changed unexpectedly.")
                           : error);
    return;
  }

  quint64 before = 0;
  if (!loadGeneration(&before)) {
    setStatus(Invalid, tr("Could not read the frame generation safely."));
    return;
  }
  if ((before & 1U) != 0)
    return;

  quint32 producerState = 0;
  if (!load32(OffsetProducerState, &producerState)) {
    setStatus(Invalid, tr("Could not read the renderer state safely."));
    return;
  }
  if (m_receivedSequence && before / 2 == m_sequence) {
    if (producerState == 3)
      setStatus(Stopped);
    else if (m_frameAge.isValid() &&
             m_frameAge.elapsed() > FrozenAfterMilliseconds)
      setStatus(Frozen);
    return;
  }

  quint32 slot = 0;
  if (!load32(OffsetActiveSlot, &slot)) {
    setStatus(Invalid, tr("Could not read the active frame slot safely."));
    return;
  }
  if (slot >= SlotCount) {
    setStatus(Invalid, tr("Renderer selected an invalid frame slot."));
    return;
  }
  const quint64 sourceOffset = HeaderBytes + m_layout.slotBytes * slot;
  if (sourceOffset + m_layout.slotBytes > quint64(m_fileBytes)) {
    setStatus(Invalid, tr("Renderer frame points outside the frame file."));
    return;
  }

  QImage candidate(int(m_layout.width), int(m_layout.height),
                   QImage::Format_ARGB32_Premultiplied);
  if (candidate.isNull() || candidate.bytesPerLine() != int(m_layout.stride) ||
      !readExact(qsizetype(sourceOffset), candidate.bits(),
                 qsizetype(m_layout.slotBytes))) {
    setStatus(Invalid,
              tr("Could not copy the complete renderer frame safely."));
    return;
  }

  quint64 after = 0;
  if (!loadGeneration(&after)) {
    setStatus(Invalid, tr("Could not verify the copied frame generation."));
    return;
  }
  if (before != after || (after & 1U) != 0)
    return;

  const bool hadFrame = hasFrame();
  const QSize oldSize = frameSize();
  m_image = std::move(candidate);
  m_sequence = after / 2;
  m_receivedSequence = true;
  m_frameAge.restart();
  setStatus(producerState == 3 ? Stopped : Live);
  emit sequenceChanged();
  if (!hadFrame)
    emit hasFrameChanged();
  if (oldSize != frameSize())
    emit frameSizeChanged();
  if (!m_frameFileReadySignaled) {
    m_frameFileReadySignaled = true;
    emit frameFileOpened(m_frameFile);
  }
  update();
}

void FrameItem::setStatus(Status status, const QString &error) {
  const bool statusChanged = m_status != status;
  const bool errorChanged = m_errorMessage != error;
  m_status = status;
  m_errorMessage = error;
  if (statusChanged)
    emit this->statusChanged();
  if (errorChanged)
    emit errorMessageChanged();
}

void FrameItem::paint(QPainter *painter) {
  painter->fillRect(boundingRect(), Qt::black);
  if (m_image.isNull())
    return;
  painter->setRenderHint(QPainter::SmoothPixmapTransform, true);
  // Fill overflows the item on one axis; the item bounds are the crop.
  painter->setClipRect(boundingRect());
  painter->drawImage(imageDestination(), m_image);
}

void FrameItem::setScaling(const QString &scaling) {
  const QString mode = scaling == QStringLiteral("fill") ||
                               scaling == QStringLiteral("stretch")
                           ? scaling
                           : QStringLiteral("aspect");
  if (m_scaling == mode)
    return;
  m_scaling = mode;
  emit scalingChanged();
  update();
}

QRectF kweFrameDestination(const QSizeF &image, const QSizeF &item,
                           const QString &mode) {
  if (image.isEmpty() || item.isEmpty())
    return {};
  if (mode == QStringLiteral("stretch"))
    return {QPointF(0, 0), item};
  QSizeF target = image;
  target.scale(item, mode == QStringLiteral("fill")
                         ? Qt::KeepAspectRatioByExpanding
                         : Qt::KeepAspectRatio);
  return {
      (item.width() - target.width()) / 2.0,
      (item.height() - target.height()) / 2.0,
      target.width(),
      target.height(),
  };
}

QRectF FrameItem::imageDestination() const {
  if (m_image.isNull())
    return {};
  return kweFrameDestination(m_image.size(), boundingRect().size(), m_scaling);
}

void FrameItem::hoverEnterEvent(QHoverEvent *event) {
  updatePointer(event->position(), false);
}

void FrameItem::hoverMoveEvent(QHoverEvent *event) {
  updatePointer(event->position(), true);
}

void FrameItem::hoverLeaveEvent(QHoverEvent *) { leavePointer(); }

void FrameItem::updatePointer(const QPointF &position, bool rateLimited) {
  const QRectF destination = imageDestination();
  if (destination.isEmpty() || !destination.contains(position)) {
    leavePointer();
    return;
  }
  const qreal x = qBound(
      0.0, (position.x() - destination.left()) / destination.width(), 1.0);
  const qreal y = qBound(
      0.0, (position.y() - destination.top()) / destination.height(), 1.0);
  const bool entered = !m_pointerInsideImage;
  m_pointerInsideImage = true;
  m_pointerX = x;
  m_pointerY = y;
  if (entered)
    emit pointerPosition(QStringLiteral("enter"), x, y);
  if (!rateLimited || entered || m_pointerRate.elapsed() >= 16) {
    m_pointerRate.restart();
    emit pointerPosition(QStringLiteral("move"), x, y);
  }
}

void FrameItem::leavePointer() {
  if (!m_pointerInsideImage)
    return;
  m_pointerInsideImage = false;
  emit pointerPosition(QStringLiteral("leave"), m_pointerX, m_pointerY);
}
