# SPDX-License-Identifier: GPL-3.0-or-later
qt_add_executable(kwe-package-installer-test
    packageinstallertest.cpp
    ../src/packageinstaller.cpp
    ../src/packageinstaller.h
)
target_link_libraries(kwe-package-installer-test PRIVATE Qt6::Core Qt6::Qml Qt6::Test)
add_test(NAME kwe-package-installer-test COMMAND kwe-package-installer-test)

qt_add_executable(kwe-workshop-client-test
    workshopclienttest.cpp
    ../src/workshopclient.cpp
    ../src/workshopclient.h
)
target_link_libraries(kwe-workshop-client-test PRIVATE Qt6::Core Qt6::Gui Qt6::Test)
add_test(NAME kwe-workshop-client-test COMMAND kwe-workshop-client-test)

qt_add_executable(kwe-daemon-activator-test
    daemonactivatortest.cpp
    ../src/daemonactivator.cpp
    ../src/daemonactivator.h
)
target_link_libraries(kwe-daemon-activator-test PRIVATE Qt6::Core Qt6::Qml Qt6::Network Qt6::Test)
add_test(NAME kwe-daemon-activator-test COMMAND kwe-daemon-activator-test)

qt_add_executable(kwe-playlist-controller-test
    playlistcontrollertest.cpp
    ../src/playlistcontroller.cpp
    ../src/playlistcontroller.h
    ../src/playlistclient.cpp
    ../src/playlistclient.h
)
target_link_libraries(kwe-playlist-controller-test PRIVATE Qt6::Core Qt6::Network Qt6::Test)
add_test(NAME kwe-playlist-controller-test COMMAND kwe-playlist-controller-test)

qt_add_executable(kwe-permissions-client-test
    permissionsclienttest.cpp
    ../src/permissionsclient.cpp
    ../src/permissionsclient.h
)
target_link_libraries(kwe-permissions-client-test PRIVATE Qt6::Core Qt6::Network Qt6::Qml Qt6::Test)
add_test(NAME kwe-permissions-client-test COMMAND kwe-permissions-client-test)

qt_add_executable(kwe-apply-client-test
    applyclienttest.cpp
    ../src/applyclient.cpp
    ../src/applyclient.h
)
target_link_libraries(kwe-apply-client-test PRIVATE Qt6::Core Qt6::Network Qt6::Qml Qt6::Test)
add_test(NAME kwe-apply-client-test COMMAND kwe-apply-client-test)

qt_add_executable(kwe-renderer-status-test
    rendererstatustest.cpp
    ../src/rendererstatus.cpp
    ../src/rendererstatus.h
)
target_link_libraries(kwe-renderer-status-test PRIVATE Qt6::Core Qt6::Network Qt6::Test)
add_test(NAME kwe-renderer-status-test COMMAND kwe-renderer-status-test)

qt_add_executable(kwe-web-preview-test
    webpreviewtest.cpp
    ../src/webpreview.cpp
    ../src/webpreview.h
    ../src/permissionsclient.cpp
    ../src/permissionsclient.h
)
target_link_libraries(kwe-web-preview-test PRIVATE Qt6::Core Qt6::Network Qt6::Qml Qt6::Test)
add_test(NAME kwe-web-preview-test COMMAND kwe-web-preview-test)

qt_add_executable(kwe-issue-reporter-test
    issuereportertest.cpp
    ../src/issuereporter.cpp
    ../src/issuereporter.h
)
target_link_libraries(kwe-issue-reporter-test PRIVATE Qt6::Core Qt6::Gui Qt6::Test)
add_test(NAME kwe-issue-reporter-test COMMAND kwe-issue-reporter-test)
