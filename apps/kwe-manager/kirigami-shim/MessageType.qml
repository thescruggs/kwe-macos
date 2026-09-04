// SPDX-License-Identifier: GPL-3.0-or-later
import QtQuick

// Kirigami.MessageType: QML-declared enums are reached through the type
// name (MessageType.Error), matching the C++ enum Kirigami exposes.
QtObject {
    enum Type {
        Information,
        Positive,
        Warning,
        Error
    }
}
