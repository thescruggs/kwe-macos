// SPDX-License-Identifier: Apache-2.0
#include "workshopclient.h"

#include <QDesktopServices>
#include <QRegularExpression>
#include <QUrl>

bool WorkshopClient::openItem(const QString &workshopId) {
  static const QRegularExpression idPattern(QStringLiteral("^[0-9]{1,20}$"));
  if (!idPattern.match(workshopId).hasMatch() || workshopId.toULongLong() == 0) {
    setError(tr("The Workshop ID is invalid."));
    return false;
  }
  const QUrl url(QStringLiteral("steam://url/CommunityFilePage/%1").arg(workshopId));
  if (!QDesktopServices::openUrl(url)) {
    setError(tr("Steam could not be opened for Workshop item %1.").arg(workshopId));
    return false;
  }
  setError({});
  return true;
}

void WorkshopClient::setError(const QString &message) {
  if (m_errorMessage == message)
    return;
  m_errorMessage = message;
  emit errorMessageChanged();
}
