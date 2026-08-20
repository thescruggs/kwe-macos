// SPDX-License-Identifier: Apache-2.0
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

/// Hard cap on the number of particle systems a scene can register.
/// Mirrors text::MAX_TEXT_LAYERS; systems past it are skipped (counted for
/// the worker's one-time diagnostic, never a rejection).
pub const MAX_PARTICLE_SYSTEMS: usize = 16;

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
/// Direction is an angle in radians from +x (y down); the WE emitters have
/// no direction/spread fields (velocity comes from the velocity-random
/// initializer) — this flat model is the M3f extension the deterministic
/// smoke oracles need (documented deviation).
pub const DEFAULT_PARTICLE_DIRECTION: f32 = 0.0;
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

/// Seed for the per-system splitmix64 stream. Deterministic by system
/// index (documented deviation from WE's per-frame randomness): the same
/// scene renders the same particles on every run, which is what makes the
/// range oracles reproducible. Systems with spread 0 and a fixed speed
/// never touch the stream at all (their trajectories are exact).
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
#[derive(Debug, Clone)]
pub(crate) struct Particle {
    x: f32,
    y: f32,
    vx: f32,
    vy: f32,
    age: f32,
    life: f32,
}

/// The runtime state of one particle system: the immutable flat emitter
/// model (from ParticleSpec), the researched instance factors (script
/// writable), and the simulation state. Shared between the script engine
/// (writes the factors/controls through js.rs) and the worker's per-frame
/// simulate/build (main.rs) via Rc<RefCell<...>>, exactly like LayerState.
#[derive(Debug)]
pub struct ParticleSystemState {
    pub name: String,
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
}

impl ParticleSystemState {
    /// Build the runtime state from the parsed spec. `system_index` is the
    /// system's position in the scene (the deterministic PRNG seed).
    pub fn from_spec(spec: &ParticleSpec, system_index: usize) -> Self {
        Self {
            name: spec.name.clone(),
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
            count: 1.0,
            speed: 1.0,
            lifetime: 1.0,
            size: 1.0,
            alpha_factor: 1.0,
            rate: 1.0,
            colorn: 1.0,
            emitting: true,
            particles: Vec::new(),
            spawn_accumulator: 0.0,
            sim_accumulator: 0.0,
            burst: 0,
            prng: SplitMix64::new(PRNG_SEED ^ (system_index as u64)),
            capped_diag: false,
            vertex_count: 0,
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

    /// One fixed step, in the documented order: spawn, integrate, age out.
    fn step_fixed(&mut self, h: f32) {
        if self.emitting || self.burst > 0 {
            let mut requested: u32 = 0;
            if self.emitting {
                self.spawn_accumulator += self.spawn_rate * self.count * h;
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
        }
        // Explicit Euler: velocity first, then position, then age — the
        // order the unit tests and smoke oracles derive exact positions from.
        for particle in &mut self.particles {
            particle.vx += self.gravity[0] * h;
            particle.vy += self.gravity[1] * h;
            particle.x += particle.vx * h;
            particle.y += particle.vy * h;
            particle.age += h;
        }
        if !self.particles.is_empty() {
            self.particles
                .retain(|particle| particle.age < particle.life);
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
        });
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

    /// Rebuild the vertex bytes for the current particles: 6 vertices per
    /// particle of {pos.xy, uv.xy, color.rgba, size, pad} (40-byte stride,
    /// shaders/particle.vert). The quad is expanded around the particle
    /// center in scene pixels; size and color are interpolated over the
    /// age/life fraction and the instance size/colorn/alpha factors are
    /// folded in here (the draw pushes no per-particle state). Returns the
    /// bytes and the vertex count (6 × alive).
    pub fn build_vertex_bytes(&self) -> (Vec<u8>, u32) {
        let mut bytes = Vec::with_capacity(self.particles.len() * 6 * PARTICLE_VERTEX_BYTES);
        let mut vertex_count = 0u32;
        for particle in &self.particles {
            let k = (particle.age / particle.life).clamp(0.0, 1.0);
            let size = (self.size_start + (self.size_end - self.size_start) * k) * self.size;
            let half = size * 0.5;
            let (x0, x1) = (particle.x - half, particle.x + half);
            let (y0, y1) = (particle.y - half, particle.y + half);
            let color = [
                (self.color_start[0] + (self.color_end[0] - self.color_start[0]) * k) * self.colorn,
                (self.color_start[1] + (self.color_end[1] - self.color_start[1]) * k) * self.colorn,
                (self.color_start[2] + (self.color_end[2] - self.color_start[2]) * k) * self.colorn,
                (self.alpha_start + (self.alpha_end - self.alpha_start) * k) * self.alpha_factor,
            ];
            // Corner order mirrors UNIT_QUAD and the text glyph quads:
            // tl, tr, br, br, bl, tl, full-texture UVs.
            for (px, py, u, v) in [
                (x0, y0, 0.0_f32, 0.0_f32),
                (x1, y0, 1.0_f32, 0.0_f32),
                (x1, y1, 1.0_f32, 1.0_f32),
                (x1, y1, 1.0_f32, 1.0_f32),
                (x0, y1, 0.0_f32, 1.0_f32),
                (x0, y0, 0.0_f32, 0.0_f32),
            ] {
                bytes.extend_from_slice(&px.to_le_bytes());
                bytes.extend_from_slice(&py.to_le_bytes());
                bytes.extend_from_slice(&u.to_le_bytes());
                bytes.extend_from_slice(&v.to_le_bytes());
                for channel in color {
                    bytes.extend_from_slice(&channel.to_le_bytes());
                }
                bytes.extend_from_slice(&size.to_le_bytes());
                bytes.extend_from_slice(&0.0f32.to_le_bytes()); // stride pad
            }
            vertex_count += 6;
        }
        (bytes, vertex_count)
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
            m: [[1.0, 0.0], [0.0, 1.0]],
            t: [0.0, 0.0],
            alpha: system.alpha,
            blend_mode: system.blend_mode,
            brightness: system.brightness,
            tint: [1.0, 1.0, 1.0, 1.0],
            kind: DrawKind::Particles {
                vertex_count: system.vertex_count,
            },
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
        value as f32
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
        });
        let (bytes, count) = state.build_vertex_bytes();
        assert_eq!(count, 6);
        assert_eq!(bytes.len(), 6 * PARTICLE_VERTEX_BYTES);
        // Vertex 0: x0 = 0 - size/2 (size = 15 at k=0.5), y0 = -7.5, uv (0,0),
        // color = (0.5, 0.5, 0, 0.5), size 15, pad 0.
        let v = |i: usize, f: usize| {
            let off = i * PARTICLE_VERTEX_BYTES + f * 4;
            f32::from_le_bytes(bytes[off..off + 4].try_into().unwrap())
        };
        assert_eq!(v(0, 0), -7.5);
        assert_eq!(v(0, 1), -7.5);
        assert_eq!(v(0, 2), 0.0);
        assert_eq!(v(0, 3), 0.0);
        assert!((v(0, 4) - 0.5).abs() < 1e-6); // r
        assert!((v(0, 5) - 0.5).abs() < 1e-6); // g
        assert_eq!(v(0, 6), 0.0); // b
        assert!((v(0, 7) - 0.5).abs() < 1e-6); // a
        assert_eq!(v(0, 8), 15.0); // size
        assert_eq!(v(0, 9), 0.0); // pad
        // Corner 3 (index 3) is br at (7.5, 7.5) with uv (1,1).
        assert_eq!(v(3, 0), 7.5);
        assert_eq!(v(3, 1), 7.5);
        assert_eq!(v(3, 2), 1.0);
        assert_eq!(v(3, 3), 1.0);
        // Endpoints: k=0 and k=1 with exact sizes.
        state.particles[0].age = 0.0;
        let (bytes, _) = state.build_vertex_bytes();
        // Corner 0 x: bytes 0..4 (8..12 would be its uv).
        assert_eq!(f32::from_le_bytes(bytes[0..4].try_into().unwrap()), -5.0);
        state.particles[0].age = state.particles[0].life;
        let (bytes, _) = state.build_vertex_bytes();
        let half = f32::from_le_bytes(bytes[0..4].try_into().unwrap());
        assert!((half + 10.0).abs() < 1e-6, "half = {half}");
        // Aged past life: the fraction clamps to 1 (compaction removes the
        // particle next step; build must still be well-defined).
        state.particles[0].age = 2.0;
        let (bytes, _) = state.build_vertex_bytes();
        let half = f32::from_le_bytes(bytes[0..4].try_into().unwrap());
        assert!((half + 10.0).abs() < 1e-6);
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
        });
        let (bytes, _) = state.build_vertex_bytes();
        let v = |f: usize| f32::from_le_bytes(bytes[f * 4..f * 4 + 4].try_into().unwrap());
        assert_eq!(v(8), 16.0, "size factor doubles the quad extent");
        assert_eq!(v(7), 0.25, "alpha factor scales the vertex alpha");
        assert_eq!(v(4), 0.5, "colorn scales RGB");
        assert_eq!(v(6), 0.5);
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
            for _ in 0..200 {
                state.simulate(1.0 / 30.0);
            }
            let (bytes, count) = state.build_vertex_bytes();
            (bytes, count, state.particles.len())
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
            MAX_PARTICLE_SYSTEMS <= 16 && MAX_PARTICLE_SYSTEMS <= MAX_LAYERS,
            "particle systems must stay within the fixed texture-slot layout"
        );
    }
}
