// SPDX-License-Identifier: GPL-3.0-or-later
#pragma once

#include <QPointF>
#include <functional>

class QWindow;

// The only OS-specific surface of the display agent. platform_mac.mm
// implements it with AppKit; platform_stub.cpp is the Linux development
// no-op (windows stay ordinary windows, pointer comes from hover events).
namespace platform {

// Turn a shown QWindow into a desktop-level, click-through, all-Spaces
// window that sits under the Finder's desktop icons.
void makeDesktopWindow(QWindow *window);

// Hide the agent from the Dock and app switcher, opt out of App Nap.
void configureAgentProcess();

// Global pointer positions in top-left-origin logical coordinates (the
// same space as QScreen::geometry). macOS needs this because desktop
// windows ignore mouse events; the stub never calls back.
void startPointerMonitor(std::function<void(QPointF)> callback);
void stopPointerMonitor();

} // namespace platform
