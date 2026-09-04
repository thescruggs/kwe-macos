// SPDX-License-Identifier: GPL-3.0-or-later
import QtQuick
import QtQuick.Layouts

// Kirigami.Separator subset: a 1px line; vertical when it is asked to
// fill height, horizontal otherwise.
Rectangle {
    id: root
    readonly property bool vertical: Layout.fillHeight && !Layout.fillWidth
    implicitWidth: vertical ? 1 : 100
    implicitHeight: vertical ? 100 : 1
    Layout.preferredWidth: vertical ? 1 : -1
    Layout.preferredHeight: vertical ? -1 : 1
    color: Qt.rgba(palette.text.r, palette.text.g, palette.text.b, 0.2)
}
