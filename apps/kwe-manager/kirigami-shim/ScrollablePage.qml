// SPDX-License-Identifier: GPL-3.0-or-later
import QtQuick
import QtQuick.Controls as Controls

// Kirigami.ScrollablePage subset: the page's single child item is placed
// in a vertically scrolling Flickable and given the page's width.
Controls.Page {
    id: root
    default property alias content: container.data
    property list<Controls.Action> actions
    property alias flickable: flickable

    padding: 0

    contentItem: Flickable {
        id: flickable
        clip: true
        contentWidth: width
        contentHeight: container.childrenRect.height + 16
        boundsBehavior: Flickable.StopAtBounds
        Controls.ScrollBar.vertical: Controls.ScrollBar {}

        Item {
            id: container
            x: 8
            y: 8
            width: flickable.width - 16
            // A single ColumnLayout child gets the full width.
            onChildrenChanged: {
                for (let i = 0; i < children.length; ++i)
                    children[i].width = Qt.binding(() => container.width);
            }
        }
    }
}
