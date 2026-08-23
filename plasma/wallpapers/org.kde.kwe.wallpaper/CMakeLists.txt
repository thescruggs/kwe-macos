# SPDX-License-Identifier: GPL-3.0-or-later
install(
    DIRECTORY contents
    DESTINATION "${CMAKE_INSTALL_DATADIR}/plasma/wallpapers/org.kde.kwe.wallpaper"
)
install(
    FILES metadata.json
    DESTINATION "${CMAKE_INSTALL_DATADIR}/plasma/wallpapers/org.kde.kwe.wallpaper"
)
