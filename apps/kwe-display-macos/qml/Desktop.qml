// SPDX-License-Identifier: GPL-3.0-or-later
import QtQuick
import org.kde.kwe.display 1.0

// The per-screen desktop surface: the same DisplaySession + FrameSurface +
// InputClient triple as the Plasma wallpaper package, without Kirigami.
// The window is black until the first validated frame; after a renderer
// loss the FrameSurface keeps its private last-good frame.
Item {
    id: root

    property string socketPath: ""

    // Called from C++ with global-monitor pointer positions on macOS
    // (desktop windows never receive hover events themselves).
    function forwardPointer(phase, x, y) {
        if (displaySession.active)
            inputClient.sendPointer(phase, x, y);
    }

    DisplaySession {
        id: displaySession
        socketPath: root.socketPath
    }

    InputClient {
        id: inputClient
        socketPath: root.socketPath
        displayGeneration: displaySession.active ? displaySession.displayGeneration : 0
    }

    Rectangle {
        anchors.fill: parent
        color: "black"
    }

    FrameSurface {
        id: frame
        objectName: "frameSurface"
        anchors.fill: parent
        frameFile: displaySession.frameFile
        scaling: displaySession.scaling
        Accessible.name: qsTr("Animated wallpaper")
        onFrameFileOpened: (path) => displaySession.acknowledgeFrameFile(path)
        // Linux development windows still get hover events; macOS desktop
        // windows do not, and use forwardPointer instead.
        onPointerPosition: (phase, x, y) => root.forwardPointer(phase, x, y)
    }
}
