# SPDX-License-Identifier: Apache-2.0
qt_add_executable(kwe-manager
    src/main.cpp
    src/catalogmodel.cpp
    src/catalogmodel.h
    src/daemonactivator.cpp
    src/daemonactivator.h
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
    src/playlistclient.cpp
    src/playlistclient.h
)

# WallpaperSelection is a pragma Singleton; the generated qmldir must
# register it as such, otherwise QML resolves the type object instead of the
# instance and every WallpaperSelection.* read is undefined. The property
# must be set before qt_add_qml_module, which reads it at call time.
set_source_files_properties(qml/WallpaperSelection.qml PROPERTIES
    QT_QML_SINGLETON_TYPE TRUE
)

qt_add_qml_module(kwe-manager
    URI org.kde.kwe
    VERSION 1.0
    NO_IMPORT_SCAN
    # These five clients carry QML_ELEMENT + QML_UNCREATABLE: the instances
    # are built in C++ and exposed as context properties, while the
    # registered type is what makes their Q_ENUM State values reachable as
    # CatalogClient.Ready / PackageInstaller.Installed /
    # PermissionsClient.Ready / ApplyClient.Applied (instance-based enum
    # access is gone on Qt 6.11). QML_UNCREATABLE alone registers NOTHING —
    # qmltyperegistrar skips a class that never declares itself an element,
    # and every type-based enum read then throws a ReferenceError at
    # runtime. scripts/check.sh asserts the generated qmltypes is not the
    # empty `Module {}` so this cannot regress silently again.
    SOURCES
        src/applyclient.cpp
        src/applyclient.h
        src/catalogclient.cpp
        src/catalogclient.h
        src/daemonactivator.cpp
        src/daemonactivator.h
        src/packageinstaller.cpp
        src/packageinstaller.h
        src/permissionsclient.cpp
        src/permissionsclient.h
    QML_FILES
        qml/Main.qml
        qml/GalleryPage.qml
        qml/WallpaperCard.qml
        qml/WallpaperDetail.qml
        qml/WallpaperSelection.qml
)

# The generated kwe-manager_qmltyperegistrations.cpp includes each registered
# class's header as <applyclient.h>, so src/ must be on the include path.
target_include_directories(kwe-manager PRIVATE src)

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
