# SPDX-License-Identifier: GPL-3.0-or-later
# macOS desktop display agent: one desktop-level window per screen showing
# the daemon's validated frames through the shared org.kde.kwe.display
# module. On Linux it builds as a windowed development harness (the
# platform shim is a no-op) so the C++/QML can be compiled and driven
# against a Linux daemon.
qt_add_executable(kwe-display-macos
    src/main.cpp
    src/desktopsurface.cpp
    src/desktopsurface.h
    src/outputswatcher.cpp
    src/outputswatcher.h
    src/platform.h
)
if(APPLE)
    enable_language(OBJCXX)
    set(CMAKE_OBJCXX_STANDARD 20)
    target_sources(kwe-display-macos PRIVATE src/platform_mac.mm)
    # ARC: the activity token and event-monitor objects live in globals and
    # must be retained (MRC would drop the autoreleased token at the next
    # pool drain and silently re-enable App Nap).
    set_source_files_properties(src/platform_mac.mm PROPERTIES COMPILE_OPTIONS "-fobjc-arc")
    target_link_libraries(kwe-display-macos PRIVATE
        "-framework AppKit"
        "-framework CoreGraphics"
        "-framework Foundation"
    )
else()
    target_sources(kwe-display-macos PRIVATE src/platform_stub.cpp)
endif()

qt_add_qml_module(kwe-display-macos
    URI org.kde.kwe.displaymacos
    VERSION 1.0
    NO_IMPORT_SCAN
    IMPORT_PATH
        "${CMAKE_BINARY_DIR}/qml"
    QML_FILES
        qml/Desktop.qml
)

target_link_libraries(kwe-display-macos PRIVATE
    kwe_display
    Qt6::Core
    Qt6::Gui
    Qt6::Network
    Qt6::Qml
    Qt6::Quick
)

install(TARGETS kwe-display-macos RUNTIME DESTINATION bin)
