// SPDX-License-Identifier: Apache-2.0
#include "../src/packageinstaller.h"

#include <QDir>
#include <QFile>
#include <QJsonDocument>
#include <QJsonObject>
#include <QTemporaryDir>
#include <QtTest>

class PackageInstallerTest final : public QObject {
  Q_OBJECT

private slots:
  void installsAndRollsBackSafely();
  void rejectsWrongPackage();
};

void writeFile(const QString &path, const QByteArray &contents) {
  QFile file(path);
  QVERIFY(file.open(QIODevice::WriteOnly));
  QCOMPARE(file.write(contents), contents.size());
}

QString makeSource(QTemporaryDir &directory, const QString &id) {
  const QString source = directory.filePath("source");
  if (!QDir().mkpath(source + "/contents/ui")) {
    QTest::qFail("could not create package fixture", __FILE__, __LINE__);
    return {};
  }
  const QJsonObject metadata{{"KPackageStructure", "Plasma/Wallpaper"}, {"Id", id}};
  writeFile(source + "/metadata.json", QJsonDocument(metadata).toJson());
  writeFile(source + "/contents/ui/main.qml", "import QtQuick\nItem {}\n");
  return source;
}

void PackageInstallerTest::installsAndRollsBackSafely() {
  QTemporaryDir directory;
  QVERIFY(directory.isValid());
  const QString source = makeSource(directory, "org.kde.kwe.wallpaper");
  const QString target = directory.filePath("data/org.kde.kwe.wallpaper");
  PackageInstaller installer(target);
  QCOMPARE(installer.state(), PackageInstaller::Unavailable);
  QVERIFY(installer.installFrom(source));
  QCOMPARE(installer.state(), PackageInstaller::Installed);
  QVERIFY(QFileInfo::exists(target + "/metadata.json"));
  QVERIFY(installer.enterSafeMode());
  QCOMPARE(installer.state(), PackageInstaller::SafeMode);
  QVERIFY(!QFileInfo::exists(target));
  QVERIFY(QFileInfo::exists(target + ".disabled/metadata.json"));
  QVERIFY(installer.leaveSafeMode());
  QCOMPARE(installer.state(), PackageInstaller::Installed);
  QVERIFY(QFileInfo::exists(target + "/contents/ui/main.qml"));
}

void PackageInstallerTest::rejectsWrongPackage() {
  QTemporaryDir directory;
  QVERIFY(directory.isValid());
  const QString source = makeSource(directory, "org.example.not-kwe");
  PackageInstaller installer(directory.filePath("data/org.kde.kwe.wallpaper"));
  QVERIFY(!installer.installFrom(source));
  QCOMPARE(installer.state(), PackageInstaller::Failed);
  QVERIFY(installer.message().contains("Package ID"));
}

QTEST_MAIN(PackageInstallerTest)
#include "packageinstallertest.moc"
