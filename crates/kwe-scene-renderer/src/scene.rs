// SPDX-License-Identifier: Apache-2.0
// Scene.json parsing for the M3a slice of the original SceneScript engine.
//
// The M3a worker understands exactly one input: a scene.json file laid out per
// docs/SCENE_FORMAT_V1.md. Everything the engine needs is either in the JSON
// (`general.clearcolor`) or referenced from it (`general.script`, resolved
// relative to the scene's content root). Unknown keys are tolerated so that
// real wallpaper packages never make the worker reject a scene; M3c+ slices
// will interpret the `layers`/`effects`/`properties` sections.
//
// Every read is bounded: the scene.json file is capped at 16 MiB (the daemon's
// preflight uses the same bound) and the referenced script at 2 MiB.

use std::error::Error as StdError;
use std::fmt;
use std::fs;
use std::path::{Component, Path, PathBuf};

use serde_json::Value;

/// Cap on the raw scene.json bytes (mirrors kwe-core preflight).
pub const MAX_SCENE_JSON_BYTES: u64 = 16 * 1024 * 1024;
/// Cap on a referenced script's bytes.
pub const MAX_SCRIPT_BYTES: u64 = 2 * 1024 * 1024;
/// The frame protocol's dimension cap (crates/kwe-frame-protocol).
pub const MAX_DIMENSION: u32 = 8192;

/// Which part of loading a scene failed; all kinds are backend rejects (exit
/// 73) in M3a, but the distinction shows up in diagnostics and unit tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SceneErrorKind {
    /// File system errors (missing file, permission, I/O).
    Read,
    /// JSON syntax errors or a root that is not an object.
    Json,
    /// Schema violations: wrong types, out-of-range values, `.pkg` scenes.
    Shape,
    /// Script reference problems: traversal outside the content root,
    /// missing script file, script too large, or a non-.js extension.
    Script,
}

#[derive(Debug)]
pub struct SceneError {
    pub kind: SceneErrorKind,
    pub message: String,
}

impl SceneError {
    pub fn new(kind: SceneErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}

impl fmt::Display for SceneError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl StdError for SceneError {}

impl From<std::io::Error> for SceneError {
    fn from(e: std::io::Error) -> Self {
        Self::new(SceneErrorKind::Read, format!("scene file I/O error: {e}"))
    }
}

impl From<serde_json::Error> for SceneError {
    fn from(e: serde_json::Error) -> Self {
        Self::new(
            SceneErrorKind::Json,
            format!("scene.json is not valid JSON: {e}"),
        )
    }
}

/// The interpreted part of a scene.json the M3a worker needs.
#[derive(Debug, Clone)]
pub struct SceneConfig {
    /// RGBA straight-alpha clear color from `general.clearcolor` (0..=1).
    /// Defaults to opaque black when absent.
    pub clear_color: [f32; 4],
    /// Absolute path of the scene's script, if the scene has one that passed
    /// every check. The caller decides whether to run it.
    pub script_path: Option<PathBuf>,
    /// Optional `general.resolution` (the worker still renders at the size
    /// the daemon asked for; the field is only validated and reported).
    pub resolution: Option<(u32, u32)>,
    /// Optional `general.fps` hint (unused by the worker in M3a; the daemon
    /// owns the pacing).
    pub fps: Option<f32>,
}

impl SceneConfig {
    /// Parse `path` and resolve the script reference, if any.
    ///
    /// Rejections (all `SceneError`):
    /// * file unreadable or larger than `MAX_SCENE_JSON_BYTES` → Read
    /// * invalid JSON or non-object root → Json
    /// * `general` not an object, `clearcolor`/`resolution`/`fps` wrong
    ///   types, values out of range, or `script` present but not a string
    ///   → Shape
    /// * script escaping the content root, missing, > 2 MiB, or not ending
    ///   in `.js` → Script
    pub fn parse(path: &Path) -> Result<SceneConfig, SceneError> {
        let root = canonical_root(path)?;
        let bytes = read_bounded(path, MAX_SCENE_JSON_BYTES)?;
        let value: Value = serde_json::from_slice(&bytes)?;
        let root_obj = value.as_object().ok_or_else(|| {
            SceneError::new(SceneErrorKind::Json, "scene.json root must be an object")
        })?;

        let empty_general = serde_json::Map::new();
        let general = match root_obj.get("general") {
            None | Some(Value::Null) => &empty_general,
            Some(Value::Object(g)) => g,
            Some(_) => {
                return Err(SceneError::new(
                    SceneErrorKind::Shape,
                    "scene.json \"general\" must be an object",
                ));
            }
        };

        let clear_color = parse_clear_color(general)?;
        let resolution = parse_resolution(general)?;
        let fps = parse_fps(general)?;

        let script_path = match general.get("script") {
            None | Some(Value::Null) => None,
            Some(Value::String(s)) => Some(resolve_script(&root, s)?),
            Some(_) => {
                return Err(SceneError::new(
                    SceneErrorKind::Shape,
                    "scene.json \"general.script\" must be a string path relative to the scene",
                ));
            }
        };

        Ok(SceneConfig {
            clear_color,
            script_path,
            resolution,
            fps,
        })
    }
}

/// Canonicalized directory that contains `path`; the root every relative
/// script reference is confined to.
fn canonical_root(path: &Path) -> Result<PathBuf, SceneError> {
    let canonical = path.canonicalize().map_err(|e| {
        SceneError::new(
            SceneErrorKind::Read,
            format!("scene.json at {}: {e}", path.display()),
        )
    })?;
    if !canonical.is_file() {
        return Err(SceneError::new(
            SceneErrorKind::Read,
            format!("scene.json at {} is not a regular file", path.display()),
        ));
    }
    let root = canonical.parent().ok_or_else(|| {
        SceneError::new(SceneErrorKind::Read, "scene.json has no parent directory")
    })?;
    Ok(root.to_path_buf())
}

/// Read `path` into memory, refusing to buffer more than `cap` bytes.
/// Shared with the script loader so the 2 MiB script cap is enforced at
/// read time (metadata pre-checks alone race a swapped/grown file).
pub(crate) fn read_bounded(path: &Path, cap: u64) -> Result<Vec<u8>, SceneError> {
    let meta = fs::metadata(path).map_err(SceneError::from)?;
    if meta.len() > cap {
        return Err(SceneError::new(
            SceneErrorKind::Read,
            format!(
                "{} is {} bytes, over the {} byte cap",
                path.display(),
                meta.len(),
                cap
            ),
        ));
    }
    let bytes = fs::read(path)?;
    if bytes.len() as u64 > cap {
        return Err(SceneError::new(
            SceneErrorKind::Read,
            format!(
                "{} grew past the {} byte cap while reading",
                path.display(),
                cap
            ),
        ));
    }
    Ok(bytes)
}

fn parse_clear_color(general: &serde_json::Map<String, Value>) -> Result<[f32; 4], SceneError> {
    let Some(value) = general.get("clearcolor") else {
        return Ok([0.0, 0.0, 0.0, 1.0]);
    };
    let array = value.as_array().ok_or_else(|| {
        SceneError::new(
            SceneErrorKind::Shape,
            "scene.json \"general.clearcolor\" must be an array of four floats",
        )
    })?;
    if array.len() != 4 {
        return Err(SceneError::new(
            SceneErrorKind::Shape,
            format!(
                "scene.json \"general.clearcolor\" must have exactly four entries, found {}",
                array.len()
            ),
        ));
    }
    let mut color = [0.0_f32; 4];
    for (i, entry) in array.iter().enumerate() {
        let channel = entry.as_f64().ok_or_else(|| {
            SceneError::new(
                SceneErrorKind::Shape,
                format!("scene.json \"general.clearcolor[{i}]\" must be a float"),
            )
        })?;
        if !channel.is_finite() || !(0.0..=1.0).contains(&channel) {
            return Err(SceneError::new(
                SceneErrorKind::Shape,
                format!("scene.json \"general.clearcolor[{i}]\" must be between 0.0 and 1.0"),
            ));
        }
        color[i] = channel as f32;
    }
    Ok(color)
}

fn parse_resolution(
    general: &serde_json::Map<String, Value>,
) -> Result<Option<(u32, u32)>, SceneError> {
    let Some(value) = general.get("resolution") else {
        return Ok(None);
    };
    let array = value.as_array().ok_or_else(|| {
        SceneError::new(
            SceneErrorKind::Shape,
            "scene.json \"general.resolution\" must be an array of two positive integers",
        )
    })?;
    if array.len() != 2 {
        return Err(SceneError::new(
            SceneErrorKind::Shape,
            format!(
                "scene.json \"general.resolution\" must have exactly two entries, found {}",
                array.len()
            ),
        ));
    }
    let mut dims = [0_u32; 2];
    for (i, entry) in array.iter().enumerate() {
        let dim = entry.as_u64().ok_or_else(|| {
            SceneError::new(
                SceneErrorKind::Shape,
                format!("scene.json \"general.resolution[{i}]\" must be a positive integer"),
            )
        })?;
        if dim == 0 || dim > u64::from(MAX_DIMENSION) {
            return Err(SceneError::new(
                SceneErrorKind::Shape,
                format!(
                    "scene.json \"general.resolution[{i}]\" must be within 1..={MAX_DIMENSION}"
                ),
            ));
        }
        dims[i] = dim as u32;
    }
    Ok(Some((dims[0], dims[1])))
}

fn parse_fps(general: &serde_json::Map<String, Value>) -> Result<Option<f32>, SceneError> {
    let Some(value) = general.get("fps") else {
        return Ok(None);
    };
    let fps = value.as_f64().ok_or_else(|| {
        SceneError::new(
            SceneErrorKind::Shape,
            "scene.json \"general.fps\" must be a float",
        )
    })?;
    if !fps.is_finite() || fps <= 0.0 || fps > 240.0 {
        return Err(SceneError::new(
            SceneErrorKind::Shape,
            "scene.json \"general.fps\" must be within (0.0, 240.0]",
        ));
    }
    Ok(Some(fps as f32))
}

/// Resolve a `general.script` reference. Must stay inside the scene's content
/// root, exist, be a regular `.js` file, and be at most `MAX_SCRIPT_BYTES`.
fn resolve_script(root: &Path, reference: &str) -> Result<PathBuf, SceneError> {
    if reference.is_empty() {
        return Err(SceneError::new(
            SceneErrorKind::Script,
            "scene.json \"general.script\" must not be empty",
        ));
    }
    if reference.ends_with(".pkg") || reference.to_ascii_lowercase().ends_with(".pkg") {
        return Err(SceneError::new(
            SceneErrorKind::Script,
            "packaged (.pkg) scenes arrive with the M3b archive reader",
        ));
    }
    if !reference.to_ascii_lowercase().ends_with(".js") {
        return Err(SceneError::new(
            SceneErrorKind::Script,
            format!("scene script must be a .js file, got \"{reference}\""),
        ));
    }

    let joined = Path::new(reference);
    if joined.is_absolute() {
        return Err(SceneError::new(
            SceneErrorKind::Script,
            format!("scene script \"{reference}\" must be relative to the scene"),
        ));
    }
    for component in joined.components() {
        if matches!(
            component,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        ) {
            return Err(SceneError::new(
                SceneErrorKind::Script,
                format!("scene script \"{reference}\" must stay inside the scene directory"),
            ));
        }
    }

    // Verify against the canonicalized target so symlinks cannot smuggle the
    // script out of the content root.
    let candidate = root.join(joined);
    let canonical = candidate.canonicalize().map_err(|e| {
        SceneError::new(
            SceneErrorKind::Script,
            format!("scene script \"{reference}\" is missing or unreadable: {e}"),
        )
    })?;
    if !canonical.starts_with(root) {
        return Err(SceneError::new(
            SceneErrorKind::Script,
            format!("scene script \"{reference}\" resolves outside the scene directory"),
        ));
    }
    if !canonical.is_file() {
        return Err(SceneError::new(
            SceneErrorKind::Script,
            format!("scene script \"{reference}\" is not a regular file"),
        ));
    }
    let meta = fs::metadata(&canonical).map_err(SceneError::from)?;
    if meta.len() > MAX_SCRIPT_BYTES {
        return Err(SceneError::new(
            SceneErrorKind::Script,
            format!(
                "scene script \"{reference}\" is {} bytes, over the {MAX_SCRIPT_BYTES} byte cap",
                meta.len()
            ),
        ));
    }
    Ok(canonical)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn tmpdir() -> PathBuf {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("kwe-scene-test-{}-{n}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write(dir: &Path, name: &str, contents: &str) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, contents).unwrap();
        path
    }

    #[test]
    fn clear_color_exact_parse() {
        let dir = tmpdir();
        let scene = write(
            &dir,
            "scene.json",
            r#"{"general": {"clearcolor": [0.25, 0.5, 0.75, 1.0], "resolution": [1920, 1080], "fps": 30}}"#,
        );
        let config = SceneConfig::parse(&scene).unwrap();
        assert_eq!(config.clear_color, [0.25, 0.5, 0.75, 1.0]);
        assert_eq!(config.resolution, Some((1920, 1080)));
        assert_eq!(config.fps, Some(30.0));
        assert!(config.script_path.is_none());
    }

    #[test]
    fn clear_color_defaults_to_opaque_black() {
        let dir = tmpdir();
        let scene = write(&dir, "scene.json", r#"{"general": {}}"#);
        let config = SceneConfig::parse(&scene).unwrap();
        assert_eq!(config.clear_color, [0.0, 0.0, 0.0, 1.0]);
    }

    #[test]
    fn missing_general_defaults_cleanly() {
        let dir = tmpdir();
        let scene = write(&dir, "scene.json", r#"{}"#);
        let config = SceneConfig::parse(&scene).unwrap();
        assert_eq!(config.clear_color, [0.0, 0.0, 0.0, 1.0]);
        assert!(config.script_path.is_none());
    }

    #[test]
    fn clear_color_rejects_wrong_arity_and_range() {
        let dir = tmpdir();
        let short = write(
            &dir,
            "short.json",
            r#"{"general": {"clearcolor": [1, 2, 3]}}"#,
        );
        assert_eq!(
            SceneConfig::parse(&short).unwrap_err().kind,
            SceneErrorKind::Shape
        );
        let over = write(
            &dir,
            "over.json",
            r#"{"general": {"clearcolor": [1.5, 0, 0, 1]}}"#,
        );
        assert_eq!(
            SceneConfig::parse(&over).unwrap_err().kind,
            SceneErrorKind::Shape
        );
        let nan = write(
            &dir,
            "nan.json",
            r#"{"general": {"clearcolor": [1, "x", 0, 1]}}"#,
        );
        assert_eq!(
            SceneConfig::parse(&nan).unwrap_err().kind,
            SceneErrorKind::Shape
        );
    }

    #[test]
    fn root_must_be_object_and_json_valid() {
        let dir = tmpdir();
        let not_object = write(&dir, "arr.json", r#"[1, 2, 3]"#);
        assert_eq!(
            SceneConfig::parse(&not_object).unwrap_err().kind,
            SceneErrorKind::Json
        );
        let not_json = write(&dir, "bad.json", r#"{"general": "#);
        assert_eq!(
            SceneConfig::parse(&not_json).unwrap_err().kind,
            SceneErrorKind::Json
        );
    }

    #[test]
    fn script_resolved_relative_to_scene() {
        let dir = tmpdir();
        fs::create_dir_all(dir.join("scripts")).unwrap();
        write(&dir.join("scripts"), "main.js", "// fixture\n");
        let scene = write(
            &dir,
            "scene.json",
            r#"{"general": {"script": "scripts/main.js"}}"#,
        );
        let config = SceneConfig::parse(&scene).unwrap();
        let expected = dir.join("scripts").join("main.js").canonicalize().unwrap();
        assert_eq!(config.script_path, Some(expected));
    }

    #[test]
    fn script_non_string_rejected_as_shape() {
        let dir = tmpdir();
        let scene = write(&dir, "scene.json", r#"{"general": {"script": 42}}"#);
        assert_eq!(
            SceneConfig::parse(&scene).unwrap_err().kind,
            SceneErrorKind::Shape
        );
    }

    #[test]
    fn script_traversal_rejected() {
        let dir = tmpdir();
        // Passes the .js check so the traversal guard is what rejects it.
        let scene = write(
            &dir,
            "scene.json",
            r#"{"general": {"script": "../../etc/passwd.js"}}"#,
        );
        let err = SceneConfig::parse(&scene).unwrap_err();
        assert_eq!(err.kind, SceneErrorKind::Script);
        assert!(
            err.message.contains("inside the scene directory"),
            "{}",
            err.message
        );
    }

    #[test]
    fn script_absolute_path_rejected() {
        let dir = tmpdir();
        let scene = write(
            &dir,
            "scene.json",
            r#"{"general": {"script": "/etc/passwd"}}"#,
        );
        assert_eq!(
            SceneConfig::parse(&scene).unwrap_err().kind,
            SceneErrorKind::Script
        );
    }

    #[test]
    fn script_missing_file_rejected() {
        let dir = tmpdir();
        let scene = write(
            &dir,
            "scene.json",
            r#"{"general": {"script": "does-not-exist.js"}}"#,
        );
        assert_eq!(
            SceneConfig::parse(&scene).unwrap_err().kind,
            SceneErrorKind::Script
        );
    }

    #[test]
    fn pkg_scene_rejected_until_m3b() {
        let dir = tmpdir();
        let scene = write(
            &dir,
            "scene.json",
            r#"{"general": {"script": "scene.pkg"}}"#,
        );
        assert_eq!(
            SceneConfig::parse(&scene).unwrap_err().kind,
            SceneErrorKind::Script
        );
    }

    #[test]
    fn non_js_script_rejected() {
        let dir = tmpdir();
        let scene = write(&dir, "scene.json", r#"{"general": {"script": "main.txt"}}"#);
        assert_eq!(
            SceneConfig::parse(&scene).unwrap_err().kind,
            SceneErrorKind::Script
        );
    }

    #[test]
    fn oversized_scene_json_rejected() {
        let dir = tmpdir();
        let path = dir.join("big.json");
        // 17 MiB of padding inside a string.
        let payload = format!(
            r#"{{"general": {{"clearcolor": [1, 0, 0, 1]}}, "pad": "{}"}}"#,
            "x".repeat(17 * 1024 * 1024)
        );
        fs::write(&path, &payload).unwrap();
        assert_eq!(
            SceneConfig::parse(&path).unwrap_err().kind,
            SceneErrorKind::Read
        );
    }

    #[test]
    fn oversized_script_rejected() {
        let dir = tmpdir();
        let big = dir.join("big.js");
        fs::write(&big, vec![b'x'; (MAX_SCRIPT_BYTES + 1) as usize]).unwrap();
        let scene = write(&dir, "scene.json", r#"{"general": {"script": "big.js"}}"#);
        assert_eq!(
            SceneConfig::parse(&scene).unwrap_err().kind,
            SceneErrorKind::Script
        );
    }
}
