// SPDX-License-Identifier: GPL-3.0-or-later
import QtQuick
import QtQuick.Controls as Controls
import QtQuick.Layouts

// Kirigami.NavigationTabBar subset: a footer tool bar with one checkable
// button per action.
Controls.ToolBar {
    id: root
    property list<Controls.Action> actions

    contentItem: RowLayout {
        spacing: 4
        Item { Layout.fillWidth: true }
        Repeater {
            model: root.actions
            delegate: Controls.ToolButton {
                required property Controls.Action modelData
                action: modelData
                checkable: true
                checked: modelData.checked
                visible: modelData.visible === undefined ? true : modelData.visible
                display: Controls.AbstractButton.TextBesideIcon
                onClicked: checked = Qt.binding(() => modelData.checked)
            }
        }
        Item { Layout.fillWidth: true }
    }
}
