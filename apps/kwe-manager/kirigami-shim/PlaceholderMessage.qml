// SPDX-License-Identifier: GPL-3.0-or-later
import QtQuick
import QtQuick.Controls as Controls
import QtQuick.Layouts

ColumnLayout {
    id: root
    property string text: ""
    property string explanation: ""
    // Grouped `icon.name:` assignments resolve against the declared TYPE,
    // so the group needs a named inline component, not a bare QtObject.
    component IconGroup: QtObject {
        property string name: ""
        property string source: ""
    }
    readonly property IconGroup icon: IconGroup {}
    property list<Controls.Action> helpfulActions

    spacing: 8
    Controls.Label {
        Layout.fillWidth: true
        text: root.text
        horizontalAlignment: Text.AlignHCenter
        wrapMode: Text.WordWrap
        font.pointSize: Qt.application.font.pointSize * 1.3
        font.weight: Font.DemiBold
        opacity: 0.8
    }
    Controls.Label {
        Layout.fillWidth: true
        visible: root.explanation !== ""
        text: root.explanation
        horizontalAlignment: Text.AlignHCenter
        wrapMode: Text.WordWrap
        opacity: 0.65
    }
}
