// SPDX-License-Identifier: Apache-2.0
// QuickJS SceneScript engine for the M3a slice.
//
// One QuickJS runtime + context per worker (rquickjs 0.12.2, MIT, see
// THIRD_PARTY.yml). Bounded execution:
//   * heap cap 64 MiB (Runtime::set_memory_limit)  -> Error::Allocation
//   * stack cap 4 MiB (Runtime::set_max_stack_size) -> exception, contained
//   * per-update wall-clock budget via the interrupt handler: 8 ms soft
//     (skip the frame, bounded `script_timeout` diagnostic) and 33 ms hard
//     (interrupt raises an uncatchable exception; the frame is skipped; the
//     renderer always keeps publishing the last good state)
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
// points `init()`, `update(dt)`, `resized(w, h)`. See the coverage matrix in
// docs/SCENE_FORMAT_V1.md for what is implemented vs planned.

use std::cell::Cell;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use rquickjs::{Context, Ctx, Error as JsError, Exception, Function, Object, Runtime};

use crate::scene::SceneConfig;

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
    script_ok: bool,
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
    /// the scene.json clear color, and the exception is logged once.
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
            script_ok: false,
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
            Ok(())
        })
    }

    /// Evaluate the scene script, then call init() and resized() if defined.
    /// Any script exception disables the script (contained) with a bounded
    /// diagnostic; only an engine-level allocation error is fatal.
    fn load_script(&mut self, config: &SceneConfig) -> Result<(), EngineStartError> {
        let Some(script_path) = &config.script_path else {
            return Ok(()); // static scene: clear color only
        };
        let source = std::fs::read_to_string(script_path).map_err(|e| {
            EngineStartError::Bootstrap(format!("read script {}: {e}", script_path.display()))
        })?;

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

        match outcome {
            LoadOutcome::Ok => {
                self.script_ok = true;
                // init() may have set the clear color already (the smoke
                // fixture relies on this); pick it up.
                self.clear_color = self.context.with(|ctx| self.read_clear_color(&ctx));
                Ok(())
            }
            LoadOutcome::Allocation => Err(EngineStartError::Allocation),
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
        if !self.script_ok {
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
    Error { class: String },
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
        let script_path = dir.join("main.js");
        fs::write(&script_path, script).unwrap();
        let scene = dir.join("scene.json");
        fs::write(&scene, r#"{"general": {}}"#).unwrap();
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
}
