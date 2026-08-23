// SPDX-License-Identifier: GPL-3.0-or-later
//! S4b: parse an external particle definition file's component model
//! (emitter/initializer/operator arrays) into `particles::ComponentModel`.
//! The FILE resolution (bounded read, `material` chain) lives in
//! `kwe_core::particlefile` (shared with preflight's honesty gate); this
//! module is the worker-only richer parse that only the simulation needs.
//!
//! Borrowed-From: Almamu/linux-wallpaperengine (GPL-3.0-or-later)
//! src/WallpaperEngine/Render/Objects/CParticle.cpp (the emitter/
//! initializer/operator field names and defaults this parser reads) @
//! b016d7d1 — adapted; see `particles.rs`'s module-level doc for the
//! documented scope cuts (2D only, no live control-point tracking,
//! turbulence is an approximation, several rarer operator/initializer
//! kinds are unimplemented and tolerated as unknown).
//!
//! Every field is parsed TOLERANTLY: a missing, malformed, or out-of-range
//! value falls back to a documented default or clamp — this parser can
//! never fail the scene load (unlike `kwe_core::particlefile::
//! resolve_particle_file`, which DOES fail on a missing/invalid file or
//! unresolvable material; by the time this runs, that step already
//! succeeded). An unrecognized emitter/initializer/operator `name` is
//! skipped (counted in the returned `ComponentParseStats`, never a panic
//! or rejection) — matches upstream's own "Unknown emitter/initializer
//! type" `sLog.out` + `continue` behavior.

use serde_json::{Map, Value};

use crate::particles::{self, ComponentModel, Emitter, Initializer, Operator};
use crate::scene::parse_vector;

/// How many emitter/initializer/operator entries this parse skipped
/// because their `name` was not one of the implemented kinds — the
/// caller's one-time, per-scene-load diagnostic (mirrors every other
/// bounded skip counter in this crate; never a per-item eprintln).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ComponentParseStats {
    pub unsupported_emitters: usize,
    pub unsupported_initializers: usize,
    pub unsupported_operators: usize,
}

/// Parse `root` (the particle file's already-bounded-and-validated root
/// object, from `kwe_core::particlefile::ResolvedParticleFile::
/// particle_json`) into a `ComponentModel`. Infallible — every field that
/// cannot be read a specific way falls back to a documented default.
pub fn parse_component_model(root: &Map<String, Value>) -> (ComponentModel, ComponentParseStats) {
    let mut stats = ComponentParseStats::default();
    let maxcount = root
        .get("maxcount")
        .and_then(number)
        .filter(|value| value.is_finite() && *value >= 0.0)
        .map(|value| value as u64)
        .map(particles::clamp_max_count)
        .unwrap_or(particles::DEFAULT_PARTICLE_MAX_COUNT);

    let mut emitters = Vec::new();
    if let Some(array) = root.get("emitter").and_then(Value::as_array) {
        for entry in array.iter().take(particles::MAX_COMPONENT_ITEMS) {
            let Some(object) = entry.as_object() else {
                continue;
            };
            match parse_emitter(object) {
                Some(emitter) => emitters.push(emitter),
                None => stats.unsupported_emitters += 1,
            }
        }
    }

    let mut initializers = Vec::new();
    if let Some(array) = root.get("initializer").and_then(Value::as_array) {
        for entry in array.iter().take(particles::MAX_COMPONENT_ITEMS) {
            let Some(object) = entry.as_object() else {
                continue;
            };
            match parse_initializer(object) {
                Some(initializer) => initializers.push(initializer),
                None => stats.unsupported_initializers += 1,
            }
        }
    }

    let mut operators = Vec::new();
    if let Some(array) = root.get("operator").and_then(Value::as_array) {
        for entry in array.iter().take(particles::MAX_COMPONENT_ITEMS) {
            let Some(object) = entry.as_object() else {
                continue;
            };
            match parse_operator(object) {
                Some(operator) => operators.push(operator),
                None => stats.unsupported_operators += 1,
            }
        }
    }

    (
        ComponentModel {
            maxcount,
            emitters,
            initializers,
            operators,
        },
        stats,
    )
}

// ---- Field-level tolerant readers. ----

fn number(value: &Value) -> Option<f64> {
    if let Some(n) = value.as_f64() {
        return Some(n);
    }
    value.as_str().and_then(|s| s.parse::<f64>().ok())
}

fn scalar(object: &Map<String, Value>, key: &str, default: f32) -> f32 {
    object
        .get(key)
        .and_then(number)
        .filter(|value| value.is_finite())
        .map(|value| value as f32)
        .unwrap_or(default)
}

fn clamped_scalar(object: &Map<String, Value>, key: &str, default: f32, min: f32, max: f32) -> f32 {
    scalar(object, key, default).clamp(min, max)
}

/// A vector field in the WE particle-file convention: `"x y z"` or `[x, y,
/// z]`, 1..=3 components (the extra components beyond the first two are
/// dropped — this module is 2D-only, see the file doc). Falls back to
/// `default` on any shape this renderer does not accept (missing, wrong
/// type, non-finite, wrong length) — `parse_vector`'s `Result` is
/// deliberately discarded here: unlike scene.json's strict M3c fields,
/// nothing in a particle file's component model may ever reject the scene.
fn vec2(object: &Map<String, Value>, key: &str, default: [f32; 2]) -> [f32; 2] {
    let Some(value) = object.get(key) else {
        return default;
    };
    match parse_vector(value, "particle-file", &[1, 2, 3], false) {
        Ok(v) => match v.as_slice() {
            [x] => [*x, *x],
            [x, y] => [*x, *y],
            [x, y, ..] => [*x, *y],
            _ => default,
        },
        Err(_) => default,
    }
}

fn parse_emitter(object: &Map<String, Value>) -> Option<Emitter> {
    let name = object.get("name").and_then(Value::as_str)?;
    let rate = clamped_scalar(
        object,
        "rate",
        particles::DEFAULT_PARTICLE_SPAWN_RATE,
        particles::MIN_PARTICLE_SPAWN_RATE,
        // Real corpus files declare rates far above the flat model's
        // MAX_PARTICLE_SPAWN_RATE (e.g. 15000/s) — the actual per-frame
        // work is still bounded by MAX_PARTICLES/the spawn accumulator's
        // own MAX_SPAWN_ACCUMULATOR cap regardless of how high `rate`
        // parses, so this allows the authored value through uncapped here
        // (a generous but still finite ceiling) rather than silently
        // reinterpreting a legitimate high-rate burst emitter as the flat
        // model's much lower cap.
        100_000.0,
    );
    let origin = vec2(object, "origin", [0.0, 0.0]);
    let directions = vec2(object, "directions", [1.0, 1.0]);
    match name {
        "boxrandom" => Some(Emitter::Box {
            rate,
            origin,
            directions,
            distance_min: vec2(object, "distancemin", [0.0, 0.0]),
            distance_max: vec2(object, "distancemax", [0.0, 0.0]),
        }),
        "sphererandom" => Some(Emitter::Sphere {
            rate,
            origin,
            directions,
            distance_min: clamped_scalar(object, "distancemin", 0.0, 0.0, 1e6),
            distance_max: clamped_scalar(object, "distancemax", 0.0, 0.0, 1e6),
            speed_min: clamped_scalar(object, "speedmin", 0.0, 0.0, particles::MAX_PARTICLE_SPEED),
            speed_max: clamped_scalar(object, "speedmax", 0.0, 0.0, particles::MAX_PARTICLE_SPEED),
        }),
        _ => None,
    }
}

fn parse_initializer(object: &Map<String, Value>) -> Option<Initializer> {
    let name = object.get("name").and_then(Value::as_str)?;
    match name {
        "lifetimerandom" => Some(Initializer::LifetimeRandom {
            min: clamped_scalar(object, "min", particles::DEFAULT_PARTICLE_LIFE, 0.0, 1e6),
            max: clamped_scalar(object, "max", particles::DEFAULT_PARTICLE_LIFE, 0.0, 1e6),
        }),
        "sizerandom" => Some(Initializer::SizeRandom {
            min: clamped_scalar(object, "min", 0.0, 0.0, particles::MAX_PARTICLE_SIZE * 4.0),
            max: clamped_scalar(object, "max", 0.0, 0.0, particles::MAX_PARTICLE_SIZE * 4.0),
            exponent: clamped_scalar(object, "exponent", 1.0, 0.01, 16.0),
        }),
        "alpharandom" => Some(Initializer::AlphaRandom {
            min: clamped_scalar(object, "min", 1.0, 0.0, 1.0),
            max: clamped_scalar(object, "max", 1.0, 0.0, 1.0),
        }),
        "velocityrandom" => Some(Initializer::VelocityRandom {
            min: vec2_clamped(object, "min", [0.0, 0.0], particles::MAX_PARTICLE_SPEED),
            max: vec2_clamped(object, "max", [0.0, 0.0], particles::MAX_PARTICLE_SPEED),
        }),
        "colorrandom" => Some(Initializer::ColorRandom {
            min: color3(object, "min"),
            max: color3(object, "max"),
        }),
        _ => None,
    }
}

fn vec2_clamped(object: &Map<String, Value>, key: &str, default: [f32; 2], bound: f32) -> [f32; 2] {
    let v = vec2(object, key, default);
    [v[0].clamp(-bound, bound), v[1].clamp(-bound, bound)]
}

/// A `colorrandom`/`colorchange` color field: WE writes 0..=255 per
/// channel; this renderer's color model is 0..=1 straight, so every
/// component divides by 255 and clamps. Missing -> opaque white.
fn color3(object: &Map<String, Value>, key: &str) -> [f32; 3] {
    let Some(value) = object.get(key) else {
        return [1.0, 1.0, 1.0];
    };
    match parse_vector(value, "particle-file", &[1, 2, 3], false) {
        Ok(v) => {
            let get = |i: usize| v.get(i).copied().unwrap_or(255.0) / 255.0;
            [
                get(0).clamp(0.0, 1.0),
                get(1).clamp(0.0, 1.0),
                get(2).clamp(0.0, 1.0),
            ]
        }
        Err(_) => [1.0, 1.0, 1.0],
    }
}

fn parse_operator(object: &Map<String, Value>) -> Option<Operator> {
    let name = object.get("name").and_then(Value::as_str)?;
    match name {
        "movement" => Some(Operator::Movement {
            gravity: vec2_clamped(
                object,
                "gravity",
                [0.0, 0.0],
                particles::MAX_PARTICLE_GRAVITY,
            ),
            drag: clamped_scalar(object, "drag", 0.0, 0.0, 1000.0),
        }),
        "alphafade" => Some(Operator::AlphaFade {
            fade_in: clamped_scalar(object, "fadeintime", 0.0, 0.0, 1.0),
            fade_out: clamped_scalar(object, "fadeouttime", 1.0, 0.0, 1.0),
        }),
        "sizechange" => Some(Operator::SizeChange {
            start_time: clamped_scalar(object, "starttime", 0.0, 0.0, 1.0),
            end_time: clamped_scalar(object, "endtime", 1.0, 0.0, 1.0),
            start_value: clamped_scalar(object, "startvalue", 1.0, 0.0, 1e3),
            end_value: clamped_scalar(object, "endvalue", 0.0, 0.0, 1e3),
        }),
        "colorchange" => Some(Operator::ColorChange {
            start_time: clamped_scalar(object, "starttime", 0.0, 0.0, 1.0),
            end_time: clamped_scalar(object, "endtime", 1.0, 0.0, 1.0),
            start_value: color3(object, "startvalue"),
            end_value: color3(object, "endvalue"),
        }),
        "oscillatealpha" => Some(Operator::OscillateAlpha {
            freq_min: clamped_scalar(object, "frequencymin", 0.0, 0.0, 1000.0),
            freq_max: clamped_scalar(object, "frequencymax", 0.0, 0.0, 1000.0),
            scale_min: clamped_scalar(object, "scalemin", 1.0, 0.0, 1000.0),
            scale_max: clamped_scalar(object, "scalemax", 1.0, 0.0, 1000.0),
            phase_min: clamped_scalar(object, "phasemin", 0.0, -1e4, 1e4),
            phase_max: clamped_scalar(object, "phasemax", 0.0, -1e4, 1e4),
        }),
        "oscillatesize" => Some(Operator::OscillateSize {
            freq_min: clamped_scalar(object, "frequencymin", 0.0, 0.0, 1000.0),
            freq_max: clamped_scalar(object, "frequencymax", 0.0, 0.0, 1000.0),
            scale_min: clamped_scalar(object, "scalemin", 1.0, 0.0, 1000.0),
            scale_max: clamped_scalar(object, "scalemax", 1.0, 0.0, 1000.0),
            phase_min: clamped_scalar(object, "phasemin", 0.0, -1e4, 1e4),
            phase_max: clamped_scalar(object, "phasemax", 0.0, -1e4, 1e4),
        }),
        "controlpointattract" => Some(Operator::ControlPointAttract {
            origin: vec2(object, "origin", [0.0, 0.0]),
            scale: clamped_scalar(object, "scale", 0.0, -1e7, 1e7),
            threshold: clamped_scalar(object, "threshold", 0.0, 0.0, 1e6),
        }),
        "turbulence" => Some(Operator::Turbulence {
            scale: clamped_scalar(object, "scale", 0.0, 0.0, 1e6),
            time_scale: clamped_scalar(object, "timescale", 0.0, -1e6, 1e6),
            speed_min: clamped_scalar(object, "speedmin", 0.0, 0.0, particles::MAX_PARTICLE_SPEED),
            speed_max: clamped_scalar(object, "speedmax", 0.0, 0.0, particles::MAX_PARTICLE_SPEED),
            phase_min: clamped_scalar(object, "phasemin", 0.0, -1e4, 1e4),
            phase_max: clamped_scalar(object, "phasemax", 0.0, -1e4, 1e4),
        }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root(json: &str) -> Map<String, Value> {
        serde_json::from_str::<Value>(json)
            .expect("test json")
            .as_object()
            .expect("test object")
            .clone()
    }

    #[test]
    fn parses_the_local_asset_corpus_shape() {
        // Mirrors assets/particles/example.json (a real WE asset).
        let (model, stats) = parse_component_model(&root(
            r#"{
                "material": "materials/particle/halo.json",
                "maxcount": 500,
                "emitter": [{"name": "sphererandom", "rate": 20, "origin": "0 0 0",
                             "directions": "1 1 0", "distancemin": 32, "distancemax": 512}],
                "initializer": [
                    {"name": "lifetimerandom", "min": 3, "max": 5},
                    {"name": "sizerandom", "min": 50, "max": 200},
                    {"name": "velocityrandom", "min": "-50 -50 0", "max": "50 50 0"},
                    {"name": "colorrandom", "min": "255 255 255", "max": "255 255 255"}
                ],
                "operator": [
                    {"name": "movement", "gravity": "0 0 0"},
                    {"name": "alphafade", "fadeintime": 0.5}
                ]
            }"#,
        ));
        assert_eq!(stats, ComponentParseStats::default());
        assert_eq!(model.maxcount, 500);
        assert_eq!(model.emitters.len(), 1);
        assert_eq!(model.initializers.len(), 4);
        assert_eq!(model.operators.len(), 2);
        match &model.emitters[0] {
            Emitter::Sphere {
                rate,
                distance_min,
                distance_max,
                ..
            } => {
                assert_eq!(*rate, 20.0);
                assert_eq!(*distance_min, 32.0);
                assert_eq!(*distance_max, 512.0);
            }
            other => panic!("expected Sphere, got {other:?}"),
        }
        match &model.initializers[3] {
            Initializer::ColorRandom { min, max } => {
                assert_eq!(*min, [1.0, 1.0, 1.0]);
                assert_eq!(*max, [1.0, 1.0, 1.0]);
            }
            other => panic!("expected ColorRandom, got {other:?}"),
        }
    }

    #[test]
    fn unknown_kinds_are_skipped_and_counted() {
        let (model, stats) = parse_component_model(&root(
            r#"{
                "emitter": [{"name": "boxrandom", "rate": 5}, {"name": "wobblerandom"}],
                "initializer": [{"name": "rotationrandom", "min": 1, "max": 2}],
                "operator": [{"name": "vortex"}, {"name": "movement"}]
            }"#,
        ));
        assert_eq!(model.emitters.len(), 1);
        assert_eq!(stats.unsupported_emitters, 1);
        assert_eq!(model.initializers.len(), 0);
        assert_eq!(stats.unsupported_initializers, 1);
        assert_eq!(model.operators.len(), 1);
        assert_eq!(stats.unsupported_operators, 1);
    }

    #[test]
    fn missing_arrays_parse_to_empty_with_defaults() {
        let (model, stats) = parse_component_model(&root(r#"{"material": "m.json"}"#));
        assert!(model.emitters.is_empty());
        assert!(model.initializers.is_empty());
        assert!(model.operators.is_empty());
        assert_eq!(model.maxcount, particles::DEFAULT_PARTICLE_MAX_COUNT);
        assert_eq!(stats, ComponentParseStats::default());
    }

    /// Hostile input (wrong types, huge numbers, NaN-producing strings)
    /// never panics and always yields bounded, finite, in-range values.
    #[test]
    fn hostile_fields_stay_bounded_never_panic() {
        let (model, _stats) = parse_component_model(&root(
            r#"{
                "maxcount": "not a number",
                "emitter": [
                    {"name": "boxrandom", "rate": 1e300, "distancemin": "nan nan nan",
                     "distancemax": [null, true, "x"]},
                    {"name": "sphererandom", "rate": -50, "distancemin": -1e20}
                ],
                "initializer": [
                    {"name": "sizerandom", "min": -1e30, "max": "inf", "exponent": 0},
                    {"name": "colorrandom", "min": [99999, -5], "max": {}}
                ],
                "operator": [
                    {"name": "movement", "gravity": "bad", "drag": "NaN"},
                    {"name": "turbulence", "scale": 1e30, "speedmax": -1}
                ]
            }"#,
        ));
        assert_eq!(model.maxcount, particles::DEFAULT_PARTICLE_MAX_COUNT);
        for emitter in &model.emitters {
            match emitter {
                Emitter::Box {
                    rate,
                    distance_min,
                    distance_max,
                    ..
                } => {
                    assert!(rate.is_finite());
                    assert!(distance_min.iter().all(|v| v.is_finite()));
                    assert!(distance_max.iter().all(|v| v.is_finite()));
                }
                Emitter::Sphere {
                    rate, distance_min, ..
                } => {
                    assert!(rate.is_finite() && *rate >= 0.0);
                    assert!(distance_min.is_finite() && *distance_min >= 0.0);
                }
            }
        }
        for initializer in &model.initializers {
            match initializer {
                Initializer::SizeRandom { min, max, exponent } => {
                    assert!(min.is_finite() && *min >= 0.0);
                    assert!(max.is_finite() && *max >= 0.0);
                    assert!(exponent.is_finite() && *exponent >= 0.01);
                }
                Initializer::ColorRandom { min, max } => {
                    assert!(min.iter().all(|v| v.is_finite() && (0.0..=1.0).contains(v)));
                    assert!(max.iter().all(|v| v.is_finite() && (0.0..=1.0).contains(v)));
                }
                _ => {}
            }
        }
        for operator in &model.operators {
            match operator {
                Operator::Movement { gravity, drag } => {
                    assert!(gravity.iter().all(|v| v.is_finite()));
                    assert!(drag.is_finite());
                }
                Operator::Turbulence {
                    scale, speed_max, ..
                } => {
                    assert!(scale.is_finite());
                    assert!(speed_max.is_finite() && *speed_max >= 0.0);
                }
                _ => {}
            }
        }
    }

    #[test]
    fn item_counts_are_bounded_by_max_component_items() {
        let mut emitters = String::new();
        for _ in 0..(particles::MAX_COMPONENT_ITEMS + 20) {
            emitters.push_str(r#"{"name": "boxrandom", "rate": 1},"#);
        }
        emitters.pop();
        let json = format!(r#"{{"emitter": [{emitters}]}}"#);
        let (model, _stats) = parse_component_model(&root(&json));
        assert_eq!(model.emitters.len(), particles::MAX_COMPONENT_ITEMS);
    }
}
