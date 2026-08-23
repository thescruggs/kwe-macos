// SPDX-License-Identifier: GPL-3.0-or-later
// VideoLayer textures via libmpv software rendering (M3g).
//
// A scene object carrying a `video` key (and no `image`) is a video layer:
// an image layer whose texture is a movie. Each such layer that fits the
// concurrency cap gets one `VideoDecoder` — a private libmpv core driven
// through the software render API (`kwe-mpv`), rendering straight into an
// RGBA8 scratch buffer that the compositor re-uploads into the layer's
// existing texture slot every frame (vulkan.rs `refresh_layer`).
//
// Corpus honesty (M3g re-scan of the 60-package Workshop corpus): NOT ONE
// scene in the corpus contains a video layer. No object carries a `video`,
// `movie`, `webm`, or `mp4` key; no package entry has a video extension;
// no scene.json string anywhere contains the substring "video". Every
// schema decision below is therefore researched + synthetic-fixture work,
// not corpus-observed — the same footing as the M3e text and M3f particle
// keys that the corpus did not exercise. docs/SCENE_FORMAT_V1.md records
// which keys are observed and which are researched.
//
// Design constraints this module exists to enforce:
//
//   * At most `MAX_VIDEO_LAYERS` concurrent decoders. A libmpv core is not
//     cheap (its own threads, demuxer cache, and decoder); a scene that
//     declares twenty video layers must not spawn twenty cores. Layers
//     past the cap register and draw nothing — never a scene rejection.
//   * Bounded frame size. The decoder renders at the video's own
//     `dwidth`x`dheight`, refused past `MAX_VIDEO_DIMENSION` /
//     `MAX_VIDEO_PIXELS`, so one hostile 16K source cannot make the worker
//     allocate an unbounded per-frame buffer.
//   * Fail-closed, never fatal. Any libmpv failure — open, initialize,
//     render, or a non-EOF end-file — marks the decoder failed; the layer
//     stops updating and keeps whatever it last showed. A video problem
//     degrades one layer, exactly like an undecodable image (M3c).
//
// Threading: `mpv_render_context_render` and `mpv_render_context_update`
// must be called on the thread that created the context (render.h). The
// scene worker is single-threaded — decoders are created and polled on the
// main thread — and `VideoDecoder` is deliberately not `Send`/`Sync` (the
// raw pointers make it so automatically); the update callback fires on a
// libmpv thread and only flips an atomic, which is all render.h permits.

use std::ffi::{CString, c_char, c_int, c_void};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

/// Maximum number of concurrently decoding video layers per scene. Layers
/// past this cap parse and register (so `objects` indices and the script's
/// layer list stay aligned) but never open a decoder.
pub const MAX_VIDEO_LAYERS: usize = 2;

/// Playback-speed bounds. libmpv accepts 0.01..=100; the wallpaper case
/// has no use for either extreme, and a hostile `rate` of 100 would burn
/// CPU decoding frames nobody sees. Out-of-range values clamp (the M3f
/// convention) rather than rejecting the scene.
pub const MIN_PLAYBACK_RATE: f32 = 0.1;
pub const MAX_PLAYBACK_RATE: f32 = 4.0;

/// Maximum decoded video edge in pixels, and maximum decoded pixel count
/// (3840x2160). Tighter than the still-image caps (textures.rs: 8192 /
/// 16.7M) on purpose: a still texture is decoded once, a video frame is
/// decoded, converted, and re-uploaded every frame.
pub const MAX_VIDEO_DIMENSION: u32 = 3840;
pub const MAX_VIDEO_PIXELS: u64 = 8_294_400;

/// Maximum bytes extracted for one package-embedded video. libmpv opens a
/// path, not a byte slice, so a pkg-embedded video must land on disk in
/// the worker's private HOME first (main.rs `extract_video`).
// The daemon's scene worker RLIMIT_FSIZE is 160 MiB. Keep package extraction
// below that kernel ceiling so the bounded read and the actual write agree.
pub const MAX_VIDEO_SOURCE_BYTES: u64 = 160 * 1024 * 1024;

/// Latest-wins media transport command applied to every open VideoLayer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaCommand {
    Play,
    Pause,
    Stop,
}

/// How long `open` waits for libmpv to report the file loaded before
/// giving up on the layer. The decoded size is only readable after
/// `MPV_EVENT_FILE_LOADED`, and the texture cannot be sized without it.
const FILE_LOAD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);
/// Poll granularity while waiting for the load event.
const FILE_LOAD_POLL: f64 = 0.05;

/// Events drained per frame, so a pathological event storm cannot stall
/// the render loop (mirrors the video worker's bound).
const MAX_EVENTS_PER_TICK: usize = 32;

/// The software-render pixel format requested from libmpv. `rgb0` is
/// R,G,B,X in memory order — byte-identical to `R8G8B8A8_UNORM` once the
/// X byte is forced opaque, so the scene path needs no channel swizzle
/// (the video worker asks for `bgr0` because its consumer wants BGRA).
const SW_FORMAT: &str = "rgb0";

/// Clamp a parsed playback rate into the supported range. Non-finite
/// values (NaN from a hostile `rate`) fall back to 1.0 rather than
/// propagating into libmpv's `speed`.
pub fn clamp_playback_rate(value: f64) -> f32 {
    if !value.is_finite() {
        return 1.0;
    }
    (value as f32).clamp(MIN_PLAYBACK_RATE, MAX_PLAYBACK_RATE)
}

/// Is a decoded video size within the per-frame caps?
pub fn video_size_allowed(width: u32, height: u32) -> bool {
    width > 0
        && height > 0
        && width <= MAX_VIDEO_DIMENSION
        && height <= MAX_VIDEO_DIMENSION
        && u64::from(width)
            .checked_mul(u64::from(height))
            .is_some_and(|pixels| pixels <= MAX_VIDEO_PIXELS)
}

/// Notification that a new frame is available. render.h forbids calling
/// any mpv API here; it only flips an atomic the render loop observes.
extern "C" fn on_render_update(context: *mut c_void) {
    let flag = context as *const AtomicBool;
    // SAFETY: the flag is boxed inside the decoder and outlives the render
    // context (Drop frees the context before the box drops); an atomic
    // store is safe from any thread.
    unsafe { (*flag).store(true, Ordering::Release) };
}

/// One video layer's decoder: a private libmpv core rendering into an
/// RGBA8 buffer sized to the video's own decoded dimensions.
pub struct VideoDecoder {
    handle: *mut kwe_mpv::MpvHandle,
    render_ctx: *mut kwe_mpv::MpvRenderContext,
    /// Boxed so the address handed to libmpv's update callback stays valid
    /// even if the decoder itself moves.
    update_flag: Box<AtomicBool>,
    width: u32,
    height: u32,
    /// The RGBA8 frame buffer. `width * height * 4` bytes, reused every
    /// frame — steady-state video costs no allocation.
    rgba: Vec<u8>,
    /// Set once a fatal libmpv condition is seen. A failed decoder stops
    /// rendering and stops being polled; the layer keeps its last frame.
    failed: bool,
    /// Whether a frame has ever been rendered (the first `poll_frame` must
    /// upload even if libmpv has not raised the update flag yet).
    rendered_any: bool,
}

impl VideoDecoder {
    /// Open `path` and prepare a decoder for it. `Err(detail)` is the
    /// caller's one-time diagnostic — the layer then registers without a
    /// decoder and draws nothing.
    pub fn open(path: &Path, loop_playback: bool, rate: f32) -> Result<Self, String> {
        // SAFETY: mpv_create has no preconditions; it returns a handle or NULL.
        let handle = unsafe { kwe_mpv::mpv_create() };
        if handle.is_null() {
            return Err("mpv_create returned a null handle".into());
        }
        let mut decoder = Self {
            handle,
            render_ctx: std::ptr::null_mut(),
            update_flag: Box::new(AtomicBool::new(false)),
            width: 0,
            height: 0,
            rgba: Vec::new(),
            failed: false,
            rendered_any: false,
        };
        // Options must be set before mpv_initialize; each is parsed exactly
        // like the matching --option=value command line flag.
        //
        //   hwdec=no       the scene worker composites on the same GPU the
        //                  Vulkan compositor owns; a hardware decode
        //                  context here would contend with it for the same
        //                  device, and the SW render path copies through
        //                  system memory anyway. Documented deviation from
        //                  the video worker (which tries hwdec first and
        //                  falls back); hwdec for scene video is a planned
        //                  optimization, not a correctness gap.
        //   audio=no       scene audio is M3i. Until then a video layer is
        //                  silent — a wallpaper that starts making noise on
        //                  login is a bug, not a feature.
        //   terminal=no    the scene worker's stderr is a structured event
        //                  stream the supervisor parses; libmpv must not
        //                  interleave its own log lines into it.
        //   keep-open      a non-looping video holds its last frame instead
        //                  of unloading (which would leave the layer
        //                  showing whatever was in the texture).
        let rate = format!("{rate}");
        for (name, value) in [
            ("loop-file", if loop_playback { "inf" } else { "no" }),
            ("keep-open", if loop_playback { "no" } else { "yes" }),
            ("hwdec", "no"),
            ("audio", "no"),
            ("terminal", "no"),
            ("idle", "yes"),
            ("vo", "libmpv"),
            // A scene decoder is a texture source, not a network player:
            // disable the unbounded cache and cap lavf's demux buffers.
            ("cache", "no"),
            ("demuxer-max-bytes", "32MiB"),
            ("demuxer-max-back-bytes", "8MiB"),
            // libavformat permits every protocol by default. Forward a
            // strict local-file whitelist through mpv's documented lavf
            // option so a crafted regular file cannot invoke a network or
            // nested protocol. The scene worker has no network grant.
            ("demuxer-lavf-o", "protocol_whitelist=file"),
            ("config", "no"),
            ("load-scripts", "no"),
            ("access-references", "no"),
            ("autoload-files", "no"),
            ("ytdl", "no"),
            ("speed", rate.as_str()),
        ] {
            decoder.set_option(name, value)?;
        }

        // The software render context must exist BEFORE mpv_initialize:
        // libmpv 0.41 aborts the process when one is created afterwards
        // (verified in M1, see docs/BETA_M1.md).
        let api_type = CString::new("sw").map_err(|_| "invalid sw API string".to_string())?;
        let mut params = [
            kwe_mpv::mpv_render_param {
                type_: kwe_mpv::MPV_RENDER_PARAM_API_TYPE,
                data: api_type.as_ptr().cast_mut().cast(),
            },
            kwe_mpv::mpv_render_param {
                type_: kwe_mpv::MPV_RENDER_PARAM_INVALID,
                data: std::ptr::null_mut(),
            },
        ];
        let mut context: *mut kwe_mpv::MpvRenderContext = std::ptr::null_mut();
        // SAFETY: the handle is valid and uninitialized; the params array
        // outlives the call.
        let code = unsafe {
            kwe_mpv::mpv_render_context_create(&mut context, handle, params.as_mut_ptr())
        };
        if code < 0 {
            return Err(format!(
                "mpv_render_context_create failed: {}",
                kwe_mpv::error_string(code)
            ));
        }
        if context.is_null() {
            return Err("mpv_render_context_create returned a null context".into());
        }
        decoder.render_ctx = context;
        // SAFETY: the boxed flag outlives the context (Drop frees the
        // context first); the callback only stores to the atomic.
        unsafe {
            kwe_mpv::mpv_render_context_set_update_callback(
                context,
                Some(on_render_update),
                (&*decoder.update_flag as *const AtomicBool)
                    .cast_mut()
                    .cast(),
            )
        };

        // SAFETY: the handle was created and configured but not initialized.
        let code = unsafe { kwe_mpv::mpv_initialize(handle) };
        if code < 0 {
            return Err(format!(
                "mpv_initialize failed: {}",
                kwe_mpv::error_string(code)
            ));
        }

        let path = path
            .to_str()
            .ok_or_else(|| "video path is not UTF-8".to_string())?;
        decoder.command(&["loadfile", path])?;

        // The decoded size is only readable once the file is loaded, and
        // the texture cannot be sized without it. `dwidth`/`dheight` are
        // the aspect-corrected size published by the video output, so
        // they are NOT set at FILE_LOADED — a query there returns nothing
        // (measured: the first M3g smoke lane failed with "video reports
        // no decoded width" against a clip that plays fine). So the same
        // bounded wait keeps pumping events past FILE_LOADED until the VO
        // publishes, and falls back to the raw decoded `width`/`height`
        // if the deadline arrives first. A source that never loads costs
        // one layer, not the scene.
        let deadline = std::time::Instant::now() + FILE_LOAD_TIMEOUT;
        let mut loaded = false;
        let mut size = None;
        while std::time::Instant::now() < deadline {
            match decoder.wait_event(FILE_LOAD_POLL)? {
                Some(Event::FileLoaded) => loaded = true,
                Some(Event::Shutdown) => return Err("libmpv core shut down during load".into()),
                Some(Event::EndFile { reason }) if reason != kwe_mpv::MPV_END_FILE_REASON_EOF => {
                    return Err(format!("libmpv end_file reason {reason} during load"));
                }
                _ => {}
            }
            if !loaded {
                continue;
            }
            if let Some(published) = decoder.property_size("dwidth", "dheight") {
                size = Some(published);
                break;
            }
        }
        if !loaded {
            return Err(format!("video did not load within {FILE_LOAD_TIMEOUT:?}"));
        }
        let Some((width, height)) = size.or_else(|| decoder.property_size("width", "height"))
        else {
            return Err(format!(
                "video loaded but published no decoded size within {FILE_LOAD_TIMEOUT:?}"
            ));
        };
        // Both halves are positive by construction; only an absurd value
        // past u32::MAX narrows to 0, which the cap check then refuses.
        let (width, height) = (
            u32::try_from(width).unwrap_or(0),
            u32::try_from(height).unwrap_or(0),
        );
        if !video_size_allowed(width, height) {
            return Err(format!(
                "decoded size {width}x{height} exceeds the video caps \
                 ({MAX_VIDEO_DIMENSION} per edge, {MAX_VIDEO_PIXELS} pixels)"
            ));
        }
        decoder.width = width;
        decoder.height = height;
        // Checked above against MAX_VIDEO_PIXELS, so the product fits.
        decoder.rgba = vec![0; width as usize * height as usize * 4];
        Ok(decoder)
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    pub fn failed(&self) -> bool {
        self.failed
    }

    /// Permanently stop polling this decoder after a compositor upload
    /// failure. The layer retains its last good texture, and callers emit a
    /// single layer diagnostic instead of retrying/logging every frame.
    pub fn disable(&mut self) {
        self.failed = true;
    }

    /// Apply the daemon's bounded media control to this layer. SceneScript
    /// does not expose per-layer controls yet; transport state fans out to
    /// all open layers, matching the standalone video worker semantics.
    pub fn apply_media(&mut self, command: MediaCommand) -> Result<(), String> {
        if self.failed {
            return Ok(());
        }
        match command {
            MediaCommand::Play => self.set_property_flag("pause", false),
            MediaCommand::Pause => self.set_property_flag("pause", true),
            MediaCommand::Stop => self
                .set_property_flag("pause", true)
                .and_then(|()| self.command(&["seek", "0", "absolute"])),
        }
    }

    /// The decoder's current frame buffer: `width * height * 4` bytes of
    /// RGBA8, zero-filled until the first successful render. The worker
    /// uploads this once at startup so the layer owns a descriptor set
    /// before any frame arrives — a video whose first frame is late then
    /// draws transparent black rather than nothing at all.
    pub fn frame(&self) -> &[u8] {
        &self.rgba
    }

    /// Drain events and render a frame when libmpv has one. `Some(rgba)`
    /// is a fresh frame the caller must upload; `None` means "nothing new
    /// this tick" (or the decoder has failed) and the layer keeps its
    /// current texture.
    ///
    /// The returned slice is `width * height * 4` bytes of RGBA8 with the
    /// alpha channel forced opaque: libmpv's `rgb0` writes an undefined
    /// padding byte there, and sampling it as alpha would make a video
    /// layer randomly transparent.
    pub fn poll_frame(&mut self) -> Option<&[u8]> {
        if self.failed {
            return None;
        }
        if let Err(detail) = self.drain_events() {
            self.fail(&detail);
            return None;
        }
        // SAFETY: the context is valid for the decoder's lifetime.
        let flags = unsafe { kwe_mpv::mpv_render_context_update(self.render_ctx) };
        let pending = flags & kwe_mpv::MPV_RENDER_UPDATE_FRAME != 0
            || self.update_flag.swap(false, Ordering::Acquire);
        if !pending && self.rendered_any {
            return None;
        }
        if let Err(detail) = self.render() {
            self.fail(&detail);
            return None;
        }
        self.rendered_any = true;
        // rgb0's fourth byte is padding, not alpha. Force it opaque so the
        // compositor's src-over blend uses the layer alpha alone.
        for pixel in self.rgba.chunks_exact_mut(4) {
            pixel[3] = 255;
        }
        Some(&self.rgba)
    }

    /// Mark the decoder dead with a bounded one-time diagnostic. Called at
    /// most once per decoder: `failed` gates every later poll.
    fn fail(&mut self, detail: &str) {
        self.failed = true;
        eprintln!("event=renderer.scene.video_error detail={detail}");
    }

    fn set_option(&mut self, name: &str, value: &str) -> Result<(), String> {
        let name_c = CString::new(name).map_err(|_| format!("option {name} contains a NUL"))?;
        let value_c =
            CString::new(value).map_err(|_| format!("value for {name} contains a NUL"))?;
        // MPV_FORMAT_STRING takes the ADDRESS of the char* (client.h), not
        // the string itself.
        let value_pointer: *const c_char = value_c.as_ptr();
        // SAFETY: both CStrings and the pointer variable outlive the call.
        let code = unsafe {
            kwe_mpv::mpv_set_option(
                self.handle,
                name_c.as_ptr(),
                kwe_mpv::MPV_FORMAT_STRING,
                (&value_pointer as *const *const c_char).cast(),
            )
        };
        if code < 0 {
            return Err(format!(
                "mpv option {name}={value} failed: {}",
                kwe_mpv::error_string(code)
            ));
        }
        Ok(())
    }

    fn command(&mut self, args: &[&str]) -> Result<(), String> {
        let owned = args
            .iter()
            .map(|arg| {
                CString::new(*arg).map_err(|_| "command argument contains a NUL".to_string())
            })
            .collect::<Result<Vec<_>, String>>()?;
        let mut pointers: Vec<*const c_char> =
            owned.iter().map(|argument| argument.as_ptr()).collect();
        pointers.push(std::ptr::null());
        // SAFETY: the NULL-terminated array is valid for the call; libmpv
        // copies whatever it keeps.
        let code = unsafe { kwe_mpv::mpv_command(self.handle, pointers.as_ptr()) };
        if code < 0 {
            return Err(format!(
                "mpv command {} failed: {}",
                args[0],
                kwe_mpv::error_string(code)
            ));
        }
        Ok(())
    }

    fn set_property_flag(&mut self, name: &str, value: bool) -> Result<(), String> {
        let name_c = CString::new(name).map_err(|_| format!("property {name} contains a NUL"))?;
        let value_int = i32::from(value);
        // SAFETY: MPV_FORMAT_FLAG reads one int immediately.
        let code = unsafe {
            kwe_mpv::mpv_set_property(
                self.handle,
                name_c.as_ptr(),
                kwe_mpv::MPV_FORMAT_FLAG,
                (&value_int as *const i32).cast(),
            )
        };
        if code < 0 {
            return Err(format!(
                "mpv property {name} failed: {}",
                kwe_mpv::error_string(code)
            ));
        }
        Ok(())
    }

    /// One int64 property, or `None` when unreadable (not yet known,
    /// unsupported, or negative — a negative size is nonsense).
    /// A published frame size, or `None` while either half is still
    /// unset. libmpv reports 0 (or nothing at all) for both properties
    /// until the relevant stage has run, so a zero is "not yet", never a
    /// real size.
    fn property_size(&mut self, width: &str, height: &str) -> Option<(i64, i64)> {
        let width = self.int_property(width).unwrap_or(0);
        let height = self.int_property(height).unwrap_or(0);
        (width > 0 && height > 0).then_some((width, height))
    }

    fn int_property(&mut self, name: &str) -> Option<i64> {
        let name = CString::new(name).ok()?;
        let mut value = 0_i64;
        // SAFETY: MPV_FORMAT_INT64 writes exactly one i64 into `value`.
        let code = unsafe {
            kwe_mpv::mpv_get_property(
                self.handle,
                name.as_ptr(),
                kwe_mpv::MPV_FORMAT_INT64,
                (&mut value as *mut i64).cast(),
            )
        };
        (code >= 0 && value > 0).then_some(value)
    }

    /// One nonblocking-or-timed event, copied into a local enum (the
    /// pointer libmpv returns is only valid until the next wait_event).
    fn wait_event(&mut self, timeout: f64) -> Result<Option<Event>, String> {
        // SAFETY: the handle is valid; the returned pointer is only read to
        // copy scalars before any further libmpv call.
        let event = unsafe { kwe_mpv::mpv_wait_event(self.handle, timeout) };
        if event.is_null() {
            return Ok(None);
        }
        // SAFETY: non-null means libmpv returned a live event record.
        let event = unsafe { &*event };
        let kind = match event.event_id {
            kwe_mpv::MPV_EVENT_END_FILE => {
                // SAFETY: `data` points at mpv_event_end_file for this id.
                let end = unsafe { &*(event.data.cast::<kwe_mpv::mpv_event_end_file>()) };
                Event::EndFile { reason: end.reason }
            }
            kwe_mpv::MPV_EVENT_SHUTDOWN => Event::Shutdown,
            kwe_mpv::MPV_EVENT_FILE_LOADED => Event::FileLoaded,
            _ => Event::Ignored,
        };
        Ok(Some(kind))
    }

    fn drain_events(&mut self) -> Result<(), String> {
        for _ in 0..MAX_EVENTS_PER_TICK {
            let Some(event) = self.wait_event(0.0)? else {
                break;
            };
            match event {
                // EOF with loop-file=inf restarts the file; EOF without it
                // is the end of a non-looping video, and keep-open=yes
                // holds the last frame. Neither is an error.
                Event::EndFile { reason } if reason != kwe_mpv::MPV_END_FILE_REASON_EOF => {
                    return Err(format!("libmpv end_file reason {reason}"));
                }
                Event::Shutdown => return Err("libmpv core shutdown".into()),
                Event::EndFile { .. } | Event::FileLoaded | Event::Ignored => {}
            }
        }
        Ok(())
    }

    fn render(&mut self) -> Result<(), String> {
        let mut size = [self.width as c_int, self.height as c_int];
        let format = CString::new(SW_FORMAT).map_err(|_| "invalid SW format string".to_string())?;
        // The stride is bounded by MAX_VIDEO_DIMENSION * 4 (15360), so the
        // c_int conversion cannot overflow.
        let mut stride = (self.width as usize * 4) as c_int;
        // 0: never block on target-time pacing inside the render call — the
        // scene worker's frame loop owns pacing.
        let block_for_target_time = 0_i32;
        let mut params = [
            kwe_mpv::mpv_render_param {
                type_: kwe_mpv::MPV_RENDER_PARAM_SW_SIZE,
                data: (&mut size as *mut [c_int; 2]).cast(),
            },
            kwe_mpv::mpv_render_param {
                type_: kwe_mpv::MPV_RENDER_PARAM_SW_FORMAT,
                data: format.as_ptr().cast_mut().cast(),
            },
            kwe_mpv::mpv_render_param {
                type_: kwe_mpv::MPV_RENDER_PARAM_SW_STRIDE,
                data: (&mut stride as *mut c_int).cast(),
            },
            kwe_mpv::mpv_render_param {
                type_: kwe_mpv::MPV_RENDER_PARAM_SW_POINTER,
                data: self.rgba.as_mut_ptr().cast(),
            },
            kwe_mpv::mpv_render_param {
                type_: kwe_mpv::MPV_RENDER_PARAM_BLOCK_FOR_TARGET_TIME,
                data: (&block_for_target_time as *const i32).cast_mut().cast(),
            },
            kwe_mpv::mpv_render_param {
                type_: kwe_mpv::MPV_RENDER_PARAM_INVALID,
                data: std::ptr::null_mut(),
            },
        ];
        // SAFETY: the context is valid, the destination buffer is exactly
        // height * stride bytes, and every param outlives the call. This
        // runs on the thread that created the context (render.h).
        let code =
            unsafe { kwe_mpv::mpv_render_context_render(self.render_ctx, params.as_mut_ptr()) };
        if code < 0 {
            return Err(format!(
                "mpv_render_context_render failed: {}",
                kwe_mpv::error_string(code)
            ));
        }
        Ok(())
    }
}

impl Drop for VideoDecoder {
    fn drop(&mut self) {
        if !self.render_ctx.is_null() {
            // SAFETY: frees the render context; after this the update
            // callback cannot fire, so the boxed flag is safe to drop.
            unsafe { kwe_mpv::mpv_render_context_free(self.render_ctx) };
            self.render_ctx = std::ptr::null_mut();
        }
        if !self.handle.is_null() {
            // SAFETY: libmpv requires the render context to be freed first
            // (done above); the handle is destroyed exactly once.
            unsafe { kwe_mpv::mpv_terminate_destroy(self.handle) };
            self.handle = std::ptr::null_mut();
        }
    }
}

/// The subset of libmpv events the scene worker acts on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Event {
    EndFile { reason: c_int },
    Shutdown,
    FileLoaded,
    Ignored,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn playback_rate_clamps_into_range() {
        assert_eq!(clamp_playback_rate(1.0), 1.0);
        assert_eq!(clamp_playback_rate(0.5), 0.5);
        assert_eq!(clamp_playback_rate(100.0), MAX_PLAYBACK_RATE);
        assert_eq!(clamp_playback_rate(0.0), MIN_PLAYBACK_RATE);
        assert_eq!(clamp_playback_rate(-4.0), MIN_PLAYBACK_RATE);
        // Non-finite rates fall back to 1.0 instead of reaching libmpv.
        assert_eq!(clamp_playback_rate(f64::NAN), 1.0);
        assert_eq!(clamp_playback_rate(f64::INFINITY), 1.0);
        assert_eq!(clamp_playback_rate(f64::NEG_INFINITY), 1.0);
    }

    #[test]
    fn video_size_caps_are_exact() {
        assert!(video_size_allowed(1920, 1080));
        assert!(video_size_allowed(MAX_VIDEO_DIMENSION, 2160));
        assert!(!video_size_allowed(0, 1080));
        assert!(!video_size_allowed(1920, 0));
        assert!(!video_size_allowed(MAX_VIDEO_DIMENSION + 1, 1));
        assert!(!video_size_allowed(1, MAX_VIDEO_DIMENSION + 1));
        // Within both edges but past the pixel budget.
        assert!(!video_size_allowed(
            MAX_VIDEO_DIMENSION,
            MAX_VIDEO_DIMENSION
        ));
        // An absurd pair cannot wrap the multiply into a pass.
        assert!(!video_size_allowed(u32::MAX, u32::MAX));
    }

    #[test]
    fn concurrency_cap_is_two() {
        // The cap is load-bearing: it is what keeps a scene declaring
        // twenty video layers from spawning twenty libmpv cores.
        assert_eq!(MAX_VIDEO_LAYERS, 2);
    }
}
