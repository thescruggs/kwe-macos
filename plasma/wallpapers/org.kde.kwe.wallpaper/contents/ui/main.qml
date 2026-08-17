// SPDX-License-Identifier: Apache-2.0
import QtQuick
import org.kde.kirigami as Kirigami
import org.kde.plasma.plasmoid
import org.kde.kwe.display 1.0

WallpaperItem {
    id: root

    loading: displaySession.state === DisplaySession.Connecting && !frame.hasFrame

    DisplaySession {
        id: displaySession
    }

    InputClient {
        id: inputClient

        socketPath: displaySession.socketPath
        displayGeneration: displaySession.active ? displaySession.displayGeneration : 0
    }

    FrameSurface {
        id: frame

        anchors.fill: parent
        frameFile: displaySession.frameFile
        Accessible.name: qsTr("Animated wallpaper")
        Accessible.description: qsTr("Validated frames from an isolated wallpaper renderer")
        onFrameFileOpened: (path) => {
            displaySession.acknowledgeFrameFile(path);
        }
        onPointerPosition: (phase, x, y) => {
            if (displaySession.active)
                inputClient.sendPointer(phase, x, y);

        }
    }

    Rectangle {
        anchors.fill: parent
        visible: !frame.hasFrame
        color: Kirigami.Theme.backgroundColor
    }

    Rectangle {
        id: statusPanel

        anchors.centerIn: parent
        width: Math.max(0, Math.min(parent.width - Kirigami.Units.gridUnit * 2,
                                   statusContent.implicitWidth + Kirigami.Units.gridUnit * 2))
        height: statusContent.implicitHeight + Kirigami.Units.gridUnit * 1.5
        radius: Kirigami.Units.cornerRadius
        color: Kirigami.Theme.backgroundColor
        opacity: 0.94
        visible: !frame.hasFrame
              || displaySession.state === DisplaySession.Degraded
              || frame.status === FrameSurface.Frozen
              || frame.status === FrameSurface.Invalid
              || frame.status === FrameSurface.Stopped

        Row {
            id: statusContent

            anchors.centerIn: parent
            spacing: Kirigami.Units.largeSpacing

            Kirigami.Icon {
                anchors.verticalCenter: parent.verticalCenter
                width: Kirigami.Units.iconSizes.medium
                height: width
                source: displaySession.state === DisplaySession.Degraded
                     || frame.status === FrameSurface.Invalid
                     ? "data-warning-symbolic"
                     : frame.hasFrame ? "media-playback-pause-symbolic"
                                      : "view-refresh-symbolic"
            }

            Text {
                anchors.verticalCenter: parent.verticalCenter
                width: Math.max(0, Math.min(implicitWidth,
                                           root.width - Kirigami.Units.gridUnit * 7))
                color: Kirigami.Theme.textColor
                text: displaySession.state === DisplaySession.Degraded
                   || displaySession.phase === "quarantined"
                   || displaySession.phase === "rolled_back"
                   || displaySession.phase === "stopped"
                    ? displaySession.stateText
                    : frame.status === FrameSurface.Frozen
                   || frame.status === FrameSurface.Invalid
                   || frame.status === FrameSurface.Stopped
                    ? frame.statusText : displaySession.stateText
                wrapMode: Text.Wrap
                horizontalAlignment: Text.AlignHCenter
                Accessible.role: Accessible.StaticText
                Accessible.name: text
            }
        }
    }
}
