// SPDX-License-Identifier: GPL-3.0-or-later
#include "platform.h"

#import <AppKit/AppKit.h>
#import <Foundation/Foundation.h>

#include <QWindow>

#if !__has_feature(objc_arc)
#error "platform_mac.mm must be compiled with -fobjc-arc (see CMakeLists.txt)"
#endif

namespace {
// Strong references under ARC: the monitor token must stay alive until
// removeMonitor:, and the activity token for as long as App Nap must stay
// off.
id globalMonitor = nil;
id<NSObject> activityToken = nil;
std::function<void(QPointF)> pointerCallback;

// AppKit reports the pointer in a bottom-left-origin space anchored on the
// primary screen; Qt's QScreen geometry is top-left-origin anchored on the
// same primary screen, so flipping y against the primary height maps one
// onto the other for every screen.
QPointF topLeftPointer() {
  const NSPoint location = [NSEvent mouseLocation];
  NSScreen *primary = [[NSScreen screens] firstObject];
  const CGFloat primaryHeight = primary ? NSMaxY(primary.frame) : 0;
  return QPointF(location.x, primaryHeight - location.y);
}
} // namespace

namespace platform {

void makeDesktopWindow(QWindow *window) {
  if (window == nullptr)
    return;
  NSView *view = reinterpret_cast<NSView *>(window->winId());
  NSWindow *nsWindow = [view window];
  if (nsWindow == nil)
    return;
  // Below the desktop icons (kCGDesktopIconWindowLevel), above the system
  // desktop picture. Finder keeps owning the icons and the desktop menu.
  [nsWindow setLevel:CGWindowLevelForKey(kCGDesktopWindowLevelKey)];
  [nsWindow setCollectionBehavior:(NSWindowCollectionBehaviorCanJoinAllSpaces |
                                   NSWindowCollectionBehaviorStationary |
                                   NSWindowCollectionBehaviorIgnoresCycle |
                                   NSWindowCollectionBehaviorFullScreenNone)];
  [nsWindow setIgnoresMouseEvents:YES];
  [nsWindow setHasShadow:NO];
  [nsWindow setOpaque:YES];
  [nsWindow setMovable:NO];
  [nsWindow setCanHide:NO];
  [nsWindow setHidesOnDeactivate:NO];
  [nsWindow setAnimationBehavior:NSWindowAnimationBehaviorNone];
  [nsWindow setExcludedFromWindowsMenu:YES];
  [nsWindow orderBack:nil];
}

void configureAgentProcess() {
  // No Dock icon, no app-switcher entry (the LSUIElement behavior without
  // needing a bundle), and no App Nap throttling of the frame poller.
  [NSApp setActivationPolicy:NSApplicationActivationPolicyAccessory];
  if (activityToken == nil) {
    activityToken = [[NSProcessInfo processInfo]
        beginActivityWithOptions:(NSActivityUserInitiatedAllowingIdleSystemSleep |
                                  NSActivityLatencyCritical)
                          reason:@"kwe desktop wallpaper frames"];
  }
}

void startPointerMonitor(std::function<void(QPointF)> callback) {
  stopPointerMonitor();
  pointerCallback = std::move(callback);
  const NSEventMask mask = NSEventMaskMouseMoved | NSEventMaskLeftMouseDragged |
                           NSEventMaskRightMouseDragged | NSEventMaskOtherMouseDragged;
  // Global monitors observe events delivered to OTHER applications; a
  // desktop window never receives them itself. Mouse-moved monitoring does
  // not need the Accessibility permission (key events would).
  // Our windows ignore mouse events, so a local monitor could never fire;
  // the global monitor is the only source.
  globalMonitor = [NSEvent addGlobalMonitorForEventsMatchingMask:mask
                                                         handler:^(NSEvent *) {
                                                           if (pointerCallback)
                                                             pointerCallback(topLeftPointer());
                                                         }];
}

void stopPointerMonitor() {
  if (globalMonitor != nil) {
    [NSEvent removeMonitor:globalMonitor];
    globalMonitor = nil;
  }
  pointerCallback = nullptr;
}

} // namespace platform
