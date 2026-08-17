# SPDX-License-Identifier: Apache-2.0
qt_add_executable(kwe-package-installer-test
    packageinstallertest.cpp
    ../src/packageinstaller.cpp
    ../src/packageinstaller.h
)
target_link_libraries(kwe-package-installer-test PRIVATE Qt6::Core Qt6::Test)
add_test(NAME kwe-package-installer-test COMMAND kwe-package-installer-test)

qt_add_executable(kwe-workshop-client-test
    workshopclienttest.cpp
    ../src/workshopclient.cpp
    ../src/workshopclient.h
)
target_link_libraries(kwe-workshop-client-test PRIVATE Qt6::Core Qt6::Gui Qt6::Test)
add_test(NAME kwe-workshop-client-test COMMAND kwe-workshop-client-test)

qt_add_executable(kwe-playlist-controller-test
    playlistcontrollertest.cpp
    ../src/playlistcontroller.cpp
    ../src/playlistcontroller.h
    ../src/playlistclient.cpp
    ../src/playlistclient.h
)
target_link_libraries(kwe-playlist-controller-test PRIVATE Qt6::Core Qt6::Network Qt6::Test)
add_test(NAME kwe-playlist-controller-test COMMAND kwe-playlist-controller-test)
