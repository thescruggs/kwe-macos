// SPDX-License-Identifier: GPL-3.0-or-later
import QtQuick
import org.kde.kwe.display 1.0

Item {
    width: 1
    height: 1

    DisplaySession {
        socketPath: ""
    }

    FrameSurface {
        width: 1
        height: 1
    }

    InputClient {
        socketPath: ""
        displayGeneration: 0
    }

    Timer {
        interval: 50
        running: true
        onTriggered: Qt.quit()
    }
}
