// SPDX-License-Identifier: GPL-3.0-or-later
import QtQuick
import QtQuick.Controls as Controls

// Kirigami.ApplicationWindow subset: `pageStack` is a QQC2 StackView
// with Kirigami's `initialPage` convenience. Grouped assignments such as
// `pageStack.initialPage:` resolve against the declared type, hence the
// inline component.
Controls.ApplicationWindow {
    id: root

    component PageStack: Controls.StackView {
        property var initialPage: null
        onInitialPageChanged: {
            if (initialPage && depth === 0)
                push(initialPage);
        }
        // Kirigami pages declare `visible: false` while parked and bind
        // their tab's `checked` to that visibility, so the outgoing page
        // must be hidden again explicitly (StackView alone leaves it).
        property Item previousItem: null
        onCurrentItemChanged: {
            if (previousItem && previousItem !== currentItem)
                previousItem.visible = false;
            if (currentItem)
                currentItem.visible = true;
            previousItem = currentItem;
        }
        pushEnter: Transition {}
        pushExit: Transition {}
        popEnter: Transition {}
        popExit: Transition {}
    }

    readonly property PageStack pageStack: PageStack {
        parent: root.contentItem
        anchors.fill: parent
    }
    property var globalDrawer: null
    property var contextDrawer: null

    color: palette.window
}
