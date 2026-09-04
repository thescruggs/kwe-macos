// SPDX-License-Identifier: GPL-3.0-or-later
import QtQuick
import QtQuick.Controls as Controls

// Kirigami.Page subset on top of QQC2 Page (title, header, footer,
// padding, contentItem are inherited).
Controls.Page {
    property list<Controls.Action> actions
    property bool refreshing: false
    property bool supportsRefreshing: false
}
