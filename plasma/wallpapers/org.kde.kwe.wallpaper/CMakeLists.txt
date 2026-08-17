# SPDX-License-Identifier: Apache-2.0
install(
    DIRECTORY contents
    DESTINATION "${CMAKE_INSTALL_DATADIR}/plasma/wallpapers/org.kde.kwe.wallpaper"
)
install(
    FILES metadata.json
    DESTINATION "${CMAKE_INSTALL_DATADIR}/plasma/wallpapers/org.kde.kwe.wallpaper"
)
