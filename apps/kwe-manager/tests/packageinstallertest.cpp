// SPDX-License-Identifier: GPL-3.0-or-later
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
  void systemWidePackageIsReportedInstalled();
  void acceptsTopLevelId();
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
  // Mirrors the shipped package: the plugin id is nested under KPlugin.
  const QJsonObject metadata{{"KPackageStructure", "Plasma/Wallpaper"},
                             {"KPlugin", QJsonObject{{"Id", id}}}};
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

void PackageInstallerTest::systemWidePackageIsReportedInstalled() {
  QTemporaryDir directory;
  QVERIFY(directory.isValid());
  const QString source = makeSource(directory, "org.kde.kwe.wallpaper");
  const QString target = directory.filePath("data/org.kde.kwe.wallpaper");
  PackageInstaller installer(target, source);
  QCOMPARE(installer.state(), PackageInstaller::Installed);
  QVERIFY(installer.message().contains("system-wide"));
  QVERIFY(!installer.userPackagePresent());
  // Safe mode needs a user-local copy it can rename; the read-only
  // system-wide package is not enough.
  QVERIFY(!installer.enterSafeMode());
  QCOMPARE(installer.state(), PackageInstaller::Failed);
  QVERIFY(installer.message().contains("user-local"));
  QVERIFY(installer.installFrom(source));
  QCOMPARE(installer.state(), PackageInstaller::Installed);
  QVERIFY(installer.userPackagePresent());
}

void PackageInstallerTest::acceptsTopLevelId() {
  QTemporaryDir directory;
  QVERIFY(directory.isValid());
  const QString source = directory.filePath("source");
  if (!QDir().mkpath(source + "/contents/ui")) {
    QTest::qFail("could not create package fixture", __FILE__, __LINE__);
    return;
  }
  const QJsonObject metadata{{"KPackageStructure", "Plasma/Wallpaper"},
                             {"Id", "org.kde.kwe.wallpaper"}};
  writeFile(source + "/metadata.json", QJsonDocument(metadata).toJson());
  writeFile(source + "/contents/ui/main.qml", "import QtQuick\nItem {}\n");
  PackageInstaller installer(directory.filePath("data/org.kde.kwe.wallpaper"));
  QVERIFY(installer.installFrom(source));
  QCOMPARE(installer.state(), PackageInstaller::Installed);
}

QTEST_MAIN(PackageInstallerTest)
#include "packageinstallertest.moc"
