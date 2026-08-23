// SPDX-License-Identifier: GPL-3.0-or-later
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

    property bool createPlaylistOpen: false

    pageStack.initialPage: installedPage

    Kirigami.Page {
        id: installedPage
        visible: false
        title: qsTr("Installed Wallpapers")
        contentItem: GalleryPage {
            anchors.fill: parent
            filterModel: wallpaperModel
            workshopView: false
            emptyExplanation: qsTr("Change the search or type filter, or ask Steam to install Workshop items.")
            onCreatePlaylistRequested: root.createPlaylistOpen = true
        }
    }

    Kirigami.Page {
        id: workshopPage
        visible: false
        title: qsTr("Workshop")
        contentItem: GalleryPage {
            anchors.fill: parent
            filterModel: workshopModel
            workshopView: true
            emptyExplanation: qsTr("No subscribed Workshop items are installed or downloading. Subscribe in Steam, then rescan here to see download progress.")
            onCreatePlaylistRequested: root.createPlaylistOpen = true
        }
    }

    footer: Kirigami.NavigationTabBar {
        actions: [
            Kirigami.Action {
                icon.name: "preferences-desktop-wallpaper-symbolic"
                text: qsTr("Installed")
                checked: installedPage.visible
                onTriggered: {
                    if (!installedPage.visible) {
                        while (pageStack.depth > 0)
                            pageStack.pop()
                        pageStack.push(installedPage)
                    }
                }
            },
            Kirigami.Action {
                icon.name: "folder-download-symbolic"
                text: qsTr("Workshop")
                checked: workshopPage.visible
                onTriggered: {
                    if (!workshopPage.visible) {
                        while (pageStack.depth > 0)
                            pageStack.pop()
                        pageStack.push(workshopPage)
                    }
                }
            }
        ]
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
