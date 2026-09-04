// SPDX-License-Identifier: GPL-3.0-or-later
pragma Singleton
import QtQuick

// System palette mapped onto the Kirigami.Theme color names the manager
// reads. Semantic colors follow Breeze's defaults.
QtObject {
    readonly property SystemPalette palette: SystemPalette { colorGroup: SystemPalette.Active }
    readonly property color textColor: palette.text
    readonly property color disabledTextColor: palette.placeholderText
    readonly property color backgroundColor: palette.base
    readonly property color alternateBackgroundColor: palette.alternateBase
    readonly property color highlightColor: palette.highlight
    readonly property color highlightedTextColor: palette.highlightedText
    readonly property color hoverColor: Qt.rgba(palette.highlight.r, palette.highlight.g, palette.highlight.b, 0.35)
    readonly property color focusColor: palette.highlight
    readonly property color linkColor: "#2980b9"
    readonly property color positiveTextColor: "#27ae60"
    readonly property color neutralTextColor: "#f67400"
    readonly property color negativeTextColor: "#da4453"
    readonly property color positiveBackgroundColor: "#27ae60"
    readonly property color neutralBackgroundColor: "#f67400"
    readonly property color negativeBackgroundColor: "#da4453"
    readonly property color activeBackgroundColor: palette.highlight
    readonly property font defaultFont: Qt.application.font
    readonly property font smallFont: Qt.font({ pointSize: Math.max(8, Qt.application.font.pointSize - 2) })
}
