// SPDX-License-Identifier: GPL-3.0-or-later
#include "../src/workshopclient.h"

#include <QtTest>

class WorkshopClientTest final : public QObject {
  Q_OBJECT

private slots:
  void rejectsMalformedIds();
};

void WorkshopClientTest::rejectsMalformedIds() {
  WorkshopClient client;
  QVERIFY(!client.openItem(QStringLiteral("abc")));
  QVERIFY(!client.errorMessage().isEmpty());
  QVERIFY(!client.openItem(QStringLiteral("0")));
  QVERIFY(!client.openItem(QStringLiteral("123456789012345678901")));
}

QTEST_GUILESS_MAIN(WorkshopClientTest)
#include "workshopclienttest.moc"
