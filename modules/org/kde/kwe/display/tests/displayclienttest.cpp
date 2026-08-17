// SPDX-License-Identifier: Apache-2.0
#include "displaysession.h"
#include "inputclient.h"

#include <QFile>
#include <QDir>
#include <QJsonDocument>
#include <QJsonObject>
#include <QLocalServer>
#include <QLocalSocket>
#include <QTemporaryDir>
#include <QTest>
#include <QUuid>
#include <QStandardPaths>

#include <functional>

class FakeService final : public QObject {
  Q_OBJECT

public:
  using Reply = std::function<QByteArray(const QJsonObject &)>;

  FakeService(const QString &socketPath, Reply reply, QObject *parent = nullptr)
      : QObject(parent), m_reply(std::move(reply)) {
    QLocalServer::removeServer(socketPath);
    QVERIFY2(m_server.listen(socketPath), qPrintable(m_server.errorString()));
    connect(&m_server, &QLocalServer::newConnection, this, [this] {
      while (QLocalSocket *socket = m_server.nextPendingConnection()) {
        connect(socket, &QLocalSocket::readyRead, socket,
                [this, socket, request = QByteArray()]() mutable {
                  request += socket->readAll();
                  const qsizetype newline = request.indexOf('\n');
                  if (newline < 0)
                    return;
                  const QJsonDocument document =
                      QJsonDocument::fromJson(request.left(newline));
                  const QByteArray reply = m_reply(document.object());
                  socket->write(reply);
                  socket->flush();
                });
      }
    });
  }

private:
  QLocalServer m_server;
  Reply m_reply;
};

QString testSocketPath() {
  return QStandardPaths::writableLocation(QStandardPaths::RuntimeLocation) +
         QStringLiteral("/kwe-display-test-") +
         QUuid::createUuid().toString(QUuid::Id128);
}

QByteArray response(const QJsonObject &request, QJsonObject result) {
  return QJsonDocument(
             QJsonObject{
                 {QStringLiteral("version"), 1},
                 {QStringLiteral("id"), request.value(QStringLiteral("id"))},
                 {QStringLiteral("ok"), true},
                 {QStringLiteral("result"), result},
             })
             .toJson(QJsonDocument::Compact) +
         '\n';
}

QJsonObject status(const QString &frameFile, bool awaiting) {
  return {
      {QStringLiteral("phase"),
       awaiting ? QStringLiteral("awaiting_ack") : QStringLiteral("live")},
      {QStringLiteral("frame_file"), frameFile},
      {QStringLiteral("display_generation"), 7},
      {QStringLiteral("awaiting_display_ack"), awaiting},
  };
}

class DisplayClientTest final : public QObject {
  Q_OBJECT

private slots:
  void unavailableServiceIsDegraded() {
    QTemporaryDir directory;
    QVERIFY(directory.isValid());
    DisplaySession session;
    session.setSocketPath(directory.path() + QStringLiteral("/missing.sock"));
    QTRY_COMPARE_WITH_TIMEOUT(session.state(), DisplaySession::Degraded, 2000);
    QVERIFY(!session.errorMessage().isEmpty());
    QVERIFY(session.frameFile().isEmpty());
  }

  void malformedReplyIsBoundedFailure() {
    QTemporaryDir directory;
    QVERIFY(directory.isValid());
    const QString socketPath = testSocketPath();
    FakeService service(
        socketPath, [](const QJsonObject &) { return QByteArray("{bad}\n"); });
    DisplaySession session;
    session.setSocketPath(socketPath);
    QTRY_COMPARE_WITH_TIMEOUT(session.state(), DisplaySession::Degraded, 2000);
    QVERIFY(session.errorMessage().contains(QStringLiteral("invalid JSON")));
  }

  void oversizedReplyIsBoundedFailure() {
    QTemporaryDir directory;
    QVERIFY(directory.isValid());
    const QString socketPath = testSocketPath();
    FakeService service(socketPath, [](const QJsonObject &) {
      return QByteArray(64 * 1024 + 1, 'x');
    });
    DisplaySession session;
    session.setSocketPath(socketPath);
    QTRY_COMPARE_WITH_TIMEOUT(session.state(), DisplaySession::Degraded, 2000);
    QVERIFY(session.errorMessage().contains(QStringLiteral("oversized")));
  }

  void validatedGenerationIsAcknowledged() {
    QTemporaryDir directory;
    QVERIFY(directory.isValid());
    QFile frame(directory.path() + QStringLiteral("/frame.bin"));
    QVERIFY(frame.open(QIODevice::WriteOnly));
    QCOMPARE(frame.write("synthetic", 9), 9);
    frame.close();
    const QString socketPath = testSocketPath();
    QStringList methods;
    FakeService service(socketPath, [&methods,
                                     &frame](const QJsonObject &request) {
      const QString method = request.value(QStringLiteral("method")).toString();
      methods.append(method);
      return response(
          request,
          status(frame.fileName(), method != QStringLiteral("renderer.ack")));
    });
    DisplaySession session;
    session.setSocketPath(socketPath);
    QTRY_VERIFY_WITH_TIMEOUT(session.active(), 2000);
    QCOMPARE(session.frameFile(), frame.fileName());
    QCOMPARE(session.displayGeneration(), qulonglong(7));
    QVERIFY(session.awaitingDisplayAck());
    session.acknowledgeFrameFile(frame.fileName());
    QTRY_VERIFY_WITH_TIMEOUT(!session.awaitingDisplayAck(), 2000);
    QVERIFY(methods.contains(QStringLiteral("renderer.status")));
    QVERIFY(methods.contains(QStringLiteral("renderer.ack")));
  }

  void pointerRequestHasFiniteTimeout() {
    QTemporaryDir directory;
    QVERIFY(directory.isValid());
    const QString socketPath = testSocketPath();
    FakeService service(socketPath,
                        [](const QJsonObject &) { return QByteArray(); });
    InputClient input;
    input.setSocketPath(socketPath);
    input.setDisplayGeneration(3);
    input.sendPointer(QStringLiteral("move"), 0.5, 0.5);
    QTRY_COMPARE_WITH_TIMEOUT(input.state(), InputClient::Error, 2000);
    QVERIFY(input.errorMessage().contains(QStringLiteral("in time")));
  }

  void pointerReplyHasFiniteByteBudget() {
    QTemporaryDir directory;
    QVERIFY(directory.isValid());
    const QString socketPath = testSocketPath();
    FakeService service(socketPath, [](const QJsonObject &) {
      return QByteArray(64 * 1024 + 1, 'x');
    });
    InputClient input;
    input.setSocketPath(socketPath);
    input.setDisplayGeneration(3);
    input.sendPointer(QStringLiteral("move"), 0.5, 0.5);
    QTRY_COMPARE_WITH_TIMEOUT(input.state(), InputClient::Error, 2000);
    QVERIFY(input.errorMessage().contains(QStringLiteral("oversized")));
  }
};

QTEST_GUILESS_MAIN(DisplayClientTest)

#include "displayclienttest.moc"
