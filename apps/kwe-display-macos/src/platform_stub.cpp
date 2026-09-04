// SPDX-License-Identifier: GPL-3.0-or-later
#include "platform.h"

namespace platform {
void makeDesktopWindow(QWindow *) {}
void configureAgentProcess() {}
void startPointerMonitor(std::function<void(QPointF)>) {}
void stopPointerMonitor() {}
} // namespace platform
