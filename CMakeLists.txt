# SPDX-License-Identifier: GPL-3.0-or-later
cmake_minimum_required(VERSION 3.28)
project(KDEWallpaperEngine VERSION 0.1.0 LANGUAGES CXX)

include(CTest)
include(GNUInstallDirs)

set(CMAKE_CXX_STANDARD 20)
set(CMAKE_CXX_STANDARD_REQUIRED ON)
set(CMAKE_CXX_EXTENSIONS OFF)
set(CMAKE_AUTOMOC ON)

# Linux/Plasma: the Kirigami manager, the frame preview, and the Plasma
# wallpaper package (upstream layout). macOS fork: the desktop display
# agent instead (docs/macos/MacOS-Port-Plan.md, MP-4). Both share the
# org.kde.kwe.display QML module.
if(APPLE)
    set(_kwe_default_manager OFF)
    set(_kwe_default_display_agent ON)
else()
    set(_kwe_default_manager ON)
    set(_kwe_default_display_agent OFF)
endif()
option(KWE_BUILD_MANAGER "Build the Kirigami manager, frame preview, and Plasma package" ${_kwe_default_manager})
option(KWE_BUILD_DISPLAY_AGENT "Build the macOS desktop display agent (Linux: windowed dev build)" ${_kwe_default_display_agent})

if(KWE_BUILD_MANAGER OR KWE_BUILD_DISPLAY_AGENT)
    find_package(Qt6 6.6 REQUIRED COMPONENTS Core Gui Network Qml Quick)
    if(KWE_BUILD_MANAGER)
        find_package(Qt6 6.6 REQUIRED COMPONENTS QuickControls2)
        find_package(KF6Kirigami REQUIRED)
    endif()
    if(BUILD_TESTING)
        find_package(Qt6 6.6 REQUIRED COMPONENTS Test)
    endif()
    qt_standard_project_setup(REQUIRES 6.6)
    qt_policy(SET QTP0004 NEW)
    add_subdirectory(modules/org/kde/kwe/display)
endif()
if(KWE_BUILD_MANAGER)
    add_subdirectory(apps/kwe-frame-preview)
    add_subdirectory(apps/kwe-manager)
    add_subdirectory(plasma/wallpapers/org.kde.kwe.wallpaper)
endif()
if(KWE_BUILD_DISPLAY_AGENT)
    add_subdirectory(apps/kwe-display-macos)
endif()
