# SPDX-License-Identifier: GPL-3.0-or-later
# A minimal stand-in for the subset of org.kde.kirigami the manager's QML
# uses, built only where KF6 Kirigami is not available (macOS fork, gate
# G4). The manager's QML files stay byte-identical to upstream; only this
# module and the QQC2 style differ. Types: ApplicationWindow (pageStack,
# footer), Page, ScrollablePage, NavigationTabBar, Action, InlineMessage,
# MessageType, Units, Theme, Heading, PlaceholderMessage, Icon, SearchField.
# pragma Singleton files must be registered as singletons in the qmldir.
set_source_files_properties(Units.qml Theme.qml PROPERTIES QT_QML_SINGLETON_TYPE TRUE)

qt_add_qml_module(kwe_kirigami_shim
    URI org.kde.kirigami
    VERSION 6.0
    OUTPUT_DIRECTORY "${CMAKE_BINARY_DIR}/qml/org/kde/kirigami"
    NO_IMPORT_SCAN
    QML_FILES
        Action.qml
        ApplicationWindow.qml
        Heading.qml
        Icon.qml
        InlineMessage.qml
        MessageType.qml
        NavigationTabBar.qml
        Page.qml
        PlaceholderMessage.qml
        ScrollablePage.qml
        SearchField.qml
        Theme.qml
        Units.qml
)
target_link_libraries(kwe_kirigami_shim PRIVATE Qt6::Core Qt6::Gui Qt6::Qml Qt6::Quick Qt6::QuickControls2)

qt_query_qml_module(kwe_kirigami_shim
    PLUGIN_TARGET kwe_kirigami_shim_plugin
    TARGET_PATH kwe_kirigami_shim_target_path
    QMLDIR kwe_kirigami_shim_qmldir
    TYPEINFO kwe_kirigami_shim_typeinfo
)
install(TARGETS kwe_kirigami_shim LIBRARY DESTINATION "${CMAKE_INSTALL_LIBDIR}")
install(TARGETS "${kwe_kirigami_shim_plugin}"
    LIBRARY DESTINATION "${QT6_INSTALL_QML}/${kwe_kirigami_shim_target_path}")
install(FILES "${kwe_kirigami_shim_qmldir}" "${kwe_kirigami_shim_typeinfo}"
    DESTINATION "${QT6_INSTALL_QML}/${kwe_kirigami_shim_target_path}")
