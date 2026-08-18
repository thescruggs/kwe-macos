// SPDX-License-Identifier: Apache-2.0
#pragma once

#include <QObject>
#include <QString>

class PackageInstaller final : public QObject {
  Q_OBJECT
  Q_PROPERTY(State state READ state NOTIFY stateChanged)
  Q_PROPERTY(QString message READ message NOTIFY messageChanged)
  Q_PROPERTY(QString packagePath READ packagePath CONSTANT)
  Q_PROPERTY(bool userPackagePresent READ userPackagePresent NOTIFY userPackagePresentChanged)

public:
  enum State { Unavailable, Ready, Installed, SafeMode, Failed };
  Q_ENUM(State)

  explicit PackageInstaller(QString packagePath, QString systemPackagePath = {},
                            QObject *parent = nullptr);

  State state() const { return m_state; }
  QString message() const { return m_message; }
  QString packagePath() const { return m_packagePath; }
  bool userPackagePresent() const;

  Q_INVOKABLE bool installFrom(const QString &sourcePath);
  Q_INVOKABLE bool enterSafeMode();
  Q_INVOKABLE bool leaveSafeMode();

signals:
  void stateChanged();
  void messageChanged();
  void userPackagePresentChanged();

private:
  bool validatePackage(const QString &sourcePath, QString *error) const;
  bool copyPackage(const QString &sourcePath, const QString &temporaryPath,
                   QString *error) const;
  void setState(State state, const QString &message);

  QString m_packagePath;
  QString m_systemPackagePath;
  QString m_message;
  State m_state = Unavailable;
};
