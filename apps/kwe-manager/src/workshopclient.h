// SPDX-License-Identifier: GPL-3.0-or-later
#pragma once

#include <QObject>
#include <QString>

class WorkshopClient final : public QObject {
  Q_OBJECT
  Q_PROPERTY(QString errorMessage READ errorMessage NOTIFY errorMessageChanged)

public:
  explicit WorkshopClient(QObject *parent = nullptr) : QObject(parent) {}

  QString errorMessage() const { return m_errorMessage; }
  Q_INVOKABLE bool openItem(const QString &workshopId);

signals:
  void errorMessageChanged();

private:
  void setError(const QString &message);
  QString m_errorMessage;
};
