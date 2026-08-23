# SPDX-License-Identifier: Apache-2.0
qt_add_executable(kwe-display-session-test
    displayclienttest.cpp
)
target_link_libraries(kwe-display-session-test PRIVATE
    kwe_display
    Qt6::Core
    Qt6::Network
    Qt6::Qml
    Qt6::Test
)
add_test(NAME kwe-display-session-test COMMAND kwe-display-session-test)

qt_add_executable(kwe-frame-mapping-test
    framemappingtest.cpp
)
target_link_libraries(kwe-frame-mapping-test PRIVATE
    kwe_display
    Qt6::Core
    Qt6::Gui
    Qt6::Qml
    Qt6::Quick
    Qt6::Test
)
add_test(NAME kwe-frame-mapping-test COMMAND kwe-frame-mapping-test)
