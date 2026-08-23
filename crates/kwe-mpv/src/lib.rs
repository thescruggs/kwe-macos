// SPDX-License-Identifier: GPL-3.0-or-later
// Shared minimal FFI to the system libmpv client + software render APIs.
//
// # Why this crate exists
//
// Two workers drive libmpv: the video renderer (BETA_M1, a whole-desktop
// movie) and the scene renderer's VideoLayer path (BETA_M3g, ≤ 2 video
// textures inside a composited scene). Before M3g the declarations lived
// inline in `kwe-video-renderer`; a second inline copy would be two
// independently drifting descriptions of the same `unsafe extern` ABI,
// where a divergence is silent memory corruption rather than a compile
// error. The declarations therefore live here once, verbatim from the M1
// original, and both workers link the same surface.
//
// # Binding policy (unchanged from M1)
//
// The `mpv` crate cannot host this: `MpvHandlerBuilder::build()` runs
// `mpv_initialize` before exposing the handle, and libmpv aborts when a
// render context is created after initialization (empirically verified
// against 0.41 — see docs/BETA_M1.md). The crate was dropped in M1e, so
// these declarations are the only linkage to libmpv in the workspace.
//
// # Threading contract for callers
//
// libmpv's render API is not thread-safe: every `mpv_render_context_*`
// call for one context must come from the thread that created it. Both
// workers keep all render calls on their main thread and let the update
// callback do nothing but flip an atomic. This crate does not enforce
// that (the types are raw pointers, deliberately `!Send`-by-usage rather
// than by wrapper) — it is the caller's obligation, documented here
// because the compiler cannot check it.
//
// Only declarations, two pure decoding helpers, and one string accessor
// live here. Session lifecycle, option policy, and error typing stay with
// each worker, which is why this crate has no dependencies.

use std::ffi::CStr;
use std::os::raw::{c_char, c_int, c_ulong, c_void};

pub const MPV_FORMAT_STRING: c_int = 1;
pub const MPV_FORMAT_FLAG: c_int = 3;
/// `MPV_FORMAT_INT64` — reads/writes exactly one `int64_t`. The scene
/// worker uses it for `dwidth`/`dheight` (the decoded, aspect-corrected
/// video size) when sizing a VideoLayer texture.
pub const MPV_FORMAT_INT64: c_int = 4;
pub const MPV_FORMAT_DOUBLE: c_int = 5;

pub const MPV_EVENT_SHUTDOWN: c_int = 1;
pub const MPV_EVENT_END_FILE: c_int = 7;
pub const MPV_EVENT_FILE_LOADED: c_int = 8;
pub const MPV_END_FILE_REASON_EOF: c_int = 0;

pub const MPV_RENDER_PARAM_INVALID: c_int = 0;
pub const MPV_RENDER_PARAM_API_TYPE: c_int = 1;
pub const MPV_RENDER_PARAM_BLOCK_FOR_TARGET_TIME: c_int = 12;
pub const MPV_RENDER_PARAM_SW_SIZE: c_int = 17;
pub const MPV_RENDER_PARAM_SW_FORMAT: c_int = 18;
pub const MPV_RENDER_PARAM_SW_STRIDE: c_int = 19;
pub const MPV_RENDER_PARAM_SW_POINTER: c_int = 20;

pub const MPV_RENDER_UPDATE_FRAME: u64 = 1;

/// `struct mpv_event` (client.h): event_id, error, reply_userdata, data.
#[repr(C)]
pub struct mpv_event {
    pub event_id: c_int,
    pub error: c_int,
    pub reply_userdata: u64,
    pub data: *mut c_void,
}

/// `struct mpv_event_end_file` (client.h): reason, error, then per-format
/// fields we never read (any trailing layout is fine).
#[repr(C)]
pub struct mpv_event_end_file {
    pub reason: c_int,
    pub error: c_int,
}

/// `struct mpv_render_param` (render.h): type, then data.
#[repr(C)]
pub struct mpv_render_param {
    pub type_: c_int,
    pub data: *mut c_void,
}

pub type MpvHandle = c_void;
pub type MpvRenderContext = c_void;

// Links the dependent binary against the system libmpv. The removed `mpv`
// crate emitted this directive through its bindgen tree; with the crate
// gone the explicit declaration is the only linkage (THIRD_PARTY.yml,
// libmpv entry). Both workers hard-require libmpv.so — a distro without
// the SW render API fails closed rather than misbehaving.
#[link(name = "mpv")]
unsafe extern "C" {
    /// `unsigned long mpv_client_api_version(void)` (client.h): the packed
    /// `MPV_MAKE_VERSION(major, minor)` this libmpv was built with.
    pub fn mpv_client_api_version() -> c_ulong;
    pub fn mpv_create() -> *mut MpvHandle;
    pub fn mpv_initialize(handle: *mut MpvHandle) -> c_int;
    pub fn mpv_terminate_destroy(handle: *mut MpvHandle);
    pub fn mpv_set_option(
        handle: *mut MpvHandle,
        name: *const c_char,
        format: c_int,
        data: *const c_void,
    ) -> c_int;
    pub fn mpv_set_property(
        handle: *mut MpvHandle,
        name: *const c_char,
        format: c_int,
        data: *const c_void,
    ) -> c_int;
    pub fn mpv_get_property(
        handle: *mut MpvHandle,
        name: *const c_char,
        format: c_int,
        data: *mut c_void,
    ) -> c_int;
    pub fn mpv_command(handle: *mut MpvHandle, args: *const *const c_char) -> c_int;
    pub fn mpv_wait_event(handle: *mut MpvHandle, timeout: f64) -> *mut mpv_event;
    pub fn mpv_error_string(code: c_int) -> *const c_char;
    pub fn mpv_render_context_create(
        res: *mut *mut MpvRenderContext,
        handle: *mut MpvHandle,
        params: *mut mpv_render_param,
    ) -> c_int;
    pub fn mpv_render_context_set_update_callback(
        context: *mut MpvRenderContext,
        callback: Option<extern "C" fn(*mut c_void)>,
        callback_ctx: *mut c_void,
    );
    pub fn mpv_render_context_update(context: *mut MpvRenderContext) -> u64;
    pub fn mpv_render_context_render(
        context: *mut MpvRenderContext,
        params: *mut mpv_render_param,
    ) -> c_int;
    pub fn mpv_render_context_free(context: *mut MpvRenderContext);
}

/// Earliest libmpv client API version with the software render API
/// (`MPV_RENDER_API_TYPE_SW`), added in libmpv 0.33.0 (client API 2.1).
/// Both workers reject older libmpv at runtime; the probes report the same
/// bound honestly.
pub const SW_RENDER_MIN_API_VERSION: c_ulong = (2_u64 << 16 | 1) as c_ulong;

/// Split the packed libmpv client API version (`MPV_MAKE_VERSION(major,
/// minor)`) into `(major, minor)`, byte-for-byte the decoding the removed
/// `mpv` crate used for the same diagnostic (docs/BETA_M1.md).
#[must_use]
pub fn decode_api_version(version: c_ulong) -> (u16, u16) {
    ((version >> 16) as u16, (version & 0xFFFF) as u16)
}

/// libmpv's own text for an error code, as an owned `String`.
///
/// `mpv_error_string` is documented to return a static, never-NULL string
/// for every input (unknown codes yield "unknown error"), so the pointer
/// needs no lifetime management. A non-UTF-8 message — libmpv ships ASCII,
/// so this is defensive — is lossily converted rather than dropped.
#[must_use]
pub fn error_string(code: c_int) -> String {
    // SAFETY: mpv_error_string takes a plain int and returns a pointer to
    // a static NUL-terminated string, valid for any code (client.h).
    let pointer = unsafe { mpv_error_string(code) };
    if pointer.is_null() {
        // Not reachable per client.h, but a NULL deref here would be a
        // crash inside an error path — the least useful place to have one.
        return format!("libmpv error {code}");
    }
    // SAFETY: the pointer is non-NULL and points at a static NUL-terminated
    // string owned by libmpv; the bytes are copied before this returns.
    unsafe { CStr::from_ptr(pointer) }
        .to_string_lossy()
        .into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_version_decodes_like_the_mpv_crate() {
        // MPV_MAKE_VERSION(2, 5) == (2 << 16) | 5 — the version the M1
        // evidence recorded on this machine.
        assert_eq!(decode_api_version((2 << 16) | 5), (2, 5));
        assert_eq!(decode_api_version(0), (0, 0));
        // The minor field is 16 bits wide and must not bleed into major.
        assert_eq!(decode_api_version((1 << 16) | 0xFFFF), (1, 0xFFFF));
    }

    #[test]
    fn sw_render_bound_is_client_api_2_1() {
        // Decode the packed value rather than comparing constants: the
        // bound is about the client-version fields, not an arbitrary
        // relation to this machine's currently installed minor version.
        assert_eq!(decode_api_version(SW_RENDER_MIN_API_VERSION), (2, 1));
    }

    #[test]
    fn error_string_is_non_empty_for_success_and_failure() {
        // Links against the real libmpv: 0 is MPV_ERROR_SUCCESS and -2 is
        // MPV_ERROR_INVALID_PARAMETER. Both must yield a usable message
        // rather than an empty string or a crash.
        assert!(!error_string(0).is_empty());
        assert!(!error_string(-2).is_empty());
        // An out-of-range code still returns libmpv's fallback text.
        assert!(!error_string(-9999).is_empty());
    }
}
