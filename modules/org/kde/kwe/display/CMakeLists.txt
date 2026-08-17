# SPDX-License-Identifier: Apache-2.0
qt_add_qml_module(kwe_display
    URI org.kde.kwe.display
    VERSION 1.0
    OUTPUT_DIRECTORY "${CMAKE_BINARY_DIR}/qml/org/kde/kwe/display"
    SOURCES
        displaysession.cpp
        displaysession.h
        frameitem.cpp
        frameitem.h
        inputclient.cpp
        inputclient.h
    DEPENDENCIES
        QtQuick
)

target_link_libraries(kwe_display PRIVATE
    Qt6::Core
    Qt6::Gui
    Qt6::Network
    Qt6::Qml
    Qt6::Quick
)
target_include_directories(kwe_display PUBLIC "${CMAKE_CURRENT_SOURCE_DIR}")

qt_query_qml_module(kwe_display
    PLUGIN_TARGET kwe_display_plugin
    TARGET_PATH kwe_display_target_path
    QMLDIR kwe_display_qmldir
    TYPEINFO kwe_display_typeinfo
)

install(TARGETS kwe_display
    LIBRARY DESTINATION "${CMAKE_INSTALL_LIBDIR}"
)
install(TARGETS "${kwe_display_plugin}"
    LIBRARY DESTINATION "${QT6_INSTALL_QML}/${kwe_display_target_path}"
)
install(FILES
    "${kwe_display_qmldir}"
    "${kwe_display_typeinfo}"
    DESTINATION "${QT6_INSTALL_QML}/${kwe_display_target_path}"
)

if(BUILD_TESTING)
    add_subdirectory(tests)
endif()
