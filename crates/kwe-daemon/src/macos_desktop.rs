// SPDX-License-Identifier: GPL-3.0-or-later
#![cfg_attr(not(target_os = "macos"), allow(dead_code))]
//! macOS desktop backend for the apply lane (docs/macos/MacOS-Port-Plan.md,
//! MP-3). Plasma is reached through JavaScript evaluated in `plasmashell`;
//! on macOS there is no shell to script, and the display agent
//! (`apps/kwe-display-macos`) simply follows the daemon. This module makes
//! the existing, heavily tested apply transaction run unchanged by
//! answering the same three script shapes the Plasma probe answers:
//!
//! - the enumeration probe (`var d = desktops();` template): one synthetic
//!   desktop per active display, index `i`, screen `i`, id `i + 1`,
//!   plugin = the plugin recorded for that display (default
//!   `org.kde.image`), connector map `display name -> i`;
//! - the apply script (`desktops()[i]` ... `wallpaperPlugin = "<p>"`):
//!   records plugin `<p>` for display `i`;
//! - the restore script (adds an optional `writeConfig("Image", "...")`):
//!   records plugin and image for display `i`.
//!
//! The recorded plugin per display is what `wallpaper.outputs` reports as
//! `wallpaper_plugin`, and that is exactly what the display agent polls to
//! decide which screens it covers with the renderer's frames. Records are
//! persisted under the apply state directory so a daemon restart keeps
//! the desktop assignment (the agent is stateless).
//!
//! Display identity: the CoreGraphics display UUID
//! (`CGDisplayCreateUUIDFromDisplayID`), the same value the agent derives
//! from `NSScreen`, so the daemon's output name and the agent's screen
//! agree without any registration protocol. Enumeration itself is behind
//! `DisplayLister` so the emulation is unit-tested on every platform.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::apply::{ProbeError, ShellProbe, SystemOutput};

/// Plugin reported for a display nothing has been applied to yet. Named
/// after the Plasma image plugin so `restore_target` and the manager's
/// "Reset to image wallpaper" flow keep their meaning.
pub const DEFAULT_PLUGIN: &str = "org.kde.image";

const STATE_FILE: &str = "macos-desktops.json";
const MAX_STATE_BYTES: u64 = 64 * 1024;
const PROBE_TEMPLATE_PREFIX: &str = "var d = desktops();\nvar out = [];";

/// One active display as the backend sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Display {
    /// Stable identity (CoreGraphics display UUID string).
    pub name: String,
    /// `[x, y, width, height]` in global display points.
    pub geometry: [i32; 4],
}

/// Source of the active display list. Production uses CoreGraphics; tests
/// inject a fixed list.
pub trait DisplayLister: Send + Sync {
    fn active_displays(&self) -> Result<Vec<Display>, ProbeError>;
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct DesktopRecord {
    plugin: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    image: Option<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct PersistedState {
    version: u32,
    #[serde(default)]
    desktops: BTreeMap<String, DesktopRecord>,
}

/// The macOS `ShellProbe`: display enumeration plus an in-process
/// emulation of the Plasma desktop scripting surface the apply lane uses.
pub struct MacDesktopProbe {
    lister: Box<dyn DisplayLister>,
    state_path: PathBuf,
    desktops: Mutex<BTreeMap<String, DesktopRecord>>,
}

impl MacDesktopProbe {
    /// The production probe, or `None` when this build is not macOS or an
    /// external switch command was configured (integration smokes keep
    /// driving the stubbed Plasma boundary even on a Mac).
    #[allow(unused_variables)]
    pub fn from_config(state_dir: &Path, switch_command: Option<&Path>) -> Option<Self> {
        #[cfg(target_os = "macos")]
        {
            if switch_command.is_some() {
                return None;
            }
            Some(Self::with_lister(Box::new(coregraphics::CoreGraphicsLister), state_dir))
        }
        #[cfg(not(target_os = "macos"))]
        {
            None
        }
    }

    pub fn with_lister(lister: Box<dyn DisplayLister>, state_dir: &Path) -> Self {
        let state_path = state_dir.join(STATE_FILE);
        let desktops = load_state(&state_path);
        Self {
            lister,
            state_path,
            desktops: Mutex::new(desktops),
        }
    }

    fn probe_reply(&self, displays: &[Display]) -> Result<String, ProbeError> {
        let desktops = self
            .desktops
            .lock()
            .map_err(|_| ProbeError::Rejected("desktop state lock poisoned".into()))?;
        let mut out = Vec::with_capacity(displays.len());
        let mut connectors = serde_json::Map::new();
        for (index, display) in displays.iter().enumerate() {
            let record = desktops.get(&display.name).cloned().unwrap_or(DesktopRecord {
                plugin: DEFAULT_PLUGIN.to_string(),
                image: None,
            });
            out.push(serde_json::json!({
                "index": index,
                "id": index as u64 + 1,
                "screen": index as i32,
                "wp": record.plugin,
                "image": record.image,
            }));
            connectors.insert(display.name.clone(), serde_json::json!(index as i32));
        }
        Ok(serde_json::json!({ "desktops": out, "connectors": connectors }).to_string())
    }

    fn apply_switch(&self, displays: &[Display], script: &str) -> Result<String, ProbeError> {
        let command = parse_switch_script(script)?;
        let display = displays.get(command.desktop_index).ok_or_else(|| {
            ProbeError::Rejected(format!("no desktop {}", command.desktop_index))
        })?;
        let mut desktops = self
            .desktops
            .lock()
            .map_err(|_| ProbeError::Rejected("desktop state lock poisoned".into()))?;
        let record = desktops.entry(display.name.clone()).or_default();
        record.plugin = command.plugin;
        if command.touches_image {
            record.image = command.image;
        }
        let snapshot = desktops.clone();
        drop(desktops);
        if let Err(error) = save_state(&self.state_path, &snapshot) {
            eprintln!(
                "event=macos_desktop.persist_error path={} detail={error}",
                self.state_path.display()
            );
        }
        // Plasma's evaluateScript returns the (empty) print buffer.
        Ok(String::new())
    }
}

impl ShellProbe for MacDesktopProbe {
    fn evaluate_script(&self, script: &str) -> Result<String, ProbeError> {
        let displays = self.lister.active_displays()?;
        if script.starts_with(PROBE_TEMPLATE_PREFIX) {
            self.probe_reply(&displays)
        } else {
            self.apply_switch(&displays, script)
        }
    }

    fn system_outputs(&self) -> Result<Vec<SystemOutput>, ProbeError> {
        let displays = self.lister.active_displays()?;
        Ok(displays
            .into_iter()
            .map(|display| SystemOutput {
                name: display.name,
                enabled: true,
                connected: true,
                geometry: Some(display.geometry),
            })
            .collect())
    }
}

#[derive(Debug, PartialEq, Eq)]
struct SwitchCommand {
    desktop_index: usize,
    plugin: String,
    /// True for the restore shape (`currentConfigGroup` present): the
    /// image is then authoritative even when absent (a null image writes
    /// nothing in Plasma, but the record must not keep a stale one).
    touches_image: bool,
    image: Option<String>,
}

/// Parses the two switch shapes `apply::apply_script` and
/// `apply::restore_script` generate. Anything else is rejected: the
/// emulation never guesses at a script it does not recognise.
fn parse_switch_script(script: &str) -> Result<SwitchCommand, ProbeError> {
    let reject = |detail: &str| ProbeError::Rejected(format!("unsupported desktop script: {detail}"));
    let index_start = script
        .find("desktops()[")
        .ok_or_else(|| reject("no desktop index"))?
        + "desktops()[".len();
    let index_end = script[index_start..]
        .find(']')
        .ok_or_else(|| reject("unterminated desktop index"))?
        + index_start;
    let desktop_index: usize = script[index_start..index_end]
        .parse()
        .map_err(|_| reject("desktop index is not a number"))?;
    let plugin = quoted_after(script, "wallpaperPlugin = \"").ok_or_else(|| reject("no plugin"))?;
    if plugin.is_empty() || !plugin.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')) {
        return Err(reject("plugin is not an identity"));
    }
    let touches_image = script.contains("currentConfigGroup = [");
    let image = quoted_after(script, "writeConfig(\"Image\", \"").map(unescape_js);
    Ok(SwitchCommand {
        desktop_index,
        plugin,
        touches_image,
        image,
    })
}

/// The JS string literal following `marker` (raw, still escaped), honouring
/// backslash escapes when looking for the closing quote.
fn quoted_after(script: &str, marker: &str) -> Option<String> {
    let start = script.find(marker)? + marker.len();
    let rest = &script[start..];
    let mut escaped = false;
    for (offset, ch) in rest.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match ch {
            '\\' => escaped = true,
            '"' => return Some(rest[..offset].to_string()),
            _ => {}
        }
    }
    None
}

/// Reverses `apply::escape_js_string`.
fn unescape_js(value: String) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('u') => {
                let code: String = chars.by_ref().take(4).collect();
                match u32::from_str_radix(&code, 16).ok().and_then(char::from_u32) {
                    Some(decoded) => out.push(decoded),
                    None => {
                        out.push_str("\\u");
                        out.push_str(&code);
                    }
                }
            }
            Some(other) => out.push(other),
            None => out.push('\\'),
        }
    }
    out
}

fn load_state(path: &Path) -> BTreeMap<String, DesktopRecord> {
    let Ok(metadata) = fs::metadata(path) else {
        return BTreeMap::new();
    };
    if !metadata.is_file() || metadata.len() > MAX_STATE_BYTES {
        eprintln!(
            "event=macos_desktop.state_ignored path={} reason=not-a-bounded-file",
            path.display()
        );
        return BTreeMap::new();
    }
    match fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<PersistedState>(&bytes).ok())
    {
        Some(state) if state.version == 1 => state.desktops,
        _ => {
            eprintln!(
                "event=macos_desktop.state_ignored path={} reason=unparsable",
                path.display()
            );
            BTreeMap::new()
        }
    }
}

fn save_state(path: &Path, desktops: &BTreeMap<String, DesktopRecord>) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let state = PersistedState {
        version: 1,
        desktops: desktops.clone(),
    };
    let bytes = serde_json::to_vec_pretty(&state)?;
    let temporary = path.with_extension("json.tmp");
    fs::write(&temporary, bytes)?;
    fs::rename(&temporary, path)
}

#[cfg(target_os = "macos")]
mod coregraphics {
    //! Active display enumeration through CoreGraphics. Bounded to 16
    //! displays; a WindowServer-less session (SSH login, pre-login agent)
    //! reports `DisplayUnavailable`, which the apply lane already surfaces
    //! as the user-fixable "no display" state.
    use std::ffi::c_void;

    use super::{Display, DisplayLister};
    use crate::apply::ProbeError;

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct CGPoint {
        x: f64,
        y: f64,
    }
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct CGSize {
        width: f64,
        height: f64,
    }
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct CGRect {
        origin: CGPoint,
        size: CGSize,
    }

    type CGDirectDisplayID = u32;
    type CGError = i32;
    type CFTypeRef = *const c_void;
    type CFIndex = isize;
    const K_CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;
    const MAX_DISPLAYS: u32 = 16;

    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGGetActiveDisplayList(
            max_displays: u32,
            active_displays: *mut CGDirectDisplayID,
            display_count: *mut u32,
        ) -> CGError;
        fn CGDisplayBounds(display: CGDirectDisplayID) -> CGRect;
        fn CGDisplayCreateUUIDFromDisplayID(display: CGDirectDisplayID) -> CFTypeRef;
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFUUIDCreateString(allocator: CFTypeRef, uuid: CFTypeRef) -> CFTypeRef;
        fn CFStringGetCString(
            string: CFTypeRef,
            buffer: *mut libc::c_char,
            buffer_size: CFIndex,
            encoding: u32,
        ) -> u8;
        fn CFRelease(value: CFTypeRef);
    }

    pub struct CoreGraphicsLister;

    impl DisplayLister for CoreGraphicsLister {
        fn active_displays(&self) -> Result<Vec<Display>, ProbeError> {
            let mut ids = [0 as CGDirectDisplayID; MAX_DISPLAYS as usize];
            let mut count: u32 = 0;
            // SAFETY: ids is a valid buffer of MAX_DISPLAYS entries and count
            // a valid out-pointer.
            let rc = unsafe { CGGetActiveDisplayList(MAX_DISPLAYS, ids.as_mut_ptr(), &mut count) };
            if rc != 0 {
                return Err(ProbeError::DisplayUnavailable(format!(
                    "CGGetActiveDisplayList failed (CGError {rc}); is a window session available?"
                )));
            }
            if count == 0 {
                return Err(ProbeError::DisplayUnavailable(
                    "no active displays reported by CoreGraphics".into(),
                ));
            }
            Ok(ids[..count as usize]
                .iter()
                .map(|&id| Display {
                    name: display_uuid(id).unwrap_or_else(|| format!("display-{id}")),
                    geometry: bounds(id),
                })
                .collect())
        }
    }

    fn bounds(id: CGDirectDisplayID) -> [i32; 4] {
        // SAFETY: plain value-returning call on a display id from the active list.
        let rect = unsafe { CGDisplayBounds(id) };
        let clamp = |value: f64| value.round().clamp(i32::MIN as f64, i32::MAX as f64) as i32;
        [
            clamp(rect.origin.x),
            clamp(rect.origin.y),
            clamp(rect.size.width),
            clamp(rect.size.height),
        ]
    }

    fn display_uuid(id: CGDirectDisplayID) -> Option<String> {
        // SAFETY: returns an owned CFUUIDRef (or null) that we release below.
        let uuid = unsafe { CGDisplayCreateUUIDFromDisplayID(id) };
        if uuid.is_null() {
            return None;
        }
        // SAFETY: uuid is a valid CFUUIDRef; the returned string is owned.
        let string = unsafe { CFUUIDCreateString(std::ptr::null(), uuid) };
        // SAFETY: uuid is owned by this function.
        unsafe { CFRelease(uuid) };
        if string.is_null() {
            return None;
        }
        let mut buffer = [0 as libc::c_char; 64];
        // SAFETY: buffer is a valid, sized out-buffer; string is a valid CFStringRef.
        let ok = unsafe {
            CFStringGetCString(string, buffer.as_mut_ptr(), buffer.len() as CFIndex, K_CF_STRING_ENCODING_UTF8)
        };
        // SAFETY: string is owned by this function.
        unsafe { CFRelease(string) };
        if ok == 0 {
            return None;
        }
        let bytes: Vec<u8> = buffer.iter().take_while(|&&c| c != 0).map(|&c| c as u8).collect();
        let text = String::from_utf8(bytes).ok()?;
        let valid = !text.is_empty()
            && text.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
        valid.then_some(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apply::{apply_script, probe_script, restore_script};

    struct FixedDisplays(Vec<Display>);
    impl DisplayLister for FixedDisplays {
        fn active_displays(&self) -> Result<Vec<Display>, ProbeError> {
            Ok(self.0.clone())
        }
    }
    struct NoDisplays;
    impl DisplayLister for NoDisplays {
        fn active_displays(&self) -> Result<Vec<Display>, ProbeError> {
            Err(ProbeError::DisplayUnavailable("none".into()))
        }
    }

    fn two_displays() -> Vec<Display> {
        vec![
            Display {
                name: "37D8832A-2D66-02CA-B9F7-8F30A301B230".into(),
                geometry: [0, 0, 3456, 2234],
            },
            Display {
                name: "1C8B7D2E-0000-4A7E-9F31-1111AAAA2222".into(),
                geometry: [3456, 0, 2560, 1440],
            },
        ]
    }

    fn temporary_directory(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "kwe-macos-desktop-{label}-{}-{}",
            std::process::id(),
            crate::persist::unix_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn enumeration_reports_one_desktop_per_display_with_the_image_plugin_by_default() {
        let dir = temporary_directory("enumerate");
        let probe = MacDesktopProbe::with_lister(Box::new(FixedDisplays(two_displays())), &dir);
        let outputs = probe.system_outputs().unwrap();
        assert_eq!(outputs.len(), 2);
        assert_eq!(outputs[1].geometry, Some([3456, 0, 2560, 1440]));
        let names: Vec<String> = outputs.iter().map(|o| o.name.clone()).collect();
        let reply = probe.evaluate_script(&probe_script(&names).unwrap()).unwrap();
        let value: serde_json::Value = serde_json::from_str(&reply).unwrap();
        assert_eq!(value["desktops"].as_array().unwrap().len(), 2);
        assert_eq!(value["desktops"][0]["wp"], DEFAULT_PLUGIN);
        assert_eq!(value["desktops"][1]["screen"], 1);
        assert_eq!(value["connectors"][&names[1]], 1);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn apply_and_restore_scripts_flip_the_recorded_plugin_and_persist() {
        let dir = temporary_directory("switch");
        let displays = two_displays();
        let names: Vec<String> = displays.iter().map(|d| d.name.clone()).collect();
        {
            let probe = MacDesktopProbe::with_lister(Box::new(FixedDisplays(displays.clone())), &dir);
            probe
                .evaluate_script(&apply_script(1, "org.kde.kwe.wallpaper").unwrap())
                .unwrap();
            let reply = probe.evaluate_script(&probe_script(&names).unwrap()).unwrap();
            let value: serde_json::Value = serde_json::from_str(&reply).unwrap();
            assert_eq!(value["desktops"][0]["wp"], DEFAULT_PLUGIN);
            assert_eq!(value["desktops"][1]["wp"], "org.kde.kwe.wallpaper");
        }
        // A fresh probe (daemon restart) reloads the record.
        let probe = MacDesktopProbe::with_lister(Box::new(FixedDisplays(displays)), &dir);
        let reply = probe.evaluate_script(&probe_script(&names).unwrap()).unwrap();
        let value: serde_json::Value = serde_json::from_str(&reply).unwrap();
        assert_eq!(value["desktops"][1]["wp"], "org.kde.kwe.wallpaper");

        let restore = restore_script(
            1,
            "org.kde.image",
            &["Wallpaper".into(), "org.kde.image".into(), "General".into()],
            Some("/Users/me/Pictures/a \"quoted\" name.jpg"),
        )
        .unwrap();
        probe.evaluate_script(&restore).unwrap();
        let reply = probe.evaluate_script(&probe_script(&names).unwrap()).unwrap();
        let value: serde_json::Value = serde_json::from_str(&reply).unwrap();
        assert_eq!(value["desktops"][1]["wp"], "org.kde.image");
        assert_eq!(value["desktops"][1]["image"], "/Users/me/Pictures/a \"quoted\" name.jpg");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn stale_desktop_index_and_unknown_scripts_are_rejected() {
        let dir = temporary_directory("reject");
        let probe = MacDesktopProbe::with_lister(Box::new(FixedDisplays(two_displays())), &dir);
        let error = probe
            .evaluate_script(&apply_script(7, "org.kde.kwe.wallpaper").unwrap())
            .unwrap_err();
        assert!(matches!(error, ProbeError::Rejected(detail) if detail.contains("no desktop 7")));
        assert!(matches!(
            probe.evaluate_script("print(1);").unwrap_err(),
            ProbeError::Rejected(_)
        ));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn no_window_session_is_display_unavailable_not_unreachable() {
        let dir = temporary_directory("nodisplay");
        let probe = MacDesktopProbe::with_lister(Box::new(NoDisplays), &dir);
        assert!(matches!(
            probe.system_outputs().unwrap_err(),
            ProbeError::DisplayUnavailable(_)
        ));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn corrupt_or_oversized_state_is_ignored_not_fatal() {
        let dir = temporary_directory("corrupt");
        fs::write(dir.join(STATE_FILE), b"{not json").unwrap();
        let probe = MacDesktopProbe::with_lister(Box::new(FixedDisplays(two_displays())), &dir);
        assert!(probe.desktops.lock().unwrap().is_empty());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn js_unescape_reverses_the_apply_escaper() {
        assert_eq!(unescape_js(r#"a\\b\"c\nd e"#.into()), "a\\b\"c\nd\u{2028}e");
    }
}
