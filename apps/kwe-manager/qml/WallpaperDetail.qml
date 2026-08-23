// SPDX-License-Identifier: GPL-3.0-or-later
import QtQuick
import QtQuick.Controls as Controls
import QtQuick.Layouts
import org.kde.kirigami as Kirigami
// The module's own C++ clients (CatalogClient, ApplyClient, …). The engine
// resolves them implicitly for a file of this module, but qmllint does not —
// these files sit one directory below the module root — and the type gate in
// scripts/qml-typecheck.sh is what keeps the QML_ELEMENT registration from
// silently disappearing again, so the dependency is stated explicitly.
import org.kde.kwe

// Details pane shared by the Installed and Workshop views; shows the item
// selected through WallpaperSelection.
Kirigami.ScrollablePage {
    id: detailPage

    required property string activePlaylistName
    required property bool detailsVisible

    visible: detailPage.detailsVisible
    title: qsTr("Wallpaper Details")

    // F4: "Report rendering issue". A local diagnostic bundle recorded by
    // hand right after seeing a problem; nothing here is uploaded anywhere.
    property bool issueReportOpen: false

    function openIssueReport() {
        issueReportNoteField.text = "";
        detailPage.issueReportOpen = true;
    }

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

    // Outputs are loaded on demand: the daemon caches its enumeration for 5 s,
    // so re-listing on every appearance is cheap and picks up hotplugs. The
    // enumeration must never depend on a visibility *edge* alone: an Item is
    // visible by default, so a pane whose binding is already true when it is
    // created never emits visibleChanged, and the picker stayed empty forever.
    function ensureOutputs() {
        if (!detailPage.visible)
            return;
        // The lane is strictly serialized; onBusyChanged re-runs this when it
        // frees up, so a skipped listing is never simply dropped.
        if (applyClient.busy)
            return;
        applyClient.listOutputs();
    }

    Component.onCompleted: detailPage.ensureOutputs()

    onVisibleChanged: {
        if (visible) {
            refreshPermissions();
            detailPage.ensureOutputs();
        }
    }

    Connections {
        target: applyClient
        // Re-arm a listing that was skipped because the lane was busy. Guarded
        // on outputsListed so a daemon that truthfully reports zero outputs is
        // asked once, not in a loop, and on Failed so an error stays on screen
        // for the user's Try Again instead of being retried behind their back.
        function onBusyChanged() {
            if (!applyClient.busy && !applyClient.outputsListed
                    && applyClient.state !== ApplyClient.Failed)
                detailPage.ensureOutputs();
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
            // A new target invalidates a stale failure: retry() replays the
            // recorded output, so leaving Try Again armed after a reselection
            // would silently apply to the old display while the UI shows the
            // new one. Clearing also hides the Try Again affordance.
            onCurrentIndexChanged: applyClient.resetStatus()
            Controls.ToolTip.visible: hovered
            Controls.ToolTip.text: applyClient.outputs.length > 0
                ? Accessible.description
                : applyClient.outputsListed
                    ? qsTr("The wallpaper service reports no display outputs")
                    : qsTr("The display outputs have not been enumerated yet")
        }
        // F1: how the picture maps onto the output. Aspect keeps the whole
        // picture (letterboxed), Fill crops to cover, Stretch ignores the
        // aspect ratio — the daemon renders the canvas at the output's own
        // size, so the mode mostly decides what a wallpaper whose own aspect
        // differs from the display does.
        RowLayout {
            Layout.fillWidth: true
            visible: WallpaperSelection.selectedId !== "" && applyClient.outputs.length > 0
            Controls.Label {
                text: qsTr("Scaling")
            }
            Controls.ComboBox {
                id: scalingPicker
                Layout.fillWidth: true
                textRole: "text"
                valueRole: "value"
                model: [
                    { text: qsTr("Aspect (fit, letterbox)"), value: "aspect" },
                    { text: qsTr("Fill (crop to cover)"), value: "fill" },
                    { text: qsTr("Stretch (ignore aspect)"), value: "stretch" }
                ]
                enabled: !applyClient.busy
                Accessible.name: qsTr("Scaling mode")
                Accessible.description: qsTr("How the wallpaper picture maps onto the display: keep the whole picture, crop to cover, or stretch")
                Controls.ToolTip.visible: hovered
                Controls.ToolTip.text: Accessible.description
            }
        }
        // F2: the renderer's publish-rate limit. The renderer paces its
        // frames to this; lower values cost less CPU/GPU (web screencast
        // decode, libmpv software render, Vulkan composite all scale with
        // it). Persisted per output with the assignment.
        RowLayout {
            Layout.fillWidth: true
            visible: WallpaperSelection.selectedId !== "" && applyClient.outputs.length > 0
            Controls.Label {
                text: qsTr("Frame rate limit")
            }
            Controls.ComboBox {
                id: fpsPicker
                Layout.fillWidth: true
                textRole: "text"
                valueRole: "value"
                currentIndex: 2
                model: [
                    { text: qsTr("15 fps"), value: 15 },
                    { text: qsTr("24 fps"), value: 24 },
                    { text: qsTr("30 fps (default)"), value: 30 },
                    { text: qsTr("60 fps"), value: 60 }
                ]
                enabled: !applyClient.busy
                Accessible.name: qsTr("Frame rate limit")
                Accessible.description: qsTr("Maximum frames per second the wallpaper renderer publishes; lower values use less CPU and GPU")
                Controls.ToolTip.visible: hovered
                Controls.ToolTip.text: Accessible.description
            }
        }
        Controls.Label {
            Layout.fillWidth: true
            visible: WallpaperSelection.selectedId !== "" && applyClient.outputs.length === 0
            text: applyClient.state === ApplyClient.ListingOutputs
                ? qsTr("Enumerating display outputs…")
                : applyClient.outputsListed
                    ? qsTr("The wallpaper service reports no display outputs. Check that a display is connected and enabled.")
                    : applyClient.errorMessage !== ""
                        ? applyClient.errorMessage
                        : qsTr("The display outputs have not been enumerated yet.")
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
                    ? qsTr("The wallpaper content path is not declared")
                    : !detailPage.rendererCompatible
                        ? qsTr("This wallpaper is not marked renderer-dependent; applying is disabled")
                        : qsTr("Apply this wallpaper to the selected display output")
            onClicked: applyClient.applyWallpaper(outputPicker.currentText,
                WallpaperSelection.selectedId, WallpaperSelection.selectedKind,
                detailPage.applyContentUrl(), scalingPicker.currentValue, fpsPicker.currentValue)
        }
        Controls.Label {
            Layout.fillWidth: true
            // The empty-outputs label above already explains the no-output
            // case; this hint only adds per-gate detail when outputs exist.
            visible: WallpaperSelection.selectedId !== "" && detailPage.applyableKind
                && !detailPage.canApply && applyClient.outputs.length > 0
            text: !detailPage.rendererCompatible
                ? qsTr("Applying is disabled: this wallpaper is not marked renderer-dependent.")
                : !detailPage.contentResolvable
                    ? qsTr("Applying is disabled: the wallpaper content path is not declared.")
                    : qsTr("Applying is disabled: no display output is selected.")
            opacity: 0.75
            wrapMode: Text.Wrap
        }
        // F4: "Report rendering issue". Available whenever a wallpaper is
        // selected, whether or not Apply succeeded — a problem noticed later
        // (after the desktop already shows it) still needs a report. Recording
        // is local and bounded; nothing is uploaded anywhere.
        Controls.Button {
            Layout.fillWidth: true
            visible: WallpaperSelection.selectedId !== ""
            text: issueReporter.busy ? qsTr("Saving report…") : qsTr("Report rendering issue…")
            icon.name: "tools-report-bug-symbolic"
            enabled: WallpaperSelection.selectedId !== "" && !issueReporter.busy
            Accessible.description: qsTr("Record a note about what looks wrong with this wallpaper, together with the current renderer diagnostics, for the next debugging session")
            onClicked: detailPage.openIssueReport()
        }
        Kirigami.InlineMessage {
            Layout.fillWidth: true
            visible: WallpaperSelection.selectedId !== "" && issueReporter.lastReportPath !== ""
            type: Kirigami.MessageType.Positive
            text: qsTr("Saved rendering issue report to:")
            actions: [
                Kirigami.Action {
                    text: qsTr("Copy path")
                    icon.name: "edit-copy-symbolic"
                    onTriggered: {
                        issueReportPathField.selectAll();
                        issueReportPathField.copy();
                        issueReportPathField.deselect();
                    }
                }
            ]
        }
        Controls.TextField {
            id: issueReportPathField
            Layout.fillWidth: true
            visible: WallpaperSelection.selectedId !== "" && issueReporter.lastReportPath !== ""
            readOnly: true
            selectByMouse: true
            text: issueReporter.lastReportPath
            Accessible.name: qsTr("Saved report folder path")
            Accessible.description: qsTr("The folder containing the recorded rendering issue report; select and copy this path to share it in the next debugging session")
        }
        Kirigami.InlineMessage {
            Layout.fillWidth: true
            visible: issueReporter.errorMessage !== ""
            type: Kirigami.MessageType.Error
            text: issueReporter.errorMessage
        }
        Controls.Button {
            Layout.fillWidth: true
            visible: WallpaperSelection.selectedId !== ""
            // "Reset", not "Restore": on an output this client never applied
            // to, the daemon falls back to the stock image — promising "the
            // previous wallpaper back" would overstate it. The fallback is
            // named so the safe-mode lane never surprises.
            text: applyClient.state === ApplyClient.Restoring
                ? qsTr("Restoring…")
                : qsTr("Reset to image wallpaper")
            icon.name: "edit-undo-symbolic"
            enabled: detailPage.hasOutput && !applyClient.busy
            Accessible.description: qsTr("Reset the selected display to the image wallpaper: the saved previous wallpaper when one exists, otherwise the stock image (safe mode)")
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

    // F4: the report note dialog. Recording is bounded and local — the
    // daemon's current renderer/assignment/health state, its recent journal,
    // and the last rendered frame are captured alongside the note; nothing
    // is uploaded anywhere.
    Controls.Dialog {
        id: issueReportDialog
        modal: true
        title: qsTr("Report rendering issue")
        visible: detailPage.issueReportOpen
        standardButtons: Controls.Dialog.NoButton
        width: Math.min(parent ? parent.width * 0.8 : 480, 480)
        onRejected: detailPage.issueReportOpen = false

        contentItem: ColumnLayout {
            spacing: Kirigami.Units.smallSpacing
            Controls.Label {
                Layout.fillWidth: true
                text: qsTr("Describe what looks wrong with %1. Saved to disk with the current renderer status, assignments, health, recent daemon journal, and the last rendered frame — nothing is sent anywhere.")
                    .arg(WallpaperSelection.selectedTitle)
                wrapMode: Text.Wrap
            }
            Controls.TextArea {
                id: issueReportNoteField
                Layout.fillWidth: true
                Layout.preferredHeight: Kirigami.Units.gridUnit * 6
                wrapMode: TextEdit.Wrap
                placeholderText: qsTr("What looks wrong? e.g. black layer, wrong colours, missing effect, offset, slow")
                Accessible.name: qsTr("Rendering issue note")
                Accessible.description: placeholderText
            }
        }

        footer: Controls.DialogButtonBox {
            Controls.Button {
                text: qsTr("Cancel")
                Controls.DialogButtonBox.buttonRole: Controls.DialogButtonBox.RejectRole
            }
            Controls.Button {
                text: issueReporter.busy ? qsTr("Saving…") : qsTr("Save report")
                icon.name: "document-save-symbolic"
                enabled: !issueReporter.busy
                Controls.DialogButtonBox.buttonRole: Controls.DialogButtonBox.AcceptRole
            }
            onAccepted: issueReportDialog.accept()
            onRejected: issueReportDialog.reject()
        }

        onAccepted: {
            issueReporter.record(WallpaperSelection.selectedId, WallpaperSelection.selectedTitle,
                WallpaperSelection.selectedKind, issueReportNoteField.text);
            detailPage.issueReportOpen = false;
        }
    }
}
