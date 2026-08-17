# SPDX-License-Identifier: Apache-2.0
qt_add_executable(kwe-manager
    src/main.cpp
    src/catalogclient.cpp
    src/catalogclient.h
    src/catalogmodel.cpp
    src/catalogmodel.h
    src/packageinstaller.cpp
    src/packageinstaller.h
    src/workshopclient.cpp
    src/workshopclient.h
    src/videopreview.cpp
    src/videopreview.h
    src/rendererstatus.cpp
    src/rendererstatus.h
    src/webpreview.cpp
    src/webpreview.h
    src/playlistcontroller.cpp
    src/playlistcontroller.h
)

qt_add_qml_module(kwe-manager
    URI org.kde.kwe
    VERSION 1.0
    NO_IMPORT_SCAN
    QML_FILES
        qml/Main.qml
)

target_link_libraries(kwe-manager PRIVATE
    KF6Kirigami
    Qt6::Core
    Qt6::Gui
    Qt6::Network
    Qt6::Qml
    Qt6::Quick
    Qt6::QuickControls2
)

install(TARGETS kwe-manager RUNTIME DESTINATION bin)

if(BUILD_TESTING)
    add_subdirectory(tests)
endif()
