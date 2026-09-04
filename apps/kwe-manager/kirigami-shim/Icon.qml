// SPDX-License-Identifier: GPL-3.0-or-later
import QtQuick
import QtQuick.Controls as Controls

// Kirigami.Icon subset: `source` is an icon NAME (theme lookup) or a URL.
// Without a Freedesktop icon theme (macOS) named icons render nothing, so
// the manager's text labels carry the meaning; a URL source still shows.
Item {
    id: root
    property var source: ""
    property color color: "transparent"
    property bool isMask: false
    implicitWidth: 22
    implicitHeight: 22

    readonly property bool isUrl: {
        const text = String(root.source);
        return text.indexOf("/") >= 0 || text.indexOf(":") >= 0;
    }

    Image {
        anchors.fill: parent
        visible: root.isUrl
        source: root.isUrl ? root.source : ""
        fillMode: Image.PreserveAspectFit
        asynchronous: true
    }
    Controls.ToolButton {
        anchors.fill: parent
        visible: !root.isUrl && String(root.source) !== ""
        enabled: false
        flat: true
        padding: 0
        icon.name: root.isUrl ? "" : String(root.source)
        icon.width: root.width
        icon.height: root.height
        background: null
    }
}
