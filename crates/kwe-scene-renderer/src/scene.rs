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
    /// Index of the `general.script` entry inside the package table, when
    /// the scene was parsed from a scene.pkg (M3b). Mutually exclusive with
    /// `script_path`; the caller extracts the entry and sets `script_path`.
    pub script_entry: Option<usize>,
    /// Raw `general.script` string, before resolution against either the
    /// content root (file scenes) or the package table (pkg scenes).
    script_reference: Option<String>,
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
        let parsed = parse_scene_json(&bytes)?;
        let script_path = match &parsed.script_reference {
            None => None,
            Some(reference) => Some(resolve_script(&root, reference)?),
        };
        Ok(SceneConfig {
            script_path,
            ..parsed
        })
    }

    /// Parse the scene.json bytes extracted from a scene.pkg (M3b) and
    /// resolve `general.script` against the package's entry table instead
    /// of the file system. `script_entry` names the entry to extract; the
    /// caller must extract it into a private directory and set
    /// `script_path` (nothing is ever resolved against the host file
    /// system from a package).
    pub fn parse_pkg(
        bytes: &[u8],
        entries: &[kwe_core::PkgEntry],
    ) -> Result<SceneConfig, SceneError> {
        let parsed = parse_scene_json(bytes)?;
        let script_entry = match &parsed.script_reference {
            None => None,
            Some(reference) => Some(resolve_pkg_script(reference, entries)?),
        };
        Ok(SceneConfig {
            script_entry,
            ..parsed
        })
    }
}

/// Shared JSON interpretation core for file and pkg scenes. The script
/// reference is left unresolved (`script_reference`); the two entry points
/// resolve it against their own root: the content directory (file scenes)
/// or the package entry table (pkg scenes).
fn parse_scene_json(bytes: &[u8]) -> Result<SceneConfig, SceneError> {
    let value: Value = serde_json::from_slice(bytes)?;
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

    let script_reference = match general.get("script") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => Some(s.clone()),
        Some(_) => {
            return Err(SceneError::new(
                SceneErrorKind::Shape,
                "scene.json \"general.script\" must be a string path relative to the scene",
            ));
        }
    };

    Ok(SceneConfig {
        clear_color,
        script_path: None,
        script_entry: None,
        script_reference,
        resolution,
        fps,
    })
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
    // Two accepted serializations, both verified on the real corpus (M3b):
    // * the array form `[r, g, b, a]` (0.0..=1.0) used by the documented
    //   schema and the M3a fixtures;
    // * the space-separated string form `"r g b"` that Wallpaper Engine
    //   actually writes — 59 of 60 corpus scene.json entries use it, e.g.
    //   `"clearcolor": "0.7 0.7 0.7"` (RGB only; alpha defaults to 1.0).
    //   The property-wrapped object form `{"user": ..., "value": ...}`
    //   (1 of 60 corpus entries) stays a Shape rejection until user
    //   properties arrive in M3c+.
    if let Some(text) = value.as_str() {
        let tokens: Vec<&str> = text.split_whitespace().collect();
        if tokens.len() != 3 {
            return Err(SceneError::new(
                SceneErrorKind::Shape,
                format!(
                    "scene.json \"general.clearcolor\" string must be \"r g b\" \
                     (three space-separated floats), found {} tokens",
                    tokens.len()
                ),
            ));
        }
        let mut color = [0.0_f32; 4];
        for (i, token) in tokens.iter().enumerate() {
            let channel = token.parse::<f64>().map_err(|_| {
                SceneError::new(
                    SceneErrorKind::Shape,
                    format!(
                        "scene.json \"general.clearcolor[{i}]\" must be a float, got \"{token}\""
                    ),
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
        color[3] = 1.0;
        return Ok(color);
    }
    let array = value.as_array().ok_or_else(|| {
        SceneError::new(
            SceneErrorKind::Shape,
            "scene.json \"general.clearcolor\" must be an array of four floats or a \"r g b\" string",
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
            "a .pkg script reference is only valid inside a packaged scene; \
             file-based scenes ship scene.json and scene.pkg side by side",
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

/// Resolve a `general.script` reference against the package entry table
/// (M3b). Rules: relative, `.js`, no `..`/backslash/NUL, and it must match
/// exactly one entry — case-insensitively, either the literal path or the
/// entry's tail after a `/` (so `scripts/main.js` finds an entry stored as
/// `wallpaper/scripts/main.js`). Entry paths were already validated at
/// package open (no `..`, no absolute paths), so resolution can never leave
/// the table; the rejection messages exist for diagnostics, not safety.
fn resolve_pkg_script(
    reference: &str,
    entries: &[kwe_core::PkgEntry],
) -> Result<usize, SceneError> {
    if reference.is_empty() {
        return Err(SceneError::new(
            SceneErrorKind::Script,
            "scene.json \"general.script\" must not be empty",
        ));
    }
    if reference.to_ascii_lowercase().ends_with(".pkg") {
        return Err(SceneError::new(
            SceneErrorKind::Script,
            "scene script must not reference \"scene.pkg\" (the archive itself)",
        ));
    }
    if !reference.to_ascii_lowercase().ends_with(".js") {
        return Err(SceneError::new(
            SceneErrorKind::Script,
            format!("scene script must be a .js file, got \"{reference}\""),
        ));
    }
    if reference.starts_with('/')
        || reference.contains('\\')
        || reference.contains('\0')
        || reference.split('/').any(|component| component == "..")
    {
        return Err(SceneError::new(
            SceneErrorKind::Script,
            format!("scene script \"{reference}\" must stay inside the package"),
        ));
    }
    let needle = reference.to_ascii_lowercase();
    let matches: Vec<usize> = entries
        .iter()
        .enumerate()
        .filter(|(_, entry)| {
            let path = entry.path.to_ascii_lowercase();
            path == needle || path.ends_with(&format!("/{needle}"))
        })
        .map(|(idx, _)| idx)
        .collect();
    match matches.as_slice() {
        [] => Err(SceneError::new(
            SceneErrorKind::Script,
            format!("scene script \"{reference}\" is not an entry of the package"),
        )),
        [idx] => Ok(*idx),
        _ => Err(SceneError::new(
            SceneErrorKind::Script,
            format!(
                "scene script \"{reference}\" matches {} package entries; exactly one is required",
                matches.len()
            ),
        )),
    }
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
    fn clear_color_string_form_accepted() {
        // The corpus serialization: 59 of 60 real scene.json entries carry
        // clearcolor as a space-separated "r g b" string (M3b finding).
        let dir = tmpdir();
        let scene = write(
            &dir,
            "scene.json",
            r#"{"general": {"clearcolor": "0.7 0.7 0.7"}}"#,
        );
        let config = SceneConfig::parse(&scene).unwrap();
        assert_eq!(config.clear_color, [0.7, 0.7, 0.7, 1.0]);
        // Whitespace-tolerant and precision-rich, like the corpus variant
        // "0.713725 0.713725 0.713725" seen on one real wallpaper.
        let wide = write(
            &dir,
            "wide.json",
            r#"{"general": {"clearcolor": "  0.25\t0.5 0.75  "}}"#,
        );
        let config = SceneConfig::parse(&wide).unwrap();
        assert_eq!(config.clear_color, [0.25, 0.5, 0.75, 1.0]);
    }

    #[test]
    fn clear_color_string_form_rejects_wrong_tokens_and_range() {
        let dir = tmpdir();
        let two = write(
            &dir,
            "two.json",
            r#"{"general": {"clearcolor": "0.7 0.7"}}"#,
        );
        assert_eq!(
            SceneConfig::parse(&two).unwrap_err().kind,
            SceneErrorKind::Shape
        );
        let four = write(
            &dir,
            "four.json",
            r#"{"general": {"clearcolor": "0.7 0.7 0.7 1.0"}}"#,
        );
        assert_eq!(
            SceneConfig::parse(&four).unwrap_err().kind,
            SceneErrorKind::Shape
        );
        let not_a_float = write(
            &dir,
            "nope.json",
            r#"{"general": {"clearcolor": "red green blue"}}"#,
        );
        assert_eq!(
            SceneConfig::parse(&not_a_float).unwrap_err().kind,
            SceneErrorKind::Shape
        );
        let over = write(
            &dir,
            "over.json",
            r#"{"general": {"clearcolor": "1.5 0 0"}}"#,
        );
        assert_eq!(
            SceneConfig::parse(&over).unwrap_err().kind,
            SceneErrorKind::Shape
        );
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
    fn pkg_script_reference_still_rejected_for_file_scenes() {
        // A file-based scene.json cannot point its script at scene.pkg: only
        // packaged scenes (parse_pkg) may reference package entries.
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

    /// Minimal scene.pkg fixture (mirrors the verified corpus layout:
    /// PKGV0001, length-prefixed paths, raw payloads).
    fn build_pkg(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&8_u32.to_le_bytes());
        out.extend_from_slice(b"PKGV0001");
        out.extend_from_slice(&(entries.len() as u32).to_le_bytes());
        let mut offset: u32 = 0;
        for (path, payload) in entries {
            out.extend_from_slice(&(path.len() as u32).to_le_bytes());
            out.extend_from_slice(path.as_bytes());
            out.extend_from_slice(&offset.to_le_bytes());
            out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
            offset += payload.len() as u32;
        }
        for (_, payload) in entries {
            out.extend_from_slice(payload);
        }
        out
    }

    fn pkg_entries(dir: &Path, bytes: &[u8]) -> Vec<kwe_core::PkgEntry> {
        let pkg = dir.join("fixture.pkg");
        fs::write(&pkg, bytes).unwrap();
        kwe_core::PkgReader::open(&pkg).unwrap().entries().to_vec()
    }

    #[test]
    fn pkg_scene_parsed_with_script_entry() {
        let dir = tmpdir();
        let scene_json =
            br#"{"general":{"clearcolor":[0.5,0.25,0.125,1.0],"script":"scripts/main.js"}}"#;
        let entries = pkg_entries(
            &dir,
            &build_pkg(&[
                ("scene.json", scene_json),
                ("scripts/main.js", b"function init(){}"),
            ]),
        );
        let config = SceneConfig::parse_pkg(scene_json, &entries).unwrap();
        assert_eq!(config.clear_color, [0.5, 0.25, 0.125, 1.0]);
        assert_eq!(config.script_entry, Some(1));
        assert!(config.script_path.is_none());
    }

    #[test]
    fn pkg_script_suffix_match_within_directories() {
        let dir = tmpdir();
        let scene_json = br#"{"general":{"script":"scripts/main.js"}}"#;
        let entries = pkg_entries(
            &dir,
            &build_pkg(&[
                ("scene.json", scene_json),
                ("wallpaper/scripts/main.js", b"function init(){}"),
                ("textures/tex.png", b"TEXV0005"),
            ]),
        );
        let config = SceneConfig::parse_pkg(scene_json, &entries).unwrap();
        assert_eq!(config.script_entry, Some(1));
    }

    #[test]
    fn pkg_scene_without_script_has_no_entry() {
        let dir = tmpdir();
        let scene_json = br#"{"general":{}}"#;
        let entries = pkg_entries(&dir, &build_pkg(&[("scene.json", scene_json)]));
        let config = SceneConfig::parse_pkg(scene_json, &entries).unwrap();
        assert!(config.script_entry.is_none());
    }

    #[test]
    fn pkg_script_missing_entry_rejected() {
        let dir = tmpdir();
        let scene_json = br#"{"general":{"script":"nope.js"}}"#;
        let entries = pkg_entries(&dir, &build_pkg(&[("scene.json", scene_json)]));
        let err = SceneConfig::parse_pkg(scene_json, &entries).unwrap_err();
        assert_eq!(err.kind, SceneErrorKind::Script);
        assert!(
            err.message.contains("is not an entry of the package"),
            "{}",
            err.message
        );
    }

    #[test]
    fn pkg_script_ambiguous_match_rejected() {
        let dir = tmpdir();
        let scene_json = br#"{"general":{"script":"main.js"}}"#;
        let entries = pkg_entries(
            &dir,
            &build_pkg(&[
                ("scene.json", scene_json),
                ("a/main.js", b""),
                ("b/main.js", b""),
            ]),
        );
        let err = SceneConfig::parse_pkg(scene_json, &entries).unwrap_err();
        assert_eq!(err.kind, SceneErrorKind::Script);
        assert!(
            err.message.contains("matches 2 package entries"),
            "{}",
            err.message
        );
    }

    #[test]
    fn pkg_script_hostile_references_rejected() {
        let dir = tmpdir();
        let entries = pkg_entries(&dir, &build_pkg(&[("scene.json", b"{}")]));
        for reference in [
            "../evil.js",
            "/etc/passwd.js",
            "back\\slash.js",
            "scene.pkg",
            "main.txt",
            "",
        ] {
            let scene_json = serde_json::json!({ "general": { "script": reference } }).to_string();
            let err = SceneConfig::parse_pkg(scene_json.as_bytes(), &entries).unwrap_err();
            assert_eq!(err.kind, SceneErrorKind::Script, "{reference:?}");
        }
    }

    #[test]
    fn pkg_scene_invalid_json_rejected() {
        let dir = tmpdir();
        let entries = pkg_entries(&dir, &build_pkg(&[("scene.json", b"not json")]));
        assert_eq!(
            SceneConfig::parse_pkg(b"not json", &entries)
                .unwrap_err()
                .kind,
            SceneErrorKind::Json
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
