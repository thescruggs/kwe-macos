// SPDX-License-Identifier: GPL-3.0-or-later
// M3f particle systems: a bounded, deterministic CPU particle simulation
// for the original SceneScript engine.
//
// WE particle systems are described by a component model (emitter,
// initializer, operator, renderer arrays — researched from
// docs.wallpaperengine.io and the OWE reference parser, see
// docs/SCENE_FORMAT_V1.md, M3f section). M3f implements a flat subset of
// that surface ("emitter" fields with documented defaults; the component
// model itself is planned) and a scene.json/JS-accessible particle object.
// The simulation lives entirely on the CPU in scene pixel units (y down):
//
// * a fixed 1/60 s timestep accumulated from wall-clock frame time (the
//   sim is deterministic: the same scene and the same dt sequence produce
//   bit-identical particles — the smoke oracles depend on it);
// * per-step order is fixed — spawn, integrate (velocity + gravity, then
//   position, explicit Euler), then age out — so exact positions are
//   derivable by hand for the unit tests;
// * spawn rate is floored per step (leftover stays in the accumulator);
//   requests beyond the free slots are DROPPED — live particles are never
//   evicted — and reported once per system (particles_capped);
// * the researched WE `IParticleSystemInstance` factors (count, speed,
//   lifetime, size, alpha, rate, colorn) multiply the corresponding
//   quantities; they are written from script and read here;
// * every bound is a constant: MAX_PARTICLE_SYSTEMS systems, max_count
//   particles per system (<= MAX_PARTICLES), capped accumulators, one
//   bounded diagnostic per system.
//
// Rendering: the worker builds 6 vertices per particle (an axis-aligned
// quad expanded around the particle center, per-particle color+size folded
// into the vertex attributes — shaders/particle.vert) and uploads them as
// one host-visible buffer per system; `particle_draws` turns non-empty
// visible textured systems into one batched draw each.

use std::cell::RefCell;
use std::rc::Rc;

use crate::layers::{BlendMode, DrawKind, LayerDraw, MAX_LAYERS};
use crate::scene::ParticleSpec;
use crate::textures::{DecodedTexture, SpritesheetGrid};

/// Hard cap on the number of particle systems a scene can register. Raised
/// from 16 to 64 in S7: the corpus's "Avatar" report showed
/// `particle_system_skip count=12 (cap is 16)` — real scenes with more
/// systems than the old cap were silently losing whole particle systems,
/// not just capping their individual particle counts. 64 is a bounded
/// generous ceiling (still a small constant multiple of the previous cap,
/// and `TEXTURE_SLOT_COUNT`/the descriptor pool in vulkan.rs are derived
/// from this constant, not hardcoded, so raising it costs a proportionally
/// larger but still fixed-size pool, never unbounded growth). Systems past
/// it are skipped (counted for the worker's one-time diagnostic, never a
/// rejection).
pub const MAX_PARTICLE_SYSTEMS: usize = 64;

/// Hard cap on live particles per system. A scene's `maxCount` clamps to
/// this; the JS emitParticles() burst clamps to it too.
pub const MAX_PARTICLES: usize = 4096;

/// The simulation timestep (seconds). Deterministic per-step execution is
/// what makes the smoke oracles exact; the accumulator runs at
/// `instance.rate` × wall time (the WE simulation-rate factor).
pub const FIXED_STEP: f32 = 1.0 / 60.0;

/// Wall-clock seconds accepted per frame from the worker's pacing loop.
/// Anything longer (a stalled frame) is dropped, so the fixed-step loop
/// below is always bounded: at most MAX_ACCUMULATED_SIM_SECONDS / FIXED_STEP
/// steps per frame.
pub const MAX_FRAME_DT: f64 = 1.0;

/// Cap on the rate-scaled sim time accumulated in one frame: 60 fixed
/// steps, the documented worst case (a hostile `rate` factor can never
/// stall the frame).
pub const MAX_ACCUMULATED_SIM_SECONDS: f32 = 1.0;

/// Cap on the spawn accumulator (particles due per step): 65536 per step,
/// minus the free slots, is dropped. Bounds `spawnRate × count × h` under
/// hostile script writes.
pub const MAX_SPAWN_ACCUMULATOR: f32 = 65536.0;

// The flat emitter-model ranges and defaults (documented in
// docs/SCENE_FORMAT_V1.md, M3f section, with the research notes).
pub const MIN_PARTICLE_SPAWN_RATE: f32 = 0.0;
pub const MAX_PARTICLE_SPAWN_RATE: f32 = 4096.0;
pub const DEFAULT_PARTICLE_SPAWN_RATE: f32 = 10.0;
pub const MIN_PARTICLE_LIFE: f32 = 0.1;
pub const MAX_PARTICLE_LIFE: f32 = 60.0;
pub const DEFAULT_PARTICLE_LIFE: f32 = 1.0;
pub const MAX_PARTICLE_SPEED: f32 = 1e6;
pub const DEFAULT_PARTICLE_SPEED: f32 = 0.0;
/// Direction is an angle in radians from +x (y down), clamped to ±1e6;
/// the WE emitters have no direction/spread fields (velocity comes from
/// the velocity-random initializer) — this flat model is the M3f extension
/// the deterministic smoke oracles need (documented deviation).
pub const DEFAULT_PARTICLE_DIRECTION: f32 = 0.0;
/// Direction range bound (radians), mirroring the layer scalars' ±1e6:
/// clamp_direction bounds finite inputs to this range IN f64 before the
/// f32 cast — a huge-but-finite f64 like 1e300 would otherwise cast to
/// f32::INFINITY, and sin/cos(∞) is NaN, permanently poisoning the system.
pub const MAX_PARTICLE_DIRECTION: f32 = 1e6;
pub const MAX_PARTICLE_SPREAD: f32 = std::f32::consts::TAU;
pub const DEFAULT_PARTICLE_SPREAD: f32 = 0.0;
pub const MAX_PARTICLE_GRAVITY: f32 = 1e6;
pub const DEFAULT_PARTICLE_GRAVITY: [f32; 2] = [0.0, 0.0];
pub const MIN_PARTICLE_SIZE: f32 = 1.0;
pub const MAX_PARTICLE_SIZE: f32 = 512.0;
pub const DEFAULT_PARTICLE_SIZE: f32 = 8.0;
pub const DEFAULT_PARTICLE_ALPHA_START: f32 = 1.0;
pub const DEFAULT_PARTICLE_ALPHA_END: f32 = 0.0;
/// Researched WE default `maxcount` is 100; 1000 is our documented
/// deviation (a default so low starves every smoke fixture).
pub const DEFAULT_PARTICLE_MAX_COUNT: u32 = 1000;

/// Vertex stride in bytes: pos.xy, uv.xy, color.rgba, size + a pad float
/// (40 bytes, 10 floats — see shaders/particle.vert).
pub const PARTICLE_VERTEX_BYTES: usize = 40;

/// Seed for the per-system splitmix64 stream. The stream itself is
/// deterministic: seeded by the system index (documented deviation from
/// WE's per-frame randomness), so the same spawn sequence repeats in the
/// same order. BUT the fixed-step schedule derives from wall-clock dt, so
/// the scene's live population still depends on real time, not on the
/// scene alone. Systems with spread 0 and a fixed speed never touch the
/// stream at all (their trajectories are exact) — that is what makes the
/// smoke oracles reproducible.
const PRNG_SEED: u64 = 0x9E3779B97F4A7C15;

/// splitmix64: a small, fast, deterministic PRNG (public domain algorithm
/// by Sebastiano Vigna). A stream per system keeps every system's particle
/// positions independent of the others.
#[derive(Debug, Clone)]
pub(crate) struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    pub(crate) fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    pub(crate) fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E3779B97F4A7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
        z ^ (z >> 31)
    }

    /// Uniform in [0, 1): 24 bits of entropy, exactly reproducible.
    pub(crate) fn next_f32(&mut self) -> f32 {
        (self.next_u64() >> 40) as f32 * (1.0 / (1u64 << 24) as f32)
    }
}

/// One live particle. Positions and velocities are scene pixel units
/// (y down); `life` is the effective per-particle life (spec × the
/// instance lifetime factor, clamped to at least one fixed step so the
/// interpolation fraction age/life is always well-defined).
///
/// S4b: `size`/`alpha`/`color`/`initial_*` are meaningful only for a
/// component-model system (`ParticleSystemState.component.is_some()`) —
/// the flat model (M3f, unchanged) keeps computing size/color/alpha at
/// `build_vertex_bytes` time from the SPEC's start/end endpoints and the
/// age/life fraction, exactly as before this slice, so every existing M3f
/// test keeps pinning the same formula. A component-model particle's
/// current size/alpha/color are instead maintained by its own operators
/// each fixed step (`step_fixed_component`), initialized by its
/// initializers at spawn (`spawn_component`).
#[derive(Debug, Clone)]
pub(crate) struct Particle {
    x: f32,
    y: f32,
    vx: f32,
    vy: f32,
    age: f32,
    life: f32,
    /// Component-model only (S4b): current size (px half-extent), alpha,
    /// and straight RGB, mutated by operators each fixed step.
    size: f32,
    alpha: f32,
    color: [f32; 3],
    /// Component-model only (S4b): the values initializers set at spawn —
    /// `sizechange`/`alphafade`/`colorchange` read these as their fade
    /// baseline instead of re-deriving from a spec-level start/end pair
    /// (upstream `ParticleInstance::initial`, `CParticle.h`).
    initial_size: f32,
    initial_alpha: f32,
    initial_color: [f32; 3],
    /// Component-model only (S4b): per-particle (frequency, phase) for the
    /// `oscillatealpha`/`oscillatesize` operators, drawn once at spawn from
    /// the system's PRNG (upstream randomizes these once per particle too,
    /// lazily on first use — spawn-time is equivalent and simpler).
    osc_alpha: [f32; 2],
    osc_size: [f32; 2],
}

impl Default for Particle {
    /// Every field zeroed except `life` (1.0, so an accidentally-unset
    /// particle's age/life fraction is well-defined rather than a
    /// division-adjacent NaN) — used as the `..Default::default()` base by
    /// both the flat-model spawn path (which only ever sets x/y/vx/vy/age/
    /// life) and `spawn_component` (which sets every field explicitly).
    fn default() -> Self {
        Self {
            x: 0.0,
            y: 0.0,
            vx: 0.0,
            vy: 0.0,
            age: 0.0,
            life: 1.0,
            size: 0.0,
            alpha: 0.0,
            color: [1.0, 1.0, 1.0],
            initial_size: 0.0,
            initial_alpha: 0.0,
            initial_color: [1.0, 1.0, 1.0],
            osc_alpha: [0.0, 0.0],
            osc_size: [0.0, 0.0],
        }
    }
}

// ---- S4b: the component model (external particle definition files). ----
//
// Borrowed-From: Almamu/linux-wallpaperengine (GPL-3.0-or-later)
// src/WallpaperEngine/Render/Objects/CParticle.cpp (createBoxEmitter,
// createSphereEmitter, the *RandomInitializer family, and
// createMovementOperator/createAlphaFadeOperator/createSizeChangeOperator/
// createColorChangeOperator/createOscillateAlphaOperator/
// createOscillateSizeOperator/createControlPointAttractOperator/
// createTurbulenceOperator) @ b016d7d1 — adapted, not ported verbatim. Two
// deliberate scope cuts, documented once here rather than at every call
// site:
//
// * everything stays 2D (x, y) — the existing M3f particle model has no z
//   axis, matching every other scene2d object in this renderer, so a
//   parsed vector field's z component is read (bounds-checked) and then
//   dropped, and the sphere emitter always takes upstream's 2D-disk branch
//   (`flags & 4 == 0`), never the 3D spherical-shell one.
// * control points are NOT tracked (no live mouse/audio-reactive
//   attractor position): `controlpointattract` anchors at the system's own
//   spawn origin plus the operator's own `origin` offset instead of a
//   `Scene.getParticleSystem`-independent mouse-linked control point.
//   `rotationrandom`/`angularvelocityrandom`/`angularmovement`/
//   `oscillateposition`/`vortex`/`mapsequencearoundcontrolpoint` are not
//   implemented (unknown-kind, tolerated — the flags/mask fields on
//   supported operators outside the plan above are ignored the same way).
//   `turbulence` is a bounded deterministic APPROXIMATION (a sine/cosine
//   directional field, not upstream's Perlin curl noise) — see
//   `apply_turbulence` for the exact formula and why.
//
// Every bound below is a hard cap enforced at PARSE time
// (`crate::particlefile::parse_component_model`), independent of anything
// upstream declares, so a hostile particle file cannot grow the runtime's
// per-frame work past a small constant multiple of the existing M3f bounds
// (MAX_PARTICLE_SYSTEMS systems x MAX_PARTICLES particles, unchanged).

/// Cap on emitters/initializers/operators per component-model system. Real
/// WE particle files declare 1-5 of each (verified against the local WE
/// asset corpus's `assets/particles/*.json` examples); this is generous
/// headroom while keeping one system's per-step operator/emitter loop a
/// small constant, not attacker-controlled.
pub const MAX_COMPONENT_ITEMS: usize = 16;

/// One emitter kind (S4b). Distances/directions/origin are already
/// axis-clamped to ±1e6 and non-finite-safe at parse time (mirrors the
/// flat model's `clamp_gravity`/`clamp_direction` bounds).
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Emitter {
    /// `boxrandom`: a per-axis random offset in `[distance_min,
    /// distance_max]` (sign randomized), scaled by `directions`, from
    /// `origin`. `distance_min == distance_max == [0,0]` (the WE default
    /// when unspecified) spawns exactly at `origin`.
    Box {
        rate: f32,
        origin: [f32; 2],
        directions: [f32; 2],
        distance_min: [f32; 2],
        distance_max: [f32; 2],
    },
    /// `sphererandom`: a random point in the 2D annulus between
    /// `distance_min` and `distance_max` (uniform by area — upstream's
    /// `sqrt(uniform(minR², maxR²))`), scaled by `directions`, from
    /// `origin`. If `speed_max > 0`, velocity points radially outward at a
    /// uniform-random speed in `[speed_min, speed_max]`; otherwise
    /// velocity stays zero (an initializer sets it — the corpus examples
    /// all pair `sphererandom` with a `velocityrandom` initializer).
    Sphere {
        rate: f32,
        origin: [f32; 2],
        directions: [f32; 2],
        distance_min: f32,
        distance_max: f32,
        speed_min: f32,
        speed_max: f32,
    },
}

impl Emitter {
    fn rate(&self) -> f32 {
        match self {
            Emitter::Box { rate, .. } | Emitter::Sphere { rate, .. } => *rate,
        }
    }
}

/// One initializer kind (S4b): runs once per particle at spawn, in file
/// order, after the emitter sets its starting position/velocity.
#[derive(Debug, Clone, PartialEq)]
// The shared `Random` suffix mirrors upstream's own type names verbatim
// (`ColorRandomInitializer`, `SizeRandomInitializer`, ... `CParticle.h`) —
// every WE initializer IS a "pick a random value" kind, so the names stay
// as close to the researched format as this renderer's other WE-derived
// enums (e.g. `Emitter::Box`/`Sphere` matching `boxrandom`/`sphererandom`)
// rather than dropping the suffix and losing that traceability.
#[allow(clippy::enum_variant_names)]
pub(crate) enum Initializer {
    LifetimeRandom {
        min: f32,
        max: f32,
    },
    /// `size = uniform(min, max) / 2`, fed into `build_vertex_bytes`'s
    /// existing `half = size * 0.5` (the SAME convention the flat model's
    /// `size_start`/`size_end` already use). S7b (B2, re-issue of P8 with
    /// the corrected justification): the earlier doc here claimed upstream
    /// consumes `p.size` as a half-extent directly, so porting its extra
    /// `/2` would halve twice — that was wrong about upstream's shader
    /// math. Upstream (`CParticle.cpp:738`) sets
    /// `p.size = (min + t·(max−min)) · sizeOverride / 2`, and
    /// `common_particles.h::ComputeParticlePosition` spans
    /// `positionAndSize.w · (uv−0.5)` with `uv` in `[0,1]`, so the quad's
    /// FULL width equals `p.size` — i.e. upstream's `/2` makes the
    /// authored min/max value a DIAMETER, and the initializer's output is
    /// already half that diameter before it ever reaches the vertex
    /// builder. Without this halving our file-based sprites rendered
    /// exactly 2× upstream's width (authored value used as the full
    /// width directly, then halved again by `build_vertex_bytes`, netting
    /// a single halve instead of the two upstream applies). `exponent`
    /// biases the random draw (1 = uniform, matching `CParticle::
    /// createSizeRandomInitializer`), clamped to a safe range at parse
    /// time so `powf` never sees 0/negative/absurdly large exponents.
    /// Borrowed-From: Almamu/linux-wallpaperengine (GPL-3.0-or-later)
    /// src/WallpaperEngine/Render/Objects/CParticle.cpp:738 @ b016d7d1
    /// (the `common_particles.h::ComputeParticlePosition` span math cited
    /// above is a WE asset shader, not part of that repo — it is quoted
    /// here only to justify why upstream's `/2` makes the authored value
    /// a diameter, not to claim it as a ported source).
    SizeRandom {
        min: f32,
        max: f32,
        exponent: f32,
    },
    AlphaRandom {
        min: f32,
        max: f32,
    },
    /// Adds to whatever velocity the emitter already set (upstream:
    /// `p.velocity += ...`), not a replacement.
    VelocityRandom {
        min: [f32; 2],
        max: [f32; 2],
    },
    /// Components are already normalized 0..=1 at parse time (the WE file
    /// format writes 0..=255).
    ColorRandom {
        min: [f32; 3],
        max: [f32; 3],
    },
}

/// One operator kind (S4b): runs every fixed step, over every alive
/// particle, in file order.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Operator {
    Movement {
        gravity: [f32; 2],
        drag: f32,
    },
    /// Life-fraction (age/life, 0..=1) gated fade: `initial_alpha` scaled
    /// to 0 below `fade_in` and above `fade_out`, full between.
    AlphaFade {
        fade_in: f32,
        fade_out: f32,
    },
    /// `size = initial_size * fade(life_fraction, start_time, end_time,
    /// start_value, end_value)` — a linear ramp clamped at the endpoints.
    SizeChange {
        start_time: f32,
        end_time: f32,
        start_value: f32,
        end_value: f32,
    },
    ColorChange {
        start_time: f32,
        end_time: f32,
        start_value: [f32; 3],
        end_value: [f32; 3],
    },
    /// Multiplies the CURRENT alpha/size (i.e., stacks on top of whatever
    /// `alphafade`/`sizechange` already computed this step, matching file
    /// order) by `mix(scale_min, scale_max, (cos(freq*age+phase)+1)/2)`,
    /// using the per-particle (freq, phase) drawn at spawn
    /// (`Particle::osc_alpha`/`osc_size`).
    OscillateAlpha {
        freq_min: f32,
        freq_max: f32,
        scale_min: f32,
        scale_max: f32,
        phase_min: f32,
        phase_max: f32,
    },
    OscillateSize {
        freq_min: f32,
        freq_max: f32,
        scale_min: f32,
        scale_max: f32,
        phase_min: f32,
        phase_max: f32,
    },
    /// A constant-force pull toward `origin` (relative to the system's own
    /// spawn origin — see the module-level doc's control-point scope cut)
    /// for particles within `threshold` px.
    ControlPointAttract {
        origin: [f32; 2],
        scale: f32,
        threshold: f32,
    },
    /// A bounded deterministic APPROXIMATION of upstream's curl-noise
    /// turbulence (see the module doc) — NOT upstream's algorithm.
    /// `phase_min/max`/`speed_min/max` are resolved into a single (phase,
    /// speed) pair ONCE per system, at `ParticleSystemState::from_spec`
    /// (`turbulence_runtime`, indexed by encounter order among this
    /// system's Turbulence operators) — matching upstream's "randomized
    /// once per operator instance, not per particle/frame" behavior.
    Turbulence {
        scale: f32,
        time_scale: f32,
        speed_min: f32,
        speed_max: f32,
        phase_min: f32,
        phase_max: f32,
    },
}

/// One parsed component-model particle file (S4b), bounded at parse time.
/// Immutable after parse; `ParticleSystemState::from_spec` derives its own
/// per-system runtime extras (emitter accumulators, resolved turbulence
/// phase/speed) from it once at construction.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ComponentModel {
    pub maxcount: u32,
    pub(crate) emitters: Vec<Emitter>,
    pub(crate) initializers: Vec<Initializer>,
    pub(crate) operators: Vec<Operator>,
}

fn uniform(prng: &mut SplitMix64, min: f32, max: f32) -> f32 {
    if max > min {
        min + (max - min) * prng.next_f32()
    } else {
        min
    }
}

/// S7 (P7): a file-based particle system's sprite aspect ratio — the
/// spritesheet FRAME ratio when the texture has one (`(texH/rows) /
/// (texW/cols)`), else the whole texture's own height/width ratio, else
/// 1.0 (no texture, or a zero-width texture — defensive, never divides by
/// zero). Free function (not a method) purely so `ParticleSystemState::
/// from_spec` and this module's tests can call it without a whole state.
///
/// Borrowed-From: Almamu/linux-wallpaperengine (GPL-3.0-or-later)
/// src/WallpaperEngine/Render/Objects/CParticle.cpp:1832-1844,1912-1940 @
/// b016d7d1 — adapted (upstream's `textureRatio`/`ComputeParticlePosition`).
fn texture_ratio_for(texture: Option<&DecodedTexture>) -> f32 {
    let Some(texture) = texture else {
        return 1.0;
    };
    if texture.width == 0 {
        return 1.0;
    }
    match texture.spritesheet {
        Some(grid) => {
            let cols = grid.cols.max(1) as f32;
            let rows = grid.rows.max(1) as f32;
            let frame_w = texture.width as f32 / cols;
            let frame_h = texture.height as f32 / rows;
            if frame_w > 0.0 {
                frame_h / frame_w
            } else {
                1.0
            }
        }
        None => texture.height as f32 / texture.width as f32,
    }
}

/// Upstream's `fadeValue`: a linear ramp from `(from, start)` to `(to,
/// end)`, clamped to `[start, end]`'s range at the endpoints (matches
/// `glm::clamp(mix(...), min(start,end), max(start,end))` semantics without
/// needing glm — `t` here is already the life fraction, 0..=1).
fn fade_value(t: f32, from: f32, to: f32, start: f32, end: f32) -> f32 {
    if to <= from {
        return end; // upstream: a zero-or-negative-width window is "past it"
    }
    let k = ((t - from) / (to - from)).clamp(0.0, 1.0);
    start + (end - start) * k
}

/// The runtime state of one particle system: the immutable flat emitter
/// model (from ParticleSpec), the researched instance factors (script
/// writable), and the simulation state. Shared between the script engine
/// (writes the factors/controls through js.rs) and the worker's per-frame
/// simulate/build (main.rs) via Rc<RefCell<...>>, exactly like LayerState.
#[derive(Debug)]
pub struct ParticleSystemState {
    pub name: String,
    /// Position in the scene.json `objects` array — the global painter's
    /// order across kinds (main.rs merges the layer and particle draw
    /// lists by it, so a particle system that appears before an image in
    /// the file draws under it).
    pub scene_order: usize,
    /// Spawn position in scene units; (0,0) is the scene center, +y down.
    pub origin: [f32; 2],
    /// Particles per second, clamped 0..=4096 at parse (default 10).
    pub spawn_rate: f32,
    /// Seconds, clamped 0.1..=60 at parse (default 1.0).
    pub life: f32,
    /// Speed range in px/s (default 0 = stationary until gravity acts).
    /// `speedMin`/`speedMax` win over a bare `speed`; a uniform pick in
    /// the range feeds the velocity when they differ.
    pub speed_min: f32,
    pub speed_max: f32,
    /// Base launch direction in radians from +x (y down), default 0.
    pub direction: f32,
    /// Launch angle spread in radians, default 0 (all particles take the
    /// exact direction — no randomness, exact trajectories).
    pub spread: f32,
    /// Acceleration in px/s², default [0, 0]. The instance `speed` factor
    /// scales gravity too (researched WE behavior).
    pub gravity: [f32; 2],
    /// Quad half-extent endpoints in px, clamped 1..=512 (default 8).
    /// Interpolated linearly over the particle's life.
    pub size_start: f32,
    pub size_end: f32,
    /// RGBA straight-alpha, clamped 0..=1 each (default white).
    pub color_start: [f32; 4],
    pub color_end: [f32; 4],
    /// Alpha endpoints, default 1 -> 0 (particles fade out).
    pub alpha_start: f32,
    pub alpha_end: f32,
    /// Live-particle cap for this system, 1..=MAX_PARTICLES (default 1000,
    /// documented deviation from WE's 100). Excess spawns drop, never evict.
    pub max_count: u32,
    /// Object-level props from the shared parse path (draw time): blend
    /// mode is clamped to the implemented set; alpha and brightness are
    /// read-only in M3f (the script surface covers the researched
    /// IParticleSystem instance factors only).
    pub blend_mode: BlendMode,
    pub alpha: f32,
    pub brightness: f32,
    pub visible: bool,
    // The researched WE IParticleSystemInstance factors, all multiplicative
    // and default 1.0 (docs.wallpaperengine.io IParticleSystemInstance).
    // Script writes them through the Scene.getParticleSystem(...).instance
    // proxy; clamps live on the Rust side (js.rs bridges).
    pub count: f32,
    pub speed: f32,
    pub lifetime: f32,
    pub size: f32,
    pub alpha_factor: f32,
    pub rate: f32,
    /// Color modifier; the trailing "n" is WE's intentional backward
    /// compatibility spelling (documented in the API research).
    pub colorn: f32,
    /// S7 (P6): the scene's `instanceoverride.rate` multiplier, folded into
    /// BOTH emission accumulators (`spawn_accumulator`/the component-model
    /// per-emitter accumulators) — distinct from `rate` above, which is a
    /// SIM-TIME multiplier (`sim_accumulator += dt * self.rate`) and must
    /// never be fed an instance-override value (WE's `rate` instance
    /// override scales EMISSION density, not simulation speed). Default
    /// 1.0; not currently script-writable (no `Scene.getParticleSystem`
    /// proxy field for it — the M3f script surface only covers the
    /// pre-existing `rate`/`count`/etc. above).
    pub emission_rate: f32,
    // Simulation state.
    /// Emission on/off. pause() leaves live particles simulating; stop()
    /// clears them immediately (researched WE semantics).
    pub emitting: bool,
    /// The live particles (crate-internal; the sim state is only read
    /// through the methods, tests and the vertex builder included).
    pub(crate) particles: Vec<Particle>,
    /// Due-but-unspawned particles from the rate (floored per step).
    pub spawn_accumulator: f32,
    /// Wall time (× instance.rate) waiting to become fixed steps.
    pub sim_accumulator: f32,
    /// Pending emitParticles() burst, consumed at the next fixed step
    /// (works while stopped/paused, like WE).
    pub burst: u32,
    pub prng: SplitMix64,
    /// One-time per-system diagnostic for dropped spawns.
    pub capped_diag: bool,
    /// Vertices currently uploaded (6 × alive); 0 means "nothing to draw".
    /// Kept by the worker after each rebuild.
    pub vertex_count: u32,
    /// S4b: the parsed component model, when this system's `particle`
    /// value was an external file reference that resolved. `None` (every
    /// pre-S4b system, and any file reference that failed to resolve or
    /// parse) keeps the flat M3f model unchanged — `step_fixed`/
    /// `build_vertex_bytes` branch on this.
    pub(crate) component: Option<ComponentModel>,
    /// S4b: one spawn-rate accumulator per `component.emitters` entry
    /// (index-aligned), same floor-per-step/carry-leftover scheme as the
    /// flat model's single `spawn_accumulator`. Empty when `component` is
    /// `None`.
    pub(crate) emitter_accumulators: Vec<f32>,
    /// S4b: one resolved (phase, speed) pair per `Operator::Turbulence` in
    /// `component.operators`, in that order — drawn from `prng` ONCE at
    /// construction (mirrors upstream drawing these once per operator
    /// instance, not per particle or per frame). Empty when `component`
    /// has no turbulence operator (or is `None`).
    pub(crate) turbulence_runtime: Vec<(f32, f32)>,
    /// S4b: accumulated fixed-step simulation seconds, used only by the
    /// turbulence approximation's time-varying phase. Component-model only
    /// (unread by the flat model); f32 seconds, same precision-over-very-
    /// long-uptime caveat every other age/time field in this module
    /// already carries.
    pub(crate) sim_time: f32,
    /// S7: this system's texture's spritesheet grid, when its material
    /// resolved to a `.tex` container with a usable `TEXS*` frame table.
    /// `None` draws the whole texture per particle (a static sprite — the
    /// pre-S7, and still correct, behavior for a non-spritesheet texture).
    pub(crate) spritesheet: Option<SpritesheetGrid>,
    /// S7 (P7): the object's own `scale` (`ParticleSpec.scale`, from
    /// `common.scale`) — applied ONLY to component-model (file-based)
    /// systems' sprite quads in `build_vertex_bytes`; the flat M3f model
    /// never reads this field (its pre-S7 square-quad-in-absolute-scene-
    /// units behavior is unchanged). Default (1, 1) = no-op.
    pub(crate) scale: [f32; 2],
    /// S7 (P7): this system's sprite aspect ratio (`texture_ratio_for`) —
    /// the spritesheet FRAME ratio when the texture has one, else the
    /// whole texture's own height/width ratio, else 1.0 (no texture).
    /// Same component-model-only scope as `scale` above.
    pub(crate) texture_ratio: f32,
}

impl ParticleSystemState {
    /// Build the runtime state from the parsed spec. `system_index` is the
    /// system's position in the scene (the deterministic PRNG seed — the
    /// draw-order merge uses spec.scene_order instead).
    pub fn from_spec(spec: &ParticleSpec, system_index: usize) -> Self {
        let mut prng = SplitMix64::new(PRNG_SEED ^ (system_index as u64));
        let component = spec.component.clone();
        let emitter_accumulators = vec![0.0f32; component.as_ref().map_or(0, |c| c.emitters.len())];
        // Resolve each Turbulence operator's (phase, speed) ONCE here, in
        // file order, consuming `prng` before any particle spawns —
        // deterministic per system, independent of simulate()'s later
        // per-step PRNG draws. `step_fixed_component` re-derives the same
        // index by counting Turbulence operators in the same order.
        let turbulence_runtime: Vec<(f32, f32)> = component
            .as_ref()
            .map(|component| {
                component
                    .operators
                    .iter()
                    .filter_map(|operator| match operator {
                        Operator::Turbulence {
                            speed_min,
                            speed_max,
                            phase_min,
                            phase_max,
                            ..
                        } => Some((
                            uniform(&mut prng, *phase_min, *phase_max),
                            uniform(&mut prng, *speed_min, *speed_max),
                        )),
                        _ => None,
                    })
                    .collect()
            })
            .unwrap_or_default();
        Self {
            name: spec.name.clone(),
            scene_order: spec.scene_order,
            origin: spec.origin,
            spawn_rate: spec.spawn_rate,
            life: spec.life,
            speed_min: spec.speed_min,
            speed_max: spec.speed_max,
            direction: spec.direction,
            spread: spec.spread,
            gravity: spec.gravity,
            size_start: spec.size_start,
            size_end: spec.size_end,
            color_start: spec.color_start,
            color_end: spec.color_end,
            alpha_start: spec.alpha_start,
            alpha_end: spec.alpha_end,
            max_count: spec.max_count,
            blend_mode: BlendMode::clamp(spec.blend_mode),
            alpha: spec.alpha,
            brightness: spec.brightness,
            visible: spec.visible,
            // S7 (P6): initial value comes from the scene's own
            // `instanceoverride` (default 1.0 when absent, `scene.rs`'s
            // job) — a scene-authored override and a later script write
            // through `Scene.getParticleSystem(...).instance` share this
            // one field, exactly like upstream shares one state slot for
            // both (`ObjectParser.cpp:867-878` sets the initial value;
            // `CParticle.cpp` reads it as of whenever it's last written).
            count: spec.instance_count,
            speed: spec.instance_speed,
            lifetime: spec.instance_lifetime,
            size: spec.instance_size,
            alpha_factor: spec.instance_alpha,
            // `rate` is the SIM-TIME multiplier, unrelated to
            // `instanceoverride.rate` — see this field's own doc comment
            // and `emission_rate` below.
            rate: 1.0,
            colorn: spec.instance_colorn,
            emission_rate: spec.instance_rate,
            emitting: true,
            particles: Vec::new(),
            spawn_accumulator: 0.0,
            sim_accumulator: 0.0,
            burst: 0,
            prng,
            capped_diag: false,
            vertex_count: 0,
            component,
            emitter_accumulators,
            turbulence_runtime,
            sim_time: 0.0,
            spritesheet: spec
                .texture
                .as_ref()
                .and_then(|texture| texture.spritesheet),
            scale: spec.scale,
            texture_ratio: texture_ratio_for(spec.texture.as_ref()),
        }
    }

    /// Advance the simulation by one wall-clock frame of `dt` seconds.
    /// The sim time (× the instance `rate` factor) accumulates into fixed
    /// 1/60 s steps; a frame longer than MAX_FRAME_DT is clamped and the
    /// accumulator is capped at MAX_ACCUMULATED_SIM_SECONDS, so a hostile
    /// `rate` factor costs at most 60 steps (bounded frame time — the M3f
    /// cap oracle asserts on it).
    ///
    /// Returns true when at least one fixed step ran — the only way the
    /// particles can have changed, so the caller rebuilds the vertex
    /// buffer only then.
    pub fn simulate(&mut self, dt: f64) -> bool {
        self.sim_accumulator += (dt.clamp(0.0, MAX_FRAME_DT) as f32) * self.rate;
        if self.sim_accumulator > MAX_ACCUMULATED_SIM_SECONDS {
            // Bounded-rate: excess accumulated sim time is dropped
            // (documented; only reachable under hostile `rate` factors).
            self.sim_accumulator = MAX_ACCUMULATED_SIM_SECONDS;
        }
        let mut changed = false;
        while self.sim_accumulator >= FIXED_STEP {
            self.step_fixed(FIXED_STEP);
            self.sim_accumulator -= FIXED_STEP;
            changed = true;
        }
        changed
    }

    /// One fixed step. The flat model (M3f, `self.component.is_none()`)
    /// runs exactly the pre-S4b order: spawn, integrate, age out — every
    /// existing test/smoke oracle depends on this being byte-for-byte
    /// unchanged. A component-model system (S4b) instead spawns via its
    /// own emitters and runs its own operator chain
    /// (`step_fixed_component`); age/life-out is shared by both.
    fn step_fixed(&mut self, h: f32) {
        if self.component.is_some() {
            self.step_fixed_component(h);
        } else if self.emitting || self.burst > 0 {
            let mut requested: u32 = 0;
            if self.emitting {
                // S7 (P6): `emission_rate` (`instanceoverride.rate`,
                // default 1.0) scales emission density — a numerically
                // inert factor for every pre-S7 scene (no `instanceoverride`
                // -> 1.0 -> byte-identical spawn counts).
                self.spawn_accumulator += self.spawn_rate * self.count * self.emission_rate * h;
                if self.spawn_accumulator > MAX_SPAWN_ACCUMULATOR {
                    self.spawn_accumulator = MAX_SPAWN_ACCUMULATOR;
                }
                let due = self.spawn_accumulator.floor();
                self.spawn_accumulator -= due;
                requested = due as u32;
            }
            requested = requested.saturating_add(self.burst);
            self.burst = 0;
            let free = self.max_count as usize - self.particles.len();
            let take = (requested as usize).min(free);
            let dropped = requested as usize - take;
            if dropped > 0 && !self.capped_diag {
                self.capped_diag = true;
                eprintln!(
                    "event=renderer.scene.particles_capped system={} requested={dropped} \
                     note=spawn-queue-dropped-alive=never-evicted",
                    self.name
                );
            }
            for _ in 0..take {
                self.spawn_one();
            }
            // Explicit Euler: velocity first, then position, then age — the
            // order the unit tests and smoke oracles derive exact positions
            // from. Component-mode movement (gravity/drag/position) is its
            // own operator (`apply_movement`), run inside
            // `step_fixed_component` instead — this loop is flat-model only.
            for particle in &mut self.particles {
                particle.vx += self.gravity[0] * h;
                particle.vy += self.gravity[1] * h;
                particle.x += particle.vx * h;
                particle.y += particle.vy * h;
                particle.age += h;
            }
        }
        if self.component.is_some() {
            self.sim_time += h;
            for particle in &mut self.particles {
                particle.age += h;
            }
        }
        if !self.particles.is_empty() {
            self.particles
                .retain(|particle| particle.age < particle.life);
        }
    }

    /// S4b component-model step: emit via every declared emitter (its own
    /// floored spawn-rate accumulator, same never-evict/cap-and-report
    /// contract as the flat model), then run every operator in file order
    /// over every alive particle. Age-out and `sim_time` bookkeeping are
    /// shared with the flat model in `step_fixed` above (both branches
    /// converge before the final `retain`).
    fn step_fixed_component(&mut self, h: f32) {
        let component = self
            .component
            .clone()
            .expect("step_fixed_component is only called when component is Some");
        if self.emitting || self.burst > 0 {
            let mut burst_remaining = self.burst;
            self.burst = 0;
            for (index, emitter) in component.emitters.iter().enumerate() {
                let rate = emitter.rate();
                let mut requested: u32 = 0;
                if self.emitting && rate > 0.0 {
                    let accumulator = &mut self.emitter_accumulators[index];
                    // S7 (P6): same `emission_rate` factor as the flat
                    // model's `spawn_accumulator` above.
                    *accumulator += rate * self.count * self.emission_rate * h;
                    if *accumulator > MAX_SPAWN_ACCUMULATOR {
                        *accumulator = MAX_SPAWN_ACCUMULATOR;
                    }
                    let due = accumulator.floor();
                    *accumulator -= due;
                    requested = due as u32;
                }
                // The burst (emitParticles()) is delivered through the
                // FIRST emitter only — a component system's script surface
                // is the same Scene.getParticleSystem() as the flat model,
                // which has no concept of "which emitter"; matching the
                // flat model's single accumulator is the simplest faithful
                // reading.
                if index == 0 {
                    requested = requested.saturating_add(burst_remaining);
                    burst_remaining = 0;
                }
                let free = self.max_count as usize - self.particles.len();
                let take = (requested as usize).min(free);
                let dropped = requested as usize - take;
                if dropped > 0 && !self.capped_diag {
                    self.capped_diag = true;
                    eprintln!(
                        "event=renderer.scene.particles_capped system={} requested={dropped} \
                         note=spawn-queue-dropped-alive=never-evicted",
                        self.name
                    );
                }
                for _ in 0..take {
                    self.spawn_component(&component, emitter);
                }
            }
        }
        let mut turbulence_index = 0usize;
        for operator in &component.operators {
            match operator {
                Operator::Movement { gravity, drag } => {
                    apply_movement(&mut self.particles, *gravity, *drag, h)
                }
                Operator::AlphaFade { fade_in, fade_out } => {
                    apply_alpha_fade(&mut self.particles, *fade_in, *fade_out);
                }
                Operator::SizeChange {
                    start_time,
                    end_time,
                    start_value,
                    end_value,
                } => apply_size_change(
                    &mut self.particles,
                    *start_time,
                    *end_time,
                    *start_value,
                    *end_value,
                ),
                Operator::ColorChange {
                    start_time,
                    end_time,
                    start_value,
                    end_value,
                } => apply_color_change(
                    &mut self.particles,
                    *start_time,
                    *end_time,
                    *start_value,
                    *end_value,
                ),
                Operator::OscillateAlpha {
                    scale_min,
                    scale_max,
                    ..
                } => apply_oscillate_alpha(&mut self.particles, *scale_min, *scale_max),
                Operator::OscillateSize {
                    scale_min,
                    scale_max,
                    ..
                } => apply_oscillate_size(&mut self.particles, *scale_min, *scale_max),
                Operator::ControlPointAttract {
                    origin,
                    scale,
                    threshold,
                } => {
                    let anchor = [self.origin[0] + origin[0], self.origin[1] + origin[1]];
                    apply_control_point_attract(&mut self.particles, anchor, *scale, *threshold, h);
                }
                Operator::Turbulence {
                    scale, time_scale, ..
                } => {
                    let (phase, speed) = self
                        .turbulence_runtime
                        .get(turbulence_index)
                        .copied()
                        .unwrap_or((0.0, 0.0));
                    turbulence_index += 1;
                    apply_turbulence(
                        &mut self.particles,
                        *scale,
                        *time_scale,
                        phase,
                        speed,
                        self.sim_time,
                        h,
                    );
                }
            }
        }
    }

    fn spawn_one(&mut self) {
        // A uniform pick in [speedMin, speedMax] when they differ; the
        // exact value otherwise (deterministic lanes never touch the PRNG).
        let speed = if self.speed_max > self.speed_min {
            self.speed_min + (self.speed_max - self.speed_min) * self.prng.next_f32()
        } else {
            self.speed_min
        } * self.speed;
        // The flat launch model: direction ± spread/2 (uniform across the
        // cone). spread 0 = exact direction.
        let angle = if self.spread > 0.0 {
            self.direction + (self.prng.next_f32() * 2.0 - 1.0) * self.spread
        } else {
            self.direction
        };
        let (vx, vy) = (speed * angle.cos(), speed * angle.sin());
        self.particles.push(Particle {
            x: self.origin[0],
            y: self.origin[1],
            vx,
            vy,
            age: 0.0,
            // The instance lifetime factor multiplies the per-particle
            // life; at least one fixed step so the age/life fraction stays
            // defined (a zero factor spawns-and-dies next step).
            life: (self.life * self.lifetime).max(FIXED_STEP),
            ..Default::default()
        });
    }

    /// S4b: spawn one particle for a component-model system through
    /// `emitter`'s own position/velocity rule, then run every initializer
    /// in file order (each may further adjust position/velocity/size/
    /// alpha/color/life — matching upstream's "emitter sets a baseline,
    /// initializers refine it" order, `CParticle::createBoxEmitter`/
    /// `createSphereEmitter` calling `for (auto& init : m_initializers)`
    /// immediately after setting the emitter defaults).
    fn spawn_component(&mut self, component: &ComponentModel, emitter: &Emitter) {
        let (position, velocity) = match *emitter {
            Emitter::Box {
                origin,
                directions,
                distance_min,
                distance_max,
                ..
            } => {
                let mut offset = [0.0f32; 2];
                for axis in 0..2 {
                    let mut d = uniform(&mut self.prng, distance_min[axis], distance_max[axis]);
                    if self.prng.next_f32() < 0.5 {
                        d = -d;
                    }
                    offset[axis] = d * directions[axis];
                }
                (
                    [
                        self.origin[0] + origin[0] + offset[0],
                        self.origin[1] + origin[1] + offset[1],
                    ],
                    [0.0, 0.0],
                )
            }
            Emitter::Sphere {
                origin,
                directions,
                distance_min,
                distance_max,
                speed_min,
                speed_max,
                ..
            } => {
                let angle = uniform(&mut self.prng, 0.0, std::f32::consts::TAU);
                let min_sq = distance_min * distance_min;
                let max_sq = distance_max * distance_max;
                let radius = uniform(&mut self.prng, min_sq, max_sq).max(0.0).sqrt();
                let offset = [
                    radius * angle.cos() * directions[0],
                    radius * angle.sin() * directions[1],
                ];
                let velocity = if speed_max > 0.0 || speed_min != 0.0 {
                    let length = (offset[0] * offset[0] + offset[1] * offset[1]).sqrt();
                    let direction = if length > 0.0 {
                        [offset[0] / length, offset[1] / length]
                    } else {
                        [0.0, 1.0]
                    };
                    let speed = uniform(&mut self.prng, speed_min, speed_max);
                    [direction[0] * speed, direction[1] * speed]
                } else {
                    [0.0, 0.0]
                };
                (
                    [
                        self.origin[0] + origin[0] + offset[0],
                        self.origin[1] + origin[1] + offset[1],
                    ],
                    velocity,
                )
            }
        };
        let mut particle = Particle {
            x: position[0],
            y: position[1],
            vx: velocity[0],
            vy: velocity[1],
            age: 0.0,
            life: 1.0,
            size: 20.0,
            initial_size: 20.0,
            alpha: 1.0,
            initial_alpha: 1.0,
            color: [1.0, 1.0, 1.0],
            initial_color: [1.0, 1.0, 1.0],
            osc_alpha: [0.0, 0.0],
            osc_size: [0.0, 0.0],
        };
        for op in &component.operators {
            if let Operator::OscillateAlpha {
                freq_min,
                freq_max,
                phase_min,
                phase_max,
                ..
            } = op
            {
                particle.osc_alpha = [
                    uniform(&mut self.prng, *freq_min, *freq_max),
                    uniform(
                        &mut self.prng,
                        *phase_min,
                        phase_max + std::f32::consts::TAU,
                    ),
                ];
            }
            if let Operator::OscillateSize {
                freq_min,
                freq_max,
                phase_min,
                phase_max,
                ..
            } = op
            {
                particle.osc_size = [
                    uniform(&mut self.prng, *freq_min, *freq_max),
                    uniform(
                        &mut self.prng,
                        *phase_min,
                        phase_max + std::f32::consts::TAU,
                    ),
                ];
            }
        }
        for initializer in &component.initializers {
            apply_initializer(initializer, &mut particle, &mut self.prng);
        }
        // S7 (P6): the instance-override factors apply AFTER the
        // initializers set their baseline values — Avatar's "Fog 2" (rate
        // 0.58), "Light shafts" (rate 3.35, alpha 0.055), "Dust motes"
        // (size 0.35) etc. all rely on this; a factor of 1.0 (the
        // pre-instanceoverride default, and every scene without one) makes
        // every line below a no-op, so this is numerically inert for the
        // flat (M3f) model, which never calls `spawn_component` at all.
        // `initial_*` are refreshed too since they are the baseline
        // `build_vertex_bytes`/operators read back for a component system.
        //
        // Borrowed-From: Almamu/linux-wallpaperengine (GPL-3.0-or-later)
        // src/WallpaperEngine/Render/Objects/CParticle.cpp:715-790 @
        // b016d7d1 — adapted.
        particle.size *= self.size;
        particle.initial_size = particle.size;
        particle.alpha *= self.alpha_factor;
        particle.initial_alpha = particle.alpha;
        for channel in &mut particle.color {
            *channel *= self.colorn;
        }
        particle.initial_color = particle.color;
        particle.life *= self.lifetime;
        particle.vx *= self.speed;
        particle.vy *= self.speed;
        particle.life = particle.life.max(FIXED_STEP);
        self.particles.push(particle);
    }

    /// Resume emission (researched WE play(): existing particles keep
    /// simulating; isPlaying() reports "emitting or alive").
    pub fn play(&mut self) {
        self.emitting = true;
    }

    /// Stop emission only: live particles keep simulating and aging out
    /// (researched WE pause()).
    pub fn pause(&mut self) {
        self.emitting = false;
    }

    /// Clear every particle immediately and stop emission (researched WE
    /// stop(); emitParticles() still works afterwards).
    pub fn stop(&mut self) {
        self.emitting = false;
        self.particles.clear();
        self.spawn_accumulator = 0.0;
        self.burst = 0;
        self.vertex_count = 0;
    }

    /// True while emission is on or any particle is still alive
    /// (researched WE isPlaying()).
    pub fn is_playing(&self) -> bool {
        self.emitting || !self.particles.is_empty()
    }

    /// Queue an instant burst, spawned at the next fixed step regardless
    /// of stop/pause (researched WE emitParticles(); default 1 at the JS
    /// boundary). Clamped to MAX_PARTICLES; excess is dropped at spawn.
    pub fn emit_particles(&mut self, count: u32) {
        self.burst = self.burst.saturating_add(count).min(MAX_PARTICLES as u32);
    }

    /// Rebuild the vertex bytes for the current particles into `scratch`
    /// (cleared and reused — the worker passes one Vec for every system
    /// every frame, so this allocates only when a system grows past its
    /// previous high-water mark): 6 vertices per particle of {pos.xy,
    /// uv.xy, color.rgba, size, pad} (40-byte stride, shaders/particle.vert).
    /// The quad is expanded around the particle center in scene pixels;
    /// size and color are interpolated over the age/life fraction and the
    /// instance size/colorn/alpha factors are folded in here (the draw
    /// pushes no per-particle state). Returns the vertex count (6 × alive);
    /// the bytes live in `scratch` until the next call.
    pub fn build_vertex_bytes(&self, scratch: &mut Vec<u8>) -> u32 {
        scratch.clear();
        scratch.reserve(self.particles.len() * 6 * PARTICLE_VERTEX_BYTES);
        let mut vertex_count = 0u32;
        for particle in &self.particles {
            // S4b: a component-model particle carries its OWN current
            // size/alpha/color, maintained by its operators each fixed
            // step (`step_fixed_component`) — the flat model (unchanged)
            // still derives them here from the spec's start/end endpoints
            // and the age/life fraction, exactly as every pre-S4b test
            // pins.
            let (size, color) = if self.component.is_some() {
                (
                    particle.size,
                    [
                        particle.color[0],
                        particle.color[1],
                        particle.color[2],
                        particle.alpha,
                    ],
                )
            } else {
                let k = (particle.age / particle.life).clamp(0.0, 1.0);
                let size = (self.size_start + (self.size_end - self.size_start) * k) * self.size;
                let color = [
                    (self.color_start[0] + (self.color_end[0] - self.color_start[0]) * k)
                        * self.colorn,
                    (self.color_start[1] + (self.color_end[1] - self.color_start[1]) * k)
                        * self.colorn,
                    (self.color_start[2] + (self.color_end[2] - self.color_start[2]) * k)
                        * self.colorn,
                    (self.alpha_start + (self.alpha_end - self.alpha_start) * k)
                        * self.alpha_factor,
                ];
                (size, color)
            };
            // S7 (P7): component-model (file-based) systems apply the
            // object's own `scale` and this system's texture aspect ratio
            // to the sprite quad — upstream's model matrix =
            // translate(origin)·rotate·scale(object scale), sprite quad =
            // size × (1, textureRatio) (`CParticle.cpp:1832-1844,
            // 1912-1940`). The flat M3f model below is BYTE-IDENTICAL to
            // pre-S7: square quads in absolute scene units, ignoring scale
            // entirely (every M3f smoke oracle depends on this).
            let (x0, x1, y0, y1) = if self.component.is_some() {
                let lx = particle.x - self.origin[0];
                let ly = particle.y - self.origin[1];
                let cx = self.origin[0] + lx * self.scale[0];
                let cy = self.origin[1] + ly * self.scale[1];
                let half_x = size * 0.5 * self.scale[0].abs();
                let half_y = size * 0.5 * self.scale[1].abs() * self.texture_ratio;
                (cx - half_x, cx + half_x, cy - half_y, cy + half_y)
            } else {
                let half = size * 0.5;
                (
                    particle.x - half,
                    particle.x + half,
                    particle.y - half,
                    particle.y + half,
                )
            };
            // S7: when this system's texture is a spritesheet, remap the
            // 0..1 corner UVs below into the current frame's box instead of
            // sampling the whole atlas (upstream `ComputeSpriteFrame` +
            // `genericparticle.vert`'s `v_TexCoord += uvOffsets.xyxy`,
            // re-derived for CPU use in `SpritesheetGrid`). The frame is
            // picked fresh every rebuild from age/life — no persisted
            // per-particle frame field needed, see `SpritesheetGrid::
            // frame_for`'s doc.
            let (uv_origin, uv_size) = match self.spritesheet {
                Some(grid) => {
                    let life_fraction = (particle.age / particle.life).clamp(0.0, 1.0);
                    let frame = grid.frame_for(particle.age, life_fraction);
                    grid.frame_uv_origin_and_size(frame)
                }
                None => ([0.0, 0.0], [1.0, 1.0]),
            };
            // Corner order mirrors UNIT_QUAD and the text glyph quads:
            // tl, tr, br, br, bl, tl, full-texture UVs (remapped into the
            // frame box above when spritesheet is Some).
            for (px, py, u, v) in [
                (x0, y0, uv_origin[0], uv_origin[1]),
                (x1, y0, uv_origin[0] + uv_size[0], uv_origin[1]),
                (x1, y1, uv_origin[0] + uv_size[0], uv_origin[1] + uv_size[1]),
                (x1, y1, uv_origin[0] + uv_size[0], uv_origin[1] + uv_size[1]),
                (x0, y1, uv_origin[0], uv_origin[1] + uv_size[1]),
                (x0, y0, uv_origin[0], uv_origin[1]),
            ] {
                scratch.extend_from_slice(&px.to_le_bytes());
                scratch.extend_from_slice(&py.to_le_bytes());
                scratch.extend_from_slice(&u.to_le_bytes());
                scratch.extend_from_slice(&v.to_le_bytes());
                for channel in color {
                    scratch.extend_from_slice(&channel.to_le_bytes());
                }
                scratch.extend_from_slice(&size.to_le_bytes());
                scratch.extend_from_slice(&0.0f32.to_le_bytes()); // stride pad
            }
            vertex_count += 6;
        }
        vertex_count
    }
}

// ---- S4b: component-model initializer/operator free functions. Free
// functions (not methods) so each is independently unit-testable against a
// bare `&mut [Particle]`/`&mut Particle` without a whole ParticleSystemState.

/// Apply one initializer to a freshly-spawned particle (S4b). Matches
/// upstream's per-initializer behavior (`CParticle::create*RandomInitializer`)
/// with the documented 2D/no-rotation scope cut.
fn apply_initializer(initializer: &Initializer, particle: &mut Particle, prng: &mut SplitMix64) {
    match initializer {
        Initializer::LifetimeRandom { min, max } => {
            particle.life = uniform(prng, *min, *max).max(FIXED_STEP);
        }
        Initializer::SizeRandom { min, max, exponent } => {
            let t = prng.next_f32().max(0.0).powf(*exponent);
            // S7b (B2): halved to match upstream's `p.size = (...) / 2.0f`
            // (`CParticle.cpp:738`) — see the variant's doc comment.
            let value = ((min + t * (max - min)) * 0.5).clamp(0.0, MAX_PARTICLE_SIZE);
            particle.size = value;
            particle.initial_size = value;
        }
        Initializer::AlphaRandom { min, max } => {
            let value = uniform(prng, *min, *max).clamp(0.0, 1.0);
            particle.alpha = value;
            particle.initial_alpha = value;
        }
        Initializer::VelocityRandom { min, max } => {
            particle.vx += uniform(prng, min[0], max[0]);
            particle.vy += uniform(prng, min[1], max[1]);
        }
        Initializer::ColorRandom { min, max } => {
            let color = [
                uniform(prng, min[0], max[0]).clamp(0.0, 1.0),
                uniform(prng, min[1], max[1]).clamp(0.0, 1.0),
                uniform(prng, min[2], max[2]).clamp(0.0, 1.0),
            ];
            particle.color = color;
            particle.initial_color = color;
        }
    }
}

/// `movement` operator (S4b): integrate position from the CURRENT
/// velocity, then apply gravity and drag to velocity for the next step —
/// upstream's exact order (`CParticle::createMovementOperator`,
/// "Update position FIRST using current velocity ... Then apply forces to
/// modify velocity for NEXT frame"), deliberately different from the flat
/// model's velocity-then-position order (that order is pinned by the M3f
/// tests and left untouched).
fn apply_movement(particles: &mut [Particle], gravity: [f32; 2], drag: f32, h: f32) {
    let drag_factor = (1.0 - drag * h).max(0.0);
    for particle in particles {
        particle.x += particle.vx * h;
        particle.y += particle.vy * h;
        particle.vx = (particle.vx + gravity[0] * h) * drag_factor;
        particle.vy = (particle.vy + gravity[1] * h) * drag_factor;
    }
}

fn apply_alpha_fade(particles: &mut [Particle], fade_in: f32, fade_out: f32) {
    for particle in particles {
        let life = (particle.age / particle.life).clamp(0.0, 1.0);
        particle.alpha = if life <= fade_in {
            particle.initial_alpha * fade_value(life, 0.0, fade_in, 0.0, 1.0)
        } else if life > fade_out {
            particle.initial_alpha * (1.0 - fade_value(life, fade_out, 1.0, 0.0, 1.0))
        } else {
            particle.initial_alpha
        };
    }
}

fn apply_size_change(
    particles: &mut [Particle],
    start_time: f32,
    end_time: f32,
    start_value: f32,
    end_value: f32,
) {
    for particle in particles {
        let life = (particle.age / particle.life).clamp(0.0, 1.0);
        let multiplier = fade_value(life, start_time, end_time, start_value, end_value);
        particle.size = (particle.initial_size * multiplier).clamp(0.0, MAX_PARTICLE_SIZE);
    }
}

fn apply_color_change(
    particles: &mut [Particle],
    start_time: f32,
    end_time: f32,
    start_value: [f32; 3],
    end_value: [f32; 3],
) {
    for particle in particles {
        let life = (particle.age / particle.life).clamp(0.0, 1.0);
        let mut color = [0.0f32; 3];
        for channel in 0..3 {
            let multiplier = fade_value(
                life,
                start_time,
                end_time,
                start_value[channel],
                end_value[channel],
            );
            color[channel] = (particle.initial_color[channel] * multiplier).clamp(0.0, 1.0);
        }
        particle.color = color;
    }
}

/// `oscillatealpha`/`oscillatesize` (S4b): multiply the CURRENT value
/// (already set by an earlier `alphafade`/`sizechange` this same step, or
/// left at its spawn value if neither ran) by a cosine wave between
/// `scale_min` and `scale_max`, using the per-particle (freq, phase)
/// `spawn_component` drew once. Matches upstream's
/// `mix(scaleMin, scaleMax, (cos(freq*age+phase)+1)/2)`.
fn apply_oscillate_alpha(particles: &mut [Particle], scale_min: f32, scale_max: f32) {
    for particle in particles {
        let [freq, phase] = particle.osc_alpha;
        let cos_val = ((freq * particle.age + phase).cos() + 1.0) * 0.5;
        let multiplier = scale_min + (scale_max - scale_min) * cos_val;
        particle.alpha = (particle.alpha * multiplier).clamp(0.0, 1.0);
    }
}

fn apply_oscillate_size(particles: &mut [Particle], scale_min: f32, scale_max: f32) {
    for particle in particles {
        let [freq, phase] = particle.osc_size;
        let cos_val = ((freq * particle.age + phase).cos() + 1.0) * 0.5;
        let multiplier = scale_min + (scale_max - scale_min) * cos_val;
        particle.size = (particle.size * multiplier).clamp(0.0, MAX_PARTICLE_SIZE);
    }
}

/// `controlpointattract` (S4b, scoped to a static anchor — see the module
/// doc's control-point cut): a constant-force pull toward `anchor` for
/// particles within `threshold` px, matching upstream's per-step
/// `velocity += normalize(anchor - position) * scale * dt`.
fn apply_control_point_attract(
    particles: &mut [Particle],
    anchor: [f32; 2],
    scale: f32,
    threshold: f32,
    h: f32,
) {
    for particle in particles {
        let dx = anchor[0] - particle.x;
        let dy = anchor[1] - particle.y;
        let distance = (dx * dx + dy * dy).sqrt();
        if distance > 0.001 && distance < threshold {
            particle.vx += (dx / distance) * scale * h;
            particle.vy += (dy / distance) * scale * h;
        }
    }
}

/// `turbulence` (S4b) — a bounded deterministic APPROXIMATION of
/// upstream's Perlin curl-noise field (see the module-level doc), NOT
/// upstream's algorithm: a sine/cosine directional field sampled at each
/// particle's own position (scaled by `scale`, clamped so the noise
/// frequency itself stays bounded) plus a time term (`time_scale *
/// sim_time`) and the per-system resolved `phase`, normalized to a unit
/// vector and scaled by the per-system resolved `speed`. `speed <=
/// 0.0001` is a no-op (matches upstream's own early-out for a degenerate
/// draw).
fn apply_turbulence(
    particles: &mut [Particle],
    scale: f32,
    time_scale: f32,
    phase: f32,
    speed: f32,
    sim_time: f32,
    h: f32,
) {
    if speed <= 0.0001 {
        return;
    }
    let freq = (scale.max(0.0) * 2.0).min(10.0);
    let time_term = time_scale * sim_time;
    for particle in particles {
        let nx = (particle.x * freq + phase + time_term).sin();
        let ny = (particle.y * freq + phase + time_term).cos();
        let len = (nx * nx + ny * ny).sqrt().max(1e-4);
        particle.vx += (nx / len) * speed * h;
        particle.vy += (ny / len) * speed * h;
    }
}

/// Build the draw entries for every visible, non-empty, textured particle
/// system. `texture_ok` is the per-system texture table (main.rs). Each
/// draw addresses texture slot MAX_LAYERS + system_index — the renderer's
/// particle slot layout — with the identity model (spawn positions are
/// baked into the CPU vertices) and the object-level alpha/brightness.
pub fn particle_draws(
    systems: &[Rc<RefCell<ParticleSystemState>>],
    texture_ok: &[bool],
) -> Vec<LayerDraw> {
    let mut draws = Vec::new();
    for (index, system) in systems.iter().enumerate() {
        let system = system.borrow();
        if !system.visible || system.vertex_count == 0 || texture_ok.get(index) != Some(&true) {
            continue;
        }
        draws.push(LayerDraw {
            layer_index: MAX_LAYERS + index,
            scene_order: system.scene_order,
            m: [[1.0, 0.0], [0.0, 1.0]],
            t: [0.0, 0.0],
            alpha: system.alpha,
            blend_mode: system.blend_mode,
            brightness: system.brightness,
            tint: [1.0, 1.0, 1.0, 1.0],
            kind: DrawKind::Particles {
                vertex_count: system.vertex_count,
            },
            material: false,
        });
    }
    draws
}

// ---- Clamp helpers shared by the parser (scene.rs) and the JS bridges
// (js.rs). The parse clamps follow the M3f policy: emitter values clamp to
// their documented ranges, non-finite values fall back to the documented
// defaults. The instance factors (script writes) treat non-finite as the
// identity 1.0 instead (researched WE default) — see clamp_instance_factor.

pub fn clamp_spawn_rate(value: f64) -> f32 {
    if !value.is_finite() {
        DEFAULT_PARTICLE_SPAWN_RATE
    } else {
        value.clamp(
            f64::from(MIN_PARTICLE_SPAWN_RATE),
            f64::from(MAX_PARTICLE_SPAWN_RATE),
        ) as f32
    }
}

pub fn clamp_life(value: f64) -> f32 {
    if !value.is_finite() {
        DEFAULT_PARTICLE_LIFE
    } else {
        value.clamp(f64::from(MIN_PARTICLE_LIFE), f64::from(MAX_PARTICLE_LIFE)) as f32
    }
}

pub fn clamp_speed(value: f64) -> f32 {
    if !value.is_finite() {
        DEFAULT_PARTICLE_SPEED
    } else {
        value.clamp(0.0, f64::from(MAX_PARTICLE_SPEED)) as f32
    }
}

pub fn clamp_direction(value: f64) -> f32 {
    if !value.is_finite() {
        DEFAULT_PARTICLE_DIRECTION
    } else {
        // Clamp in f64 BEFORE the f32 cast: a finite f64 like 1e300 casts
        // to f32::INFINITY, and sin/cos(∞) is NaN — the per-particle
        // launch angle would be NaN and the system would be poisoned
        // permanently (NaN positions never integrate out).
        value.clamp(
            -f64::from(MAX_PARTICLE_DIRECTION),
            f64::from(MAX_PARTICLE_DIRECTION),
        ) as f32
    }
}

pub fn clamp_spread(value: f64) -> f32 {
    if !value.is_finite() {
        DEFAULT_PARTICLE_SPREAD
    } else {
        value.clamp(0.0, f64::from(MAX_PARTICLE_SPREAD)) as f32
    }
}

pub fn clamp_gravity(value: f64) -> f32 {
    if !value.is_finite() {
        0.0
    } else {
        value.clamp(
            -f64::from(MAX_PARTICLE_GRAVITY),
            f64::from(MAX_PARTICLE_GRAVITY),
        ) as f32
    }
}

pub fn clamp_size(value: f64) -> f32 {
    if !value.is_finite() {
        DEFAULT_PARTICLE_SIZE
    } else {
        value.clamp(f64::from(MIN_PARTICLE_SIZE), f64::from(MAX_PARTICLE_SIZE)) as f32
    }
}

pub fn clamp_alpha(value: f64) -> f32 {
    if !value.is_finite() {
        0.0
    } else {
        value.clamp(0.0, 1.0) as f32
    }
}

pub fn clamp_color_component(value: f64) -> f32 {
    if !value.is_finite() {
        0.0
    } else {
        value.clamp(0.0, 1.0) as f32
    }
}

pub fn clamp_max_count(value: u64) -> u32 {
    u32::try_from(value.clamp(1, MAX_PARTICLES as u64)).unwrap_or(DEFAULT_PARTICLE_MAX_COUNT)
}

/// Clamp an instance factor written from script: non-finite values become
/// the identity 1.0 (the researched WE default — a hostile NaN must never
/// kill a system), magnitude clamps to 0..=`max` (1e6 for count/speed/
/// lifetime/size/rate, 1.0 for alpha/colorn).
pub fn clamp_instance_factor(value: f64, max: f64) -> f32 {
    if !value.is_finite() {
        return 1.0;
    }
    value.clamp(0.0, max) as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A bare spec with the documented defaults, fields overridable per test.
    fn spec_with(f: impl FnOnce(&mut ParticleSpec)) -> ParticleSpec {
        let mut spec = ParticleSpec {
            name: "dust".to_string(),
            scene_order: 0,
            origin: [0.0, 0.0],
            spawn_rate: DEFAULT_PARTICLE_SPAWN_RATE,
            life: DEFAULT_PARTICLE_LIFE,
            speed_min: 0.0,
            speed_max: 0.0,
            direction: 0.0,
            spread: 0.0,
            gravity: [0.0, 0.0],
            size_start: DEFAULT_PARTICLE_SIZE,
            size_end: DEFAULT_PARTICLE_SIZE,
            color_start: [1.0, 1.0, 1.0, 1.0],
            color_end: [1.0, 1.0, 1.0, 1.0],
            alpha_start: DEFAULT_PARTICLE_ALPHA_START,
            alpha_end: DEFAULT_PARTICLE_ALPHA_END,
            max_count: DEFAULT_PARTICLE_MAX_COUNT,
            material: None,
            blend_mode: 0,
            alpha: 1.0,
            visible: true,
            brightness: 1.0,
            texture: None,
            file_ref: None,
            component: None,
            instance_count: 1.0,
            instance_rate: 1.0,
            instance_size: 1.0,
            instance_lifetime: 1.0,
            instance_speed: 1.0,
            instance_alpha: 1.0,
            instance_colorn: 1.0,
            scale: [1.0, 1.0],
        };
        f(&mut spec);
        spec
    }

    fn state_with(f: impl FnOnce(&mut ParticleSpec)) -> ParticleSystemState {
        ParticleSystemState::from_spec(&spec_with(f), 0)
    }

    /// Run `frames` frames of `dt` seconds and return the states.
    fn simulate_frames(state: &mut ParticleSystemState, dt: f64, frames: usize) -> usize {
        (0..frames).filter(|_| state.simulate(dt)).count()
    }

    #[test]
    fn exact_positions_for_fixed_dt_sequence() {
        // No gravity, fixed speed, direction 0, spread 0: every particle
        // moves exactly `speed × h` per fixed step in +x, so a particle
        // spawned in step s sits at x = (n - s + 1) × speed × h after n
        // steps — the smoke oracle's travel band is derived from this.
        let mut state = state_with(|s| {
            s.spawn_rate = 60.0; // exactly one particle per step
            s.life = 2.0;
            s.speed_min = 60.0;
            s.speed_max = 60.0;
            s.direction = 0.0;
            s.spread = 0.0;
        });
        let mut changed = 0;
        for _frame in 0..30 {
            changed += usize::from(state.simulate(1.0 / 30.0));
        }
        // simulate() reports "a fixed step ran", so 30 frames with 2 fixed
        // steps each report 30 changes (the 60 steps are the particle
        // count below).
        assert_eq!(changed, 30);
        // 60 steps of 1/60 s at 60/s = 60 spawns; life 2 s = 120 steps, all alive.
        assert_eq!(state.particles.len(), 60);
        // The i-th particle (0-based) spawned at step i: x = (59 - i + 1) px,
        // y exactly 0 (no gravity, no spread).
        for (i, particle) in state.particles.iter().enumerate() {
            let expected_x = (60 - i) as f32;
            assert!(
                (particle.x - expected_x).abs() < 1e-6,
                "particle {i}: x = {}, expected {expected_x}",
                particle.x
            );
            assert_eq!(particle.y, 0.0, "particle {i}: y must stay exactly 0");
            // age accumulates as repeated f32 additions (one per fixed
            // step), so compare against the single-multiply expectation
            // with tolerance.
            assert!(
                (particle.age - ((60 - i) as f32) * FIXED_STEP).abs() < 1e-4,
                "particle {i}: age = {}, expected {}",
                particle.age,
                ((60 - i) as f32) * FIXED_STEP
            );
            assert_eq!(particle.vx, 60.0);
            assert_eq!(particle.vy, 0.0);
        }
        // Aging: the oldest particle's age after n steps is n repeated f32
        // additions of FIXED_STEP — at 120 steps it is 1.9999988 (still
        // alive), at 121 it crosses life 2.0. Deaths then cascade one per
        // step, so the population settles at the steady state
        // spawn_rate × life = 60/s × 2 s = 120 alive: at 180 steps exactly
        // the 60 originals (spawned in steps 0..59) have aged out and 180
        // spawns have run, leaving 120.
        simulate_frames(&mut state, 1.0 / 30.0, 30); // 120 steps total
        assert_eq!(state.particles.len(), 120, "all alive at 120 steps");
        simulate_frames(&mut state, 1.0 / 30.0, 30); // 180 steps total
        assert_eq!(
            state.particles.len(),
            120,
            "steady state at 180 steps: 60 aged out, 120 alive (len {})",
            state.particles.len()
        );
        // The survivors are exactly the particles spawned in steps 60..179;
        // the oldest still alive (spawned in step 60) has run 120
        // integrations and sits just below life.
        assert!(
            state.particles[0].age < 2.0,
            "oldest survivor age {} must be below life 2.0",
            state.particles[0].age
        );
        // No particle ever violates the age/life invariant.
        for particle in &state.particles {
            assert!(particle.age < particle.life);
        }
    }

    #[test]
    fn gravity_integrates_explicitly() {
        // Gravity [0, 80], speed 0: after n steps a particle at rest
        // (spawned in step 1) sits at y = Σ_{k=1..n} 80·k·h² = 80·h²·n(n+1)/2.
        let mut state = state_with(|s| {
            s.spawn_rate = 60.0;
            s.life = 10.0;
            s.gravity = [0.0, 80.0];
        });
        simulate_frames(&mut state, 1.0 / 30.0, 30);
        let first = &state.particles[0]; // spawned in step 0, moved 60 times
        assert_eq!(first.vx, 0.0);
        assert!((first.vy - 80.0).abs() < 1e-6, "vy = {}", first.vy);
        let expected_y = 80.0 * FIXED_STEP * FIXED_STEP * (60.0 * 61.0 / 2.0);
        assert!(
            (first.y - expected_y).abs() < 1e-3,
            "y = {}, expected {expected_y}",
            first.y
        );
        assert_eq!(first.x, 0.0);
    }

    #[test]
    fn size_and_color_interpolate_over_life() {
        // One particle, aged by hand to exact fractions of its life: the
        // vertex color and size must sit at the lerp points.
        let mut state = state_with(|s| {
            s.spawn_rate = 0.0; // manual control
            s.size_start = 10.0;
            s.size_end = 20.0;
            s.color_start = [1.0, 0.0, 0.0, 1.0];
            s.color_end = [0.0, 1.0, 0.0, 1.0];
            s.alpha_start = 1.0;
            s.alpha_end = 0.0;
            s.life = 1.0;
        });
        state.particles.push(Particle {
            x: 0.0,
            y: 0.0,
            vx: 0.0,
            vy: 0.0,
            age: 0.5, // k = 0.5
            life: 1.0,
            ..Default::default()
        });
        let mut scratch = Vec::new();
        let count = state.build_vertex_bytes(&mut scratch);
        assert_eq!(count, 6);
        assert_eq!(scratch.len(), 6 * PARTICLE_VERTEX_BYTES);
        // Vertex 0: x0 = 0 - size/2 (size = 15 at k=0.5), y0 = -7.5, uv (0,0),
        // color = (0.5, 0.5, 0, 0.5), size 15, pad 0.
        let v = |bytes: &[u8], i: usize, f: usize| {
            let off = i * PARTICLE_VERTEX_BYTES + f * 4;
            f32::from_le_bytes(bytes[off..off + 4].try_into().unwrap())
        };
        assert_eq!(v(&scratch, 0, 0), -7.5);
        assert_eq!(v(&scratch, 0, 1), -7.5);
        assert_eq!(v(&scratch, 0, 2), 0.0);
        assert_eq!(v(&scratch, 0, 3), 0.0);
        assert!((v(&scratch, 0, 4) - 0.5).abs() < 1e-6); // r
        assert!((v(&scratch, 0, 5) - 0.5).abs() < 1e-6); // g
        assert_eq!(v(&scratch, 0, 6), 0.0); // b
        assert!((v(&scratch, 0, 7) - 0.5).abs() < 1e-6); // a
        assert_eq!(v(&scratch, 0, 8), 15.0); // size
        assert_eq!(v(&scratch, 0, 9), 0.0); // pad
        // Corner 3 (index 3) is br at (7.5, 7.5) with uv (1,1).
        assert_eq!(v(&scratch, 3, 0), 7.5);
        assert_eq!(v(&scratch, 3, 1), 7.5);
        assert_eq!(v(&scratch, 3, 2), 1.0);
        assert_eq!(v(&scratch, 3, 3), 1.0);
        // Endpoints: k=0 and k=1 with exact sizes (scratch is cleared and
        // reused by each build — the worker's per-frame pattern).
        state.particles[0].age = 0.0;
        let _ = state.build_vertex_bytes(&mut scratch);
        // Corner 0 x: bytes 0..4 (8..12 would be its uv).
        assert_eq!(f32::from_le_bytes(scratch[0..4].try_into().unwrap()), -5.0);
        state.particles[0].age = state.particles[0].life;
        let _ = state.build_vertex_bytes(&mut scratch);
        let half = f32::from_le_bytes(scratch[0..4].try_into().unwrap());
        assert!((half + 10.0).abs() < 1e-6, "half = {half}");
        // Aged past life: the fraction clamps to 1 (compaction removes the
        // particle next step; build must still be well-defined).
        state.particles[0].age = 2.0;
        let _ = state.build_vertex_bytes(&mut scratch);
        let half = f32::from_le_bytes(scratch[0..4].try_into().unwrap());
        assert!((half + 10.0).abs() < 1e-6);
    }

    #[test]
    fn spritesheet_remaps_particle_uvs_into_the_current_frame_box() {
        // S7 regression for the "rainbow stars" bug: a system whose texture
        // is a 4x1 spritesheet must sample ONE frame's box per particle,
        // never the whole atlas (the pre-S7 behavior — uv (0,0)..(1,1)
        // every time, drawing every frame's colors stacked into one
        // sprite).
        let mut state = state_with(|s| {
            s.spawn_rate = 0.0;
            s.size_start = 10.0;
            s.size_end = 10.0;
            s.life = 4.0;
        });
        state.spritesheet = Some(SpritesheetGrid {
            cols: 4,
            rows: 1,
            frame_count: 4,
            duration: 0.0, // life-fraction driven
        });
        state.particles.push(Particle {
            age: 2.0, // life fraction 0.5 -> frame 2 of 4 (col 2, row 0)
            life: 4.0,
            ..Default::default()
        });
        let mut scratch = Vec::new();
        let count = state.build_vertex_bytes(&mut scratch);
        assert_eq!(count, 6);
        let uv = |i: usize| {
            let off = i * PARTICLE_VERTEX_BYTES + 2 * 4;
            let u = f32::from_le_bytes(scratch[off..off + 4].try_into().unwrap());
            let v = f32::from_le_bytes(scratch[off + 4..off + 8].try_into().unwrap());
            (u, v)
        };
        // Corner 0 (tl) sits at the frame origin; corner 3 (br) at
        // origin + frame size (0.25, 1.0) — NEVER (0,0)/(1,1), which would
        // be the whole-atlas UVs this fix replaces.
        assert_eq!(uv(0), (0.5, 0.0));
        assert_eq!(uv(3), (0.75, 1.0));
    }

    #[test]
    fn no_spritesheet_still_draws_the_whole_texture() {
        // Non-spritesheet systems (the overwhelming majority) must keep the
        // pre-S7 full 0..1 UVs exactly — this is the regression guard for
        // every existing static-sprite particle system.
        let mut state = state_with(|s| {
            s.spawn_rate = 0.0;
            s.size_start = 10.0;
            s.size_end = 10.0;
        });
        assert!(state.spritesheet.is_none());
        state.particles.push(Particle {
            age: 0.5,
            life: 1.0,
            ..Default::default()
        });
        let mut scratch = Vec::new();
        state.build_vertex_bytes(&mut scratch);
        let uv = |i: usize| {
            let off = i * PARTICLE_VERTEX_BYTES + 2 * 4;
            let u = f32::from_le_bytes(scratch[off..off + 4].try_into().unwrap());
            let v = f32::from_le_bytes(scratch[off + 4..off + 8].try_into().unwrap());
            (u, v)
        };
        assert_eq!(uv(0), (0.0, 0.0));
        assert_eq!(uv(3), (1.0, 1.0));
    }

    #[test]
    fn instance_factors_fold_into_vertices() {
        let mut state = state_with(|s| {
            s.spawn_rate = 0.0;
            s.size_start = 8.0;
            s.size_end = 8.0;
            s.alpha_start = 1.0;
            s.alpha_end = 1.0;
        });
        state.size = 2.0;
        state.alpha_factor = 0.25;
        state.colorn = 0.5;
        state.particles.push(Particle {
            x: 0.0,
            y: 0.0,
            vx: 0.0,
            vy: 0.0,
            age: 0.0,
            life: 1.0,
            ..Default::default()
        });
        let mut scratch = Vec::new();
        let _ = state.build_vertex_bytes(&mut scratch);
        let v = |bytes: &[u8], f: usize| {
            f32::from_le_bytes(bytes[f * 4..f * 4 + 4].try_into().unwrap())
        };
        assert_eq!(v(&scratch, 8), 16.0, "size factor doubles the quad extent");
        assert_eq!(v(&scratch, 7), 0.25, "alpha factor scales the vertex alpha");
        assert_eq!(v(&scratch, 4), 0.5, "colorn scales RGB");
        assert_eq!(v(&scratch, 6), 0.5);
    }

    /// S7 (P6): `instanceoverride.count` (fed into `spec.instance_count` ->
    /// `state.count`, the SAME field the flat model's spawn accumulator
    /// already used) at 0.0 must stop emission entirely — WE's day/night
    /// star systems (Avatar) rely on this to show no stars by day. Not
    /// just "fewer" particles: exactly zero, after a full 2 s of updates.
    #[test]
    fn instance_count_zero_spawns_nothing_after_two_seconds() {
        let mut state = state_with(|s| {
            s.spawn_rate = 100.0;
            s.life = 10.0;
            s.instance_count = 0.0;
        });
        simulate_frames(&mut state, 1.0 / 60.0, 120); // 2 s
        assert_eq!(state.particles.len(), 0);
    }

    /// S7 (P6): the component-model path's own instance factors
    /// (`spawn_component`, applied AFTER the initializers) — `instance_size`
    /// 2.0 doubles the freshly spawned particle's size (baseline 20.0, no
    /// `SizeRandom` initializer in this fixture) and the refreshed
    /// `initial_size` the operators would read back.
    #[test]
    fn component_model_instance_size_factor_doubles_a_freshly_spawned_particle() {
        let component = ComponentModel {
            maxcount: 100,
            emitters: vec![Emitter::Box {
                rate: 60.0,
                origin: [0.0, 0.0],
                directions: [1.0, 1.0],
                distance_min: [0.0, 0.0],
                distance_max: [0.0, 0.0],
            }],
            initializers: vec![],
            operators: vec![],
        };
        let mut spec = spec_with(|s| {
            s.max_count = 100;
            s.instance_size = 2.0;
        });
        spec.component = Some(component);
        let mut state = ParticleSystemState::from_spec(&spec, 0);
        state.simulate(1.0 / 60.0);
        assert_eq!(state.particles.len(), 1);
        assert_eq!(state.particles[0].size, 40.0);
        assert_eq!(state.particles[0].initial_size, 40.0);
    }

    /// Reads vertex `i`'s (x, y) position out of a `build_vertex_bytes`
    /// scratch buffer (40-byte stride: pos.xy, uv.xy, color.rgba, size,
    /// pad — see `PARTICLE_VERTEX_BYTES`).
    fn vertex_pos(bytes: &[u8], i: usize) -> (f32, f32) {
        let off = i * PARTICLE_VERTEX_BYTES;
        let x = f32::from_le_bytes(bytes[off..off + 4].try_into().unwrap());
        let y = f32::from_le_bytes(bytes[off + 4..off + 8].try_into().unwrap());
        (x, y)
    }

    /// S7 (P7): a component-model system's own `scale` multiplies BOTH the
    /// particle's offset from the system origin AND the quad's half-extent
    /// — upstream's translate(origin)·rotate·scale(object scale) model
    /// matrix. Corner order is tl(0), tr(1), br(2)/(2 dup at 3), bl(4),
    /// tl(5) — see the doc comment above `build_vertex_bytes`'s corner
    /// array — so vertex 0's x is the quad's left edge and vertex 1's x is
    /// the right edge.
    #[test]
    fn component_model_scale_doubles_offset_and_quad_width() {
        let mut state = component_state(
            ComponentModel {
                maxcount: 100,
                emitters: vec![],
                initializers: vec![],
                operators: vec![],
            },
            100,
        );
        state.scale = [2.0, 1.0];
        state.particles.push(Particle {
            x: 10.0, // 10 px right of the origin (0, 0)
            y: 0.0,
            size: 4.0,
            life: 1.0,
            ..Default::default()
        });
        let mut scratch = Vec::new();
        let count = state.build_vertex_bytes(&mut scratch);
        assert_eq!(count, 6);
        let (x0, _) = vertex_pos(&scratch, 0);
        let (x1, _) = vertex_pos(&scratch, 1);
        // cx = origin.x + (10 - origin.x) * 2 = 20; half_x = 4 * 0.5 * 2 = 4.
        assert_eq!((x0, x1), (16.0, 24.0));
    }

    /// S7 (P7): `texture_ratio` (from the system's texture aspect) scales
    /// only the quad's HEIGHT, not its width — a 0.5 ratio halves the
    /// y-extent while the x-extent stays the plain `size * 0.5 *
    /// scale.x.abs()`.
    #[test]
    fn component_model_texture_ratio_scales_quad_height_only() {
        let mut state = component_state(
            ComponentModel {
                maxcount: 100,
                emitters: vec![],
                initializers: vec![],
                operators: vec![],
            },
            100,
        );
        state.texture_ratio = 0.5;
        state.particles.push(Particle {
            x: 0.0,
            y: 0.0,
            size: 10.0,
            life: 1.0,
            ..Default::default()
        });
        let mut scratch = Vec::new();
        state.build_vertex_bytes(&mut scratch);
        let (x0, y0) = vertex_pos(&scratch, 0);
        let (x1, y1) = vertex_pos(&scratch, 2); // br corner
        assert_eq!(
            (x0, x1),
            (-5.0, 5.0),
            "x-extent unaffected by texture_ratio"
        );
        assert_eq!(
            (y0, y1),
            (-2.5, 2.5),
            "y-extent halved by texture_ratio 0.5"
        );
    }

    /// S7 (P7): a negative scale component mirrors that axis — the
    /// particle's offset from the origin flips sign (upstream's
    /// `wind-blur` object, scale `(-2, 2)`, is the corpus example this
    /// regresses against). The quad half-extent uses the ABSOLUTE value
    /// (a mirrored quad is not an inside-out one).
    #[test]
    fn component_model_negative_scale_mirrors_the_axis() {
        let mut state = component_state(
            ComponentModel {
                maxcount: 100,
                emitters: vec![],
                initializers: vec![],
                operators: vec![],
            },
            100,
        );
        state.scale = [-2.0, 1.0];
        state.particles.push(Particle {
            x: 10.0,
            y: 0.0,
            size: 4.0,
            life: 1.0,
            ..Default::default()
        });
        let mut scratch = Vec::new();
        state.build_vertex_bytes(&mut scratch);
        let (x0, _) = vertex_pos(&scratch, 0);
        let (x1, _) = vertex_pos(&scratch, 1);
        // cx = 0 + (10 - 0) * -2 = -20; half_x = 4 * 0.5 * |-2| = 4.
        assert_eq!((x0, x1), (-24.0, -16.0));
    }

    #[test]
    fn cap_drops_excess_spawns_never_evicts_live() {
        // maxCount 8; 100/s at 1/60 steps = 1.6667 per step: the 9th, 10th,
        // ... particles are dropped at the cap, live ones are never touched.
        let mut state = state_with(|s| {
            s.spawn_rate = 100.0;
            s.life = 60.0;
            s.max_count = 8;
        });
        let changed = simulate_frames(&mut state, 1.0 / 30.0, 15); // 30 steps
        assert_eq!(changed, 15, "one report per frame with a fixed step");
        assert_eq!(state.particles.len(), 8);
        assert!(state.capped_diag);
        // The first particle survived untouched (never evicted); its age
        // is the accumulated f32 sum of 30 fixed steps.
        assert!(
            (state.particles[0].age - 30.0 * FIXED_STEP).abs() < 1e-4,
            "age = {}",
            state.particles[0].age
        );
        // And the cap holds forever: 300 more steps.
        simulate_frames(&mut state, 1.0 / 30.0, 150);
        assert_eq!(state.particles.len(), 8);
        // The original 8 are still the same particles (never evicted,
        // life 60 s = 3600 steps): the first one's age accumulated
        // through all 330 steps.
        assert!(
            (state.particles[0].age - 330.0 * FIXED_STEP).abs() < 1e-2,
            "age = {}",
            state.particles[0].age
        );
    }

    #[test]
    fn cap_diag_emitted_once() {
        let mut state = state_with(|s| {
            s.spawn_rate = 100.0;
            s.life = 60.0;
            s.max_count = 8;
        });
        simulate_frames(&mut state, 1.0 / 30.0, 60);
        assert!(state.capped_diag);
        // The flag is one-time; the diagnostic itself is eprintln (bounded
        // by construction — a smoke oracle greps the renderer's stderr).
        state.capped_diag = false;
        simulate_frames(&mut state, 1.0 / 30.0, 60);
        assert!(state.capped_diag);
    }

    #[test]
    fn deterministic_across_independent_runs() {
        // Two identical systems driven through the same dt sequence must
        // produce bit-identical particles (the fixed-step + seeded PRNG
        // contract the smoke oracles rely on).
        let run = |seed: u64| {
            let mut state = ParticleSystemState::from_spec(
                &spec_with(|s| {
                    s.spawn_rate = 25.0;
                    s.life = 3.0;
                    s.speed_min = 10.0;
                    s.speed_max = 100.0; // PRNG range, spread too
                    s.spread = std::f32::consts::TAU;
                    s.gravity = [0.0, 30.0];
                }),
                seed as usize,
            );
            let mut scratch = Vec::new();
            for _ in 0..200 {
                state.simulate(1.0 / 30.0);
            }
            let count = state.build_vertex_bytes(&mut scratch);
            (scratch, count, state.particles.len())
        };
        assert_eq!(run(3), run(3));
        // Different seeds give different streams (the system index seeds
        // each system independently).
        let (a, _, _) = run(3);
        let (b, _, _) = run(4);
        assert_ne!(a, b);
    }

    #[test]
    fn controls_pause_stop_play_emit() {
        let mut state = state_with(|s| {
            s.spawn_rate = 60.0;
            s.life = 60.0;
        });
        simulate_frames(&mut state, 1.0 / 30.0, 1); // 2 steps, 2 particles
        assert!(state.is_playing());
        state.pause();
        assert!(!state.emitting);
        let before = state.particles.len();
        simulate_frames(&mut state, 1.0 / 30.0, 5);
        assert_eq!(state.particles.len(), before, "pause stops emission");
        assert!(
            state.particles.iter().all(|p| p.age > 0.0),
            "particles keep aging"
        );
        // emitParticles works while paused (researched WE).
        state.emit_particles(3);
        simulate_frames(&mut state, 1.0 / 30.0, 1);
        assert_eq!(state.particles.len(), before + 3);
        state.stop();
        assert!(state.particles.is_empty());
        assert!(!state.is_playing());
        simulate_frames(&mut state, 1.0 / 30.0, 5);
        assert!(state.particles.is_empty(), "stop clears forever");
        state.play();
        assert!(state.is_playing());
        simulate_frames(&mut state, 1.0 / 30.0, 1);
        assert_eq!(state.particles.len(), 2);
    }

    #[test]
    fn burst_clamped_to_max_particles() {
        let mut state = state_with(|s| {
            s.spawn_rate = 0.0;
            s.life = 60.0;
            s.max_count = 4;
        });
        state.emit_particles(MAX_PARTICLES as u32);
        assert_eq!(state.burst, MAX_PARTICLES as u32);
        // Spawned at the next step: 4 fit, the rest drop (with the diag).
        simulate_frames(&mut state, 1.0 / 30.0, 1);
        assert_eq!(state.particles.len(), 4);
        assert!(state.capped_diag);
    }

    #[test]
    fn spawn_accumulator_floored_per_step() {
        // 10/s at 1/60 steps leaves 1/6 of a particle per step: 6 steps
        // spawn exactly 1 particle (floor 1.6667 = 1, 3.3333... floor 3...),
        // i.e. step boundaries keep the leftover: 30 steps -> 5 particles.
        let mut state = state_with(|s| {
            s.spawn_rate = 10.0;
            s.life = 60.0;
        });
        simulate_frames(&mut state, 1.0 / 30.0, 15);
        assert_eq!(state.particles.len(), 5);
        // 300 steps -> 50 particles.
        simulate_frames(&mut state, 1.0 / 30.0, 135);
        assert_eq!(state.particles.len(), 50);
    }

    #[test]
    fn hostile_factors_stay_bounded() {
        // rate 1e6: the accumulator caps at MAX_ACCUMULATED_SIM_SECONDS,
        // so a frame costs at most 60 steps — bounded frame time.
        let mut state = state_with(|s| {
            s.spawn_rate = 4096.0;
            s.life = 60.0;
            s.max_count = MAX_PARTICLES as u32;
        });
        state.rate = 1e6;
        let start = std::time::Instant::now();
        let changed = simulate_frames(&mut state, 1.0 / 30.0, 30);
        assert_eq!(changed, 30, "one report per frame");
        // The accumulator cap means every frame ran exactly 60 fixed steps:
        // 30 frames of 1/30 s wall time produced 1800 steps = 30 s of
        // simulation, visible as the oldest particle's age.
        assert!(
            (state.particles[0].age - 30.0).abs() < 1e-3,
            "oldest age = {}",
            state.particles[0].age
        );
        assert!(start.elapsed() < std::time::Duration::from_secs(5));
        // The spawn accumulator is capped too: never more than
        // MAX_SPAWN_ACCUMULATOR pending, and the live set never exceeds the
        // hard cap.
        assert!(state.spawn_accumulator <= MAX_SPAWN_ACCUMULATOR);
        assert!(state.particles.len() <= MAX_PARTICLES);
    }

    #[test]
    fn factor_clamps() {
        // Script writes: non-finite -> identity 1.0.
        assert_eq!(clamp_instance_factor(f64::NAN, 1e6), 1.0);
        assert_eq!(clamp_instance_factor(f64::INFINITY, 1e6), 1.0);
        assert_eq!(clamp_instance_factor(f64::NEG_INFINITY, 1e6), 1.0);
        // Range: count/speed/lifetime/size/rate clamp 0..=1e6.
        assert_eq!(clamp_instance_factor(-5.0, 1e6), 0.0);
        assert_eq!(clamp_instance_factor(2e9, 1e6), 1e6);
        assert_eq!(clamp_instance_factor(2.5, 1e6), 2.5);
        // Alpha/colorn clamp 0..=1.
        assert_eq!(clamp_instance_factor(-1.0, 1.0), 0.0);
        assert_eq!(clamp_instance_factor(7.0, 1.0), 1.0);
    }

    #[test]
    fn parse_clamps_fall_back_to_defaults() {
        assert_eq!(clamp_spawn_rate(999999.0), MAX_PARTICLE_SPAWN_RATE);
        assert_eq!(clamp_spawn_rate(f64::NAN), DEFAULT_PARTICLE_SPAWN_RATE);
        assert_eq!(clamp_spawn_rate(-1.0), 0.0);
        assert_eq!(clamp_life(0.0), MIN_PARTICLE_LIFE);
        assert_eq!(clamp_life(999.0), MAX_PARTICLE_LIFE);
        assert_eq!(clamp_life(f64::INFINITY), DEFAULT_PARTICLE_LIFE);
        assert_eq!(clamp_speed(-1.0), 0.0);
        assert_eq!(clamp_speed(1e12), MAX_PARTICLE_SPEED);
        assert_eq!(clamp_speed(f64::NAN), 0.0);
        assert_eq!(clamp_spread(1e6), MAX_PARTICLE_SPREAD);
        assert_eq!(clamp_spread(-1.0), 0.0);
        assert_eq!(clamp_spread(f64::NAN), 0.0);
        assert_eq!(clamp_gravity(1e12), MAX_PARTICLE_GRAVITY);
        assert_eq!(clamp_gravity(-1e12), -MAX_PARTICLE_GRAVITY);
        assert_eq!(clamp_gravity(f64::NAN), 0.0);
        assert_eq!(clamp_size(0.5), MIN_PARTICLE_SIZE);
        assert_eq!(clamp_size(9999.0), MAX_PARTICLE_SIZE);
        assert_eq!(clamp_size(f64::NAN), DEFAULT_PARTICLE_SIZE);
        assert_eq!(clamp_alpha(-1.0), 0.0);
        assert_eq!(clamp_alpha(2.0), 1.0);
        assert_eq!(clamp_alpha(f64::NAN), 0.0);
        assert_eq!(clamp_color_component(2.0), 1.0);
        assert_eq!(clamp_max_count(0), 1);
        assert_eq!(clamp_max_count(1_000_000), MAX_PARTICLES as u32);
        assert_eq!(clamp_max_count(5), 5);
        assert_eq!(
            clamp_direction(std::f64::consts::FRAC_PI_2),
            std::f32::consts::FRAC_PI_2
        );
        assert_eq!(clamp_direction(f64::NAN), 0.0);
        // Finite but huge: the f64 -> f32 cast of 1e300 overflows to
        // f32::INFINITY, and sin/cos(INFINITY) is NaN — the launch angle
        // must clamp to ±1e6 in f64 before the cast instead.
        assert_eq!(clamp_direction(1e300), MAX_PARTICLE_DIRECTION);
        assert_eq!(clamp_direction(-1e300), -MAX_PARTICLE_DIRECTION);
        assert_eq!(clamp_direction(f64::INFINITY), 0.0);
        assert!(clamp_direction(1e300).is_finite());
    }

    #[test]
    fn particle_draws_skip_invisible_empty_untextured() {
        use std::rc::Rc;
        let mut spec = spec_with(|s| {
            s.spawn_rate = 60.0;
            s.life = 60.0;
        });
        spec.blend_mode = 6; // add — the pipeline variant must follow the spec
        let state = Rc::new(RefCell::new(ParticleSystemState::from_spec(&spec, 0)));
        let states = std::slice::from_ref(&state);
        // Untextured: skipped.
        assert!(particle_draws(states, &[false]).is_empty());
        // Textured but empty: skipped.
        let draw = particle_draws(states, &[true]);
        assert!(draw.is_empty());
        state.borrow_mut().simulate(1.0 / 30.0);
        state.borrow_mut().vertex_count = 6;
        let draws = particle_draws(states, &[true]);
        assert_eq!(draws.len(), 1);
        let draw = &draws[0];
        assert_eq!(draw.layer_index, MAX_LAYERS);
        assert_eq!(draw.scene_order, 0, "the spec's objects-array position");
        assert_eq!(draw.blend_mode, BlendMode::Add);
        assert_eq!(draw.kind, DrawKind::Particles { vertex_count: 6 });
        assert_eq!(draw.m, [[1.0, 0.0], [0.0, 1.0]]);
        assert_eq!(draw.t, [0.0, 0.0]);
        assert_eq!(draw.tint, [1.0, 1.0, 1.0, 1.0]);
        // Invisible or texture-failed: skipped.
        state.borrow_mut().visible = false;
        assert!(particle_draws(states, &[true]).is_empty());
        state.borrow_mut().visible = true;
        assert!(particle_draws(states, &[false]).is_empty());
    }

    #[test]
    fn max_systems_cap_guarded_by_parser() {
        // The runtime never holds more than MAX_PARTICLE_SYSTEMS systems;
        // the parser enforces it (scene.rs). This pins the constant the
        // texture slot layout (MAX_LAYERS + i) and the descriptor pool are
        // sized from.
        const _: () = assert!(
            MAX_PARTICLE_SYSTEMS <= 64 && MAX_PARTICLE_SYSTEMS <= MAX_LAYERS,
            "particle systems must stay within the fixed texture-slot layout"
        );
    }

    // ---- S4b: component-model simulation tests. ----

    fn component_state(component: ComponentModel, max_count: u32) -> ParticleSystemState {
        let mut spec = spec_with(|s| {
            s.max_count = max_count;
        });
        spec.component = Some(component);
        ParticleSystemState::from_spec(&spec, 0)
    }

    #[test]
    fn box_emitter_defaults_spawn_exactly_at_origin() {
        let component = ComponentModel {
            maxcount: 100,
            emitters: vec![Emitter::Box {
                rate: 60.0,
                origin: [0.0, 0.0],
                directions: [1.0, 1.0],
                distance_min: [0.0, 0.0],
                distance_max: [0.0, 0.0],
            }],
            initializers: vec![],
            operators: vec![],
        };
        let mut state = component_state(component, 100);
        state.simulate(1.0 / 60.0);
        assert_eq!(state.particles.len(), 1);
        let p = &state.particles[0];
        assert_eq!((p.x, p.y), (0.0, 0.0));
        assert_eq!((p.vx, p.vy), (0.0, 0.0));
        // No lifetimerandom initializer: the spawn-time default (1.0) holds.
        assert_eq!(p.life, 1.0);
    }

    #[test]
    fn sphere_emitter_with_speed_launches_radially_outward() {
        let component = ComponentModel {
            maxcount: 100,
            emitters: vec![Emitter::Sphere {
                rate: 60.0,
                origin: [0.0, 0.0],
                directions: [1.0, 1.0],
                distance_min: 10.0,
                distance_max: 10.0,
                speed_min: 50.0,
                speed_max: 50.0,
            }],
            initializers: vec![],
            operators: vec![],
        };
        let mut state = component_state(component, 100);
        state.simulate(1.0 / 60.0);
        assert_eq!(state.particles.len(), 1);
        let p = &state.particles[0];
        let radius = (p.x * p.x + p.y * p.y).sqrt();
        assert!((radius - 10.0).abs() < 1e-3, "radius = {radius}");
        let speed = (p.vx * p.vx + p.vy * p.vy).sqrt();
        assert!((speed - 50.0).abs() < 1e-3, "speed = {speed}");
        // Velocity points radially outward: v is parallel to (x, y).
        let cross = p.x * p.vy - p.y * p.vx;
        assert!(cross.abs() < 1e-2, "velocity not radial: cross = {cross}");
    }

    #[test]
    fn velocity_random_initializer_adds_to_emitter_velocity() {
        let component = ComponentModel {
            maxcount: 10,
            emitters: vec![Emitter::Box {
                rate: 60.0,
                origin: [0.0, 0.0],
                directions: [1.0, 1.0],
                distance_min: [0.0, 0.0],
                distance_max: [0.0, 0.0],
            }],
            initializers: vec![Initializer::VelocityRandom {
                min: [30.0, 30.0],
                max: [30.0, 30.0],
            }],
            operators: vec![],
        };
        let mut state = component_state(component, 10);
        state.simulate(1.0 / 60.0);
        assert_eq!((state.particles[0].vx, state.particles[0].vy), (30.0, 30.0));
    }

    #[test]
    fn lifetime_size_alpha_color_random_initializers_set_exact_values_at_zero_spread() {
        let component = ComponentModel {
            maxcount: 10,
            emitters: vec![Emitter::Box {
                rate: 60.0,
                origin: [0.0, 0.0],
                directions: [1.0, 1.0],
                distance_min: [0.0, 0.0],
                distance_max: [0.0, 0.0],
            }],
            initializers: vec![
                Initializer::LifetimeRandom { min: 4.0, max: 4.0 },
                Initializer::SizeRandom {
                    min: 40.0,
                    max: 40.0,
                    exponent: 1.0,
                },
                Initializer::AlphaRandom { min: 0.5, max: 0.5 },
                Initializer::ColorRandom {
                    min: [0.2, 0.4, 0.6],
                    max: [0.2, 0.4, 0.6],
                },
            ],
            operators: vec![],
        };
        let mut state = component_state(component, 10);
        state.simulate(1.0 / 60.0);
        let p = &state.particles[0];
        assert_eq!(p.life, 4.0);
        assert_eq!(
            p.size, 20.0,
            "sizerandom halves the authored diameter (S7b/B2, CParticle.cpp:738 p.size = (...) / 2.0f) before feeding build_vertex_bytes's own half=size*0.5"
        );
        assert_eq!(p.initial_size, 20.0);
        assert_eq!(p.alpha, 0.5);
        assert_eq!(p.initial_alpha, 0.5);
        assert_eq!(p.color, [0.2, 0.4, 0.6]);
        assert_eq!(p.initial_color, [0.2, 0.4, 0.6]);
    }

    #[test]
    fn movement_operator_integrates_position_then_applies_gravity_and_drag() {
        // Upstream order: position += velocity*h FIRST, then velocity +=
        // gravity*h and *= (1 - drag*h) — verified directly on the free
        // function against one particle so the order is pinned exactly.
        let mut particles = vec![Particle {
            x: 0.0,
            y: 0.0,
            vx: 10.0,
            vy: 0.0,
            ..Default::default()
        }];
        apply_movement(&mut particles, [0.0, 20.0], 0.5, 1.0);
        let p = &particles[0];
        assert_eq!(p.x, 10.0, "position used the PRE-update velocity");
        assert_eq!(p.y, 0.0);
        assert_eq!(p.vy, 20.0 * 0.5, "gravity applied, then drag halves it");
        assert_eq!(p.vx, 10.0 * 0.5);
    }

    #[test]
    fn movement_drag_never_reverses_velocity() {
        let mut particles = vec![Particle {
            vx: 10.0,
            ..Default::default()
        }];
        // drag*h = 5.0 > 1.0: the drag factor clamps to 0, not negative.
        apply_movement(&mut particles, [0.0, 0.0], 5.0, 1.0);
        assert_eq!(particles[0].vx, 0.0);
    }

    #[test]
    fn alpha_fade_ramps_in_and_out_and_holds_between() {
        let mut particles = vec![
            particle_at_life_fraction(0.05, 1.0), // inside fade-in
            particle_at_life_fraction(0.5, 1.0),  // held
            particle_at_life_fraction(0.95, 1.0), // inside fade-out
        ];
        apply_alpha_fade(&mut particles, 0.1, 0.9);
        assert!(
            (particles[0].alpha - 0.5).abs() < 1e-4,
            "{}",
            particles[0].alpha
        );
        assert_eq!(particles[1].alpha, 1.0);
        assert!(
            (particles[2].alpha - 0.5).abs() < 1e-4,
            "{}",
            particles[2].alpha
        );
    }

    fn particle_at_life_fraction(fraction: f32, life: f32) -> Particle {
        Particle {
            age: fraction * life,
            life,
            initial_alpha: 1.0,
            initial_size: 100.0,
            ..Default::default()
        }
    }

    #[test]
    fn size_change_ramps_linearly_between_endpoints() {
        // start_value/end_value are MULTIPLIERS on initial_size (upstream:
        // `p.size = p.initial.size * fadeValue(...)`), typically 1.0 ->
        // 0.0 (shrink to nothing over life) — not absolute sizes.
        let mut particles = vec![particle_at_life_fraction(0.25, 4.0)];
        apply_size_change(&mut particles, 0.0, 1.0, 1.0, 0.0);
        // life fraction 0.25 of a linear 1.0 -> 0.0 ramp = 0.75, times the
        // fixture's initial_size (100.0) = 75.
        assert!(
            (particles[0].size - 75.0).abs() < 1e-3,
            "{}",
            particles[0].size
        );
    }

    #[test]
    fn size_change_clamps_to_the_hard_size_cap() {
        let mut particles = vec![particle_at_life_fraction(0.0, 1.0)];
        apply_size_change(&mut particles, 0.0, 1.0, 1e9, 1e9);
        assert!(particles[0].size <= MAX_PARTICLE_SIZE);
        assert!(particles[0].size.is_finite());
    }

    #[test]
    fn color_change_ramps_each_channel_independently() {
        let mut particles = vec![Particle {
            age: 0.5,
            life: 1.0,
            initial_color: [1.0, 1.0, 1.0],
            ..Default::default()
        }];
        apply_color_change(&mut particles, 0.0, 1.0, [1.0, 0.0, 0.5], [0.0, 1.0, 0.5]);
        let color = particles[0].color;
        assert!((color[0] - 0.5).abs() < 1e-3);
        assert!((color[1] - 0.5).abs() < 1e-3);
        assert!((color[2] - 0.5).abs() < 1e-3);
    }

    #[test]
    fn oscillate_alpha_and_size_stay_bounded_by_scale_range() {
        let mut particles = vec![Particle {
            age: 0.0,
            alpha: 1.0,
            size: 10.0,
            osc_alpha: [1.0, 0.0],
            osc_size: [1.0, std::f32::consts::PI], // phase pi: cos(0+pi) = -1 -> min
            ..Default::default()
        }];
        apply_oscillate_alpha(&mut particles, 0.2, 0.8);
        // age=0, phase=0: cos(0) = 1 -> cos_val = 1 -> multiplier = scale_max.
        assert!(
            (particles[0].alpha - 0.8).abs() < 1e-4,
            "{}",
            particles[0].alpha
        );
        apply_oscillate_size(&mut particles, 2.0, 4.0);
        // phase=pi: cos_val = 0 -> multiplier = scale_min = 2.0.
        assert!(
            (particles[0].size - 20.0).abs() < 1e-3,
            "{}",
            particles[0].size
        );
    }

    #[test]
    fn control_point_attract_only_pulls_within_threshold() {
        let mut particles = vec![
            Particle {
                x: 5.0,
                y: 0.0,
                ..Default::default()
            }, // inside threshold
            Particle {
                x: 500.0,
                y: 0.0,
                ..Default::default()
            }, // outside threshold
        ];
        apply_control_point_attract(&mut particles, [0.0, 0.0], 100.0, 50.0, 1.0);
        assert!(particles[0].vx < 0.0, "pulled toward the anchor");
        assert_eq!(particles[1].vx, 0.0, "outside threshold: untouched");
    }

    #[test]
    fn turbulence_is_deterministic_bounded_and_off_below_the_speed_floor() {
        let mut a = vec![Particle {
            x: 12.0,
            y: -7.0,
            ..Default::default()
        }];
        let mut b = a.clone();
        apply_turbulence(&mut a, 0.5, 1.0, 0.3, 200.0, 5.0, 1.0 / 60.0);
        apply_turbulence(&mut b, 0.5, 1.0, 0.3, 200.0, 5.0, 1.0 / 60.0);
        assert_eq!((a[0].vx, a[0].vy), (b[0].vx, b[0].vy), "deterministic");
        let delta = (a[0].vx * a[0].vx + a[0].vy * a[0].vy).sqrt();
        // Bounded: at most speed * h per step (a unit-length force scaled
        // by speed and h).
        assert!(delta <= 200.0 / 60.0 + 1e-3, "delta = {delta}");
        // Below the speed floor: a documented no-op (matches upstream's
        // own early-out for a degenerate draw).
        let mut c = vec![Particle::default()];
        apply_turbulence(&mut c, 0.5, 1.0, 0.3, 0.00001, 5.0, 1.0 / 60.0);
        assert_eq!((c[0].vx, c[0].vy), (0.0, 0.0));
    }

    #[test]
    fn component_system_respects_max_count_and_reports_capped_diag() {
        let component = ComponentModel {
            maxcount: 4,
            emitters: vec![Emitter::Box {
                rate: 4096.0,
                origin: [0.0, 0.0],
                directions: [1.0, 1.0],
                distance_min: [0.0, 0.0],
                distance_max: [0.0, 0.0],
            }],
            initializers: vec![Initializer::LifetimeRandom {
                min: 60.0,
                max: 60.0,
            }],
            operators: vec![],
        };
        let mut state = component_state(component, 4);
        for _ in 0..120 {
            state.simulate(1.0 / 60.0);
        }
        assert!(state.particles.len() <= 4);
        assert!(state.capped_diag);
    }

    #[test]
    fn component_system_stays_finite_and_bounded_under_a_hostile_operator_chain() {
        // Every field at an extreme-but-parser-legal value (mirrors what
        // particlefile.rs's clamps would actually let through): the sim
        // must never produce NaN/Inf and must never exceed the hard caps,
        // over many steps.
        let component = ComponentModel {
            maxcount: MAX_PARTICLES as u32,
            emitters: vec![
                Emitter::Box {
                    rate: 100_000.0,
                    origin: [0.0, 0.0],
                    directions: [1.0, 1.0],
                    distance_min: [0.0, 0.0],
                    distance_max: [1e6, 1e6],
                },
                Emitter::Sphere {
                    rate: 100_000.0,
                    origin: [0.0, 0.0],
                    directions: [1.0, 1.0],
                    distance_min: 0.0,
                    distance_max: 1e6,
                    speed_min: 0.0,
                    speed_max: MAX_PARTICLE_SPEED,
                },
            ],
            initializers: vec![
                Initializer::LifetimeRandom { min: 0.0, max: 1e6 },
                Initializer::SizeRandom {
                    min: 0.0,
                    max: MAX_PARTICLE_SIZE * 4.0,
                    exponent: 0.01,
                },
                Initializer::VelocityRandom {
                    min: [-MAX_PARTICLE_SPEED, -MAX_PARTICLE_SPEED],
                    max: [MAX_PARTICLE_SPEED, MAX_PARTICLE_SPEED],
                },
            ],
            operators: vec![
                Operator::Movement {
                    gravity: [MAX_PARTICLE_GRAVITY, -MAX_PARTICLE_GRAVITY],
                    drag: 1000.0,
                },
                Operator::AlphaFade {
                    fade_in: 0.0,
                    fade_out: 1.0,
                },
                Operator::SizeChange {
                    start_time: 0.0,
                    end_time: 1.0,
                    start_value: 1e3,
                    end_value: 0.0,
                },
                Operator::ColorChange {
                    start_time: 0.0,
                    end_time: 1.0,
                    start_value: [1.0, 0.0, 0.5],
                    end_value: [0.0, 1.0, 0.5],
                },
                Operator::OscillateAlpha {
                    freq_min: 0.0,
                    freq_max: 1000.0,
                    scale_min: 0.0,
                    scale_max: 1000.0,
                    phase_min: -1e4,
                    phase_max: 1e4,
                },
                Operator::OscillateSize {
                    freq_min: 0.0,
                    freq_max: 1000.0,
                    scale_min: 0.0,
                    scale_max: 1000.0,
                    phase_min: -1e4,
                    phase_max: 1e4,
                },
                Operator::ControlPointAttract {
                    origin: [0.0, 0.0],
                    scale: 1e7,
                    threshold: 1e6,
                },
                Operator::Turbulence {
                    scale: 1e6,
                    time_scale: 1e6,
                    speed_min: 0.0,
                    speed_max: MAX_PARTICLE_SPEED,
                    phase_min: -1e4,
                    phase_max: 1e4,
                },
            ],
        };
        let mut state = component_state(component, MAX_PARTICLES as u32);
        let start = std::time::Instant::now();
        for _ in 0..300 {
            state.simulate(1.0 / 30.0);
        }
        assert!(start.elapsed() < std::time::Duration::from_secs(10));
        assert!(state.particles.len() <= MAX_PARTICLES);
        let mut scratch = Vec::new();
        state.build_vertex_bytes(&mut scratch);
        for chunk in scratch.chunks_exact(4) {
            let value = f32::from_le_bytes(chunk.try_into().unwrap());
            assert!(
                value.is_finite(),
                "hostile component sim produced non-finite vertex data"
            );
        }
    }
}
