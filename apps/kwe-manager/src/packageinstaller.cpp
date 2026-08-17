// SPDX-License-Identifier: Apache-2.0
#include "packageinstaller.h"

#include <QDir>
#include <QDirIterator>
#include <QFile>
#include <QFileInfo>
#include <QJsonDocument>
#include <QJsonObject>

namespace {
constexpr auto PackageId = "org.kde.kwe.wallpaper";
constexpr auto MetadataName = "metadata.json";
constexpr auto MainQml = "contents/ui/main.qml";
constexpr auto DisabledSuffix = ".disabled";
}

PackageInstaller::PackageInstaller(QString packagePath, QObject *parent)
    : QObject(parent), m_packagePath(std::move(packagePath)) {
  if (QFileInfo::exists(m_packagePath + DisabledSuffix))
    setState(SafeMode, tr("The Plasma package is disabled in safe mode."));
  else if (QFileInfo::exists(m_packagePath))
    setState(Installed, tr("The Plasma display package is installed."));
  else
    setState(Unavailable, tr("The Plasma display package is not installed."));
}

bool PackageInstaller::validatePackage(const QString &sourcePath,
                                       QString *error) const {
  const QFileInfo sourceInfo(sourcePath);
  if (!sourceInfo.isDir() || sourceInfo.isSymLink()) {
    *error = tr("Package source is not a real directory.");
    return false;
  }
  const QFileInfo metadata(sourcePath + QLatin1Char('/') + MetadataName);
  const QFileInfo mainQml(sourcePath + QLatin1Char('/') + MainQml);
  if (!metadata.isFile() || metadata.isSymLink() || !mainQml.isFile() ||
      mainQml.isSymLink()) {
    *error = tr("Package is missing metadata.json or contents/ui/main.qml.");
    return false;
  }
  QFile metadataFile(metadata.filePath());
  if (!metadataFile.open(QIODevice::ReadOnly)) {
    *error = tr("Could not read package metadata.");
    return false;
  }
  QJsonParseError parseError;
  const QJsonDocument document =
      QJsonDocument::fromJson(metadataFile.read(256 * 1024), &parseError);
  if (parseError.error != QJsonParseError::NoError || !document.isObject() ||
      document.object().value(QStringLiteral("KPackageStructure")).toString() !=
          QStringLiteral("Plasma/Wallpaper")) {
    *error = tr("Package metadata is invalid or is not a Plasma wallpaper.");
    return false;
  }
  if (document.object().value(QStringLiteral("Id")).toString() !=
      QString::fromLatin1(PackageId)) {
    *error = tr("Package ID does not match the KDE Wallpaper Engine package.");
    return false;
  }
  return true;
}

bool PackageInstaller::copyPackage(const QString &sourcePath,
                                   const QString &temporaryPath,
                                   QString *error) const {
  QDir temporary(temporaryPath);
  if (!temporary.mkpath(QStringLiteral("contents/ui"))) {
    *error = tr("Could not create the temporary package directory.");
    return false;
  }
  QDirIterator iterator(sourcePath, QDir::Files | QDir::NoDotAndDotDot,
                        QDirIterator::Subdirectories);
  while (iterator.hasNext()) {
    const QFileInfo source(iterator.next());
    if (source.isSymLink() || !source.isFile()) {
      *error = tr("Package contains an unsafe symbolic link or non-file.");
      return false;
    }
    const QString relative = QDir(sourcePath).relativeFilePath(source.filePath());
    const QString destinationPath = temporary.filePath(relative);
    if (!temporary.mkpath(QFileInfo(destinationPath).path()) ||
        !QFile::copy(source.filePath(), destinationPath)) {
      *error = tr("Could not copy package file: %1").arg(relative);
      return false;
    }
  }
  return true;
}

bool PackageInstaller::installFrom(const QString &sourcePath) {
  QString error;
  if (!validatePackage(sourcePath, &error)) {
    setState(Failed, error);
    return false;
  }
  const QFileInfo targetInfo(m_packagePath);
  QDir parent = targetInfo.dir();
  if (!parent.mkpath(QStringLiteral("."))) {
    setState(Failed, tr("Could not create the Plasma package directory."));
    return false;
  }
  const QString temporaryPath = m_packagePath + QStringLiteral(".new");
  const QString backupPath = m_packagePath + QStringLiteral(".previous");
  QDir(temporaryPath).removeRecursively();
  if (!copyPackage(sourcePath, temporaryPath, &error)) {
    QDir(temporaryPath).removeRecursively();
    setState(Failed, error);
    return false;
  }
  QDir(backupPath).removeRecursively();
  if (QFileInfo::exists(m_packagePath) && !parent.rename(m_packagePath, backupPath)) {
    QDir(temporaryPath).removeRecursively();
    setState(Failed, tr("Could not retain the previous package for rollback."));
    return false;
  }
  if (!parent.rename(temporaryPath, m_packagePath)) {
    if (QFileInfo::exists(backupPath))
      parent.rename(backupPath, m_packagePath);
    QDir(temporaryPath).removeRecursively();
    setState(Failed, tr("Could not activate the new package."));
    return false;
  }
  setState(Installed, tr("The Plasma display package was installed safely."));
  return true;
}

bool PackageInstaller::enterSafeMode() {
  if (m_state == SafeMode)
    return true;
  const QString disabledPath = m_packagePath + QLatin1String(DisabledSuffix);
  if (!QFileInfo::exists(m_packagePath) ||
      !QDir(QFileInfo(m_packagePath).dir()).rename(m_packagePath, disabledPath)) {
    setState(Failed, tr("Could not disable the Plasma package for safe mode."));
    return false;
  }
  setState(SafeMode, tr("Safe mode is active; KDE will not load this package."));
  return true;
}

bool PackageInstaller::leaveSafeMode() {
  if (m_state != SafeMode)
    return m_state == Installed;
  const QString disabledPath = m_packagePath + QLatin1String(DisabledSuffix);
  if (!QDir(QFileInfo(m_packagePath).dir()).rename(disabledPath, m_packagePath)) {
    setState(Failed, tr("Could not restore the Plasma package from safe mode."));
    return false;
  }
  setState(Installed, tr("The Plasma display package is enabled again."));
  return true;
}

void PackageInstaller::setState(State state, const QString &message) {
  const bool stateChanged = m_state != state;
  const bool messageWasChanged = m_message != message;
  m_state = state;
  m_message = message;
  if (stateChanged)
    emit this->stateChanged();
  if (messageWasChanged)
    emit messageChanged();
}
