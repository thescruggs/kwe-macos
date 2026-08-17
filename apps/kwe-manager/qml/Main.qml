// SPDX-License-Identifier: Apache-2.0
import QtQuick
import QtQuick.Controls as Controls
import QtQuick.Layouts
import org.kde.kirigami as Kirigami

Kirigami.ApplicationWindow {
    id: root
    width: 1220
    height: 760
    minimumWidth: 760
    minimumHeight: 520
    visible: true
    title: qsTr("KDE Wallpaper Engine")

    property string selectedTitle: ""
    property string selectedId: ""
    property string selectedKind: ""
    property string selectedCompatibility: ""
    property string selectedDetail: ""
    property url selectedPreview: ""
    property var selectedTags: []
    property url selectedEntry: ""
    property string selectedDiagnosticSummary: ""
    property var selectedPermissions: []
    property bool createPlaylistOpen: false

    function selectWallpaper(title, id, kind, compatibility, detail, preview, tags, entry, diagnosticSummary, permissions) {
        selectedTitle = title
        selectedId = id
        selectedKind = kind
        selectedCompatibility = compatibility
        selectedDetail = detail
        selectedPreview = preview
        selectedTags = tags
        selectedEntry = entry
        selectedDiagnosticSummary = diagnosticSummary
        selectedPermissions = permissions
    }

    function compatibilityLabel(value) {
        switch (value) {
        case "renderer_dependent": return qsTr("Renderer-dependent")
        case "backend_missing": return qsTr("Backend planned")
        case "unsupported": return qsTr("Unsupported")
        case "invalid": return qsTr("Needs attention")
        default: return value
        }
    }

    function compatibilityIcon(value) {
        return value === "renderer_dependent" ? "dialog-information-symbolic"
             : value === "backend_missing" ? "tools-wizard-symbolic"
             : "data-warning-symbolic"
    }

    pageStack.initialPage: Kirigami.Page {
        id: galleryPage
        title: qsTr("Installed Wallpapers")
        padding: 0

        header: Controls.ToolBar {
            contentItem: RowLayout {
                spacing: Kirigami.Units.smallSpacing

                Kirigami.SearchField {
                    id: searchField
                    Layout.fillWidth: true
                    Layout.maximumWidth: Kirigami.Units.gridUnit * 28
                    placeholderText: qsTr("Search title or Workshop ID…")
                    Accessible.name: qsTr("Search installed wallpapers")
                    onTextChanged: wallpaperModel.searchText = text
                }

                Controls.ComboBox {
                    id: typeFilter
                    textRole: "label"
                    valueRole: "value"
                    Accessible.name: qsTr("Filter by wallpaper type")
                    model: [
                        { label: qsTr("All types"), value: "all" },
                        { label: qsTr("Scenes"), value: "scene" },
                        { label: qsTr("Videos"), value: "video" },
                        { label: qsTr("Web"), value: "web" },
                        { label: qsTr("Unknown"), value: "unknown" },
                        { label: qsTr("Invalid"), value: "invalid" }
                    ]
                    onCurrentValueChanged: wallpaperModel.kindFilter = currentValue
                }

                Controls.ComboBox {
                    id: sortFilter
                    textRole: "label"
                    valueRole: "value"
                    Accessible.name: qsTr("Sort wallpapers")
                    model: [
                        { label: qsTr("Title"), value: "title" },
                        { label: qsTr("Type"), value: "kind" }
                    ]
                    onCurrentValueChanged: wallpaperModel.sortMode = currentValue
                }

                Controls.ComboBox {
                    id: playlistSelector
                    visible: playlistController.names.length > 0
                    model: playlistController.names
                    Accessible.name: qsTr("Select playlist")
                }
                Controls.ToolButton {
                    text: qsTr("New playlist")
                    icon.name: "list-add-symbolic"
                    onClicked: root.createPlaylistOpen = true
                }
                Controls.Label {
                    visible: playlistSelector.currentText !== ""
                    text: qsTr("%1 items").arg(playlistController.entries(playlistSelector.currentText).length)
                    opacity: 0.75
                }

                Controls.ToolButton {
                    text: wallpaperModel.favoritesOnly ? qsTr("All") : qsTr("Favorites")
                    icon.name: wallpaperModel.favoritesOnly ? "view-list-symbolic" : "starred-symbolic"
                    display: Controls.AbstractButton.TextBesideIcon
                    Accessible.name: wallpaperModel.favoritesOnly ? qsTr("Show all wallpapers") : qsTr("Show favorite wallpapers")
                    onClicked: wallpaperModel.favoritesOnly = !wallpaperModel.favoritesOnly
                }

                Controls.ToolButton {
                    text: qsTr("Rescan")
                    icon.name: "view-refresh-symbolic"
                    display: Controls.AbstractButton.TextBesideIcon
                    enabled: catalogClient.state !== catalogClient.Loading
                    Accessible.name: qsTr("Rescan Steam Workshop folders")
                    onClicked: catalogClient.rescan()
                }

                Controls.ToolButton {
                    text: packageInstaller.state === packageInstaller.SafeMode
                        ? qsTr("Leave safe mode") : qsTr("Safe mode")
                    icon.name: packageInstaller.state === packageInstaller.SafeMode
                        ? "dialog-ok-apply-symbolic" : "security-high-symbolic"
                    display: Controls.AbstractButton.TextBesideIcon
                    enabled: packageInstaller.state === packageInstaller.Installed
                        || packageInstaller.state === packageInstaller.SafeMode
                    Accessible.name: text
                    onClicked: packageInstaller.state === packageInstaller.SafeMode
                        ? packageInstaller.leaveSafeMode() : packageInstaller.enterSafeMode()
                }
            }
        }

        ColumnLayout {
            anchors.fill: parent
            spacing: 0

            Kirigami.InlineMessage {
                Layout.fillWidth: true
                type: Kirigami.MessageType.Error
                visible: catalogClient.state === catalogClient.Error
                text: catalogClient.errorMessage
                actions: [
                    Kirigami.Action {
                        text: qsTr("Try Again")
                        icon.name: "view-refresh-symbolic"
                        onTriggered: catalogClient.refresh()
                    }
                ]
            }

            Kirigami.InlineMessage {
                Layout.fillWidth: true
                type: Kirigami.MessageType.Error
                visible: playlistController.errorMessage !== ""
                text: playlistController.errorMessage
            }

            Controls.Frame {
                Layout.fillWidth: true
                visible: playlistSelector.currentText !== ""
                Accessible.name: qsTr("Selected playlist playback settings")

                contentItem: ColumnLayout {
                    spacing: Kirigami.Units.smallSpacing

                    Flow {
                        Layout.fillWidth: true
                        Layout.preferredHeight: childrenRect.height
                        spacing: Kirigami.Units.largeSpacing

                        Controls.Label {
                            text: qsTr("Playlist settings")
                            font.bold: true
                        }
                        Controls.CheckBox {
                            text: qsTr("Shuffle")
                            checked: playlistController.shuffle(playlistSelector.currentText)
                            Accessible.name: qsTr("Shuffle selected playlist")
                            onToggled: playlistController.setShuffle(playlistSelector.currentText, checked)
                        }
                        Controls.CheckBox {
                            text: qsTr("Repeat")
                            checked: playlistController.repeat(playlistSelector.currentText)
                            Accessible.name: qsTr("Repeat selected playlist")
                            onToggled: playlistController.setRepeat(playlistSelector.currentText, checked)
                        }
                        Controls.Label { text: qsTr("Duration") }
                        Controls.SpinBox {
                            id: playlistDuration
                            from: 10
                            to: 86400
                            stepSize: 10
                            value: playlistController.durationSeconds(playlistSelector.currentText)
                            editable: true
                            Accessible.name: qsTr("Wallpaper duration in seconds")
                            Accessible.description: qsTr("Time before the playlist selects the next available wallpaper")
                            onValueModified: playlistController.setDurationSeconds(playlistSelector.currentText, value)
                        }
                        Controls.Label { text: qsTr("seconds") }
                        Controls.ComboBox {
                            id: playlistTransition
                            textRole: "label"
                            valueRole: "value"
                            model: [
                                { label: qsTr("No transition"), value: "none" },
                                { label: qsTr("Crossfade"), value: "crossfade" }
                            ]
                            currentIndex: playlistController.transition(playlistSelector.currentText) === "crossfade" ? 1 : 0
                            Accessible.name: qsTr("Playlist transition")
                            onActivated: playlistController.setTransition(playlistSelector.currentText, currentValue)
                        }
                        Controls.SpinBox {
                            from: 0
                            to: 10
                            value: playlistController.transitionSeconds(playlistSelector.currentText)
                            enabled: playlistTransition.currentValue === "crossfade"
                            Accessible.name: qsTr("Transition duration in seconds")
                            onValueModified: playlistController.setTransitionSeconds(playlistSelector.currentText, value)
                        }
                    }
                    Controls.Label {
                        text: qsTr("Display assignment is not enabled yet")
                        opacity: 0.7
                    }
                }
            }

            Kirigami.InlineMessage {
                Layout.fillWidth: true
                visible: catalogClient.changeHistory.length > 0
                type: Kirigami.MessageType.Information
                text: qsTr("Recent Workshop changes\n%1").arg(catalogClient.changeHistory.join("\n"))
                actions: [
                    Kirigami.Action {
                        text: qsTr("Clear history")
                        icon.name: "edit-clear-history-symbolic"
                        onTriggered: catalogClient.clearHistory()
                    }
                ]
            }

            Kirigami.InlineMessage {
                Layout.fillWidth: true
                visible: rendererStatus.quarantined
                type: Kirigami.MessageType.Error
                text: qsTr("Renderer quarantined wallpaper %1. %2").arg(rendererStatus.wallpaperId, rendererStatus.detail !== "" ? rendererStatus.detail : qsTr("The last-known-good frame remains active."))
            }

            Kirigami.InlineMessage {
                Layout.fillWidth: true
                type: Kirigami.MessageType.Information
                visible: catalogClient.state === catalogClient.Ready
                text: qsTr("Alpha 0.1 indexes installed content safely. Applying wallpapers stays disabled until the isolated Plasma frame bridge is ready.")
                showCloseButton: true
            }

            Kirigami.InlineMessage {
                Layout.fillWidth: true
                visible: packageInstaller.state !== packageInstaller.Installed
                type: packageInstaller.state === packageInstaller.Failed
                    ? Kirigami.MessageType.Error : Kirigami.MessageType.Warning
                text: packageInstaller.message
                actions: [
                    Kirigami.Action {
                        text: qsTr("Install display bridge")
                        icon.name: "install-symbolic"
                        visible: packageSource !== ""
                            && packageInstaller.state !== packageInstaller.SafeMode
                        onTriggered: packageInstaller.installFrom(packageSource)
                    },
                    Kirigami.Action {
                        text: qsTr("Leave safe mode")
                        icon.name: "dialog-ok-apply-symbolic"
                        visible: packageInstaller.state === packageInstaller.SafeMode
                        onTriggered: packageInstaller.leaveSafeMode()
                    }
                ]
            }

            Kirigami.InlineMessage {
                Layout.fillWidth: true
                visible: catalogClient.changeMessage !== ""
                type: Kirigami.MessageType.Information
                text: catalogClient.changeMessage
                actions: [
                    Kirigami.Action {
                        text: qsTr("Rescan now")
                        icon.name: "view-refresh-symbolic"
                        onTriggered: catalogClient.rescan()
                    },
                    Kirigami.Action {
                        text: qsTr("Dismiss")
                        icon.name: "dialog-close-symbolic"
                        onTriggered: catalogClient.dismissChange()
                    }
                ]
            }

            RowLayout {
                Layout.fillWidth: true
                Layout.margins: Kirigami.Units.largeSpacing
                spacing: Kirigami.Units.largeSpacing

                Kirigami.Heading {
                    text: qsTr("%1 installed").arg(catalogStats.totalCount)
                    level: 2
                }
                Controls.Label { text: qsTr("%1 scenes").arg(catalogStats.sceneCount) }
                Controls.Label { text: qsTr("%1 videos").arg(catalogStats.videoCount) }
                Controls.Label { text: qsTr("%1 web").arg(catalogStats.webCount) }
                Controls.Label {
                    visible: catalogStats.subscribedCount > 0
                    text: qsTr("%1 subscribed").arg(catalogStats.subscribedCount)
                }
                Controls.Label {
                    visible: catalogStats.missingCount > 0
                    text: qsTr("%1 awaiting download").arg(catalogStats.missingCount)
                }
                Controls.Label {
                    visible: catalogStats.downloadingCount > 0
                    text: qsTr("%1 downloading").arg(catalogStats.downloadingCount)
                }
                Controls.Label {
                    visible: catalogStats.issueCount > 0
                    text: qsTr("%1 need attention").arg(catalogStats.issueCount)
                    Accessible.name: text
                }
                Item { Layout.fillWidth: true }
                Controls.Label {
                    text: qsTr("Showing %1").arg(wallpaperModel.count)
                    opacity: 0.75
                }
            }

            Controls.SplitView {
                Layout.fillWidth: true
                Layout.fillHeight: true

                Item {
                    Controls.SplitView.fillWidth: true
                    Controls.SplitView.minimumWidth: Kirigami.Units.gridUnit * 24

                    GridView {
                        id: grid
                        anchors.fill: parent
                        anchors.margins: Kirigami.Units.largeSpacing
                        clip: true
                        model: wallpaperModel
                        cellWidth: Math.max(Kirigami.Units.gridUnit * 13, width / Math.max(1, Math.floor(width / (Kirigami.Units.gridUnit * 13))))
                        cellHeight: Kirigami.Units.gridUnit * 11
                        reuseItems: true
                        keyNavigationEnabled: true
                        activeFocusOnTab: true

                        delegate: Item {
                            id: cardRoot
                            width: grid.cellWidth
                            height: grid.cellHeight
                            required property string title
                            required property string workshopId
                            required property string kind
                            required property string compatibility
                            required property string compatibilityDetail
                            required property url previewUrl
                            required property int diagnosticCount
                            required property string workshopState
                            required property int workshopProgress
                            required property bool favorite
                            required property var tags
                            required property url entryUrl
                            required property string diagnosticSummary
                            required property var requestedPermissions

                            Controls.AbstractButton {
                                id: card
                                anchors.fill: parent
                                anchors.margins: Kirigami.Units.smallSpacing
                                hoverEnabled: true
                                activeFocusOnTab: true
                                Accessible.name: qsTr("%1, %2 wallpaper, %3").arg(cardRoot.title, cardRoot.kind, root.compatibilityLabel(cardRoot.compatibility))
                                onClicked: root.selectWallpaper(cardRoot.title, cardRoot.workshopId, cardRoot.kind, cardRoot.compatibility, cardRoot.compatibilityDetail, cardRoot.previewUrl, cardRoot.tags, cardRoot.entryUrl, cardRoot.diagnosticSummary, cardRoot.requestedPermissions)

                                background: Rectangle {
                                    radius: Kirigami.Units.cornerRadius
                                    color: card.down || card.checked ? Kirigami.Theme.alternateBackgroundColor
                                         : card.hovered ? Kirigami.Theme.hoverColor
                                         : Kirigami.Theme.backgroundColor
                                    border.width: card.activeFocus ? 2 : 1
                                    border.color: card.activeFocus ? Kirigami.Theme.focusColor : Kirigami.Theme.separatorColor
                                }

                                contentItem: ColumnLayout {
                                    spacing: Kirigami.Units.smallSpacing

                                    Item {
                                        Layout.fillWidth: true
                                        Layout.fillHeight: true
                                        Layout.minimumHeight: Kirigami.Units.gridUnit * 6

                                        Image {
                                            id: previewImage
                                            anchors.fill: parent
                                            source: cardRoot.previewUrl
                                            fillMode: Image.PreserveAspectCrop
                                            asynchronous: true
                                            cache: true
                                            sourceSize.width: 384
                                            sourceSize.height: 216
                                            visible: status === Image.Ready
                                        }
                                        Kirigami.Icon {
                                            anchors.centerIn: parent
                                            width: Kirigami.Units.iconSizes.huge
                                            height: width
                                            source: cardRoot.kind === "video" ? "video-x-generic-symbolic"
                                                  : cardRoot.kind === "web" ? "internet-web-browser-symbolic"
                                                  : "preferences-desktop-wallpaper-symbolic"
                                            opacity: 0.55
                                            visible: previewImage.status !== Image.Ready
                                        }
                                    }

                                    Kirigami.Heading {
                                        Layout.fillWidth: true
                                        level: 4
                                        text: cardRoot.title
                                        elide: Text.ElideRight
                                        maximumLineCount: 1
                                    }
                                    RowLayout {
                                        Layout.fillWidth: true
                                        spacing: Kirigami.Units.smallSpacing
                                        Kirigami.Icon {
                                            width: Kirigami.Units.iconSizes.small
                                            height: width
                                            source: root.compatibilityIcon(cardRoot.compatibility)
                                        }
                                        Controls.Label {
                                            Layout.fillWidth: true
                                            text: root.compatibilityLabel(cardRoot.compatibility)
                                            elide: Text.ElideRight
                                        }
                                        Controls.ToolButton {
                                            Layout.alignment: Qt.AlignRight
                                            icon.name: cardRoot.favorite ? "starred-symbolic" : "non-starred-symbolic"
                                            Accessible.name: cardRoot.favorite ? qsTr("Remove from favorites") : qsTr("Add to favorites")
                                            Controls.ToolTip.visible: hovered
                                            Controls.ToolTip.text: Accessible.name
                                            onClicked: {
                                                catalogStats.toggleFavorite(cardRoot.workshopId)
                                            }
                                        }
                                        Controls.Label {
                                            visible: cardRoot.diagnosticCount > 0
                                            text: qsTr("%1 issue(s)").arg(cardRoot.diagnosticCount)
                                            Accessible.name: text
                                        }
                                        Controls.Label {
                                            visible: cardRoot.workshopState === "subscribed_missing"
                                            text: qsTr("Awaiting Steam download")
                                            color: Kirigami.Theme.neutralTextColor
                                        }
                                        Controls.Label {
                                            visible: cardRoot.workshopState === "downloading"
                                            text: cardRoot.workshopProgress >= 0
                                                ? qsTr("Downloading · %1%").arg(cardRoot.workshopProgress)
                                                : qsTr("Downloading")
                                            color: Kirigami.Theme.neutralTextColor
                                        }
                                    }
                                }
                            }
                        }

                        Kirigami.PlaceholderMessage {
                            anchors.centerIn: parent
                            width: Math.min(parent.width - Kirigami.Units.gridUnit * 2, Kirigami.Units.gridUnit * 28)
                            visible: catalogClient.state === catalogClient.Loading || (catalogClient.state === catalogClient.Ready && wallpaperModel.count === 0)
                            text: catalogClient.state === catalogClient.Loading ? qsTr("Scanning installed wallpapers…") : qsTr("No matching wallpapers")
                            explanation: catalogClient.state === catalogClient.Loading ? qsTr("Workshop metadata is parsed by the isolated service.") : qsTr("Change the search or type filter, or ask Steam to install Workshop items.")
                            icon.name: catalogClient.state === catalogClient.Loading ? "view-refresh-symbolic" : "edit-find-symbolic"
                        }
                    }
                }

                Kirigami.ScrollablePage {
                    Controls.SplitView.preferredWidth: Kirigami.Units.gridUnit * 20
                    Controls.SplitView.minimumWidth: Kirigami.Units.gridUnit * 16
                    visible: root.width >= Kirigami.Units.gridUnit * 48
                    title: qsTr("Wallpaper Details")

                    ColumnLayout {
                        width: parent.width
                        spacing: Kirigami.Units.largeSpacing

                        Kirigami.PlaceholderMessage {
                            Layout.fillWidth: true
                            visible: root.selectedId === ""
                            text: qsTr("Select a wallpaper")
                            explanation: qsTr("Compatibility and diagnostic details appear here before Apply is enabled in a future alpha.")
                            icon.name: "preferences-desktop-wallpaper-symbolic"
                        }

                        Image {
                            Layout.fillWidth: true
                            Layout.preferredHeight: width * 0.5625
                            visible: root.selectedId !== "" && status === Image.Ready
                            source: root.selectedPreview
                            fillMode: Image.PreserveAspectFit
                            asynchronous: true
                        }
                        Kirigami.Heading {
                            Layout.fillWidth: true
                            visible: root.selectedId !== ""
                            text: root.selectedTitle
                            level: 2
                            wrapMode: Text.Wrap
                        }
                        Controls.Label {
                            Layout.fillWidth: true
                            visible: root.selectedId !== ""
                            text: qsTr("Workshop %1 · %2").arg(root.selectedId, root.selectedKind)
                            opacity: 0.75
                            wrapMode: Text.Wrap
                        }
                        Flow {
                            Layout.fillWidth: true
                            visible: root.selectedId !== "" && root.selectedPermissions.length > 0
                            spacing: Kirigami.Units.smallSpacing
                            Repeater {
                                model: root.selectedPermissions
                                delegate: Controls.Button {
                                    required property string modelData
                                    text: catalogStats.isPermissionGranted(root.selectedId, modelData)
                                        ? qsTr("%1 granted").arg(modelData)
                                        : qsTr("Grant %1").arg(modelData)
                                    icon.name: catalogStats.isPermissionGranted(root.selectedId, modelData)
                                        ? "dialog-ok-apply-symbolic" : "dialog-cancel-symbolic"
                                    onClicked: catalogStats.togglePermission(root.selectedId, modelData)
                                }
                            }
                        }
                        Controls.Label {
                            Layout.fillWidth: true
                            visible: root.selectedId !== "" && root.selectedPermissions.length > 0
                            text: qsTr("Requested permissions: %1 (not granted in this alpha)").arg(root.selectedPermissions.join(", "))
                            color: Kirigami.Theme.neutralTextColor
                            wrapMode: Text.Wrap
                        }
                        Controls.Label {
                            Layout.fillWidth: true
                            visible: root.selectedId !== "" && root.selectedTags.length > 0
                            text: qsTr("Tags: %1").arg(root.selectedTags.join(", "))
                            opacity: 0.75
                            wrapMode: Text.Wrap
                        }
                        Kirigami.InlineMessage {
                            Layout.fillWidth: true
                            visible: root.selectedId !== ""
                            type: root.selectedCompatibility === "invalid" || root.selectedCompatibility === "unsupported"
                                ? Kirigami.MessageType.Warning : Kirigami.MessageType.Information
                            text: qsTr("%1: %2").arg(root.compatibilityLabel(root.selectedCompatibility), root.selectedDetail)
                        }
                        Controls.Label {
                            Layout.fillWidth: true
                            visible: root.selectedId !== "" && root.selectedDiagnosticSummary !== ""
                            text: root.selectedDiagnosticSummary
                            color: Kirigami.Theme.negativeTextColor
                            wrapMode: Text.Wrap
                        }
                        Controls.Button {
                            Layout.fillWidth: true
                            visible: root.selectedId !== ""
                            enabled: false
                            text: qsTr("Apply (available after safe bridge)")
                            icon.name: "dialog-ok-apply-symbolic"
                            Accessible.description: qsTr("Disabled in Alpha 0.1 to protect the Plasma desktop")
                            Controls.ToolTip.visible: hovered
                            Controls.ToolTip.text: Accessible.description
                        }
                        Controls.Button {
                            Layout.fillWidth: true
                            visible: root.selectedId !== "" && playlistSelector.currentText !== ""
                            text: qsTr("Add to %1").arg(playlistSelector.currentText)
                            icon.name: "list-add-symbolic"
                            onClicked: playlistController.add(playlistSelector.currentText, root.selectedId)
                        }
                        Controls.Button {
                            Layout.fillWidth: true
                            visible: root.selectedId !== "" && root.selectedKind === "video"
                            enabled: root.selectedEntry !== ""
                            text: videoPreview.running ? qsTr("Video preview running") : qsTr("Preview video")
                            icon.name: videoPreview.running ? "media-playback-stop-symbolic" : "media-playback-start-symbolic"
                            onClicked: videoPreview.running ? videoPreview.stop() : videoPreview.play(root.selectedEntry)
                        }
                        Controls.Button {
                            Layout.fillWidth: true
                            visible: root.selectedId !== "" && root.selectedKind === "web"
                            enabled: root.selectedEntry !== ""
                            text: webPreview.running ? qsTr("Web preview running") : qsTr("Preview web wallpaper")
                            icon.name: webPreview.running ? "media-playback-stop-symbolic" : "internet-web-browser-symbolic"
                            onClicked: webPreview.running ? webPreview.stop() : webPreview.play(root.selectedEntry)
                        }
                        Controls.Button {
                            Layout.fillWidth: true
                            visible: root.selectedId !== ""
                            text: qsTr("Open in Steam Workshop")
                            icon.name: "internet-services-symbolic"
                            onClicked: workshopClient.openItem(root.selectedId)
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
            }
        }
    }

    Controls.Dialog {
        id: createPlaylistDialog
        modal: true
        title: qsTr("Create playlist")
        visible: root.createPlaylistOpen
        standardButtons: Controls.Dialog.Ok | Controls.Dialog.Cancel
        onAccepted: {
            playlistController.create(nameField.text)
            nameField.clear()
            root.createPlaylistOpen = false
        }
        onRejected: root.createPlaylistOpen = false
        contentItem: Controls.TextField {
            id: nameField
            placeholderText: qsTr("Playlist name")
            Accessible.name: qsTr("New playlist name")
        }
    }
}
