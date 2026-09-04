// SPDX-License-Identifier: GPL-3.0-or-later
import QtQuick
import QtQuick.Controls as Controls

// Kirigami.Action subset: a QQC2 Action plus `visible` and `tooltip`.
Controls.Action {
    property bool visible: true
    property string tooltip: ""
    property list<QtObject> children
}
