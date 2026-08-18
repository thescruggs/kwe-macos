// SPDX-License-Identifier: Apache-2.0
import QtQuick
import QtQuick.Controls as Controls
import QtQuick.Layouts
import org.kde.kirigami as Kirigami

// Details pane shared by the Installed and Workshop views; shows the item
// selected through WallpaperSelection.
Kirigami.ScrollablePage {
    id: detailPage

    required property string activePlaylistName
    required property bool detailsVisible

    visible: detailPage.detailsVisible
    title: qsTr("Wallpaper Details")

    ColumnLayout {
        width: parent.width
        spacing: Kirigami.Units.largeSpacing

        Kirigami.PlaceholderMessage {
            Layout.fillWidth: true
            visible: WallpaperSelection.selectedId === ""
            text: qsTr("Select a wallpaper")
            explanation: qsTr("Compatibility and diagnostic details appear here before Apply is enabled in a future alpha.")
            icon.name: "preferences-desktop-wallpaper-symbolic"
        }

        Image {
            Layout.fillWidth: true
            Layout.preferredHeight: width * 0.5625
            visible: WallpaperSelection.selectedId !== "" && status === Image.Ready
            source: WallpaperSelection.selectedPreview
            fillMode: Image.PreserveAspectFit
            asynchronous: true
        }
        Kirigami.Heading {
            Layout.fillWidth: true
            visible: WallpaperSelection.selectedId !== ""
            text: WallpaperSelection.selectedTitle
            level: 2
            wrapMode: Text.Wrap
        }
        Controls.Label {
            Layout.fillWidth: true
            visible: WallpaperSelection.selectedId !== ""
            text: qsTr("Workshop %1 · %2").arg(WallpaperSelection.selectedId).arg(WallpaperSelection.selectedKind)
            opacity: 0.75
            wrapMode: Text.Wrap
        }
        Flow {
            Layout.fillWidth: true
            visible: WallpaperSelection.selectedId !== "" && WallpaperSelection.selectedPermissions.length > 0
            spacing: Kirigami.Units.smallSpacing
            Repeater {
                model: WallpaperSelection.selectedPermissions
                delegate: Controls.Button {
                    required property string modelData
                    text: catalogStats.isPermissionGranted(WallpaperSelection.selectedId, modelData)
                        ? qsTr("%1 granted").arg(modelData)
                        : qsTr("Grant %1").arg(modelData)
                    icon.name: catalogStats.isPermissionGranted(WallpaperSelection.selectedId, modelData)
                        ? "dialog-ok-apply-symbolic" : "dialog-cancel-symbolic"
                    onClicked: catalogStats.togglePermission(WallpaperSelection.selectedId, modelData)
                }
            }
        }
        Controls.Label {
            Layout.fillWidth: true
            visible: WallpaperSelection.selectedId !== "" && WallpaperSelection.selectedPermissions.length > 0
            text: qsTr("Requested permissions: %1 (not granted in this alpha)").arg(WallpaperSelection.selectedPermissions.join(", "))
            color: Kirigami.Theme.neutralTextColor
            wrapMode: Text.Wrap
        }
        Controls.Label {
            Layout.fillWidth: true
            visible: WallpaperSelection.selectedId !== "" && WallpaperSelection.selectedTags.length > 0
            text: qsTr("Tags: %1").arg(WallpaperSelection.selectedTags.join(", "))
            opacity: 0.75
            wrapMode: Text.Wrap
        }
        Kirigami.InlineMessage {
            Layout.fillWidth: true
            visible: WallpaperSelection.selectedId !== ""
            type: WallpaperSelection.selectedCompatibility === "invalid" || WallpaperSelection.selectedCompatibility === "unsupported"
                ? Kirigami.MessageType.Warning : Kirigami.MessageType.Information
            text: qsTr("%1: %2").arg(WallpaperSelection.compatibilityLabel(WallpaperSelection.selectedCompatibility)).arg(WallpaperSelection.selectedDetail)
        }
        Controls.Label {
            Layout.fillWidth: true
            visible: WallpaperSelection.selectedId !== "" && WallpaperSelection.selectedDiagnosticSummary !== ""
            text: WallpaperSelection.selectedDiagnosticSummary
            color: Kirigami.Theme.negativeTextColor
            wrapMode: Text.Wrap
        }
        Controls.Button {
            Layout.fillWidth: true
            visible: WallpaperSelection.selectedId !== ""
            enabled: false
            text: qsTr("Apply (available after safe bridge)")
            icon.name: "dialog-ok-apply-symbolic"
            Accessible.description: qsTr("Disabled in Alpha 0.1 to protect the Plasma desktop")
            Controls.ToolTip.visible: hovered
            Controls.ToolTip.text: Accessible.description
        }
        Controls.Button {
            Layout.fillWidth: true
            visible: WallpaperSelection.selectedId !== "" && detailPage.activePlaylistName !== ""
            text: qsTr("Add to %1").arg(detailPage.activePlaylistName)
            icon.name: "list-add-symbolic"
            onClicked: playlistController.add(detailPage.activePlaylistName, WallpaperSelection.selectedId)
        }
        Controls.Button {
            Layout.fillWidth: true
            visible: WallpaperSelection.selectedId !== "" && WallpaperSelection.selectedKind === "video"
            enabled: WallpaperSelection.selectedEntry !== ""
            text: videoPreview.running ? qsTr("Video preview running") : qsTr("Preview video")
            icon.name: videoPreview.running ? "media-playback-stop-symbolic" : "media-playback-start-symbolic"
            onClicked: videoPreview.running ? videoPreview.stop() : videoPreview.play(WallpaperSelection.selectedEntry)
        }
        Controls.Button {
            Layout.fillWidth: true
            visible: WallpaperSelection.selectedId !== "" && WallpaperSelection.selectedKind === "web"
            enabled: WallpaperSelection.selectedEntry !== ""
            text: webPreview.running ? qsTr("Web preview running") : qsTr("Preview web wallpaper")
            icon.name: webPreview.running ? "media-playback-stop-symbolic" : "internet-web-browser-symbolic"
            onClicked: webPreview.running ? webPreview.stop() : webPreview.play(WallpaperSelection.selectedEntry)
        }
        Controls.Button {
            Layout.fillWidth: true
            visible: WallpaperSelection.selectedId !== ""
            text: qsTr("Open in Steam Workshop")
            icon.name: "internet-services-symbolic"
            onClicked: workshopClient.openItem(WallpaperSelection.selectedId)
            Accessible.description: qsTr("Open the canonical Workshop item in the Steam client")
        }
        Kirigami.InlineMessage {
            Layout.fillWidth: true
            visible: workshopClient.errorMessage !== ""
            type: Kirigami.MessageType.Error
            text: workshopClient.errorMessage
        }
        Kirigami.InlineMessage {
            Layout.fillWidth: true
            visible: videoPreview.errorMessage !== ""
            type: Kirigami.MessageType.Warning
            text: videoPreview.errorMessage
        }
        Kirigami.InlineMessage {
            Layout.fillWidth: true
            visible: webPreview.errorMessage !== ""
            type: Kirigami.MessageType.Warning
            text: webPreview.errorMessage
        }
    }
}
