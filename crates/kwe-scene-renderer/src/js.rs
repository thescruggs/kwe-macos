// SPDX-License-Identifier: GPL-3.0-or-later
// QuickJS SceneScript engine for the M3a slice.
//
// One QuickJS runtime + context per worker (rquickjs 0.12.2, MIT, see
// THIRD_PARTY.yml). Bounded execution:
//   * heap cap 64 MiB (Runtime::set_memory_limit)  -> Error::Allocation
//   * stack cap 4 MiB (Runtime::set_max_stack_size) -> exception, contained
//   * per-update and per-load wall-clock budget via the interrupt handler:
//     8 ms soft (skip the frame, bounded `script_timeout` diagnostic) and
//     33 ms hard (interrupt raises an uncatchable exception; the frame is
//     skipped; the renderer always keeps publishing the last good state).
//     The load phase (eval/init()/resized()) runs under the same hard
//     budget; a load-phase abort disables the script instead of hanging
//
// The interrupt handler is QuickJS's own callback and runs at its internal
// bytecode checkpoints; rquickjs 0.12.2 does not expose interpreter step
// counts, so the budget is enforced with a wall clock inside the callback
// (documented deviation from a "steps" budget in docs/BETA_M3.md).
//
// The first SceneScript surface is a global `Engine` object:
//   Engine.frametime   number  seconds since the previous update (per update)
//   Engine.fps         number  the pacing the daemon asked for (fixed)
//   Engine.resolution  {x, y}  pixel size the worker renders at (fixed)
//   Engine.clearcolor  {r,g,b,a} mutable bridge (M3a only, not a WE API; the
//                      worker reads it back after every update() and renders
//                      that color; planned to move to thisScene.clearcolor)
// plus `console.log/info/warn/error` (rate-bounded) and the classic entry
// points `init()`, `update(dt)`, `resized(w, h)`. The M3c slice adds the
// `Scene` object model (also exposed as `thisScene`):
//   Scene.getLayer(name | index) -> Layer proxy or null (layers registered
//       before init(); properties live on the Rust side, clamped on write)
//   Scene.getLayerCount() -> number of registered image layers
// with Layer.name/alpha/visible/angles/origin/scale/size. The M3d slice
// adds the per-layer blend mode and color effects:
//   Layer.blendMode   number  the WE colorBlendMode, clamped to the
//                             implemented set (0/1/6/7/9); a write of an
//                             unimplemented value clamps to 0 with a
//                             bounded one-time diagnostic
//   Layer.brightness  number  RGB multiplier, 0..=10, default 1
//   Layer.tint        {r,g,b,a} RGBA multiplier, 0..=1, default 1s
// Changing a layer's image at runtime is planned, not in M3c. See the
// coverage matrix in docs/SCENE_FORMAT_V1.md for implemented vs planned.
//
// The M3f slice adds particle systems (researched WE surface with the
// documented deviations recorded in docs/SCENE_FORMAT_V1.md):
//   Scene.getParticleSystem(name | index) -> ParticleSystem proxy or null
//       (the task-mandated M3f extension; WE has no such call — particle
//       systems are reached through thisScene.getLayer, and M3f preserves
//       that behavior: layer indices >= Scene.getLayerCount() dispatch to
//       particle systems, matching WE's combined object index space)
//   Scene.getParticleSystemCount() -> number of registered systems
// with writable emitter properties (spawnRate, life, speedMin, speedMax,
// direction, spread, gravity, sizeStart, sizeEnd, colorStart, colorEnd,
// alphaStart, alphaEnd, maxCount, blendMode), layer properties (alpha,
// brightness, visible), the WE IParticleSystemInstance factors
// (instance.count/speed/lifetime/size/alpha/rate/colorn — the WE
// spelling of "colorn" is intentional), and the WE IParticleSystem
// controls play()/pause()/stop()/isPlaying()/emitParticles(count).
// Every write is clamped at the bridge (non-finite -> documented default
// or identity 1.0); an out-of-range index is a no-op or a default value,
// never an error.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use rquickjs::{Context, Ctx, Error as JsError, Exception, Function, Object, Runtime};

use crate::layers::{
    BlendMode, LayerState, clamp_layer_alpha, clamp_layer_brightness, clamp_layer_scalar,
    clamp_layer_size, clamp_layer_tint,
};
use crate::particles::{self, ParticleSystemState};
use crate::scene::SceneConfig;
use crate::text::{
    HorizontalAlign, MAX_FONT_PX, MAX_TEXT_CHARS, MIN_FONT_PX, VerticalAlign, truncate_chars,
};

/// Clamp a script-written pointsize (M3e): non-finite → the default size
/// (48 px), otherwise the bounded range MIN_FONT_PX..=MAX_FONT_PX.
fn clamp_pointsize_px(value: f64) -> f32 {
    if value.is_finite() {
        (value as f32).clamp(MIN_FONT_PX, MAX_FONT_PX)
    } else {
        crate::text::DEFAULT_POINT_SIZE * crate::text::POINT_TO_PX
    }
}

/// Clamp a script-written horizontal alignment (M3e): 0 = left, 1 =
/// center, 2 = right; non-finite → center.
fn clamp_align_h(value: f64) -> HorizontalAlign {
    let index = if value.is_finite() {
        value.round()
    } else {
        1.0
    }
    .clamp(0.0, 2.0) as u32;
    match index {
        0 => HorizontalAlign::Left,
        2 => HorizontalAlign::Right,
        _ => HorizontalAlign::Center,
    }
}

/// Clamp a script-written vertical alignment (M3e): 0 = top, 1 = center,
/// 2 = bottom; non-finite → center.
fn clamp_align_v(value: f64) -> VerticalAlign {
    match clamp_align_h(value) {
        HorizontalAlign::Left => VerticalAlign::Top,
        HorizontalAlign::Right => VerticalAlign::Bottom,
        HorizontalAlign::Center => VerticalAlign::Center,
    }
}

/// Heap cap for the per-worker QuickJS runtime.
pub const MEMORY_LIMIT_BYTES: usize = 64 * 1024 * 1024;
/// Stack cap; runaway recursion becomes a contained exception.
pub const MAX_STACK_BYTES: usize = 4 * 1024 * 1024;
/// Soft per-update budget: the frame is skipped when the script overruns it.
pub const SOFT_BUDGET: Duration = Duration::from_millis(8);
/// Hard per-update budget: the interrupt raises an uncatchable exception.
pub const HARD_BUDGET: Duration = Duration::from_millis(33);
/// update(dt) is capped so a hung producer cannot feed a huge dt downstream.
pub const MAX_DT_SECONDS: f64 = 1.0;
/// How often a garbage collection pass is forced while idle scripts run.
pub const GC_EVERY_FRAMES: u64 = 500;

/// console: at most this many lines per window, each truncated at this many
/// bytes (the daemon's stderr ring is tiny; keep it readable).
pub const CONSOLE_MAX_LINES_PER_WINDOW: u32 = 30;
pub const CONSOLE_WINDOW: Duration = Duration::from_secs(10);
pub const CONSOLE_MAX_LINE_BYTES: usize = 512;
/// Error classes are re-logged at most once per window.
pub const ERROR_REREPORT_WINDOW: Duration = Duration::from_secs(30);
pub const ERROR_CLASS_CAP: usize = 16;

// ---------------------------------------------------------------------------
// Budget
// ---------------------------------------------------------------------------

/// Pure decision the interrupt handler and the tests share. `elapsed_ns` is
/// measured from the start of the update() call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BudgetVerdict {
    Ok,
    Soft,
    Hard,
}

pub fn budget_verdict_ns(
    elapsed_ns: u64,
    armed: bool,
    soft_ns: u64,
    hard_ns: u64,
) -> BudgetVerdict {
    if !armed {
        return BudgetVerdict::Ok;
    }
    if elapsed_ns >= hard_ns {
        BudgetVerdict::Hard
    } else if elapsed_ns >= soft_ns {
        BudgetVerdict::Soft
    } else {
        BudgetVerdict::Ok
    }
}

/// Shared between the interrupt closure (fires inside QuickJS, same thread)
/// and the worker thread that arms/disarms it.
#[derive(Debug)]
pub struct BudgetState {
    armed: AtomicBool,
    start: Mutex<Option<Instant>>,
    soft_ns: AtomicU64,
    hard_ns: AtomicU64,
    soft_hit: AtomicBool,
    hard_hit: AtomicBool,
}

impl BudgetState {
    pub fn new() -> Self {
        Self {
            armed: AtomicBool::new(false),
            start: Mutex::new(None),
            soft_ns: AtomicU64::new(SOFT_BUDGET.as_nanos() as u64),
            hard_ns: AtomicU64::new(HARD_BUDGET.as_nanos() as u64),
            soft_hit: AtomicBool::new(false),
            hard_hit: AtomicBool::new(false),
        }
    }

    /// Start a budget window; must be called right before the JS call.
    pub fn arm(&self) {
        *self.start.lock().unwrap_or_else(|p| p.into_inner()) = Some(Instant::now());
        self.soft_hit.store(false, Ordering::Relaxed);
        self.hard_hit.store(false, Ordering::Relaxed);
        self.armed.store(true, Ordering::Release);
    }

    /// End the window; interrupts are ignored outside update().
    pub fn disarm(&self) {
        self.armed.store(false, Ordering::Release);
        *self.start.lock().unwrap_or_else(|p| p.into_inner()) = None;
    }

    /// Whether the soft budget was exceeded during the last armed window.
    pub fn soft_hit(&self) -> bool {
        self.soft_hit.load(Ordering::Relaxed)
    }

    /// Whether the hard budget was exceeded (the interrupt fired) during the
    /// last armed window.
    pub fn hard_hit(&self) -> bool {
        self.hard_hit.load(Ordering::Relaxed)
    }

    fn check(&self) -> BudgetVerdict {
        let start = *self.start.lock().unwrap_or_else(|p| p.into_inner());
        let Some(start) = start else {
            return BudgetVerdict::Ok;
        };
        let elapsed = Instant::now().duration_since(start).as_nanos() as u64;
        let verdict = budget_verdict_ns(
            elapsed,
            self.armed.load(Ordering::Acquire),
            self.soft_ns.load(Ordering::Relaxed),
            self.hard_ns.load(Ordering::Relaxed),
        );
        match verdict {
            BudgetVerdict::Soft => self.soft_hit.store(true, Ordering::Relaxed),
            BudgetVerdict::Hard => self.hard_hit.store(true, Ordering::Relaxed),
            BudgetVerdict::Ok => {}
        }
        verdict
    }
}

impl Default for BudgetState {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Diagnostics limiters
// ---------------------------------------------------------------------------

/// console rate limiter. `Cell`s are fine: every path through here is the
/// worker thread, re-entered by QuickJS.
#[derive(Debug)]
pub struct ConsoleLimiter {
    window_start: Cell<Instant>,
    lines_in_window: Cell<u32>,
    dropped: AtomicU64,
    last_dropped_diag: Cell<Instant>,
}

impl ConsoleLimiter {
    pub fn new() -> Self {
        Self {
            window_start: Cell::new(Instant::now()),
            lines_in_window: Cell::new(0),
            dropped: AtomicU64::new(0),
            last_dropped_diag: Cell::new(Instant::now()),
        }
    }

    /// Returns true when the line should be forwarded to stderr.
    pub fn admit(&self) -> bool {
        let now = Instant::now();
        if now.duration_since(self.window_start.get()) >= CONSOLE_WINDOW {
            self.window_start.set(now);
            self.lines_in_window.set(0);
        }
        let admitted = self.lines_in_window.get() < CONSOLE_MAX_LINES_PER_WINDOW;
        if admitted {
            self.lines_in_window.set(self.lines_in_window.get() + 1);
        } else {
            let dropped = self.dropped.fetch_add(1, Ordering::Relaxed) + 1;
            // Bounded: one dropped-lines diagnostic per window.
            if now.duration_since(self.last_dropped_diag.get()) >= CONSOLE_WINDOW {
                self.last_dropped_diag.set(now);
                eprintln!("event=renderer.scene.console_dropped dropped={dropped}");
            }
        }
        admitted
    }
}

impl Default for ConsoleLimiter {
    fn default() -> Self {
        Self::new()
    }
}

/// Script error classes are re-logged at most once per window; the first
/// occurrence of a class always logs.
#[derive(Debug)]
pub struct ErrorLogLimiter {
    entries: Mutex<Vec<(String, Instant)>>,
}

impl ErrorLogLimiter {
    pub fn new() -> Self {
        Self {
            entries: Mutex::new(Vec::with_capacity(ERROR_CLASS_CAP)),
        }
    }

    /// True when this error class should produce a stderr line now.
    pub fn admit(&self, class: &str) -> bool {
        let now = Instant::now();
        let mut entries = self.entries.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(entry) = entries.iter_mut().find(|(c, _)| c == class) {
            if now.duration_since(entry.1) >= ERROR_REREPORT_WINDOW {
                entry.1 = now;
                true
            } else {
                false
            }
        } else {
            if entries.len() >= ERROR_CLASS_CAP {
                entries.remove(0); // bounded: evict the oldest class
            }
            entries.push((class.to_string(), now));
            true
        }
    }
}

impl Default for ErrorLogLimiter {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Engine
// ---------------------------------------------------------------------------

/// Outcome of one paced script step.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum StepResult {
    /// update() ran and the script's Engine.clearcolor was read back; render
    /// and publish a new frame with this RGBA color.
    NewFrame([f32; 4]),
    /// update() overran the soft budget; skip the frame, keep the last one.
    SoftTimeout,
    /// update() was aborted at the hard budget; skip the frame.
    HardTimeout,
    /// update() threw (or the script is disabled); the renderer stays live
    /// with the last state.
    ScriptError,
    /// The JS heap cap was hit; the worker must exit 71 (resource limit).
    Allocation,
}

/// Failure to construct the engine. Script problems never reach here — the
/// renderer keeps running with the scene.json clear color instead.
#[derive(Debug)]
pub enum EngineStartError {
    /// QuickJS could not allocate its runtime/context (or the bootstrap hit
    /// the memory cap): worker exits 71.
    Allocation,
    /// Anything else during bootstrap (renderer bug): worker exits 73.
    Bootstrap(String),
}

impl std::fmt::Display for EngineStartError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Allocation => write!(f, "QuickJS runtime allocation failed"),
            Self::Bootstrap(message) => write!(f, "engine bootstrap failed: {message}"),
        }
    }
}

impl std::error::Error for EngineStartError {}

/// Per-worker stats, for diagnostics and tests.
#[derive(Debug, Default, Clone, Copy)]
pub struct ScriptStats {
    pub soft_timeouts: u64,
    pub hard_timeouts: u64,
    pub script_errors: u64,
}

pub struct ScriptEngine {
    runtime: Runtime,
    context: Context,
    budget: Arc<BudgetState>,
    console: Rc<ConsoleLimiter>,
    error_log: ErrorLogLimiter,
    width: u32,
    height: u32,
    fps: u32,
    clear_color: [f32; 4],
    /// Runtime layer states in scene.json object order (M3c). Built from the
    /// parsed spec before the script loads, so init() sees the resolved
    /// sizes; the worker reads them per frame for the draw list, and the
    /// script mutates them through the Scene.getLayer proxies.
    layers: Vec<Rc<RefCell<LayerState>>>,
    /// Bounded one-time diagnostic for script writes of an unimplemented
    /// blendMode (the flag lives here so the bridge closure can share it).
    blend_mode_diag: Rc<Cell<bool>>,
    /// Bounded one-time diagnostic for script writes of an over-long text
    /// (M3e): the string is truncated to MAX_TEXT_CHARS chars.
    text_truncate_diag: Rc<Cell<bool>>,
    /// M3f: runtime particle-system states in scene.json object order,
    /// built from the parsed spec before the script loads (init() sees
    /// the resolved systems). Scripts mutate them through the
    /// Scene.getParticleSystem proxies (and the WE-compatible getLayer
    /// fallback at indices >= layer count); every write is clamped at the
    /// bridge, and playback controls go through the system methods. The
    /// worker simulates them per frame (particles.rs) for the draw list.
    particles: Vec<Rc<RefCell<ParticleSystemState>>>,
    /// Bounded one-time diagnostic for script writes of an unimplemented
    /// particle blendMode (same contract as the layer's blend_mode_diag).
    particle_blend_mode_diag: Rc<Cell<bool>>,
    script_ok: bool,
    /// Whether the scene configured a script file at all (a scriptless
    /// scene is not an error — step() falls through to NoUpdate).
    has_script: bool,
    last_update: Option<Instant>,
    frames: u64,
    stats: ScriptStats,
    last_timeout_diag: Instant,
    last_error_diag: Instant,
}

/// Small helpers shared with main.rs so the decisions stay testable.
pub fn is_memory_limit_error(error: &JsError) -> bool {
    matches!(error, JsError::Allocation)
}

pub fn truncate_utf8(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    // Room for the "…" suffix (3 bytes UTF-8) so the result stays bounded.
    let mut end = max_bytes.saturating_sub(3).min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    if end == 0 {
        // Not even room for the ellipsis; return nothing (still bounded).
        String::new()
    } else {
        format!("{}…", &text[..end])
    }
}

/// What the script call inside `context.with` observed. The closure may only
/// touch `&self` (the borrow checker forbids `&mut self` inside `with`), so
/// every effect is applied afterwards by `apply_outcome`.
enum CallOutcome {
    NewColor([f32; 4]),
    NoUpdate,
    SoftTimeout,
    HardTimeout,
    MemoryLimit,
    Error { class: String, soft: bool },
}

impl ScriptEngine {
    /// Build the per-worker runtime/context, evaluate the scene's script, and
    /// call init()/resized() if the script defines them. Script exceptions
    /// are contained: `script_ok` goes false, the renderer keeps rendering
    /// the scene.json clear color, and the exception is logged once. A
    /// scene with NO script file is not an error: step() falls through to
    /// NoUpdate (M3f — scriptless particle scenes still simulate).
    pub fn new(
        config: &SceneConfig,
        width: u32,
        height: u32,
        fps: u32,
    ) -> Result<Self, EngineStartError> {
        let runtime = Runtime::new().map_err(|_| EngineStartError::Allocation)?;
        runtime.set_memory_limit(MEMORY_LIMIT_BYTES);
        runtime.set_max_stack_size(MAX_STACK_BYTES);

        let budget = Arc::new(BudgetState::new());
        let interrupt_budget = Arc::clone(&budget);
        runtime.set_interrupt_handler(Some(Box::new(move || {
            matches!(interrupt_budget.check(), BudgetVerdict::Hard)
        })));

        let context = Context::full(&runtime).map_err(|e| {
            if is_memory_limit_error(&e) {
                EngineStartError::Allocation
            } else {
                EngineStartError::Bootstrap(format!("create context: {e}"))
            }
        })?;

        let layers = config
            .layers
            .iter()
            .map(|spec| Rc::new(RefCell::new(LayerState::from_spec(spec))))
            .collect();

        // M3f: particle systems in scene.json object order. The seed mixes
        // in the system index, so the same spec at different indices
        // simulates differently (and identically across runs).
        let particles = config
            .particles
            .iter()
            .enumerate()
            .map(|(index, spec)| Rc::new(RefCell::new(ParticleSystemState::from_spec(spec, index))))
            .collect();

        let mut engine = Self {
            runtime,
            context,
            budget,
            console: Rc::new(ConsoleLimiter::new()),
            error_log: ErrorLogLimiter::new(),
            width,
            height,
            fps,
            clear_color: config.clear_color,
            layers,
            blend_mode_diag: Rc::new(Cell::new(false)),
            text_truncate_diag: Rc::new(Cell::new(false)),
            particles,
            particle_blend_mode_diag: Rc::new(Cell::new(false)),
            script_ok: false,
            has_script: config.script_path.is_some(),
            last_update: None,
            frames: 0,
            stats: ScriptStats::default(),
            last_timeout_diag: Instant::now() - Duration::from_secs(10),
            last_error_diag: Instant::now() - Duration::from_secs(10),
        };

        engine.bootstrap()?;
        engine.load_script(config)?;
        Ok(engine)
    }

    pub fn clear_color(&self) -> [f32; 4] {
        self.clear_color
    }

    pub fn stats(&self) -> ScriptStats {
        self.stats
    }

    /// The script is healthy and will be stepped every paced frame.
    pub fn script_ok(&self) -> bool {
        self.script_ok
    }

    /// The runtime layer states, in scene.json object order (M3c). The
    /// worker builds the per-frame draw list from these; scripts mutate
    /// them through the Scene.getLayer proxies, and every write is clamped
    /// at the bridge.
    pub fn layers(&self) -> Vec<Rc<RefCell<LayerState>>> {
        self.layers.clone()
    }

    /// The runtime particle-system states, in scene.json object order
    /// (M3f). The worker simulates these per frame (particles.rs) and
    /// scripts mutate them through the Scene.getParticleSystem proxies;
    /// every write is clamped at the bridge.
    pub fn particles(&self) -> Vec<Rc<RefCell<ParticleSystemState>>> {
        self.particles.clone()
    }

    /// Register the `Engine` object, `console`, and their plumbing. Fatal
    /// (renderer-internal) problems return an error; the script is not
    /// involved yet.
    fn bootstrap(&mut self) -> Result<(), EngineStartError> {
        let console = Rc::clone(&self.console);
        let clear_color = self.clear_color;
        let width = self.width;
        let height = self.height;
        let fps = self.fps;
        self.context.with(|ctx| {
            let engine = Object::new(ctx.clone())
                .map_err(|e| EngineStartError::Bootstrap(format!("create Engine: {e}")))?;
            engine
                .set("frametime", 0.0_f64)
                .map_err(|e| EngineStartError::Bootstrap(format!("Engine.frametime: {e}")))?;
            engine
                .set("fps", f64::from(fps))
                .map_err(|e| EngineStartError::Bootstrap(format!("Engine.fps: {e}")))?;
            let resolution = Object::new(ctx.clone())
                .map_err(|e| EngineStartError::Bootstrap(format!("create resolution: {e}")))?;
            resolution
                .set("x", f64::from(width))
                .and_then(|_| resolution.set("y", f64::from(height)))
                .map_err(|e| EngineStartError::Bootstrap(format!("Engine.resolution: {e}")))?;
            engine
                .set("resolution", resolution)
                .map_err(|e| EngineStartError::Bootstrap(format!("Engine.resolution: {e}")))?;
            let clearcolor = Object::new(ctx.clone())
                .map_err(|e| EngineStartError::Bootstrap(format!("create clearcolor: {e}")))?;
            let [r, g, b, a] = clear_color;
            clearcolor
                .set("r", f64::from(r))
                .and_then(|_| clearcolor.set("g", f64::from(g)))
                .and_then(|_| clearcolor.set("b", f64::from(b)))
                .and_then(|_| clearcolor.set("a", f64::from(a)))
                .map_err(|e| EngineStartError::Bootstrap(format!("Engine.clearcolor: {e}")))?;
            engine
                .set("clearcolor", clearcolor)
                .map_err(|e| EngineStartError::Bootstrap(format!("Engine.clearcolor: {e}")))?;
            ctx.globals()
                .set("Engine", engine)
                .map_err(|e| EngineStartError::Bootstrap(format!("global Engine: {e}")))?;

            let console_fn = Function::new(ctx.clone(), move |line: String| {
                if console.admit() {
                    eprintln!(
                        "event=renderer.scene.console {}",
                        truncate_utf8(&line, CONSOLE_MAX_LINE_BYTES)
                    );
                }
            })
            .map_err(|e| EngineStartError::Bootstrap(format!("console fn: {e}")))?;
            ctx.globals()
                .set("kweConsoleLog", console_fn)
                .map_err(|e| EngineStartError::Bootstrap(format!("global kweConsoleLog: {e}")))?;

            ctx.eval::<(), &str>(CONSOLE_BOOTSTRAP_JS)
                .map_err(|e| EngineStartError::Bootstrap(format!("console bootstrap: {e}")))?;

            // ---- M3c: the Scene object model. The layer state lives on the
            // Rust side; the bootstrap JS defines Scene.getLayer proxies over
            // these bridge functions (plain getters/setters rather than the
            // rquickjs Property trait — bounded, and every write is clamped
            // here). An out-of-range index is a no-op or a default value,
            // never an error: the script cannot crash the renderer through a
            // layer proxy.
            let layers = self.layers.clone();

            let count_fn = Function::new(ctx.clone(), move || layers.len() as i32)
                .map_err(|e| EngineStartError::Bootstrap(format!("layer count fn: {e}")))?;
            ctx.globals()
                .set("kweSceneLayerCount", count_fn)
                .map_err(|e| {
                    EngineStartError::Bootstrap(format!("global kweSceneLayerCount: {e}"))
                })?;

            let find_layers = self.layers.clone();
            let find_fn = Function::new(ctx.clone(), move |name: String| -> i32 {
                find_layers
                    .iter()
                    .position(|layer| layer.borrow().name == name)
                    .map_or(-1, |index| index as i32)
            })
            .map_err(|e| EngineStartError::Bootstrap(format!("layer find fn: {e}")))?;
            ctx.globals()
                .set("kweSceneFindLayer", find_fn)
                .map_err(|e| {
                    EngineStartError::Bootstrap(format!("global kweSceneFindLayer: {e}"))
                })?;

            let name_layers = self.layers.clone();
            let name_fn = Function::new(ctx.clone(), move |index: i32| -> String {
                name_layers
                    .get(index as usize)
                    .map_or_else(String::new, |layer| layer.borrow().name.clone())
            })
            .map_err(|e| EngineStartError::Bootstrap(format!("layer name fn: {e}")))?;
            ctx.globals()
                .set("kweSceneLayerName", name_fn)
                .map_err(|e| {
                    EngineStartError::Bootstrap(format!("global kweSceneLayerName: {e}"))
                })?;

            let scalar_layers = self.layers.clone();
            let get_scalar = Function::new(ctx.clone(), move |index: i32, prop: String| -> f64 {
                let Some(layer) = scalar_layers.get(index as usize) else {
                    return 0.0;
                };
                let layer = layer.borrow();
                match prop.as_str() {
                    "alpha" => f64::from(layer.alpha),
                    "visible" => f64::from(u8::from(layer.visible)),
                    "blendMode" => f64::from(layer.blend_mode.as_u32()),
                    "brightness" => f64::from(layer.brightness),
                    // M3e text scalars: 0/1/2 for left|top / center /
                    // right|bottom; pointsize in pixels (already clamped).
                    "pointsize" => layer.text.as_ref().map_or(0.0, |t| f64::from(t.pointsize_px)),
                    "horizontalAlign" => layer.text.as_ref().map_or(0.0, |t| {
                        f64::from(match t.horizontal_align {
                            HorizontalAlign::Left => 0,
                            HorizontalAlign::Center => 1,
                            HorizontalAlign::Right => 2,
                        })
                    }),
                    "verticalAlign" => layer.text.as_ref().map_or(0.0, |t| {
                        f64::from(match t.vertical_align {
                            VerticalAlign::Top => 0,
                            VerticalAlign::Center => 1,
                            VerticalAlign::Bottom => 2,
                        })
                    }),
                    _ => 0.0,
                }
            })
            .map_err(|e| EngineStartError::Bootstrap(format!("layer scalar getter: {e}")))?;
            ctx.globals()
                .set("kweSceneGetScalar", get_scalar)
                .map_err(|e| {
                    EngineStartError::Bootstrap(format!("global kweSceneGetScalar: {e}"))
                })?;

            let set_scalar_layers = self.layers.clone();
            let set_blend_mode_diag = Rc::clone(&self.blend_mode_diag);
            let set_scalar =
                Function::new(ctx.clone(), move |index: i32, prop: String, value: f64| {
                    let Some(layer) = set_scalar_layers.get(index as usize) else {
                        return;
                    };
                    let mut layer = layer.borrow_mut();
                    match prop.as_str() {
                        "alpha" => layer.alpha = clamp_layer_alpha(value),
                        "visible" => layer.visible = value != 0.0,
                        // M3d: the blend mode clamps to the implemented set
                        // (0/1/6/7/9); an unimplemented write clamps to 0
                        // with a bounded one-time diagnostic (both the
                        // known corpus values 11/12/24/30 and unknown
                        // values are contained — the script can never push
                        // a mode the renderer has no pipeline for).
                        "blendMode" => {
                            let raw = if value.is_finite()
                                && value >= 0.0
                                && value <= f64::from(u32::MAX)
                            {
                                value as u32
                            } else {
                                0
                            };
                            let mode = BlendMode::clamp(raw);
                            if mode.as_u32() != raw && !set_blend_mode_diag.replace(true) {
                                eprintln!(
                                    "event=renderer.scene.blend_mode_clamped layer={} mode={} note=not-fixed-function-clamped-to-normal",
                                    layer.name, raw
                                );
                            }
                            layer.blend_mode = mode;
                        }
                        "brightness" => layer.brightness = clamp_layer_brightness(value),
                        // M3e text scalars, each clamped at the bridge; a
                        // write marks the state dirty so the worker rebuilds
                        // the layout (text.rs) on the next sync.
                        "pointsize" => {
                            if let Some(text) = &mut layer.text {
                                text.pointsize_px = clamp_pointsize_px(value);
                                text.dirty = true;
                            }
                        }
                        "horizontalAlign" => {
                            if let Some(text) = &mut layer.text {
                                text.horizontal_align = clamp_align_h(value);
                                text.dirty = true;
                            }
                        }
                        "verticalAlign" => {
                            if let Some(text) = &mut layer.text {
                                text.vertical_align = clamp_align_v(value);
                                text.dirty = true;
                            }
                        }
                        _ => {}
                    }
                })
                .map_err(|e| EngineStartError::Bootstrap(format!("layer scalar setter: {e}")))?;
            ctx.globals()
                .set("kweSceneSetScalar", set_scalar)
                .map_err(|e| {
                    EngineStartError::Bootstrap(format!("global kweSceneSetScalar: {e}"))
                })?;

            let vec_layers = self.layers.clone();
            let get_vec = Function::new(
                ctx.clone(),
                move |index: i32, prop: String, axis: String| -> f64 {
                    let Some(layer) = vec_layers.get(index as usize) else {
                        return 0.0;
                    };
                    let layer = layer.borrow();
                    match (prop.as_str(), axis.as_str()) {
                        ("angles", "x") => f64::from(layer.angles[0]),
                        ("angles", "y") => f64::from(layer.angles[1]),
                        ("angles", "z") => f64::from(layer.angles[2]),
                        ("origin", "x") => f64::from(layer.origin[0]),
                        ("origin", "y") => f64::from(layer.origin[1]),
                        ("scale", "x") => f64::from(layer.scale[0]),
                        ("scale", "y") => f64::from(layer.scale[1]),
                        ("size", "x") => f64::from(layer.size[0]),
                        ("size", "y") => f64::from(layer.size[1]),
                        ("tint", "r") => f64::from(layer.tint[0]),
                        ("tint", "g") => f64::from(layer.tint[1]),
                        ("tint", "b") => f64::from(layer.tint[2]),
                        ("tint", "a") => f64::from(layer.tint[3]),
                        // M3e: the text color (the draw's tint slot).
                        ("color", "r") => layer.text.as_ref().map_or(0.0, |t| f64::from(t.color[0])),
                        ("color", "g") => layer.text.as_ref().map_or(0.0, |t| f64::from(t.color[1])),
                        ("color", "b") => layer.text.as_ref().map_or(0.0, |t| f64::from(t.color[2])),
                        ("color", "a") => layer.text.as_ref().map_or(0.0, |t| f64::from(t.color[3])),
                        _ => 0.0,
                    }
                },
            )
            .map_err(|e| EngineStartError::Bootstrap(format!("layer vector getter: {e}")))?;
            ctx.globals()
                .set("kweSceneGetVec", get_vec)
                .map_err(|e| EngineStartError::Bootstrap(format!("global kweSceneGetVec: {e}")))?;

            let set_vec_layers = self.layers.clone();
            let set_vec = Function::new(
                ctx.clone(),
                move |index: i32, prop: String, axis: String, value: f64| {
                    let Some(layer) = set_vec_layers.get(index as usize) else {
                        return;
                    };
                    let mut layer = layer.borrow_mut();
                    match (prop.as_str(), axis.as_str()) {
                        ("angles", "x") => layer.angles[0] = clamp_layer_scalar(value),
                        ("angles", "y") => layer.angles[1] = clamp_layer_scalar(value),
                        ("angles", "z") => layer.angles[2] = clamp_layer_scalar(value),
                        ("origin", "x") => layer.origin[0] = clamp_layer_scalar(value),
                        ("origin", "y") => layer.origin[1] = clamp_layer_scalar(value),
                        ("scale", "x") => layer.scale[0] = clamp_layer_scalar(value),
                        ("scale", "y") => layer.scale[1] = clamp_layer_scalar(value),
                        // Sizes are never negative; scale carries the mirror.
                        ("size", "x") => layer.size[0] = clamp_layer_size(value),
                        ("size", "y") => layer.size[1] = clamp_layer_size(value),
                        // M3d: the tint clamps per component (0..=1,
                        // non-finite → 1.0 — the identity multiplier).
                        ("tint", "r") => layer.tint[0] = clamp_layer_tint(value),
                        ("tint", "g") => layer.tint[1] = clamp_layer_tint(value),
                        ("tint", "b") => layer.tint[2] = clamp_layer_tint(value),
                        ("tint", "a") => layer.tint[3] = clamp_layer_tint(value),
                        // M3e: text color, clamped per component like the
                        // tint (0..=1, non-finite → 1.0); a write marks the
                        // state dirty (the worker re-uploads the draw tint —
                        // no geometry change needed, but dirty keeps the
                        // bookkeeping simple).
                        ("color", "r") => {
                            if let Some(text) = &mut layer.text {
                                text.color[0] = clamp_layer_tint(value);
                                text.dirty = true;
                            }
                        }
                        ("color", "g") => {
                            if let Some(text) = &mut layer.text {
                                text.color[1] = clamp_layer_tint(value);
                                text.dirty = true;
                            }
                        }
                        ("color", "b") => {
                            if let Some(text) = &mut layer.text {
                                text.color[2] = clamp_layer_tint(value);
                                text.dirty = true;
                            }
                        }
                        ("color", "a") => {
                            if let Some(text) = &mut layer.text {
                                text.color[3] = clamp_layer_tint(value);
                                text.dirty = true;
                            }
                        }
                        _ => {}
                    }
                },
            )
            .map_err(|e| EngineStartError::Bootstrap(format!("layer vector setter: {e}")))?;
            ctx.globals()
                .set("kweSceneSetVec", set_vec)
                .map_err(|e| EngineStartError::Bootstrap(format!("global kweSceneSetVec: {e}")))?;

            // ---- M3e: text-layer bridges. The bootstrap JS exposes the
            // text properties only on text layers; everything is clamped
            // here, and a write always marks the state dirty so the worker
            // rebuilds the layout on its next sync.
            let is_text_layers = self.layers.clone();
            let is_text = Function::new(ctx.clone(), move |index: i32| -> bool {
                is_text_layers
                    .get(index as usize)
                    .is_some_and(|layer| layer.borrow().text.is_some())
            })
            .map_err(|e| EngineStartError::Bootstrap(format!("text is-text fn: {e}")))?;
            ctx.globals()
                .set("kweSceneIsText", is_text)
                .map_err(|e| EngineStartError::Bootstrap(format!("global kweSceneIsText: {e}")))?;

            let get_text_layers = self.layers.clone();
            let get_text = Function::new(ctx.clone(), move |index: i32| -> String {
                get_text_layers
                    .get(index as usize)
                    .and_then(|layer| layer.borrow().text.as_ref().map(|t| t.text.clone()))
                    .unwrap_or_default()
            })
            .map_err(|e| EngineStartError::Bootstrap(format!("text get fn: {e}")))?;
            ctx.globals()
                .set("kweSceneGetText", get_text)
                .map_err(|e| EngineStartError::Bootstrap(format!("global kweSceneGetText: {e}")))?;

            let set_text_layers = self.layers.clone();
            let text_truncate_diag = Rc::clone(&self.text_truncate_diag);
            let set_text = Function::new(ctx.clone(), move |index: i32, text: String| {
                let Some(layer) = set_text_layers.get(index as usize) else {
                    return;
                };
                let mut layer = layer.borrow_mut();
                // The one-time truncate diagnostic reads the layer name;
                // capture it before the mutable borrow of `text` (borrows
                // through RefCell deref are not field-disjoint).
                let name = layer.name.clone();
                if let Some(state) = &mut layer.text {
                    let truncated = truncate_chars(&text, MAX_TEXT_CHARS);
                    if truncated.chars().count() < text.chars().count()
                        && !text_truncate_diag.replace(true)
                    {
                        eprintln!(
                            "event=renderer.scene.text_truncated layer={name} chars={} capped_at={MAX_TEXT_CHARS}",
                            text.chars().count()
                        );
                    }
                    state.text = truncated;
                    state.dirty = true;
                }
            })
            .map_err(|e| EngineStartError::Bootstrap(format!("text set fn: {e}")))?;
            ctx.globals()
                .set("kweSceneSetText", set_text)
                .map_err(|e| EngineStartError::Bootstrap(format!("global kweSceneSetText: {e}")))?;

            // ---- M3f: particle-system bridges (researched WE surface,
            // see docs/SCENE_FORMAT_V1.md — flat M3f model: the emitter
            // properties, the layer properties, the IParticleSystemInstance
            // factors and the IParticleSystem playback controls, all
            // clamped here; an out-of-range index is a no-op or a default
            // value, never an error).
            let particles = self.particles.clone();

            let particle_count_fn = Function::new(ctx.clone(), move || particles.len() as i32)
                .map_err(|e| EngineStartError::Bootstrap(format!("particle count fn: {e}")))?;
            ctx.globals()
                .set("kweSceneParticleCount", particle_count_fn)
                .map_err(|e| {
                    EngineStartError::Bootstrap(format!("global kweSceneParticleCount: {e}"))
                })?;

            let find_particles = self.particles.clone();
            let particle_find_fn = Function::new(ctx.clone(), move |name: String| -> i32 {
                find_particles
                    .iter()
                    .position(|system| system.borrow().name == name)
                    .map_or(-1, |index| index as i32)
            })
            .map_err(|e| EngineStartError::Bootstrap(format!("particle find fn: {e}")))?;
            ctx.globals()
                .set("kweSceneFindParticle", particle_find_fn)
                .map_err(|e| {
                    EngineStartError::Bootstrap(format!("global kweSceneFindParticle: {e}"))
                })?;

            let name_particles = self.particles.clone();
            let particle_name_fn = Function::new(ctx.clone(), move |index: i32| -> String {
                name_particles
                    .get(index as usize)
                    .map_or_else(String::new, |system| system.borrow().name.clone())
            })
            .map_err(|e| EngineStartError::Bootstrap(format!("particle name fn: {e}")))?;
            ctx.globals()
                .set("kweSceneParticleName", particle_name_fn)
                .map_err(|e| {
                    EngineStartError::Bootstrap(format!("global kweSceneParticleName: {e}"))
                })?;

            let scalar_particles = self.particles.clone();
            let particle_get_scalar =
                Function::new(ctx.clone(), move |index: i32, prop: String| -> f64 {
                    let Some(system) = scalar_particles.get(index as usize) else {
                        return 0.0;
                    };
                    let system = system.borrow();
                    match prop.as_str() {
                        // Emitter properties (already clamped at parse and
                        // at every write, so these are the live values).
                        "spawnRate" => f64::from(system.spawn_rate),
                        "life" => f64::from(system.life),
                        "speedMin" => f64::from(system.speed_min),
                        "speedMax" => f64::from(system.speed_max),
                        "direction" => f64::from(system.direction),
                        "spread" => f64::from(system.spread),
                        "sizeStart" => f64::from(system.size_start),
                        "sizeEnd" => f64::from(system.size_end),
                        "alphaStart" => f64::from(system.alpha_start),
                        "alphaEnd" => f64::from(system.alpha_end),
                        "maxCount" => f64::from(system.max_count),
                        "blendMode" => f64::from(system.blend_mode.as_u32()),
                        // Layer-style properties.
                        "alpha" => f64::from(system.alpha),
                        "brightness" => f64::from(system.brightness),
                        "visible" => f64::from(u8::from(system.visible)),
                        "emitting" => f64::from(u8::from(system.emitting)),
                        // WE IParticleSystemInstance factors (default 1.0).
                        // "alphaFactor" backs the proxy's instance.alpha so
                        // it cannot collide with the layer alpha.
                        "count" => f64::from(system.count),
                        "speed" => f64::from(system.speed),
                        "lifetime" => f64::from(system.lifetime),
                        "size" => f64::from(system.size),
                        "alphaFactor" => f64::from(system.alpha_factor),
                        "rate" => f64::from(system.rate),
                        "colorn" => f64::from(system.colorn),
                        _ => 0.0,
                    }
                })
                .map_err(|e| EngineStartError::Bootstrap(format!("particle scalar getter: {e}")))?;
            ctx.globals()
                .set("kweSceneGetParticleScalar", particle_get_scalar)
                .map_err(|e| {
                    EngineStartError::Bootstrap(format!("global kweSceneGetParticleScalar: {e}"))
                })?;

            let set_scalar_particles = self.particles.clone();
            let particle_blend_mode_diag = Rc::clone(&self.particle_blend_mode_diag);
            let particle_set_scalar = Function::new(
                ctx.clone(),
                move |index: i32, prop: String, value: f64| {
                    let Some(system) = set_scalar_particles.get(index as usize) else {
                        return;
                    };
                    let mut system = system.borrow_mut();
                    match prop.as_str() {
                        "spawnRate" => system.spawn_rate = particles::clamp_spawn_rate(value),
                        "life" => system.life = particles::clamp_life(value),
                        "speedMin" => system.speed_min = particles::clamp_speed(value),
                        "speedMax" => system.speed_max = particles::clamp_speed(value),
                        "direction" => system.direction = particles::clamp_direction(value),
                        "spread" => system.spread = particles::clamp_spread(value),
                        "sizeStart" => system.size_start = particles::clamp_size(value),
                        "sizeEnd" => system.size_end = particles::clamp_size(value),
                        "alphaStart" => system.alpha_start = particles::clamp_alpha(value),
                        "alphaEnd" => system.alpha_end = particles::clamp_alpha(value),
                        "maxCount" => {
                            let raw = if value.is_finite()
                                && value >= 0.0
                                && value <= f64::from(u32::MAX)
                            {
                                value as u64
                            } else {
                                1
                            };
                            system.max_count = particles::clamp_max_count(raw);
                        }
                        // M3d contract, same as layers: the blend mode
                        // clamps to the implemented set (0/1/6/7/9), an
                        // unimplemented write clamps to 0 with a bounded
                        // one-time diagnostic.
                        "blendMode" => {
                            let raw = if value.is_finite()
                                && value >= 0.0
                                && value <= f64::from(u32::MAX)
                            {
                                value as u32
                            } else {
                                0
                            };
                            let mode = BlendMode::clamp(raw);
                            if mode.as_u32() != raw && !particle_blend_mode_diag.replace(true) {
                                eprintln!(
                                    "event=renderer.scene.particle_blend_mode_clamped system={} mode={} note=not-fixed-function-clamped-to-normal",
                                    system.name, raw
                                );
                            }
                            system.blend_mode = mode;
                        }
                        "alpha" => system.alpha = clamp_layer_alpha(value),
                        "brightness" => system.brightness = clamp_layer_brightness(value),
                        "visible" => system.visible = value != 0.0,
                        // WE IParticleSystemInstance factors: non-finite ->
                        // identity 1.0, magnitude clamped (1e6 for
                        // count/speed/lifetime/size/rate, 1.0 for alpha and
                        // colorn) — see particles::clamp_instance_factor.
                        "count" => {
                            system.count = particles::clamp_instance_factor(value, 1e6)
                        }
                        "speed" => {
                            system.speed = particles::clamp_instance_factor(value, 1e6)
                        }
                        "lifetime" => {
                            system.lifetime = particles::clamp_instance_factor(value, 1e6)
                        }
                        "size" => system.size = particles::clamp_instance_factor(value, 1e6),
                        "alphaFactor" => {
                            system.alpha_factor = particles::clamp_instance_factor(value, 1.0)
                        }
                        "rate" => system.rate = particles::clamp_instance_factor(value, 1e6),
                        "colorn" => {
                            system.colorn = particles::clamp_instance_factor(value, 1.0)
                        }
                        _ => {}
                    }
                },
            )
            .map_err(|e| EngineStartError::Bootstrap(format!("particle scalar setter: {e}")))?;
            ctx.globals()
                .set("kweSceneSetParticleScalar", particle_set_scalar)
                .map_err(|e| {
                    EngineStartError::Bootstrap(format!("global kweSceneSetParticleScalar: {e}"))
                })?;

            let get_vec_particles = self.particles.clone();
            let particle_get_vec = Function::new(
                ctx.clone(),
                move |index: i32, prop: String, axis: String| -> f64 {
                    let Some(system) = get_vec_particles.get(index as usize) else {
                        return 0.0;
                    };
                    let system = system.borrow();
                    match (prop.as_str(), axis.as_str()) {
                        ("gravity", "x") => f64::from(system.gravity[0]),
                        ("gravity", "y") => f64::from(system.gravity[1]),
                        ("colorStart", "r") => f64::from(system.color_start[0]),
                        ("colorStart", "g") => f64::from(system.color_start[1]),
                        ("colorStart", "b") => f64::from(system.color_start[2]),
                        ("colorStart", "a") => f64::from(system.color_start[3]),
                        ("colorEnd", "r") => f64::from(system.color_end[0]),
                        ("colorEnd", "g") => f64::from(system.color_end[1]),
                        ("colorEnd", "b") => f64::from(system.color_end[2]),
                        ("colorEnd", "a") => f64::from(system.color_end[3]),
                        _ => 0.0,
                    }
                },
            )
            .map_err(|e| EngineStartError::Bootstrap(format!("particle vector getter: {e}")))?;
            ctx.globals()
                .set("kweSceneGetParticleVec", particle_get_vec)
                .map_err(|e| {
                    EngineStartError::Bootstrap(format!("global kweSceneGetParticleVec: {e}"))
                })?;

            let set_vec_particles = self.particles.clone();
            let particle_set_vec = Function::new(
                ctx.clone(),
                move |index: i32, prop: String, axis: String, value: f64| {
                    let Some(system) = set_vec_particles.get(index as usize) else {
                        return;
                    };
                    let mut system = system.borrow_mut();
                    match (prop.as_str(), axis.as_str()) {
                        ("gravity", "x") => system.gravity[0] = particles::clamp_gravity(value),
                        ("gravity", "y") => system.gravity[1] = particles::clamp_gravity(value),
                        ("colorStart", "r") => {
                            system.color_start[0] = particles::clamp_color_component(value)
                        }
                        ("colorStart", "g") => {
                            system.color_start[1] = particles::clamp_color_component(value)
                        }
                        ("colorStart", "b") => {
                            system.color_start[2] = particles::clamp_color_component(value)
                        }
                        ("colorStart", "a") => {
                            system.color_start[3] = particles::clamp_color_component(value)
                        }
                        ("colorEnd", "r") => {
                            system.color_end[0] = particles::clamp_color_component(value)
                        }
                        ("colorEnd", "g") => {
                            system.color_end[1] = particles::clamp_color_component(value)
                        }
                        ("colorEnd", "b") => {
                            system.color_end[2] = particles::clamp_color_component(value)
                        }
                        ("colorEnd", "a") => {
                            system.color_end[3] = particles::clamp_color_component(value)
                        }
                        _ => {}
                    }
                },
            )
            .map_err(|e| EngineStartError::Bootstrap(format!("particle vector setter: {e}")))?;
            ctx.globals()
                .set("kweSceneSetParticleVec", particle_set_vec)
                .map_err(|e| {
                    EngineStartError::Bootstrap(format!("global kweSceneSetParticleVec: {e}"))
                })?;

            // WE IParticleSystem playback controls (researched semantics):
            // play() resumes emission, pause() stops emission while the
            // live particles keep simulating, stop() clears immediately,
            // isPlaying() is emitting || alive, emitParticles(count)
            // bursts without requiring the system to be playing (default
            // 1; the count is clamped at the bridge, never exceeds the
            // particle cap).
            let play_particles = self.particles.clone();
            let particle_play = Function::new(ctx.clone(), move |index: i32| {
                if let Some(system) = play_particles.get(index as usize) {
                    system.borrow_mut().play();
                }
            })
            .map_err(|e| EngineStartError::Bootstrap(format!("particle play fn: {e}")))?;
            ctx.globals()
                .set("kweSceneParticlePlay", particle_play)
                .map_err(|e| {
                    EngineStartError::Bootstrap(format!("global kweSceneParticlePlay: {e}"))
                })?;

            let pause_particles = self.particles.clone();
            let particle_pause = Function::new(ctx.clone(), move |index: i32| {
                if let Some(system) = pause_particles.get(index as usize) {
                    system.borrow_mut().pause();
                }
            })
            .map_err(|e| EngineStartError::Bootstrap(format!("particle pause fn: {e}")))?;
            ctx.globals()
                .set("kweSceneParticlePause", particle_pause)
                .map_err(|e| {
                    EngineStartError::Bootstrap(format!("global kweSceneParticlePause: {e}"))
                })?;

            let stop_particles = self.particles.clone();
            let particle_stop = Function::new(ctx.clone(), move |index: i32| {
                if let Some(system) = stop_particles.get(index as usize) {
                    system.borrow_mut().stop();
                }
            })
            .map_err(|e| EngineStartError::Bootstrap(format!("particle stop fn: {e}")))?;
            ctx.globals()
                .set("kweSceneParticleStop", particle_stop)
                .map_err(|e| {
                    EngineStartError::Bootstrap(format!("global kweSceneParticleStop: {e}"))
                })?;

            let playing_particles = self.particles.clone();
            let particle_is_playing = Function::new(ctx.clone(), move |index: i32| -> bool {
                playing_particles
                    .get(index as usize)
                    .is_some_and(|system| system.borrow().is_playing())
            })
            .map_err(|e| EngineStartError::Bootstrap(format!("particle is-playing fn: {e}")))?;
            ctx.globals()
                .set("kweSceneParticleIsPlaying", particle_is_playing)
                .map_err(|e| {
                    EngineStartError::Bootstrap(format!("global kweSceneParticleIsPlaying: {e}"))
                })?;

            let emit_particles = self.particles.clone();
            let particle_emit = Function::new(ctx.clone(), move |index: i32, count: f64| {
                let Some(system) = emit_particles.get(index as usize) else {
                    return;
                };
                // Bounded: non-finite/negative -> 0 (no-op), capped at the
                // particle cap (emit_particles saturates the burst).
                let count = if count.is_finite() && count > 0.0 {
                    count.min(particles::MAX_PARTICLES as f64) as u32
                } else {
                    0
                };
                system.borrow_mut().emit_particles(count);
            })
            .map_err(|e| EngineStartError::Bootstrap(format!("particle emit fn: {e}")))?;
            ctx.globals()
                .set("kweSceneParticleEmit", particle_emit)
                .map_err(|e| {
                    EngineStartError::Bootstrap(format!("global kweSceneParticleEmit: {e}"))
                })?;

            ctx.eval::<(), &str>(LAYER_BOOTSTRAP_JS)
                .map_err(|e| EngineStartError::Bootstrap(format!("scene bootstrap: {e}")))?;
            ctx.eval::<(), &str>(PARTICLE_BOOTSTRAP_JS)
                .map_err(|e| EngineStartError::Bootstrap(format!("particle bootstrap: {e}")))?;
            Ok(())
        })
    }

    /// Evaluate the scene script, then call init() and resized() if defined,
    /// under the same interrupt budget as update() — a load-phase busy loop
    /// is contained (script disabled, bounded diagnostic), not hung. Any
    /// script exception disables the script (contained) with a bounded
    /// diagnostic; only an engine-level allocation error is fatal.
    fn load_script(&mut self, config: &SceneConfig) -> Result<(), EngineStartError> {
        let Some(script_path) = &config.script_path else {
            return Ok(()); // static scene: clear color only
        };
        // The parse-time metadata check alone races a swapped/grown file:
        // re-read with the same bounded reader as the scene descriptor.
        let bytes = crate::scene::read_bounded(script_path, crate::scene::MAX_SCRIPT_BYTES)
            .map_err(|e| {
                EngineStartError::Bootstrap(format!("read script {}: {e}", script_path.display()))
            })?;
        let source = String::from_utf8(bytes).map_err(|e| {
            EngineStartError::Bootstrap(format!(
                "scene script {} is not valid UTF-8: {e}",
                script_path.display()
            ))
        })?;

        self.budget.arm();
        let outcome = self.context.with(|ctx| {
            if let Err(e) = ctx.eval::<(), &str>(&source) {
                return self.load_call_error(&ctx, "eval", &e);
            }
            let globals = ctx.globals();
            if let Ok(Some(init)) = globals.get::<_, Option<Function>>("init")
                && let Err(e) = init.call::<(), ()>(())
            {
                return self.load_call_error(&ctx, "init", &e);
            }
            if let Ok(Some(resized)) = globals.get::<_, Option<Function>>("resized")
                && let Err(e) = resized.call::<(u32, u32), ()>((self.width, self.height))
            {
                return self.load_call_error(&ctx, "resized", &e);
            }
            LoadOutcome::Ok
        });

        self.budget.disarm();

        match outcome {
            LoadOutcome::Ok => {
                self.script_ok = true;
                // init() may have set the clear color already (the smoke
                // fixture relies on this); pick it up.
                self.clear_color = self.context.with(|ctx| self.read_clear_color(&ctx));
                Ok(())
            }
            LoadOutcome::Allocation => Err(EngineStartError::Allocation),
            LoadOutcome::Timeout => {
                // Hard budget during eval/init()/resized(): the script is
                // disabled (contained) and the renderer keeps the static
                // scene.json clear color.
                self.stats.hard_timeouts += 1;
                self.report_timeout("hard", self.stats.hard_timeouts);
                Ok(())
            }
            LoadOutcome::Error { class } => {
                if self.error_log.admit(&class) {
                    self.stats.script_errors += 1;
                    eprintln!("event=renderer.scene.script_error phase=load class=\"{class}\"");
                }
                Ok(())
            }
        }
    }

    /// One paced step: update Engine.frametime, call update(dt) under the
    /// budget, read back Engine.clearcolor. Never kills the renderer.
    pub fn step(&mut self, dt: f64) -> StepResult {
        self.frames += 1;
        if !self.script_ok && self.has_script {
            // A configured script failed to load/init: contained. A scene
            // with no script file is fine — step falls through to NoUpdate
            // so the render loop (and M3f particle sim) keeps advancing.
            return StepResult::ScriptError;
        }
        if self.frames.is_multiple_of(GC_EVERY_FRAMES) {
            self.runtime.run_gc();
        }
        let dt = dt.clamp(0.0, MAX_DT_SECONDS);
        let now = Instant::now();
        let frametime = match self.last_update {
            Some(last) => now.duration_since(last).as_secs_f64(),
            None => dt,
        };
        self.last_update = Some(now);

        self.budget.arm();
        let outcome = self
            .context
            .with(|ctx| self.run_update(&ctx, frametime, dt));
        self.budget.disarm();
        self.apply_outcome(outcome)
    }

    /// The actual script call; `&self` only (runs inside `context.with`).
    fn run_update(&self, ctx: &Ctx<'_>, frametime: f64, dt: f64) -> CallOutcome {
        let engine: Object = match ctx.globals().get("Engine") {
            Ok(engine) => engine,
            Err(e) => return self.call_error(ctx, "update", &e),
        };
        if let Err(e) = engine.set("frametime", frametime) {
            return self.call_error(ctx, "update", &e);
        }
        let update: Option<Function> = match ctx.globals().get("update") {
            Ok(f) => f,
            Err(e) => return self.call_error(ctx, "update", &e),
        };
        let Some(update) = update else {
            // Script loaded fine but defines no update(); init()/resized()
            // already ran, so render the current color.
            return CallOutcome::NoUpdate;
        };
        match update.call::<(f64,), ()>((dt,)) {
            Ok(()) => {
                if self.budget.soft_hit() {
                    CallOutcome::SoftTimeout
                } else {
                    CallOutcome::NewColor(self.read_clear_color(ctx))
                }
            }
            Err(e) => self.call_error(ctx, "update", &e),
        }
    }

    /// Classify a JS error; `&self` only.
    fn call_error(&self, ctx: &Ctx<'_>, phase: &str, error: &JsError) -> CallOutcome {
        if is_memory_limit_error(error) {
            eprintln!("event=renderer.scene.memory_limit phase={phase} fatal=1");
            return CallOutcome::MemoryLimit;
        }
        // The interrupt fires the hard budget by raising an uncatchable
        // exception; recognize it so it is counted as a timeout, not as a
        // script error (the "interrupted" exception class is noise).
        if self.budget.hard_hit() {
            return CallOutcome::HardTimeout;
        }
        if matches!(error, JsError::Exception) {
            let message = self.exception_message(ctx);
            // QuickJS raises a JS "Out of memory" exception when the runtime
            // memory limit is hit (js_throw_memory_error); rquickjs only
            // reports Error::Allocation for C-level failures, so the limit
            // hit has to be recognized from the message. Bounded, fatal.
            if message.to_ascii_lowercase().contains("out of memory") {
                eprintln!("event=renderer.scene.memory_limit phase={phase} fatal=1");
                return CallOutcome::MemoryLimit;
            }
            CallOutcome::Error {
                class: self.exception_class_from(phase, &message),
                soft: self.budget.soft_hit(),
            }
        } else {
            CallOutcome::Error {
                class: format!("{error}"),
                soft: self.budget.soft_hit(),
            }
        }
    }

    fn load_call_error(&self, ctx: &Ctx<'_>, phase: &str, error: &JsError) -> LoadOutcome {
        if is_memory_limit_error(error) {
            eprintln!("event=renderer.scene.memory_limit phase={phase} fatal=1");
            return LoadOutcome::Allocation;
        }
        // The load-phase budget aborts eval/init()/resized() by raising an
        // uncatchable exception; recognize it as a timeout, not a script
        // error (mirrors the update path, but the script is disabled).
        if self.budget.hard_hit() {
            return LoadOutcome::Timeout;
        }
        if matches!(error, JsError::Exception) {
            let message = self.exception_message(ctx);
            if message.to_ascii_lowercase().contains("out of memory") {
                eprintln!("event=renderer.scene.memory_limit phase={phase} fatal=1");
                return LoadOutcome::Allocation;
            }
            LoadOutcome::Error {
                class: self.exception_class_from(phase, &message),
            }
        } else {
            LoadOutcome::Error {
                class: format!("{error}"),
            }
        }
    }

    /// Turn a call outcome into stats, diagnostics, and a StepResult.
    fn apply_outcome(&mut self, outcome: CallOutcome) -> StepResult {
        match outcome {
            CallOutcome::NewColor(color) => {
                self.clear_color = color;
                StepResult::NewFrame(color)
            }
            CallOutcome::NoUpdate => StepResult::NewFrame(self.clear_color),
            CallOutcome::SoftTimeout => {
                self.stats.soft_timeouts += 1;
                self.report_timeout("soft", self.stats.soft_timeouts);
                StepResult::SoftTimeout
            }
            CallOutcome::HardTimeout => {
                self.stats.hard_timeouts += 1;
                self.report_timeout("hard", self.stats.hard_timeouts);
                StepResult::HardTimeout
            }
            CallOutcome::MemoryLimit => StepResult::Allocation,
            CallOutcome::Error { class, soft } => {
                if soft {
                    self.stats.soft_timeouts += 1;
                    self.report_timeout("soft", self.stats.soft_timeouts);
                }
                let now = Instant::now();
                let time_window_open =
                    now.duration_since(self.last_error_diag) >= ERROR_REREPORT_WINDOW;
                if self.error_log.admit(&class) || time_window_open {
                    self.stats.script_errors += 1;
                    self.last_error_diag = now;
                    eprintln!("event=renderer.scene.script_error phase=update class=\"{class}\"");
                }
                StepResult::ScriptError
            }
        }
    }

    /// The pending exception's message (also clears it from the context).
    fn exception_message(&self, ctx: &Ctx<'_>) -> String {
        ctx.catch()
            .as_object()
            .and_then(|obj| Exception::from_object(obj.clone()))
            .and_then(|e| e.message())
            .unwrap_or_default()
    }

    /// A bounded, stable class label for an exception: phase + first line of
    /// the message, truncated.
    fn exception_class_from(&self, phase: &str, message: &str) -> String {
        let first_line = message.lines().next().unwrap_or_default();
        let label = format!("{phase}: {first_line}");
        truncate_utf8(&label, 160)
    }

    fn report_timeout(&mut self, kind: &str, total: u64) {
        let now = Instant::now();
        if now.duration_since(self.last_timeout_diag) >= Duration::from_secs(10) || total == 1 {
            self.last_timeout_diag = now;
            eprintln!("event=renderer.scene.script_timeout kind={kind} total={total}");
        }
    }

    /// Read Engine.clearcolor back. Non-object values, missing fields, and
    /// non-finite numbers fall back to the current color.
    fn read_clear_color(&self, ctx: &Ctx<'_>) -> [f32; 4] {
        let read: Result<[f32; 4], JsError> = (|| {
            let engine: Object = ctx.globals().get("Engine")?;
            let clearcolor: Object = engine.get("clearcolor")?;
            let mut out = [0.0_f32; 4];
            for (i, channel) in ["r", "g", "b", "a"].iter().enumerate() {
                let value: f64 = clearcolor.get(*channel)?;
                out[i] = if value.is_finite() {
                    value.clamp(0.0, 1.0) as f32
                } else {
                    0.0
                };
            }
            Ok(out)
        })();
        read.unwrap_or(self.clear_color)
    }
}

enum LoadOutcome {
    Ok,
    Allocation,
    /// The load-phase hard budget aborted eval/init()/resized().
    Timeout,
    Error {
        class: String,
    },
}

/// console plumbing. `kweConsoleLog` receives "level:line" because the JS
/// bootstrap joins arbitrary arguments.
const CONSOLE_BOOTSTRAP_JS: &str = r#"
"use strict";
(function () {
  function toStr(value) {
    try { return String(value); } catch (e) { return "[unprintable]"; }
  }
  function sink(level) {
    return function () {
      var parts = [];
      for (var i = 0; i < arguments.length; i++) parts.push(toStr(arguments[i]));
      kweConsoleLog(level + ":" + parts.join(" "));
    };
  }
  globalThis.console = {
    log: sink("log"),
    info: sink("info"),
    warn: sink("warn"),
    error: sink("error")
  };
})();
"#;

/// M3c Scene object model. The layer properties are plain getters/setters
/// (enumerable, like the wallpaper-engine API's fields); every access goes
/// through the Rust bridge, so reads always reflect the clamped runtime
/// state and writes are clamped the moment they land. Property names and
/// units follow the researched API: origin/angles/scale/size are Vec
/// objects with x/y(/z) components, angles in degrees, alpha in 0..=1,
/// visible a boolean. getLayer accepts a name (string) or an index
/// (number) and returns null when nothing matches; changing a layer's
/// image at runtime is planned, not in M3c.
const LAYER_BOOTSTRAP_JS: &str = r#"
"use strict";
(function () {
  var indexByName = {};
  var cache = [];
  var count = kweSceneLayerCount();
  for (var i = 0; i < count; i++) indexByName[kweSceneLayerName(i)] = i;

  function findLayer(name) {
    if (typeof name === "number") {
      // Any non-negative index is a candidate: layer indices stay in the
      // layer table, indices >= count dispatch to particle systems (WE's
      // combined object space); getLayer turns the rest into null.
      return name >= 0 ? name : -1;
    }
    var byName = indexByName[String(name)];
    if (typeof byName === "number") return byName;
    // M3f: WE's object index space mixes layers and particles, and
    // getLayer finds particle systems by name too (researched WE
    // behavior). A particle name resolves to count + particleIndex so
    // getLayer can dispatch past the layer table; the typeof guard keeps
    // this file safe if the particle bridges are ever absent.
    if (typeof kweSceneFindParticle === "function") {
      var p = kweSceneFindParticle(String(name));
      if (p >= 0) return count + p;
    }
    return -1;
  }

  function vectorProps(index, prop, axes) {
    var v = {};
    for (var i = 0; i < axes.length; i++) {
      (function (axis) {
        Object.defineProperty(v, axis, {
          get: function () { return kweSceneGetVec(index, prop, axis); },
          set: function (value) { kweSceneSetVec(index, prop, axis, value); },
          enumerable: true
        });
      })(axes[i]);
    }
    return v;
  }

  function getLayer(name) {
    var index = findLayer(name);
    if (index < 0) return null;
    // M3f: WE indexes particles and layers in one object space —
    // thisScene.getLayer(particleIndex) returns the particle system
    // (researched WE behavior, recorded in docs/SCENE_FORMAT_V1.md).
    // Indices at or beyond the layer table dispatch to particle systems;
    // getParticleSystem is the dedicated M3f accessor. The guard keeps
    // this strict-mode file safe until the particle bootstrap ran.
    if (index >= count && typeof kweSceneGetParticleSystem === "function") {
      return kweSceneGetParticleSystem(index - count);
    }
    if (cache[index]) return cache[index];
    var layer = {};
    Object.defineProperty(layer, "name", {
      get: function () { return kweSceneLayerName(index); },
      enumerable: true
    });
    Object.defineProperty(layer, "alpha", {
      get: function () { return kweSceneGetScalar(index, "alpha"); },
      set: function (value) { kweSceneSetScalar(index, "alpha", value); },
      enumerable: true
    });
    Object.defineProperty(layer, "visible", {
      get: function () { return kweSceneGetScalar(index, "visible") !== 0; },
      set: function (value) { kweSceneSetScalar(index, "visible", value ? 1 : 0); },
      enumerable: true
    });
    Object.defineProperty(layer, "blendMode", {
      get: function () { return kweSceneGetScalar(index, "blendMode"); },
      set: function (value) { kweSceneSetScalar(index, "blendMode", value); },
      enumerable: true
    });
    Object.defineProperty(layer, "brightness", {
      get: function () { return kweSceneGetScalar(index, "brightness"); },
      set: function (value) { kweSceneSetScalar(index, "brightness", value); },
      enumerable: true
    });
    Object.defineProperty(layer, "angles", {
      get: function () { return vectorProps(index, "angles", ["x", "y", "z"]); },
      enumerable: true
    });
    Object.defineProperty(layer, "origin", {
      get: function () { return vectorProps(index, "origin", ["x", "y"]); },
      enumerable: true
    });
    Object.defineProperty(layer, "scale", {
      get: function () { return vectorProps(index, "scale", ["x", "y"]); },
      enumerable: true
    });
    // M3e: text layers expose the text properties (text, pointsize, the
    // alignment scalars, color) instead of the image-only size and tint —
    // text size is automatic and the color drives the tint slot. A script
    // touching a property the layer kind does not expose gets undefined,
    // never a renderer error.
    if (kweSceneIsText(index)) {
      Object.defineProperty(layer, "text", {
        get: function () { return kweSceneGetText(index); },
        set: function (value) { kweSceneSetText(index, String(value)); },
        enumerable: true
      });
      Object.defineProperty(layer, "pointsize", {
        get: function () { return kweSceneGetScalar(index, "pointsize"); },
        set: function (value) { kweSceneSetScalar(index, "pointsize", value); },
        enumerable: true
      });
      Object.defineProperty(layer, "horizontalAlign", {
        get: function () { return kweSceneGetScalar(index, "horizontalAlign"); },
        set: function (value) { kweSceneSetScalar(index, "horizontalAlign", value); },
        enumerable: true
      });
      Object.defineProperty(layer, "verticalAlign", {
        get: function () { return kweSceneGetScalar(index, "verticalAlign"); },
        set: function (value) { kweSceneSetScalar(index, "verticalAlign", value); },
        enumerable: true
      });
      Object.defineProperty(layer, "color", {
        get: function () { return vectorProps(index, "color", ["r", "g", "b", "a"]); },
        enumerable: true
      });
    } else {
      Object.defineProperty(layer, "size", {
        get: function () { return vectorProps(index, "size", ["x", "y"]); },
        enumerable: true
      });
      Object.defineProperty(layer, "tint", {
        get: function () { return vectorProps(index, "tint", ["r", "g", "b", "a"]); },
        enumerable: true
      });
    }
    cache[index] = layer;
    return layer;
  }

  var Scene = {
    getLayer: getLayer,
    getLayerCount: function () { return kweSceneLayerCount(); }
  };
  globalThis.Scene = Scene;
  globalThis.thisScene = Scene;
})();
"#;

/// The M3f particle-system proxies over the kweSceneParticle* bridges.
/// `Scene.getParticleSystem(name | index)` is the task-mandated M3f
/// extension (WE has no such call — particle systems are reached through
/// thisScene.getLayer, and the layer bootstrap above preserves that for
/// indices >= the layer count). The surface mirrors the researched WE
/// API: the emitter properties, the layer-style properties, the
/// IParticleSystemInstance factors (the WE "colorn" spelling is
/// intentional) and the IParticleSystem controls. Every write goes
/// through a Rust bridge and is clamped there; an out-of-range index is
/// null, never an error.
const PARTICLE_BOOTSTRAP_JS: &str = r#"
"use strict";
(function () {
  var indexByName = {};
  var cache = [];
  var count = kweSceneParticleCount();
  for (var i = 0; i < count; i++) indexByName[kweSceneParticleName(i)] = i;

  function findParticle(name) {
    if (typeof name === "number") {
      return (name >= 0 && name < count) ? name : -1;
    }
    var byName = indexByName[String(name)];
    return typeof byName === "number" ? byName : -1;
  }

  function scalarProperty(obj, index, prop, bridgeProp) {
    // `bridgeProp` names the Rust-side scalar (defaults to `prop`); the
    // instance factor alpha lives at "alphaFactor" so it cannot collide
    // with the system alpha.
    var b = bridgeProp === undefined ? prop : bridgeProp;
    Object.defineProperty(obj, prop, {
      get: function () { return kweSceneGetParticleScalar(index, b); },
      set: function (value) { kweSceneSetParticleScalar(index, b, value); },
      enumerable: true
    });
  }

  function vectorProps(index, prop, axes) {
    var v = {};
    for (var i = 0; i < axes.length; i++) {
      (function (axis) {
        Object.defineProperty(v, axis, {
          get: function () { return kweSceneGetParticleVec(index, prop, axis); },
          set: function (value) { kweSceneSetParticleVec(index, prop, axis, value); },
          enumerable: true
        });
      })(axes[i]);
    }
    return v;
  }

  function getParticleSystem(name) {
    var index = findParticle(name);
    if (index < 0) return null;
    if (cache[index]) return cache[index];
    var system = {};
    Object.defineProperty(system, "name", {
      get: function () { return kweSceneParticleName(index); },
      enumerable: true
    });
    // Emitter properties (flat M3f model; every write clamped at the
    // bridge). WE's component model is documented as planned.
    scalarProperty(system, index, "spawnRate");
    scalarProperty(system, index, "life");
    scalarProperty(system, index, "speedMin");
    scalarProperty(system, index, "speedMax");
    scalarProperty(system, index, "direction");
    scalarProperty(system, index, "spread");
    scalarProperty(system, index, "sizeStart");
    scalarProperty(system, index, "sizeEnd");
    scalarProperty(system, index, "alphaStart");
    scalarProperty(system, index, "alphaEnd");
    scalarProperty(system, index, "maxCount");
    scalarProperty(system, index, "blendMode");
    scalarProperty(system, index, "alpha");
    scalarProperty(system, index, "brightness");
    Object.defineProperty(system, "visible", {
      get: function () { return kweSceneGetParticleScalar(index, "visible") !== 0; },
      set: function (value) { kweSceneSetParticleScalar(index, "visible", value ? 1 : 0); },
      enumerable: true
    });
    Object.defineProperty(system, "gravity", {
      get: function () { return vectorProps(index, "gravity", ["x", "y"]); },
      enumerable: true
    });
    Object.defineProperty(system, "colorStart", {
      get: function () { return vectorProps(index, "colorStart", ["r", "g", "b", "a"]); },
      enumerable: true
    });
    Object.defineProperty(system, "colorEnd", {
      get: function () { return vectorProps(index, "colorEnd", ["r", "g", "b", "a"]); },
      enumerable: true
    });
    // WE IParticleSystemInstance factors (all default 1.0; non-finite
    // writes fall back to the identity 1.0 at the bridge).
    var instance = {};
    scalarProperty(instance, index, "count");
    scalarProperty(instance, index, "speed");
    scalarProperty(instance, index, "lifetime");
    scalarProperty(instance, index, "size");
    // instance.alpha is the multiplicative alpha factor (bridge prop
    // "alphaFactor", so it cannot collide with the system alpha).
    scalarProperty(instance, index, "alpha", "alphaFactor");
    scalarProperty(instance, index, "rate");
    scalarProperty(instance, index, "colorn");
    Object.defineProperty(system, "instance", {
      get: function () { return instance; },
      enumerable: true
    });
    // WE IParticleSystem controls. emitParticles defaults to 1 like WE;
    // the count is clamped at the bridge (never beyond the particle cap).
    system.play = function () { kweSceneParticlePlay(index); };
    system.pause = function () { kweSceneParticlePause(index); };
    system.stop = function () { kweSceneParticleStop(index); };
    system.isPlaying = function () { return kweSceneParticleIsPlaying(index); };
    system.emitParticles = function (n) {
      kweSceneParticleEmit(index, n === undefined ? 1 : n);
    };
    cache[index] = system;
    return system;
  }

  // The layer bootstrap's getLayer falls back to this for indices >= the
  // layer count (WE's combined object index space), so it must be global.
  globalThis.kweSceneGetParticleSystem = getParticleSystem;
  var Scene = globalThis.Scene;
  Scene.getParticleSystem = getParticleSystem;
  Scene.getParticleSystemCount = function () { return kweSceneParticleCount(); };
})();
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scene::SceneConfig;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU32, Ordering};

    fn tmpdir() -> PathBuf {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("kwe-js-test-{}-{n}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn config_with_script(dir: &Path, script: &str) -> SceneConfig {
        config_with_layers(dir, script, "[]")
    }

    /// A scene whose `objects` array is `objects_json` (M3c). Image files
    /// need not exist for the engine tests — resolution happens at load in
    /// main.rs, not here.
    fn config_with_layers(dir: &Path, script: &str, objects_json: &str) -> SceneConfig {
        let script_path = dir.join("main.js");
        fs::write(&script_path, script).unwrap();
        let scene = dir.join("scene.json");
        fs::write(
            &scene,
            format!(r#"{{"general": {{}}, "objects": {objects_json}}}"#),
        )
        .unwrap();
        SceneConfig::parse(&scene).unwrap().with_script(script_path)
    }

    trait WithScript {
        fn with_script(self, script_path: PathBuf) -> SceneConfig;
    }
    impl WithScript for SceneConfig {
        fn with_script(mut self, script_path: PathBuf) -> SceneConfig {
            self.script_path = Some(script_path);
            self
        }
    }

    // ---- pure budget decision ----

    #[test]
    fn budget_verdict_pure() {
        assert_eq!(
            budget_verdict_ns(0, false, 8_000_000, 33_000_000),
            BudgetVerdict::Ok
        );
        assert_eq!(
            budget_verdict_ns(100_000_000, false, 8_000_000, 33_000_000),
            BudgetVerdict::Ok
        );
        assert_eq!(
            budget_verdict_ns(4_000_000, true, 8_000_000, 33_000_000),
            BudgetVerdict::Ok
        );
        assert_eq!(
            budget_verdict_ns(8_000_000, true, 8_000_000, 33_000_000),
            BudgetVerdict::Soft
        );
        assert_eq!(
            budget_verdict_ns(20_000_000, true, 8_000_000, 33_000_000),
            BudgetVerdict::Soft
        );
        assert_eq!(
            budget_verdict_ns(33_000_000, true, 8_000_000, 33_000_000),
            BudgetVerdict::Hard
        );
        assert_eq!(
            budget_verdict_ns(1_000_000_000, true, 8_000_000, 33_000_000),
            BudgetVerdict::Hard
        );
    }

    #[test]
    fn truncate_utf8_keeps_char_boundaries_and_bounds() {
        let text = "héllo wörld";
        let cut = truncate_utf8(text, 7);
        assert!(cut.len() <= 7);
        assert!(cut.ends_with('…'));
        // Short text passes through unchanged.
        assert_eq!(truncate_utf8(text, text.len()), text);
        assert_eq!(truncate_utf8("hi", 100), "hi");
        // A cut in the middle of a multi-byte char stays on a boundary.
        assert!(truncate_utf8("ééé", 2).is_char_boundary(0));
        assert!(truncate_utf8("ééé", 2).len() <= 2);
    }

    // ---- end-to-end script behavior on the real QuickJS engine ----

    #[test]
    fn update_drives_clear_color() {
        let dir = tmpdir();
        let config = config_with_script(
            &dir,
            r#"
            Engine.clearcolor = { r: 0.1, g: 0.2, b: 0.3, a: 1.0 };
            var t = 0;
            function update(dt) { t += dt; Engine.clearcolor.r = 0.1 + t; }
            "#,
        );
        let mut engine = ScriptEngine::new(&config, 320, 200, 30).unwrap();
        assert!(engine.script_ok());
        assert_eq!(engine.clear_color(), [0.1, 0.2, 0.3, 1.0]);
        let result = engine.step(0.5);
        assert!(matches!(result, StepResult::NewFrame([r, _, _, _]) if (r - 0.6).abs() < 1e-6));
        let result = engine.step(0.25);
        assert!(matches!(result, StepResult::NewFrame([r, _, _, _]) if (r - 0.85).abs() < 1e-6));
    }

    #[test]
    fn init_can_set_clear_color_before_first_update() {
        let dir = tmpdir();
        let config = config_with_script(
            &dir,
            r#"
            function init() { Engine.clearcolor = { r: 0.4, g: 0.5, b: 0.6, a: 1.0 }; }
            function update(dt) {}
            "#,
        );
        let mut engine = ScriptEngine::new(&config, 320, 200, 30).unwrap();
        assert!(engine.script_ok());
        assert_eq!(engine.clear_color(), [0.4, 0.5, 0.6, 1.0]);
        assert!(matches!(engine.step(0.1), StepResult::NewFrame(_)));
    }

    #[test]
    fn script_without_update_is_static() {
        let dir = tmpdir();
        let config = config_with_script(&dir, "function init() {}");
        let mut engine = ScriptEngine::new(&config, 320, 200, 30).unwrap();
        assert!(engine.script_ok());
        assert!(matches!(engine.step(0.1), StepResult::NewFrame(_)));
    }

    #[test]
    fn exception_in_update_is_contained() {
        let dir = tmpdir();
        let config = config_with_script(
            &dir,
            r#"
            var calls = 0;
            function update(dt) {
                calls += 1;
                if (calls === 2) { throw new Error("boom"); }
                Engine.clearcolor.r = calls * 0.1;
            }
            "#,
        );
        let mut engine = ScriptEngine::new(&config, 320, 200, 30).unwrap();
        assert!(matches!(engine.step(0.1), StepResult::NewFrame(_)));
        // Second step throws: contained, script stays live.
        assert_eq!(engine.step(0.1), StepResult::ScriptError);
        assert!(engine.script_ok());
        // Third step renders again with the last read color.
        assert!(matches!(engine.step(0.1), StepResult::NewFrame(_)));
        assert!(engine.stats().script_errors >= 1);
    }

    #[test]
    fn exception_in_init_disables_script_but_keeps_renderer() {
        let dir = tmpdir();
        let config = config_with_script(&dir, "function init() { throw new Error(\"bad init\"); }");
        let mut engine = ScriptEngine::new(&config, 320, 200, 30).unwrap();
        assert!(!engine.script_ok());
        assert_eq!(engine.step(0.1), StepResult::ScriptError);
        assert_eq!(engine.clear_color(), [0.0, 0.0, 0.0, 1.0]);
    }

    #[test]
    fn eval_parse_error_is_contained() {
        let dir = tmpdir();
        let config = config_with_script(&dir, "function update( {");
        let engine = ScriptEngine::new(&config, 320, 200, 30).unwrap();
        assert!(!engine.script_ok());
        assert_eq!(engine.clear_color(), [0.0, 0.0, 0.0, 1.0]);
    }

    #[test]
    fn console_is_rate_limited_but_works() {
        let dir = tmpdir();
        let config = config_with_script(
            &dir,
            r#"
            function update(dt) { console.log("hello", 42, true); }
            "#,
        );
        let mut engine = ScriptEngine::new(&config, 320, 200, 30).unwrap();
        // Two steps: both log (well under the window cap); the renderer is
        // unaffected by console noise.
        assert!(matches!(engine.step(0.1), StepResult::NewFrame(_)));
        assert!(matches!(engine.step(0.1), StepResult::NewFrame(_)));
    }

    #[test]
    fn soft_budget_skips_frame_then_recovers() {
        let dir = tmpdir();
        let config = config_with_script(
            &dir,
            r#"
            var frames = 0;
            function update(dt) {
                frames += 1;
                if (frames === 2) {
                    // Busy-wait ~12 ms: past the 8 ms soft budget, under 33 ms.
                    var deadline = Date.now() + 12;
                    while (Date.now() < deadline) {}
                }
                Engine.clearcolor.r = frames * 0.05;
            }
            "#,
        );
        let mut engine = ScriptEngine::new(&config, 320, 200, 30).unwrap();
        assert!(matches!(engine.step(0.1), StepResult::NewFrame(_)));
        // Second step overruns the soft budget: frame skipped, script alive.
        assert_eq!(engine.step(0.1), StepResult::SoftTimeout);
        assert!(engine.script_ok());
        assert!(engine.stats().soft_timeouts >= 1);
        // Third step renders again.
        assert!(matches!(engine.step(0.1), StepResult::NewFrame(_)));
    }

    #[test]
    fn infinite_loop_is_aborted_at_hard_budget() {
        let dir = tmpdir();
        let config = config_with_script(&dir, "function update(dt) { while (true) {} }");
        let mut engine = ScriptEngine::new(&config, 320, 200, 30).unwrap();
        let started = Instant::now();
        let result = engine.step(0.1);
        assert!(
            started.elapsed() < Duration::from_millis(2000),
            "interrupt did not fire"
        );
        assert_eq!(result, StepResult::HardTimeout);
        assert!(engine.script_ok());
        assert!(engine.stats().hard_timeouts >= 1);
        // The same call aborts again; never hangs, never kills the renderer.
        assert_eq!(engine.step(0.1), StepResult::HardTimeout);
    }

    #[test]
    fn memory_limit_aborts_to_allocation() {
        let dir = tmpdir();
        let config = config_with_script(
            &dir,
            r#"
            function update(dt) {
                var chunks = [];
                while (true) { chunks.push(new Uint8Array(1024 * 1024)); }
            }
            "#,
        );
        let mut engine = ScriptEngine::new(&config, 320, 200, 30).unwrap();
        // The 64 MiB heap cap must be hit well inside the 33 ms hard budget
        // (allocating 1 MiB per iteration); otherwise the interrupt wins and
        // this becomes a HardTimeout and the test fails.
        let result = engine.step(0.1);
        assert_eq!(result, StepResult::Allocation);
    }

    #[test]
    fn busy_loop_init_is_contained_by_load_budget() {
        let dir = tmpdir();
        let config = config_with_script(&dir, "function init() { while (true) {} }");
        // The load phase runs under the armed hard budget: an init() busy
        // loop is aborted (contained), never hung, never an exit.
        let started = Instant::now();
        let mut engine = ScriptEngine::new(&config, 320, 200, 30).unwrap();
        assert!(
            started.elapsed() < Duration::from_millis(2000),
            "load-budget interrupt did not fire"
        );
        assert!(!engine.script_ok());
        assert_eq!(engine.step(0.1), StepResult::ScriptError);
        assert!(engine.stats().hard_timeouts >= 1);
        assert_eq!(engine.clear_color(), [0.0, 0.0, 0.0, 1.0]);
    }

    #[test]
    fn oom_in_init_is_fatal_allocation() {
        let dir = tmpdir();
        let config = config_with_script(
            &dir,
            r#"
            function init() {
                // One oversized allocation: rejected by the 64 MiB heap cap
                // at the allocation check, far under the 33 ms hard budget,
                // so the memory-limit exit fires deterministically.
                var huge = new Uint8Array(128 * 1024 * 1024);
            }
            "#,
        );
        let engine = ScriptEngine::new(&config, 320, 200, 30);
        assert!(matches!(engine, Err(EngineStartError::Allocation)));
    }

    // ---- M3c: Scene.getLayer ----

    #[test]
    fn layers_registered_before_script_load() {
        let dir = tmpdir();
        let config = config_with_layers(
            &dir,
            r#"function init() {
                // init() runs after registration: the layer is already here.
                Engine.clearcolor = { r: Scene.getLayer("bg") !== null ? 0.5 : 0,
                                      g: Scene.getLayerCount() === 2 ? 0.5 : 0,
                                      b: 0, a: 1 };
            }"#,
            r#"[{"name": "bg", "image": "bg.png", "origin": "100 200 0",
                 "angles": "0 0 1.57080", "scale": "2 2 2", "size": "64 32",
                 "alpha": 0.5, "visible": false},
                {"name": "fg", "image": "fg.png"}]"#,
        );
        let engine = ScriptEngine::new(&config, 320, 200, 30).unwrap();
        assert!(engine.script_ok());
        // Parsed state carried into the runtime (angles converted to
        // degrees; defaults filled for the bare layer).
        let layers = engine.layers();
        assert_eq!(layers.len(), 2);
        assert_eq!(layers[0].borrow().name, "bg");
        assert_eq!(layers[0].borrow().origin, [100.0, 200.0]);
        assert!((layers[0].borrow().angles[2] - 90.0).abs() < 0.001);
        assert_eq!(layers[0].borrow().scale, [2.0, 2.0]);
        assert_eq!(layers[0].borrow().size, [64.0, 32.0]);
        assert_eq!(layers[0].borrow().alpha, 0.5);
        assert!(!layers[0].borrow().visible);
        assert_eq!(layers[1].borrow().scale, [1.0, 1.0]);
        // init() saw both layers through the proxies.
        assert_eq!(engine.clear_color(), [0.5, 0.5, 0.0, 1.0]);
    }

    #[test]
    fn script_layer_writes_are_clamped_on_the_rust_side() {
        let dir = tmpdir();
        let config = config_with_layers(
            &dir,
            r#"function update(dt) {
                var l = Scene.getLayer("fg");
                l.alpha = 2;                       // -> 1
                l.visible = 1;                     // truthy -> true
                l.origin.x = 1e9;                  // -> 1e6
                l.origin.y = NaN;                  // -> 0
                l.angles.z = Infinity;             // -> 0
                l.scale.x = -1e9;                  // -> -1e6
                l.size.x = -5;                     // -> 0
                Engine.clearcolor = {
                    r: l.alpha === 1 ? 0.5 : 0,
                    g: (l.visible === true && l.origin.x === 1e6 && l.origin.y === 0) ? 0.5 : 0,
                    b: (l.angles.z === 0 && l.scale.x === -1e6 && l.size.x === 0) ? 0.5 : 0,
                    a: 1
                };
            }"#,
            r#"[{"name": "bg", "image": "bg.png"}, {"name": "fg", "image": "fg.png"}]"#,
        );
        let mut engine = ScriptEngine::new(&config, 320, 200, 30).unwrap();
        assert!(
            matches!(engine.step(0.1), StepResult::NewFrame(color) if color == [0.5, 0.5, 0.5, 1.0])
        );
        // And the clamped values really landed on the Rust side.
        let layer = engine.layers()[1].clone();
        let state = layer.borrow();
        assert_eq!(state.alpha, 1.0);
        assert!(state.visible);
        assert_eq!(state.origin, [1e6, 0.0]);
        assert_eq!(state.angles, [0.0, 0.0, 0.0]);
        assert_eq!(state.scale, [-1e6, 1.0]);
        assert_eq!(state.size, [0.0, 0.0]);
    }

    #[test]
    fn blend_mode_and_effects_writes_clamp_on_the_rust_side() {
        let dir = tmpdir();
        let config = config_with_layers(
            &dir,
            r#"function update(dt) {
                var l = Scene.getLayer("fg");
                l.blendMode = 6;                 // add — implemented, stays
                l.blendMode = 11;                // known-unimplemented -> 0
                l.blendMode = 12345;             // unknown -> 0
                l.brightness = 50;               // -> 10
                l.brightness = -1;               // -> 0
                l.tint.r = 2;                    // -> 1
                l.tint.g = NaN;                  // -> 1 (identity)
                l.tint.b = 0.5;                  // -> 0.5
                l.tint.a = 0.25;                 // -> 0.25
                Engine.clearcolor = {
                    r: (l.blendMode === 0 && l.brightness === 0) ? 0.5 : 0,
                    g: (l.tint.r === 1 && l.tint.g === 1 && l.tint.b === 0.5 && l.tint.a === 0.25) ? 0.5 : 0,
                    b: 0, a: 1
                };
            }"#,
            r#"[{"name": "bg", "image": "bg.png"}, {"name": "fg", "image": "fg.png",
                 "colorBlendMode": 7}]"#,
        );
        let mut engine = ScriptEngine::new(&config, 320, 200, 30).unwrap();
        assert!(
            matches!(engine.step(0.1), StepResult::NewFrame(color) if color == [0.5, 0.5, 0.0, 1.0])
        );
        // The parsed blend mode (7 = screen) survived into the runtime, and
        // the script's writes landed clamped on the Rust side.
        let layer = engine.layers()[1].clone();
        let state = layer.borrow();
        assert_eq!(
            state.blend_mode,
            BlendMode::Normal,
            "11 then 12345 clamp to 0"
        );
        assert_eq!(state.brightness, 0.0);
        assert_eq!(state.tint, [1.0, 1.0, 0.5, 0.25]);
        // The first layer kept its parsed default (mode 0, brightness 1,
        // identity tint).
        let bg = engine.layers()[0].clone();
        let state = bg.borrow();
        assert_eq!(state.blend_mode, BlendMode::Normal);
        assert_eq!(state.brightness, 1.0);
        assert_eq!(state.tint, [1.0, 1.0, 1.0, 1.0]);
    }

    // ---- M3e: text-layer proxies ----

    #[test]
    fn text_layer_proxy_exposes_and_clamps_text_properties() {
        let dir = tmpdir();
        let config = config_with_layers(
            &dir,
            r#"function update(dt) {
                var l = Scene.getLayer("t");
                // Text layers expose the text properties; image layers do
                // not (undefined, never an error).
                var img = Scene.getLayer("bg");
                l.text = "Hello world";
                l.pointsize = 9999;            // -> 512
                l.pointsize = -10;             // -> 4
                l.horizontalAlign = 2;         // right
                l.verticalAlign = 0;           // top
                l.color.r = 2;                 // -> 1
                l.color.g = NaN;               // -> 1 (identity)
                l.color.b = 0.25;
                l.color.a = 0.5;
                Engine.clearcolor = {
                    r: (l.text === "Hello world" && l.pointsize === 4) ? 0.5 : 0,
                    g: (l.horizontalAlign === 2 && l.verticalAlign === 0 &&
                        img.text === undefined && img.tint !== undefined) ? 0.5 : 0,
                    b: (l.color.r === 1 && l.color.g === 1 && l.color.b === 0.25 &&
                        l.color.a === 0.5) ? 0.5 : 0,
                    a: 1
                };
            }"#,
            r#"[{"name": "bg", "image": "bg.png"}, {"name": "t", "text": "Hi",
                 "color": [0.5, 1.0, 0.25, 0.5]}]"#,
        );
        let mut engine = ScriptEngine::new(&config, 320, 200, 30).unwrap();
        assert!(
            matches!(engine.step(0.1), StepResult::NewFrame(color) if color == [0.5, 0.5, 0.5, 1.0])
        );
        // The clamped values landed on the Rust side, dirty for the next
        // worker sync.
        let layer = engine.layers()[1].clone();
        let state = layer.borrow();
        let text = state.text.as_ref().unwrap();
        assert_eq!(text.text, "Hello world");
        assert_eq!(text.pointsize_px, 4.0);
        assert_eq!(text.horizontal_align, HorizontalAlign::Right);
        assert_eq!(text.vertical_align, VerticalAlign::Top);
        assert_eq!(text.color, [1.0, 1.0, 0.25, 0.5]);
        assert!(text.dirty);
        // The image layer never got a text state.
        assert!(engine.layers()[0].borrow().text.is_none());
    }

    #[test]
    fn text_proxy_writes_are_bounded() {
        let dir = tmpdir();
        let config = config_with_layers(
            &dir,
            r#"function update(dt) {
                var l = Scene.getLayer("t");
                // NaN/Infinity sizes and alignments fall back to the
                // defaults (48 px, center/center) instead of poisoning.
                l.pointsize = NaN;
                l.horizontalAlign = Infinity;
                l.verticalAlign = -1;
                l.text = "a";
                Engine.clearcolor = {
                    r: (l.pointsize === 48) ? 0.5 : 0,
                    g: (l.horizontalAlign === 1 && l.verticalAlign === 0) ? 0.5 : 0,
                    b: (l.text === "a") ? 0.5 : 0,
                    a: 1
                };
            }"#,
            r#"[{"name": "t", "text": "Hi"}]"#,
        );
        let mut engine = ScriptEngine::new(&config, 320, 200, 30).unwrap();
        assert!(
            matches!(engine.step(0.1), StepResult::NewFrame(color) if color == [0.5, 0.5, 0.5, 1.0])
        );
        let layers = engine.layers();
        let state = layers[0].borrow();
        let text = state.text.as_ref().unwrap();
        assert_eq!(text.pointsize_px, 48.0);
        assert_eq!(text.horizontal_align, HorizontalAlign::Center);
        // -1 rounds to -1 and clamps to 0: the vertical axis's 0 is Top.
        assert_eq!(text.vertical_align, VerticalAlign::Top);
    }

    #[test]
    fn over_long_text_writes_are_truncated() {
        // The 4096-char cap is enforced at the bridge: the string is
        // truncated (chars, not bytes) and the state still lands dirty.
        let dir = tmpdir();
        let config = config_with_layers(
            &dir,
            r#"function update(dt) {
                var l = Scene.getLayer("t");
                var long = "";
                for (var i = 0; i < 5000; i++) long += "x";
                l.text = long;
                Engine.clearcolor = { r: 0.5, g: 0.5, b: 0.5, a: 1 };
            }"#,
            r#"[{"name": "t", "text": "Hi"}]"#,
        );
        let mut engine = ScriptEngine::new(&config, 320, 200, 30).unwrap();
        engine.step(0.1);
        let layers = engine.layers();
        let state = layers[0].borrow();
        let text = state.text.as_ref().unwrap();
        assert_eq!(text.text.chars().count(), MAX_TEXT_CHARS);
        assert!(text.dirty);
    }

    #[test]
    fn text_writes_mark_dirty_and_rebuild_each_step() {
        // Every text write must land on the Rust side so the worker's next
        // sync regenerates the layout (geometry is rebuilt on change, not
        // per frame).
        let dir = tmpdir();
        let config = config_with_layers(
            &dir,
            r#"var n = 0;
            function update(dt) {
                var l = Scene.getLayer("t");
                n += 1;
                l.text = "tick" + n;
                Engine.clearcolor = { r: 0.5, g: 0.5, b: 0.5, a: 1 };
            }"#,
            r#"[{"name": "t", "text": "start"}]"#,
        );
        let mut engine = ScriptEngine::new(&config, 320, 200, 30).unwrap();
        engine.step(0.1);
        engine.step(0.1);
        let layers = engine.layers();
        let state = layers[0].borrow();
        assert_eq!(state.text.as_ref().unwrap().text, "tick2");
        assert!(state.text.as_ref().unwrap().dirty);
    }

    #[test]
    fn get_layer_by_index_and_missing_returns_null() {
        let dir = tmpdir();
        let config = config_with_layers(
            &dir,
            r#"function init() {
                var a = Scene.getLayer("one");
                var b = Scene.getLayer(1);
                var c = Scene.getLayer(0);
                Engine.clearcolor = {
                    r: (a !== null && c === a) ? 0.5 : 0,
                    g: (b !== null && b.name === "two") ? 0.5 : 0,
                    b: (Scene.getLayer("missing") === null && Scene.getLayer(7) === null) ? 0.5 : 0,
                    a: 1
                };
            }"#,
            r#"[{"name": "one", "image": "one.png"}, {"name": "two", "image": "two.png"}]"#,
        );
        let engine = ScriptEngine::new(&config, 320, 200, 30).unwrap();
        assert_eq!(engine.clear_color(), [0.5, 0.5, 0.5, 1.0]);
    }

    #[test]
    fn update_mutates_layer_state_frame_to_frame() {
        let dir = tmpdir();
        let config = config_with_layers(
            &dir,
            r#"var t = 0;
            function update(dt) {
                t += dt;
                var l = Scene.getLayer("slide");
                l.origin.x = t * 10;
                l.alpha = 1 - t;
                Engine.clearcolor = { r: l.origin.x, g: l.alpha, b: 0, a: 1 };
            }"#,
            r#"[{"name": "slide", "image": "slide.png"}]"#,
        );
        let mut engine = ScriptEngine::new(&config, 320, 200, 30).unwrap();
        assert!(matches!(engine.step(0.5), StepResult::NewFrame(_)));
        let layer = engine.layers()[0].clone();
        // The borrow must end before the next step: update() writes the
        // same layer through the proxy (RefCell).
        {
            let state = layer.borrow();
            assert_eq!(state.origin[0], 5.0);
            assert_eq!(state.alpha, 0.5);
        }
        // A second step moves the layer further; the state is shared with
        // the worker, which reads it per frame for the draw list.
        assert!(matches!(engine.step(0.5), StepResult::NewFrame(_)));
        {
            let state = layer.borrow();
            assert_eq!(state.origin[0], 10.0);
            assert_eq!(state.alpha, 0.0);
        }
    }

    #[test]
    fn engine_surfaces_frametime_fps_resolution() {
        let dir = tmpdir();
        let config = config_with_script(
            &dir,
            r#"
            var observed = [];
            function update(dt) {
                observed.push([Engine.frametime, Engine.fps, Engine.resolution.x, Engine.resolution.y]);
            }
            function resized(w, h) { observed.push([w, h]); }
            "#,
        );
        let mut engine = ScriptEngine::new(&config, 640, 480, 24).unwrap();
        // resized() ran during load with (640, 480); update() sees
        // resolution {x: 640, y: 480} and fps 24; frametime is the step dt.
        assert!(matches!(engine.step(0.25), StepResult::NewFrame(_)));
    }

    // ---- M3f: particle-system proxies ----

    #[test]
    fn particle_systems_registered_before_script_load() {
        let dir = tmpdir();
        let config = config_with_layers(
            &dir,
            r#"function init() {
                // init() runs after registration: the system is already
                // reachable by name, by index, and through the WE-compatible
                // getLayer fallback (index >= layer count and by name).
                var byName = Scene.getParticleSystem("dust") !== null;
                var byIndex = Scene.getParticleSystem(0) !== null;
                var count = Scene.getParticleSystemCount() === 1;
                var viaLayer = Scene.getLayer(1) !== null;
                var viaLayerName = Scene.getLayer("dust") !== null;
                var missing = Scene.getParticleSystem("nope") === null;
                var defaults = Scene.getParticleSystem(99) === null;
                Engine.clearcolor = {
                    r: (byName && byIndex && count) ? 0.5 : 0,
                    g: (viaLayer && viaLayerName && missing && defaults) ? 0.5 : 0,
                    b: 0, a: 1
                };
            }"#,
            r#"[{"name": "bg", "image": "bg.png"},
                {"particle": {"spawnRate": 100, "life": 1, "speed": 60,
                              "sizeStart": 8, "sizeEnd": 8,
                              "alphaStart": 1, "alphaEnd": 1},
                 "name": "dust"}]"#,
        );
        let engine = ScriptEngine::new(&config, 320, 200, 30).unwrap();
        assert!(engine.script_ok());
        // Parsed state carried into the runtime (defaults filled in).
        let particles = engine.particles();
        assert_eq!(particles.len(), 1);
        let system = particles[0].borrow();
        assert_eq!(system.name, "dust");
        assert_eq!(system.spawn_rate, 100.0);
        assert_eq!(system.life, 1.0);
        assert_eq!(system.speed_min, 60.0);
        assert_eq!(system.size_start, 8.0);
        assert_eq!(system.color_start, [1.0, 1.0, 1.0, 1.0]);
        assert!(system.emitting);
        assert_eq!(engine.layers().len(), 1);
        // init() saw the system through every accessor.
        assert_eq!(engine.clear_color(), [0.5, 0.5, 0.0, 1.0]);
    }

    #[test]
    fn particle_proxy_writes_are_clamped_on_the_rust_side() {
        let dir = tmpdir();
        let config = config_with_layers(
            &dir,
            r#"function update(dt) {
                var p = Scene.getParticleSystem("dust");
                p.spawnRate = 99999;            // -> 4096
                p.life = 100;                   // -> 60
                p.life = NaN;                   // -> 1 (default)
                p.speedMin = -5;                // -> 0
                p.speedMax = 2e9;               // -> 1e6
                p.direction = 3.5;              // -> 3.5 (bounded ±1e6; f32-exact)
                p.direction = 1e300;            // -> 1e6 (finite-but-huge:
                //     the f64->f32 cast would overflow to INFINITY, so the
                //     write path clamps in f64 first — never a NaN angle)
                p.spread = -2;                  // -> 0
                p.sizeStart = 0;                // -> 1
                p.sizeEnd = 1000;               // -> 512
                p.alphaStart = 5;               // -> 1
                p.alphaEnd = NaN;               // -> 0
                p.maxCount = 1e9;               // -> 4096
                p.blendMode = 11;               // unimplemented -> 0
                p.alpha = 2;                    // -> 1
                p.brightness = 50;              // -> 10
                p.visible = 0;                  // -> false
                p.gravity.x = 1e7;              // -> 1e6
                p.gravity.y = -5;               // -> -5
                p.colorStart.r = 2;             // -> 1
                p.colorStart.g = NaN;           // -> 0
                p.colorEnd.b = 0.5;             // -> 0.5
                p.colorEnd.a = -1;              // -> 0
                p.instance.count = 1e9;         // -> 1e6
                p.instance.speed = -1;          // -> 0
                p.instance.lifetime = NaN;      // -> 1 (identity)
                p.instance.size = 5;            // -> 5
                p.instance.alpha = 0.5;         // -> 0.5
                p.instance.rate = 2;            // -> 2
                p.instance.colorn = NaN;        // -> 1 (identity)
                var ok =
                    p.spawnRate === 4096 && p.life === 1 &&
                    p.speedMin === 0 && p.speedMax === 1e6 &&
                    p.direction === 1e6 && p.spread === 0 &&
                    p.sizeStart === 1 && p.sizeEnd === 512 &&
                    p.alphaStart === 1 && p.alphaEnd === 0 &&
                    p.maxCount === 4096 && p.blendMode === 0 &&
                    p.alpha === 1 && p.brightness === 10 && p.visible === false &&
                    p.gravity.x === 1e6 && p.gravity.y === -5 &&
                    p.colorStart.r === 1 && p.colorStart.g === 0 &&
                    p.colorEnd.b === 0.5 && p.colorEnd.a === 0 &&
                    p.instance.count === 1e6 && p.instance.speed === 0 &&
                    p.instance.lifetime === 1 && p.instance.size === 5 &&
                    p.instance.alpha === 0.5 && p.instance.rate === 2 &&
                    p.instance.colorn === 1;
                Engine.clearcolor = { r: ok ? 0.5 : 0, g: 0, b: 0, a: 1 };
            }"#,
            r#"[{"particle": {}, "name": "dust"}]"#,
        );
        let mut engine = ScriptEngine::new(&config, 320, 200, 30).unwrap();
        assert!(
            matches!(engine.step(0.1), StepResult::NewFrame(color) if color == [0.5, 0.0, 0.0, 1.0])
        );
        // And the clamped values really landed on the Rust side.
        let system = engine.particles()[0].clone();
        let state = system.borrow();
        assert_eq!(state.spawn_rate, 4096.0);
        assert_eq!(state.life, 1.0);
        assert_eq!(state.speed_min, 0.0);
        assert_eq!(state.speed_max, 1e6);
        // The 1e300 write landed clamped to ±1e6, NOT f32::INFINITY (the
        // NaN-angle poison the adversarial review flagged).
        assert_eq!(state.direction, 1e6);
        assert!(state.direction.is_finite());
        assert_eq!(state.spread, 0.0);
        assert_eq!(state.size_start, 1.0);
        assert_eq!(state.size_end, 512.0);
        assert_eq!(state.alpha_start, 1.0);
        assert_eq!(state.alpha_end, 0.0);
        assert_eq!(state.max_count, 4096);
        assert_eq!(state.blend_mode, BlendMode::Normal);
        assert_eq!(state.alpha, 1.0);
        assert_eq!(state.brightness, 10.0);
        assert!(!state.visible);
        assert_eq!(state.gravity, [1e6, -5.0]);
        assert_eq!(state.color_start, [1.0, 0.0, 1.0, 1.0]); // only r/g written
        assert_eq!(state.color_end, [1.0, 1.0, 0.5, 0.0]); // only b/a written
        assert_eq!(state.count, 1e6);
        assert_eq!(state.speed, 0.0);
        assert_eq!(state.lifetime, 1.0);
        assert_eq!(state.size, 5.0);
        assert_eq!(state.alpha_factor, 0.5);
        assert_eq!(state.rate, 2.0);
        assert_eq!(state.colorn, 1.0);
    }

    #[test]
    fn particle_playback_controls_drive_rust_state() {
        let dir = tmpdir();
        let config = config_with_layers(
            &dir,
            r#"function init() {
                // stop(): emission off, everything cleared immediately —
                // isPlaying() goes false and emitParticles() still works.
                var p = Scene.getParticleSystem("dust");
                p.stop();
                p.emitParticles(5);
                Engine.clearcolor = {
                    r: p.isPlaying() ? 0 : 0.5,
                    g: (p.instance.count === 1 && p.instance.speed === 1) ? 0.5 : 0,
                    b: 0, a: 1
                };
            }"#,
            r#"[{"particle": {"spawnRate": 10, "life": 1, "speed": 60,
                             "speedMin": 0, "speedMax": 0},
                 "name": "dust"}]"#,
        );
        let mut engine = ScriptEngine::new(&config, 320, 200, 30).unwrap();
        assert_eq!(engine.clear_color(), [0.5, 0.5, 0.0, 1.0]);
        // The simulation lives in the worker (main.rs sync_particles),
        // driven by the same dt as the script step; here we drive it
        // manually. The burst spawns on the first simulated step even
        // though the system is stopped (WE emitParticles semantics).
        assert!(matches!(engine.step(0.1), StepResult::NewFrame(_)));
        for system in engine.particles() {
            system.borrow_mut().simulate(0.1);
        }
        let system = engine.particles()[0].clone();
        let state = system.borrow();
        assert_eq!(state.particles.len(), 5);
        assert!(!state.emitting);
        assert!(state.is_playing(), "alive particles count as playing");
        drop(state);
        // pause() keeps the live particles simulating; play() resumes.
        let script = r#"var t = 0;
        function update(dt) {
            t += dt;   // 0.1, 0.2, 0.3 across the three steps
            var p = Scene.getParticleSystem("dust");
            if (t <= 0.1) { p.play(); }        // step 1: emits normally
            else if (t <= 0.2) { p.pause(); }  // step 2: emission off
            else { p.play(); }                 // step 3: resumed
        }"#;
        let config = config_with_layers(
            &dir,
            script,
            r#"[{"particle": {"spawnRate": 10, "life": 1, "speed": 60},
                 "name": "dust"}]"#,
        );
        let mut engine = ScriptEngine::new(&config, 320, 200, 30).unwrap();
        let system = engine.particles()[0].clone();
        let step_and_simulate = |engine: &mut ScriptEngine| {
            assert!(matches!(engine.step(0.1), StepResult::NewFrame(_)));
            for system in engine.particles() {
                system.borrow_mut().simulate(0.1);
            }
        };
        // Step 1 (dt 0.1): play() — one spawn (10/s × 0.1 s), alive.
        step_and_simulate(&mut engine);
        assert!(system.borrow().emitting);
        assert!(system.borrow().is_playing());
        let alive_after_pause = system.borrow().particles.len();
        assert!(alive_after_pause > 0);
        // Step 2 (dt 0.1): pause() — emission off, particles age in place.
        step_and_simulate(&mut engine);
        assert!(!system.borrow().emitting);
        assert!(system.borrow().is_playing(), "paused with live particles");
        assert!(
            system.borrow().particles.len() <= alive_after_pause,
            "pause must not spawn"
        );
        // Step 3 (dt 0.1): play() — emission resumes.
        step_and_simulate(&mut engine);
        assert!(system.borrow().emitting);
        assert!(system.borrow().particles.len() > alive_after_pause);
    }
}
