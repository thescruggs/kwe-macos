// SPDX-License-Identifier: Apache-2.0
import QtQuick
import QtQuick.Controls as Controls
import QtQuick.Layouts
import org.kde.kirigami as Kirigami
import org.kde.kwe.display 1.0

Kirigami.ApplicationWindow {
    id: root

    required property string framePathOption
    required property string daemonSocketOption
    required property double displayGenerationOption
    required property bool followDaemonOption

    width: 1050
    height: 680
    minimumWidth: 640
    minimumHeight: 420
    visible: true
    title: qsTr("Safe Frame Transport Preview")

    DisplaySession {
        id: displaySession

        socketPath: root.followDaemonOption ? root.daemonSocketOption : ""
    }

    InputClient {
        id: inputClient

        objectName: "inputClient"
        socketPath: root.daemonSocketOption
        displayGeneration: root.followDaemonOption ? displaySession.displayGeneration : root.displayGenerationOption
    }

    pageStack.initialPage: Kirigami.Page {
        title: qsTr("Generated Test Pattern")
        padding: 0

        ColumnLayout {
            anchors.fill: parent
            spacing: 0

            Kirigami.InlineMessage {
                Layout.fillWidth: true
                visible: frame.status === FrameSurface.Frozen || frame.status === FrameSurface.Invalid || frame.status === FrameSurface.Stopped
                type: frame.status === FrameSurface.Invalid ? Kirigami.MessageType.Error : Kirigami.MessageType.Warning
                text: frame.errorMessage.length > 0 ? frame.errorMessage : frame.statusText
            }

            Kirigami.InlineMessage {
                Layout.fillWidth: true
                visible: inputClient.errorMessage.length > 0
                type: Kirigami.MessageType.Warning
                text: inputClient.errorMessage
            }

            FrameSurface {
                id: frame

                objectName: "frameSurface"
                Layout.fillWidth: true
                Layout.fillHeight: true
                Accessible.name: qsTr("Generated renderer test pattern")
                Accessible.description: qsTr("A validated frame copied from an external process")
                frameFile: root.followDaemonOption ? displaySession.frameFile : root.framePathOption
                onFrameFileOpened: (path) => {
                    displaySession.acknowledgeFrameFile(path);
                }
                onPointerPosition: (phase, x, y) => {
                    inputClient.sendPointer(phase, x, y);
                }
            }

            Controls.Label {
                Layout.fillWidth: true
                Layout.margins: Kirigami.Units.largeSpacing
                text: inputClient.enabled ? qsTr("Pointer position is forwarded passively. Mouse buttons, touch, right-click, and long-press remain reserved for Plasma.") : qsTr("The producer is outside Plasma. If it exits, hangs, or corrupts its header, this viewer keeps the last validated frame.")
                wrapMode: Text.Wrap
                horizontalAlignment: Text.AlignHCenter
                opacity: 0.8
            }

        }

        header: Controls.ToolBar {

            contentItem: RowLayout {
                Kirigami.Icon {
                    source: frame.status === FrameSurface.Live ? "emblem-checked-symbolic" : frame.status === FrameSurface.Waiting ? "view-refresh-symbolic" : "data-warning-symbolic"
                    Layout.preferredWidth: Kirigami.Units.iconSizes.smallMedium
                    Layout.preferredHeight: width
                }

                Controls.Label {
                    text: frame.statusText
                    Accessible.name: text
                }

                Kirigami.Separator {
                    Layout.fillHeight: true
                }

                Kirigami.Icon {
                    source: inputClient.enabled ? "input-mouse-symbolic" : "input-mouse-click-left-symbolic"
                    Layout.preferredWidth: Kirigami.Units.iconSizes.smallMedium
                    Layout.preferredHeight: width
                    opacity: inputClient.enabled ? 1 : 0.55
                }

                Controls.Label {
                    text: inputClient.stateText
                    Accessible.name: text
                    opacity: inputClient.enabled ? 1 : 0.7
                }

                Item {
                    Layout.fillWidth: true
                }

                Controls.Label {
                    text: qsTr("Frame %1 · %2×%3").arg(frame.sequence).arg(frame.frameSize.width).arg(frame.frameSize.height)
                    opacity: 0.75
                }

            }

        }

    }

}
