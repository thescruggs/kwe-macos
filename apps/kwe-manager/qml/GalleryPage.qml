// SPDX-License-Identifier: Apache-2.0
import QtQuick
import QtQuick.Controls as Controls
import QtQuick.Layouts
import QtQuick.Window
import org.kde.kirigami as Kirigami

// One gallery destination: the Installed view shows every indexed item, the
// Workshop view shows only subscribed items (installed, downloading, or
// awaiting download). Both share the card and details components.
Kirigami.Page {
    id: galleryPage

    required property var filterModel
    required property bool workshopView
    required property string emptyExplanation

    signal createPlaylistRequested()

    padding: 0

    header: Controls.ToolBar {
        contentItem: RowLayout {
            spacing: Kirigami.Units.smallSpacing

            Kirigami.SearchField {
                Layout.fillWidth: true
                Layout.maximumWidth: Kirigami.Units.gridUnit * 28
                placeholderText: qsTr("Search title or Workshop ID…")
                Accessible.name: qsTr("Search wallpapers")
                onTextChanged: galleryPage.filterModel.searchText = text
            }

            Controls.ComboBox {
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
                onCurrentValueChanged: galleryPage.filterModel.kindFilter = currentValue
            }

            Controls.ComboBox {
                textRole: "label"
                valueRole: "value"
                Accessible.name: qsTr("Sort wallpapers")
                model: [
                    { label: qsTr("Title"), value: "title" },
                    { label: qsTr("Type"), value: "kind" }
                ]
                onCurrentValueChanged: galleryPage.filterModel.sortMode = currentValue
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
                onClicked: galleryPage.createPlaylistRequested()
            }
            Controls.Label {
                visible: playlistSelector.currentText !== ""
                text: qsTr("%1 items").arg(playlistController.entries(playlistSelector.currentText).length)
                opacity: 0.75
            }

            Controls.ToolButton {
                text: galleryPage.filterModel.favoritesOnly ? qsTr("All") : qsTr("Favorites")
                icon.name: galleryPage.filterModel.favoritesOnly ? "view-list-symbolic" : "starred-symbolic"
                display: Controls.AbstractButton.TextBesideIcon
                Accessible.name: galleryPage.filterModel.favoritesOnly ? qsTr("Show all wallpapers") : qsTr("Show favorite wallpapers")
                onClicked: galleryPage.filterModel.favoritesOnly = !galleryPage.filterModel.favoritesOnly
            }

            Controls.ToolButton {
                text: qsTr("Rescan")
                icon.name: "view-refresh-symbolic"
                display: Controls.AbstractButton.TextBesideIcon
                enabled: catalogClient.state !== CatalogClient.Loading
                Accessible.name: qsTr("Rescan Steam Workshop folders")
                onClicked: catalogClient.rescan()
            }

            Controls.ToolButton {
                readonly property bool inSafeMode: packageInstaller.state === PackageInstaller.SafeMode
                text: inSafeMode
                    ? qsTr("Leave safe mode") : qsTr("Safe mode")
                icon.name: inSafeMode
                    ? "dialog-ok-apply-symbolic" : "security-high-symbolic"
                display: Controls.AbstractButton.TextBesideIcon
                enabled: (packageInstaller.state === PackageInstaller.Installed
                        && packageInstaller.userPackagePresent)
                    || inSafeMode
                Accessible.name: text
                onClicked: inSafeMode
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
            // The activator's Failed banner is authoritative for a
            // daemon-down startup; showing both would stack two actions for
            // the same root cause.
            visible: catalogClient.state === CatalogClient.Error
                && daemonActivator.state !== DaemonActivator.Failed
            text: catalogClient.errorMessage
            actions: [
                Kirigami.Action {
                    text: qsTr("Try Again")
                    icon.name: "view-refresh-symbolic"
                    onTriggered: catalogClient.refresh()
                },
                Kirigami.Action {
                    text: qsTr("Start service")
                    icon.name: "system-run-symbolic"
                    // Belt-and-braces: whenever the daemon is not known to
                    // be up, the catalog error offers the activation action
                    // too, even if the activator never got a chance to run.
                    visible: daemonActivator.state !== DaemonActivator.Running
                    onTriggered: daemonActivator.activate()
                }
            ]
        }

        // The manager starts the daemon user service itself when its socket
        // is absent; these two messages cover the activation in progress and
        // the actionable failure state once the bounded attempts run out.
        Kirigami.InlineMessage {
            Layout.fillWidth: true
            type: Kirigami.MessageType.Information
            visible: daemonActivator.state === DaemonActivator.Activating
            text: daemonActivator.message
        }

        Kirigami.InlineMessage {
            Layout.fillWidth: true
            type: Kirigami.MessageType.Error
            visible: daemonActivator.state === DaemonActivator.Failed
            text: daemonActivator.message
            actions: [
                Kirigami.Action {
                    text: qsTr("Start service")
                    icon.name: "system-run-symbolic"
                    onTriggered: daemonActivator.activate()
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
                    // Single-wallpaper display assignment exists since BETA_M4b
                    // (the details pane Apply flow); playlist-to-display
                    // assignment is a later milestone.
                    text: qsTr("Playlist display assignment arrives in a later milestone")
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
            text: qsTr("Renderer quarantined wallpaper %1. %2")
                .arg(rendererStatus.wallpaperId)
                .arg(rendererStatus.detail !== "" ? rendererStatus.detail : qsTr("The last-known-good frame remains active."))
        }

        Kirigami.InlineMessage {
            Layout.fillWidth: true
            type: Kirigami.MessageType.Information
            visible: catalogClient.state === CatalogClient.Ready
            // BETA_M4b: applying is no longer alpha-gated; the daemon's apply
            // transaction is what the details pane drives, and every switch
            // is reversible from the same page.
            text: qsTr("Applying wallpapers is available for video, web, and scene content. Use Restore on the wallpaper details page to switch back to the image wallpaper.")
            showCloseButton: true
        }

        Kirigami.InlineMessage {
            Layout.fillWidth: true
            visible: packageInstaller.state !== PackageInstaller.Installed
            type: packageInstaller.state === PackageInstaller.Failed
                ? Kirigami.MessageType.Error : Kirigami.MessageType.Warning
            text: packageInstaller.message
            actions: [
                Kirigami.Action {
                    text: qsTr("Install display bridge")
                    icon.name: "install-symbolic"
                    visible: packageSource !== ""
                        && packageInstaller.state !== PackageInstaller.SafeMode
                    onTriggered: packageInstaller.installFrom(packageSource)
                },
                Kirigami.Action {
                    text: qsTr("Leave safe mode")
                    icon.name: "dialog-ok-apply-symbolic"
                    visible: packageInstaller.state === PackageInstaller.SafeMode
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
                text: galleryPage.workshopView
                    ? qsTr("%1 subscribed").arg(catalogStats.subscribedCount)
                    : qsTr("%1 installed").arg(catalogStats.totalCount)
                level: 2
            }
            Controls.Label { visible: !galleryPage.workshopView; text: qsTr("%1 scenes").arg(catalogStats.sceneCount) }
            Controls.Label { visible: !galleryPage.workshopView; text: qsTr("%1 videos").arg(catalogStats.videoCount) }
            Controls.Label { visible: !galleryPage.workshopView; text: qsTr("%1 web").arg(catalogStats.webCount) }
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
                text: qsTr("Showing %1").arg(galleryPage.filterModel.count)
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
                    model: galleryPage.filterModel
                    cellWidth: Math.max(Kirigami.Units.gridUnit * 13, width / Math.max(1, Math.floor(width / (Kirigami.Units.gridUnit * 13))))
                    cellHeight: Kirigami.Units.gridUnit * 11
                    reuseItems: true
                    keyNavigationEnabled: true
                    activeFocusOnTab: true

                    delegate: WallpaperCard {
                        // The view initializes the card's required properties
                        // from the model roles directly. On Qt 6.11
                        // re-declaring them here shadows the component's own
                        // properties, leaving them unset, so every card
                        // failed to instantiate and the gallery stayed empty.
                        cellWidth: grid.cellWidth
                        cellHeight: grid.cellHeight
                        workshopView: galleryPage.workshopView
                    }

                    Kirigami.PlaceholderMessage {
                        anchors.centerIn: parent
                        width: Math.min(parent.width - Kirigami.Units.gridUnit * 2, Kirigami.Units.gridUnit * 28)
                        visible: catalogClient.state === CatalogClient.Loading || (catalogClient.state === CatalogClient.Ready && galleryPage.filterModel.count === 0)
                        text: catalogClient.state === catalogClient.Loading ? qsTr("Scanning installed wallpapers…") : qsTr("No matching wallpapers")
                        explanation: catalogClient.state === catalogClient.Loading ? qsTr("Workshop metadata is parsed by the isolated service.") : galleryPage.emptyExplanation
                        icon.name: catalogClient.state === CatalogClient.Loading ? "view-refresh-symbolic" : "edit-find-symbolic"
                    }
                }
            }

            WallpaperDetail {
                id: detailsPane
                Controls.SplitView.preferredWidth: Kirigami.Units.gridUnit * 20
                Controls.SplitView.minimumWidth: Kirigami.Units.gridUnit * 16
                // Qt 6.11 dropped the bare `window` identifier; use the
                // Window attached property (null until the page is attached).
                detailsVisible: Window.window !== null
                    && Window.window.width >= Kirigami.Units.gridUnit * 48
                activePlaylistName: playlistSelector.currentText
            }
        }
    }
}
