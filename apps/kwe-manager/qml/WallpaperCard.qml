// SPDX-License-Identifier: Apache-2.0
import QtQuick
import QtQuick.Controls as Controls
import QtQuick.Layouts
import org.kde.kirigami as Kirigami

// Gallery card shared by the Installed and Workshop views. Consumes the
// CatalogModel roles directly; selection goes through WallpaperSelection so
// both views present the same details pane.
Item {
    id: cardRoot

    required property real cellWidth
    required property real cellHeight
    required property bool workshopView

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

    width: cardRoot.cellWidth
    height: cardRoot.cellHeight

    Controls.AbstractButton {
        id: card
        anchors.fill: parent
        anchors.margins: Kirigami.Units.smallSpacing
        hoverEnabled: true
        activeFocusOnTab: true
        Accessible.name: qsTr("%1, %2 wallpaper, %3").arg(cardRoot.title, cardRoot.kind, WallpaperSelection.compatibilityLabel(cardRoot.compatibility))
        onClicked: WallpaperSelection.select(cardRoot.title, cardRoot.workshopId, cardRoot.kind, cardRoot.compatibility, cardRoot.compatibilityDetail, cardRoot.previewUrl, cardRoot.tags, cardRoot.entryUrl, cardRoot.diagnosticSummary, cardRoot.requestedPermissions)

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
                    source: WallpaperSelection.compatibilityIcon(cardRoot.compatibility)
                }
                Controls.Label {
                    Layout.fillWidth: true
                    text: WallpaperSelection.compatibilityLabel(cardRoot.compatibility)
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
                Controls.Label {
                    visible: cardRoot.workshopView && cardRoot.workshopState === "subscribed_installed"
                    text: qsTr("Installed")
                    color: Kirigami.Theme.positiveTextColor
                }
            }
        }
    }
}
