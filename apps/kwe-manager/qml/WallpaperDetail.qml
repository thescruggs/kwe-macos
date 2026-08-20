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

    // Mirror the selected wallpaper's daemon-held grant record whenever the
    // details pane appears or the selection changes.
    function refreshPermissions() {
        if (WallpaperSelection.selectedId !== "")
            permissionsClient.requestPermissions(WallpaperSelection.selectedId);
    }

    // BETA_M4b: the live apply lane. The daemon owns the transaction; these
    // gates mirror its contract (docs/SUPERVISOR_API_V1.md) so Apply only
    // enables for content the service can actually start.
    readonly property bool applyableKind: WallpaperSelection.selectedKind === "video"
        || WallpaperSelection.selectedKind === "web"
        || WallpaperSelection.selectedKind === "scene"
    readonly property bool contentResolvable: detailPage.applyContentUrl() !== ""
    readonly property bool rendererCompatible: WallpaperSelection.selectedCompatibility === "renderer_dependent"
    // currentIndex alone is not enough: a ComboBox with an empty model must
    // not read as a valid target.
    readonly property bool hasOutput: outputPicker.currentIndex >= 0 && applyClient.outputs.length > 0
    readonly property bool canApply: detailPage.applyableKind && detailPage.contentResolvable
        && detailPage.rendererCompatible && detailPage.hasOutput

    // The daemon's catalog content path per kind: the runnable entry file for
    // video/scene, the content root for web (the renderer serves the whole
    // root). Empty means unresolvable; the client then omits content and the
    // daemon starts its own catalog content.
    function applyContentUrl() {
        if (WallpaperSelection.selectedKind === "web")
            return WallpaperSelection.selectedContentRoot;
        return WallpaperSelection.selectedEntry;
    }

    onVisibleChanged: {
        if (visible) {
            refreshPermissions();
            // Outputs are loaded on demand: the daemon caches its
            // enumeration for 5 s, so re-listing on every appearance is
            // cheap and picks up hotplugs.
            if (!applyClient.busy)
                applyClient.listOutputs();
        }
    }

    Connections {
        target: WallpaperSelection
        function onSelectedIdChanged() {
            // A result (confirmation or failure) belongs to the wallpaper
            // that produced it; never let it read as the next selection's.
            applyClient.resetStatus();
            if (detailPage.visible)
                refreshPermissions();
        }
    }

    // Bounded staleness guard for the grant mirror: while the pane is
    // visible, re-read the daemon-held record periodically so grants changed
    // by another client (or by a daemon restart) surface without waiting for
    // a toggle. Skipped while a request is queued or in flight — that request
    // already refreshes the mirror, and stacking redundant gets would only
    // lengthen the queue.
    Timer {
        id: permissionRefreshTimer
        interval: 5000
        repeat: true
        running: detailPage.visible
        onTriggered: {
            if (!permissionsClient.isPending(WallpaperSelection.selectedId))
                refreshPermissions();
        }
    }

    ColumnLayout {
        width: parent.width
        spacing: Kirigami.Units.largeSpacing

        Kirigami.PlaceholderMessage {
            Layout.fillWidth: true
            visible: WallpaperSelection.selectedId === ""
            text: qsTr("Select a wallpaper")
            explanation: qsTr("Compatibility, apply, and restore controls appear here.")
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
        // BETA_M2c: the daemon owns each wallpaper's grant record
        // (permissions-v1.json); the toggles below read and write it through
        // permissionsClient, so grant state survives restarts and is shared
        // with every other client of the wallpaper service.
        Flow {
            Layout.fillWidth: true
            visible: WallpaperSelection.selectedId !== "" && WallpaperSelection.selectedPermissions.length > 0
            spacing: Kirigami.Units.smallSpacing
            Repeater {
                model: WallpaperSelection.selectedPermissions
                delegate: Controls.Button {
                    required property string modelData
                    readonly property bool granted: permissionsClient.isGranted(WallpaperSelection.selectedId, modelData)
                    readonly property bool pending: permissionsClient.isPending(WallpaperSelection.selectedId)
                    // Every state is carried by the text so screen readers see
                    // the same meaning as sighted users (never color alone).
                    text: pending
                        ? qsTr("Updating %1…").arg(modelData)
                        : granted
                            ? qsTr("%1 granted").arg(modelData)
                            : qsTr("Grant %1").arg(modelData)
                    icon.name: granted
                        ? "dialog-ok-apply-symbolic" : "dialog-cancel-symbolic"
                    enabled: !pending
                    Accessible.description: pending
                        ? qsTr("The %1 permission is being updated in the wallpaper service").arg(modelData)
                        : text
                    onClicked: permissionsClient.setPermission(WallpaperSelection.selectedId, modelData, !granted)
                }
            }
        }
        Controls.Label {
            Layout.fillWidth: true
            visible: WallpaperSelection.selectedId !== "" && WallpaperSelection.selectedPermissions.length > 0
            text: qsTr("Requested permissions: %1").arg(WallpaperSelection.selectedPermissions.join(", "))
            opacity: 0.75
            wrapMode: Text.Wrap
        }
        Kirigami.InlineMessage {
            Layout.fillWidth: true
            visible: permissionsClient.errorMessage !== ""
            type: Kirigami.MessageType.Error
            text: qsTr("Permission grants: %1").arg(permissionsClient.errorMessage)
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
        // BETA_M4b: the live apply lane. Outputs are enumerated on demand by
        // the daemon; Apply runs its bounded transaction (validate -> start ->
        // promote -> persist -> switch); Restore is the safe-mode lane back to
        // the previous (or stock) image wallpaper. Every failure surfaces the
        // daemon's detail in text — never color alone — and the daemon reverts
        // the desktop on its own failure path.
        Controls.Label {
            Layout.fillWidth: true
            visible: WallpaperSelection.selectedId !== ""
            text: qsTr("Apply to display")
            font.bold: true
        }
        Controls.ComboBox {
            id: outputPicker
            Layout.fillWidth: true
            visible: WallpaperSelection.selectedId !== ""
            model: applyClient.outputs
            // Disabled while the enumeration is loading or while any apply-lane
            // operation is in flight (the lane is strictly serialized).
            enabled: !applyClient.busy && applyClient.outputs.length > 0
            Accessible.name: qsTr("Output to apply the wallpaper to")
            Accessible.description: qsTr("The wallpaper is applied to the selected display output")
            Controls.ToolTip.visible: hovered
            Controls.ToolTip.text: applyClient.outputs.length === 0
                ? qsTr("No display outputs are available yet")
                : Accessible.description
        }
        Controls.Label {
            Layout.fillWidth: true
            visible: WallpaperSelection.selectedId !== "" && applyClient.outputs.length === 0
            text: applyClient.state === ApplyClient.ListingOutputs
                ? qsTr("Enumerating display outputs…")
                : qsTr("No display outputs are available.")
            opacity: 0.75
            wrapMode: Text.Wrap
        }
        Controls.Button {
            Layout.fillWidth: true
            visible: WallpaperSelection.selectedId !== ""
            text: applyClient.state === ApplyClient.Applying
                ? qsTr("Applying…")
                : detailPage.hasOutput
                    ? qsTr("Apply to %1").arg(outputPicker.currentText)
                    : qsTr("Apply")
            icon.name: "dialog-ok-apply-symbolic"
            enabled: detailPage.canApply && !applyClient.busy
            Accessible.description: !detailPage.applyableKind
                ? qsTr("This wallpaper type cannot be applied")
                : !detailPage.contentResolvable
                    ? qsTr("The wallpaper content path is not available")
                    : !detailPage.rendererCompatible
                        ? qsTr("This wallpaper is not marked renderer-dependent; applying is disabled")
                        : qsTr("Apply this wallpaper to the selected display output")
            onClicked: applyClient.applyWallpaper(outputPicker.currentText,
                WallpaperSelection.selectedId, WallpaperSelection.selectedKind,
                detailPage.applyContentUrl())
        }
        Controls.Label {
            Layout.fillWidth: true
            visible: WallpaperSelection.selectedId !== "" && detailPage.applyableKind && !detailPage.canApply
            text: !detailPage.rendererCompatible
                ? qsTr("Applying is disabled: this wallpaper is not marked renderer-dependent.")
                : !detailPage.contentResolvable
                    ? qsTr("Applying is disabled: the wallpaper content path is not available.")
                    : qsTr("Applying is disabled: no display output is selected.")
            opacity: 0.75
            wrapMode: Text.Wrap
        }
        Controls.Button {
            Layout.fillWidth: true
            visible: WallpaperSelection.selectedId !== ""
            text: applyClient.state === ApplyClient.Restoring
                ? qsTr("Restoring…")
                : qsTr("Restore KDE wallpaper")
            icon.name: "edit-undo-symbolic"
            enabled: detailPage.hasOutput && !applyClient.busy
            Accessible.description: qsTr("Restore the previous image wallpaper on the selected display (safe mode)")
            onClicked: applyClient.restoreWallpaper(outputPicker.currentText)
        }
        Kirigami.InlineMessage {
            Layout.fillWidth: true
            visible: WallpaperSelection.selectedId !== "" && applyClient.state === ApplyClient.Applied
                && applyClient.appliedWallpaperId === WallpaperSelection.selectedId
                && applyClient.appliedOutput === outputPicker.currentText
            type: Kirigami.MessageType.Positive
            text: qsTr("Applied %1 to %2")
                .arg(WallpaperSelection.selectedTitle)
                .arg(applyClient.appliedOutput)
        }
        Kirigami.InlineMessage {
            Layout.fillWidth: true
            visible: WallpaperSelection.selectedId !== ""
                && applyClient.restoredOutput !== ""
                && applyClient.restoredOutput === outputPicker.currentText
            type: Kirigami.MessageType.Information
            text: applyClient.restoreMode === "stock"
                ? qsTr("Restored the image wallpaper on %1 (stock image fallback)")
                    .arg(applyClient.restoredOutput)
                : qsTr("Restored the image wallpaper on %1").arg(applyClient.restoredOutput)
        }
        Kirigami.InlineMessage {
            Layout.fillWidth: true
            visible: WallpaperSelection.selectedId !== "" && applyClient.state === ApplyClient.Failed
            type: Kirigami.MessageType.Error
            text: applyClient.errorMessage
            actions: [
                Kirigami.Action {
                    // Re-runs the operation that failed (apply or restore),
                    // never a different one.
                    text: qsTr("Try Again")
                    icon.name: "view-refresh-symbolic"
                    visible: applyClient.failedMethod !== ""
                    enabled: !applyClient.busy
                    onTriggered: applyClient.retry()
                }
            ]
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
            onClicked: webPreview.running ? webPreview.stop() : webPreview.play(WallpaperSelection.selectedEntry, WallpaperSelection.selectedId)
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
