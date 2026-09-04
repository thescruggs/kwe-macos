// SPDX-License-Identifier: GPL-3.0-or-later
import QtQuick
import QtQuick.Controls as Controls
import QtQuick.Layouts

// Kirigami.InlineMessage subset: type-tinted frame, wrapped text, a row of
// action buttons (Kirigami.Action list) and an optional close button.
Controls.Frame {
    id: root

    property int type: 0
    property string text: ""
    property list<Controls.Action> actions
    property bool showCloseButton: false
    // Grouped `icon.name:` assignments resolve against the declared TYPE,
    // so the group needs a named inline component, not a bare QtObject.
    component IconGroup: QtObject {
        property string name: ""
        property string source: ""
    }
    readonly property IconGroup icon: IconGroup {}

    readonly property color tint: type === 3 ? "#da4453" : type === 2 ? "#f67400" : type === 1 ? "#27ae60" : "#2980b9"

    Layout.fillWidth: true
    padding: 8
    background: Rectangle {
        radius: 5
        color: Qt.rgba(root.tint.r, root.tint.g, root.tint.b, 0.12)
        border.color: root.tint
        border.width: 1
    }

    contentItem: RowLayout {
        spacing: 8
        Rectangle {
            Layout.alignment: Qt.AlignTop
            Layout.topMargin: 4
            width: 8
            height: 8
            radius: 4
            color: root.tint
            Accessible.ignored: true
        }
        Controls.Label {
            Layout.fillWidth: true
            text: root.text
            wrapMode: Text.WordWrap
            Accessible.role: Accessible.StaticText
            Accessible.name: text
        }
        Flow {
            Layout.alignment: Qt.AlignTop
            spacing: 4
            Repeater {
                model: root.actions
                delegate: Controls.Button {
                    required property Controls.Action modelData
                    action: modelData
                    visible: modelData.visible === undefined ? true : modelData.visible
                    flat: true
                }
            }
            Controls.ToolButton {
                visible: root.showCloseButton
                text: "✕"
                Accessible.name: qsTr("Close message")
                onClicked: root.visible = false
            }
        }
    }
}
