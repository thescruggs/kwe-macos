// SPDX-License-Identifier: Apache-2.0
pragma Singleton

import QtQml

// Shared selection state for the gallery and detail panes so the Installed
// and Workshop views present the identical details page.
QtObject {
    property string selectedTitle: ""
    property string selectedId: ""
    property string selectedKind: ""
    property string selectedCompatibility: ""
    property string selectedDetail: ""
    property url selectedPreview: ""
    property var selectedTags: []
    property url selectedEntry: ""
    property url selectedContentRoot: ""
    property string selectedDiagnosticSummary: ""
    property var selectedPermissions: []

    function select(title, id, kind, compatibility, detail, preview, tags, entry, contentRoot, diagnosticSummary, permissions) {
        selectedTitle = title
        selectedId = id
        selectedKind = kind
        selectedCompatibility = compatibility
        selectedDetail = detail
        selectedPreview = preview
        selectedTags = tags
        selectedEntry = entry
        selectedContentRoot = contentRoot
        selectedDiagnosticSummary = diagnosticSummary
        selectedPermissions = permissions
    }

    function compatibilityLabel(value) {
        switch (value) {
        case "renderer_dependent": return qsTr("Renderer-dependent")
        case "backend_missing": return qsTr("Backend planned")
        case "unsupported": return qsTr("Unsupported")
        case "invalid": return qsTr("Needs attention")
        default: return value
        }
    }

    function compatibilityIcon(value) {
        return value === "renderer_dependent" ? "dialog-information-symbolic"
             : value === "backend_missing" ? "tools-wizard-symbolic"
             : "data-warning-symbolic"
    }
}
