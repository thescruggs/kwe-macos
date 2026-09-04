// SPDX-License-Identifier: GPL-3.0-or-later
import QtQuick
import QtQuick.Controls as Controls

Controls.TextField {
    placeholderText: qsTr("Search…")
    inputMethodHints: Qt.ImhNoPredictiveText
    Keys.onEscapePressed: clear()
}
