// SPDX-License-Identifier: GPL-3.0-or-later
//! SR-3c decision (a): the single source of truth for the material-shader
//! `shaderc::CompileOptions` BOTH `kwe-scene-renderer`'s in-thread compile
//! path (`materialshader::compile_stage`) and the killable helper
//! (`kwe-shader-compiler`) must use — "same target env/version, same
//! optimization level, same everything" is a property of sharing ONE
//! recipe, not two hand-kept-in-sync copies.
//!
//! Deliberately holds no `shaderc` TYPE, only plain string constants:
//! `kwe-core` is a dependency of nearly every binary in this workspace
//! (`kwe-daemon`, `kwe-cli`, `kwe-audio-worker`, `kwe-web-renderer`,
//! `kwe-scene-inspector`, ...), and dragging `libshaderc` into all of them
//! for two crates' benefit would be a bad trade — decision (a)'s own
//! explicit preference ("prefer sharing via kwe-core only if that doesn't
//! drag shaderc into kwe-core's dependency set"). Each of the two
//! shaderc-linking crates maps these plain values onto its own
//! `shaderc::CompileOptions`/`compile_into_spirv` call locally — see
//! `kwe-scene-renderer::materialshader::compile_stage` and
//! `kwe-shader-compiler::compile_request`, plus the renderer's own
//! differential-oracle test (`shader_helper.rs`) that keeps the two
//! mappings honest by comparing actual output bytes, not by trusting this
//! module's values alone.
//!
//! These are also the values `kwe-scene-renderer` sends in a
//! `shader-compile-request-v1`'s (kind 16) `"options"` object (SR-3c) — the
//! wire schema names the SAME three knobs.

/// `shaderc::TargetEnv::Vulkan` — the only target environment this
/// pipeline compiles for.
pub const TARGET_ENV: &str = "vulkan";
/// `shaderc::EnvVersion::Vulkan1_2 as u32` — Vulkan 1.2.
pub const TARGET_ENV_VERSION: &str = "1.2";
/// `shaderc::OptimizationLevel::Zero` — `materialshader.rs`'s own
/// reasoning: this pipeline's shaders are already small/simple, so
/// optimization buys little.
pub const OPTIMIZATION_LEVEL: &str = "zero";
/// The `shaderc::Compiler::compile_into_spirv` entry-point name argument —
/// fixed, never varies, so it is not part of the wire `"options"` object
/// (nothing to negotiate; both sides simply use this constant).
pub const ENTRY_POINT: &str = "main";
