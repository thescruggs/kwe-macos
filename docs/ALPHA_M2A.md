# Alpha M2a — Steam library discovery

M2a hardens the local catalog's Steam discovery boundary. The scanner now has
coverage for the primary Steam root plus additional paths from
`steamapps/libraryfolders.vdf`, canonicalizes duplicate paths, and reports
whether Wallpaper Engine and its Workshop content are available in each
library.

Discovery remains read-only and bounded. Invalid or unreadable manifests become
diagnostics instead of aborting the catalog scan. This keeps removable drives
and partially mounted Steam libraries visible without taking down the daemon.
