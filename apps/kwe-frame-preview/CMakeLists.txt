# SPDX-License-Identifier: GPL-3.0-or-later
qt_add_executable(kwe-frame-preview
    src/main.cpp
)

qt_add_qml_module(kwe-frame-preview
    URI org.kde.kwe.framepreview
    VERSION 1.0
    NO_IMPORT_SCAN
    IMPORT_PATH
        "${CMAKE_BINARY_DIR}/qml"
    QML_FILES
        qml/Preview.qml
)

target_link_libraries(kwe-frame-preview PRIVATE
    KF6Kirigami
    kwe_display
    Qt6::Core
    Qt6::Gui
    Qt6::Network
    Qt6::Qml
    Qt6::Quick
    Qt6::QuickControls2
)

install(TARGETS kwe-frame-preview RUNTIME DESTINATION bin)
