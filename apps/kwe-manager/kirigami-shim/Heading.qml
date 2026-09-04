// SPDX-License-Identifier: GPL-3.0-or-later
import QtQuick
import QtQuick.Controls as Controls

Controls.Label {
    property int level: 1
    font.pointSize: Qt.application.font.pointSize * (level <= 1 ? 1.75 : level === 2 ? 1.5 : level === 3 ? 1.3 : level === 4 ? 1.15 : 1.0)
    font.weight: level <= 3 ? Font.DemiBold : Font.Medium
    wrapMode: Text.WordWrap
}
