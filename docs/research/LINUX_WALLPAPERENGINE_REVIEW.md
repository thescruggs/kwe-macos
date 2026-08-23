# linux-wallpaperengine (Almamu) — feasibility review

Source reviewed at `/home/qcv123/gitClones/linux-wallpaperengine` (not part of this
repo; cloned for this review only). Goal: assess it as a basis for our scene
renderer (`crates/kwe-scene-renderer`, Rust + Vulkan + QuickJS), specifically the
46/60 locally-refused scenes (`docs/FEATURE_COMPATIBILITY.md` row `content.scene2d`
and `content.scene3d`; M3h not started) that are all-model-layer or particle-file
scenes we cannot draw yet.

## 1. Repo identity

- URL: `https://github.com/Almamu/linux-wallpaperengine`
- Commit reviewed: `b016d7d1fdcf4e5fd2f9c9fa420a8aaa07fee02d`, 2026-06-09 03:03:27 +0200
  ("refactor: remove subprocess in favor of dbus and wire up to javascript (#606)")
- License: `LICENSE` at repo root is the unmodified GPLv3 text, and it carries the
  "or (at your option) any later version" clause (`LICENSE:640`) → **GPL-3.0-or-later**.
  No SPDX headers found in the project's own `src/` files; the license is asserted
  only via the root `LICENSE` file and the README badge.

### Submodules / vendored code (`src/External/*`, from `.gitmodules`)

| Path | Upstream | License found |
|---|---|---|
| `glslang-WallpaperEngine` | Almamu fork of Khronos glslang | `LICENSE.txt`: mixed permissive (BSD-3-Clause-style + a few files under their own headers); no copyleft |
| `SPIRV-Cross-WallpaperEngine` | Almamu fork of KhronosGroup/SPIRV-Cross | `LICENSE`: Apache-2.0 |
| `stb` | nothings/stb | `LICENSE`: dual MIT / Unlicense |
| `json` | nlohmann/json | `LICENSE.MIT`: MIT |
| `MimeTypes` | lasselukkari/MimeTypes | `LICENSE` present, not read in full; upstream is MIT |
| `kissfft` | mborgerding/kissfft | `COPYING`: BSD-3-Clause |
| `argparse` | p-ranav/argparse | `LICENSE`: MIT |
| `Catch2` | catchorg/Catch2 (test-only, `BUILD_TESTING`) | `LICENSE.txt`: Boost Software License 1.0 |
| `quickjs` | quickjs-ng/quickjs | `LICENSE`: MIT |

None of the vendored deps are copyleft; all are permissive (MIT/BSD/Apache/BSL).
The GPL-3.0-or-later obligation comes entirely from Almamu's own `src/` code.

Additionally, at configure time CMake downloads a **prebuilt CEF (Chromium
Embedded Framework) binary distribution** (`CEF_VERSION
135.0.17+gcbc1c5b+chromium-135.0.7049.52`, `linux64_minimal`) from
`cef-builds.spotifycdn.com` (`CMakeLists.txt:47-114`, `CMakeModules/DownloadCEF.cmake`).
This is a ~360 MB tarball, not a system package. CEF/Chromium is BSD-style
licensed, not GPL, but it is a heavyweight, network-fetched, closed-build-process
dependency used only for the `content.web` (CEF-based) wallpaper type in this
project — irrelevant to our scene-worker scope but unavoidable at `cmake`
configure time because it's unconditional in `CMakeLists.txt`.

## 2. Build dependencies, backends, CLI (from `README.md` + `CMakeLists.txt`)

Required libs (`CMakeLists.txt:27-39`): X11 (optional), OpenGL 3.3+, GLEW, DBus,
GLUT, ZLIB, SDL2, MPV (`libmpv`), LZ4, FFmpeg (avcodec/avformat/avutil/swscale),
PulseAudio, Freetype, plus glm/GLFW (linked but not `find_package`d — pulled via
pkg-config/system paths) and CEF (auto-downloaded).

Backends: X11 (`X11Output`, needs Xrandr + Xxf86vm) and Wayland
(`WaylandOutput`, needs `wlr-layer-shell-unstable` + `xdg-output-unstable-v1`,
detected via `pkg_check_modules` at `CMakeLists.txt:154`). A window mode
(`GLFWWindowOutput`) always exists as a third option. If neither X11 nor
Wayland support is detected, the build still succeeds with only "preview"
(windowed) capability (`CMakeLists.txt:252-254`).

Relevant CLI flags (`README.md` "Common Options" table; parsed in
`ApplicationContext::loadSettingsFromArgv`, `src/WallpaperEngine/Application/ApplicationContext.cpp:250-604`):

- `--assets-dir <path>` (line 557) — override Wallpaper Engine assets location
- `--window <XxYxWxH>` (line 264) — run windowed instead of as a screen background
- `--fps <val>` (line 491) — cap frame rate
- `--screenshot <file>` / `--screenshot-delay` (lines 542-550) — single-frame PNG/JPEG/BMP capture
- `--silent`, `--volume`, `--noautomute`, `--no-audio-processing` (lines 520-535) — audio muting/processing toggles
- `--disable-particles`, `--disable-mouse`, `--disable-parallax` (lines 564-573)
- `--set-property name=value`, `--list-properties` (lines 578-599)
- `--dump-structure`, `--render-debug` (lines 599-604) — debugging aids

There is **no built-in "headless N-frame render loop at WxH, write PNG per
frame" mode** — `--screenshot` captures exactly one frame after an optional
delay, then the process keeps running until killed. Continuous offscreen
capture would have to be driven from outside (see §5A).

## 3. Scene rendering pipeline (file:line map)

**Parsing** (`src/WallpaperEngine/Data/Parsers/*`, `src/WallpaperEngine/Data/Model/*`):

- `ProjectParser::parse` (`ProjectParser.cpp:16`) reads `project.json`: title,
  type (`scene`/`video`/`web`), workshop id, `properties` (user-configurable
  sliders/combos/colors via `PropertyParser`), then delegates to
  `WallpaperParser::parse`.
- `WallpaperParser::parseScene` (`WallpaperParser.cpp:23`) reads the scene's
  main file (`scene.json` for `content.scene2d`/`scene3d`): `camera` (eye/
  center/up + orthogonal projection width/height/auto, fov/nearz/farz),
  `general` (ambient/skylight/clearcolor, bloom, parallax, camera-shake), and
  `objects` (array) → `ObjectParser::parse` (`WallpaperParser.cpp:96`,
  `ObjectParser.cpp:20`).
- `ObjectParser::parse` (`ObjectParser.cpp:20-25`) dispatches per-object on
  presence of an `image`, `sound`, `particle`, `text`, or `light` key. Image
  objects (`ObjectParser::parseImage`, line 145) carry a `material` (path to a
  `.json` material file) which `MaterialParser::load`/`parsePass`
  (`MaterialParser.cpp:12,38`) resolves into `MaterialPass` entries: shader
  name, blend/cull/depth modes, `textures`/`usertextures` maps (ordered texture
  slots → `.tex` names), `combos` (shader permutation flags), and
  `constantshadervalues` (`ShaderConstantParser`).
- Model objects (`ModelParser::parse`, `ModelParser.cpp:19`) parse a `.json`
  model file whose required `material` field is resolved the same way via
  `MaterialParser::load` — i.e. models and images share one
  material→passes→textures pipeline; only the mesh/puppet source differs.
  **This project's 3D-model support (`puppet`/mesh loading itself) was not
  found as a separately named parser file in this pass** — model geometry
  loading lives elsewhere in `Render/Objects` and was not traced in depth;
  budget was spent on the texture/shader/effect chain that blocks our 46
  scenes.
- Effects (`EffectParser::parse`, `EffectParser.cpp:19`) parse a `.json` effect
  file: ordered `passes` (each either a `material` pass or a `command`
  copy/swap between named FBOs, `EffectParser.cpp:49-73`) and an `fbos` array
  (`name`, `format`, `scale`, `unique`, `EffectParser.cpp:97-111`).
- Package container (`PackageParser::parse`, `PackageParser.cpp:16`): a
  `scene.pkg` is `"PKGV" + fileCount + [name, offset, length]* + rawBytes`
  — a flat, offset-addressed archive, no compression at the container level
  (compression, if any, is per-entry inside `.tex` files). This is exactly the
  format of the acceptance-test file
  `/media/crushinator/steamapps/workshop/content/431960/1725674512/scene.pkg`.

**Textures / TEXV decoder** (`Data/Parsers/TextureParser.cpp`,
`Data/Assets/Texture.h`, `Render/CTexture.cpp`):

- Container: magic `"TEXV0005"` + `"TEXI0001"` header
  (`TextureParser::parseTextureHeader`, `TextureParser.cpp:178-200`) giving
  format enum, flags, in-memory (power-of-2) width/height, and real width/
  height. Then a sub-container magic `TEXB0001..TEXB0004`
  (`parseContainer`, `TextureParser.cpp:202-225`) giving image count and,
  for TEXB0003/0004, an embedded "FreeImage format" (FIF) enum id.
- Per-image mipmap chain (`parseMipmap`, `TextureParser.cpp:39-83`): each
  mipmap has width/height and, for TEXB0002+, a `compression` flag; if
  `compression==1` the payload is **LZ4-compressed** and decompressed via
  `LZ4_decompress_safe` (line 76) into a raw buffer — LZ4 is the only
  container-level compression; there is no zlib/deflate path here.
- Animated textures (`.tex` GIF-style spritesheets): `TEXS0001/0002/0003`
  magics (`parseAnimations`, `TextureParser.cpp:227-284`) with per-frame
  timing/UV-rect data.
- **Format decode is mostly punted to the GPU or to `stb_image`, not done in
  CPU code**: `CTexture::CTexture` (`CTexture.cpp:12-104`) checks
  `freeImageFormat != FIF_UNKNOWN` and if so runs the mipmap bytes through
  **`stb_image`** (`stbi_load_from_memory`, line 65) — despite the `FIF_*`
  enum naming ("FreeImage format"), the actual real FreeImage library is
  **not linked or used**; it's a vestige of the original Windows tool's
  container metadata, reinterpreted through stb_image, which covers
  PNG/JPEG/BMP/etc. If instead `freeImageFormat==FIF_UNKNOWN`, the format is
  one of the GPU-native compressed/raw enums (`TextureFormat_DXT1/3/5`,
  `_BC7`, `_ARGB8888`, `_R8`, `_RG88`, …) and the **raw (LZ4-decompressed)
  bytes are handed directly to OpenGL** — `glCompressedTexImage2D` for
  DXT1/3/5 (`CTexture.cpp:91-95`, `CTexture::setupInternalFormat`,
  `CTexture.cpp:150-170`) or `glTexImage2D` for uncompressed R8/RG8/RGBA8
  (`CTexture.cpp:86-90`). **There is no CPU-side DXT/BC7 block decompressor
  in this codebase** — it relies entirely on GPU driver support for
  `GL_COMPRESSED_RGBA_S3TC_DXT{1,3,5}_EXT`. BC7 is parsed as an enum value
  (`TextureFormat_BC7 = 12`, `Texture.h`) but **`CTexture::setupInternalFormat`
  has no `case` for it** (`CTexture.cpp:150-170`) — BC7 textures would hit the
  `default: sLog.exception(...)` and fail to load in this exact revision.

**Shaders** (`Render/Shaders/ShaderUnit.cpp`, `GLSLContext.cpp`):

- `AssetLocator::shader/vertexShader/fragmentShader/includeShader`
  (`AssetLocator.cpp:11-56`) resolve `assets/shaders/<name>.{vert,frag,h}`,
  with a workshop-shader compat redirect: a `workshop/<id>/<file>` shader path
  first tries `zcompat/scene/shaders/<id>/<file>` before falling back to the
  normal shaders dir (`AssetLocator.cpp:16-28`).
- `ShaderUnit::preprocess` (`ShaderUnit.cpp:78-90`) runs, per unit
  (vertex/fragment): `preprocessIncludes` (custom single-token `#include
  "file"` resolver that inlines `.h` files found via `AssetLocator`, then
  places all accumulated include text just before `main()`, respecting
  `#if`/`#endif` nesting via a hand-rolled stack scan — `ShaderUnit.cpp:157-306`);
  `preprocessRequires` (a small `#require ModuleName` directive, currently only
  resolving `LightingV1` to a stub function since lighting objects aren't
  implemented — `ShaderUnit.cpp:311-343`); `preprocessVariables` (scans for
  `// [COMBO] {json}` comments to register shader combos, and
  `uniform TYPE name; // {json}` lines to register user-tunable shader
  constants — `ShaderUnit.cpp:107-142`); then a literal `gl_FragColor` →
  `out_FragColor` replacement.
- A large `SHADER_HEADER` macro block (`ShaderUnit.cpp:24-52`) defines an
  HLSL-to-GLSL compatibility shim: `mul`, `lerp`→`mix`, `frac`→`fract`,
  `float2/3/4`, `int2/3/4`, `CAST2/3/4`, `saturate`, `texSample2D`→`texture`,
  `atan2`→`atan`, `fmod`, `ddx`/`ddy`, etc. This is how Wallpaper Engine's
  (HLSL-flavored) shader source runs as GLSL 330 unmodified.
- `GLSLContext::toGlsl` (`GLSLContext.cpp:141-192`) compiles the preprocessed
  GLSL through **glslang → SPIR-V** (`glslang::GlslangToSpv`), then
  **SPIR-V-Cross back to GLSL 330** (`spirv_cross::CompilerGLSL`). This
  round-trip exists purely to normalize/validate the shader for OpenGL 330;
  for a Vulkan port, the SPIR-V produced mid-pipeline (before the
  Cross-back-to-GLSL step) is exactly the artifact a Vulkan renderer needs —
  the spirv-cross step back to GLSL would not be needed at all if targeting
  Vulkan directly.
- Combos are plumbed as `#define NAME value` lines
  (`DEFINE_COMBO` macro, `ShaderUnit.cpp:53`), i.e. shader variants are
  produced by textual `#define` injection + recompilation, not by runtime
  branching — matches Wallpaper Engine's original combo model.

**Effects / passes / FBOs** (`Render/Objects/Effects/CPass.cpp`,
`Render/CFBO.cpp`):

- `CPass` (`CPass.cpp:44-56`) binds a `MaterialPass`'s shader + texture slots
  (scene textures, override textures for effect binds, and pass-level
  textures) and issues the draw with the pass's blend mode.
- `CFBO` (`CFBO.cpp:6-79`) is a single-color-attachment FBO: allocates an RGBA8
  texture at `textureWidth × textureHeight` (padded/power-of-2), explicitly
  clears it to **transparent black** (not the scene clear color — a comment
  at `CFBO.cpp:56-62` flags this as a fix for effects rendering solid
  rectangles otherwise), and exposes itself as a `TextureProvider` (so an FBO
  can be sampled exactly like a `.tex` file by a later pass) — this is the
  "effect chain" mechanism: `EffectParser`'s `fbos` list creates these,
  and passes read/write them via `binds` (bind index → FBO name) and
  `command: copy/swap` operations (`EffectParser.cpp:56-64`).

**Camera / resolution** (`Render/Camera.cpp`):

- `Camera::setOrthogonalProjection(width, height)` (`Camera.cpp:41-49`)
  builds a `glm::ortho(-w/2, w/2, -h/2, h/2, nearZ, farZ)` matrix translated
  by the eye position — a flat 2D-in-3D-space orthographic camera, matching
  the scene's declared `orthogonalprojection.width/height` (or window/output
  size if `auto`, per `WallpaperParser.cpp:71-72`). This maps 1:1 onto our
  own orthographic-camera assumption for `content.scene2d`.

**Particles** (`Render/Objects/CParticle.h`, 2323-line `.cpp`):

- Fully CPU-simulated (not GPU compute): `ParticleInstance`
  (`CParticle.h:26-79`) carries position/velocity/acceleration, rotation +
  angular velocity/acceleration, color/alpha/size/frame, age/lifetime, and
  per-particle oscillator state for alpha/size/position (sine-wave-driven
  "oscillate" operators). Emission is via `EmitterFunc` closures
  (`std::function<void(vector<ParticleInstance>&, uint32_t&, float)>`,
  `CParticle.h:95`) created per emitter shape — `createBoxEmitter`,
  `createSphereEmitter` (`CParticle.h:134-135`) — and per-frame behavior
  is `OperatorFunc` closures taking the instance vector, a live count, the
  scene's control points, and dt. This is a plain-data, closure-driven
  particle system with no dependency on any external physics/particle
  library — a clean, if sizeable (~2300 LOC across `.h`+`.cpp`), porting
  target.

**Video-as-texture**: `.tex` files can themselves be MP4 (`isVideoMp4`,
`Texture.h`); `CTexture` special-cases this to spin up an MPV-backed
`GLPlayer` (`CTexture.cpp:20-38`) rather than treat it as static image data —
separate code path from the `Render/VideoPlayback/MPV/GLPlayer` used for
`content.video` top-level wallpapers.

## 4. Asset-root requirement

Confirmed **required**: `AssetLocator::shader/texture` (`AssetLocator.cpp:11,
70-77`) resolve paths as `shaders/<file>` and `materials/<file>.tex` against
the mounted `Container` (VFS), which by default mounts the Wallpaper Engine
`assets/` folder plus the scene's own project directory/`.pkg`
(`FileSystem/Container.h`; mount-point wiring not fully traced but confirmed
by the `AssetLocator` relative paths and by `README.md`'s explicit "You must
own and install Wallpaper Engine" language).

Auto-detected Steam paths (`src/Steam/FileSystem/FileSystem.cpp:9-19,58-68`):
`~/.steam/steam/steamapps/common`, `~/.local/share/Steam/steamapps/common`,
`~/.var/app/com.valvesoftware.Steam/.local/share/Steam/steamapps/common`,
`~/snap/steam/common/.local/share/Steam/steamapps/common` (and the parallel
`workshop/content` paths for `--bg <workshopid>` resolution). None of these
match this machine's actual asset location
(`/media/crushinator/steamapps/common/wallpaper_engine/assets`), so
`--assets-dir /media/crushinator/steamapps/common/wallpaper_engine/assets`
would be required here.

Confirmed subfolders present on this machine that the code reads:
`assets/shaders` (referenced by `AssetLocator::shader`), `assets/materials`
(referenced by `AssetLocator::texture`, appending `.tex`), plus
`assets/effects`, `assets/models`, `assets/particles`, `assets/presets`,
`assets/fonts`, `assets/scenes`, `assets/zcompat` (the workshop-shader-compat
override root used by `AssetLocator::shader`) — every one of these exists
locally at `/media/crushinator/steamapps/common/wallpaper_engine/assets/`.

## 5. Integration options

### A — WRAP: link/spawn the C++ renderer as an offscreen worker

Concrete reuse points:

- `GLFWOpenGLDriver` (`Render/Drivers/GLFWOpenGLDriver.cpp:20-77`) **already
  creates its GLFW window with `GLFW_VISIBLE = GLFW_FALSE`**
  (line 34) — the app runs with a hidden 640×480 GL context by default and
  only calls `glfwShowWindow` (`GLFWWindowOutput`/output-specific code) when
  actually presenting. This means the "no window" requirement is nearly
  free: don't call whatever shows it.
- `WallpaperApplication::takeScreenshot` (`WallpaperApplication.cpp:542-620`)
  is the cleanest per-frame readback template: it binds the wallpaper's own
  FBO directly (`glBindFramebuffer(GL_FRAMEBUFFER, wallpaper->getWallpaperFramebuffer())`,
  around line 587), `glFinish()`s, then `glReadnPixels`/`glReadPixels` in
  `GL_RGB` (would need changing to `GL_BGRA` to match our frame protocol,
  which is exactly what `GLFWOpenGLDriver::dispatchEventQueue`'s
  `haveImageBuffer` path already does at `GLFWOpenGLDriver.cpp:127-135` for
  the X11 root-pixmap output — `GL_BGRA`/`GL_UNSIGNED_BYTE` straight into a
  caller-owned buffer). A worker could call the scene's FBO-bind-and-read
  step every frame instead of relying on any `Output` implementation at all.
- `RenderContext::render` (`RenderContext.cpp:19-37`) and
  `CWallpaper`/`CScene` are the minimum object graph needed to drive one
  frame once a `Project` is loaded; `WallpaperApplication`
  (`Application/WallpaperApplication.cpp`, 999 lines) is the natural
  top-level object to embed (it owns the driver, render context, and the
  playlist/background list) but pulls in audio (`SDLAudioDriver`,
  `PulseAudioPlaybackRecorder`), the DBus media-source, and CEF-backed
  `WebBrowserContext` unconditionally at link time even if unused at runtime.
- **What needs patching**: (1) force `ApplicationContext::settings.render.mode`
  down a path that never touches `X11Output`/`WaylandOutput` (both require a
  live compositor/X server) — likely need a new minimal `Output` subclass (or
  reuse `GLFWWindowOutput`, which already has no compositor dependency and
  stubs `haveImageBuffer()→false`, `getImageBuffer()→nullptr`) and drive
  frame capture externally via the `takeScreenshot`-style FBO read instead of
  trusting any `Output::haveImageBuffer` path; (2) silence/neutralize audio
  (`--silent` exists but the SDL/Pulse driver objects are still constructed);
  (3) drive the loop ourselves — call `dispatchEventQueue`/`update` at our
  own cadence rather than `glfwPollEvents`-driven vsync, and set
  `--fps`/`maximumFPS` to our target; (4) EGL/pbuffer vs GLFW-hidden-window:
  GLFW hidden window is sufficient (it already needs no visible surface) so
  an EGL pbuffer rewrite is not required, only "don't show the window, read
  the FBO instead of presenting."
- **Dependency pull-in for a WRAP worker**: this is the crux problem. Even a
  gutted "scene-only, no video, no web, no audio" worker links against the
  full `linux-wallpaperengine-lib` target as built by upstream's
  `CMakeLists.txt:570-627`, which unconditionally requires **GLEW, GLUT
  (`freeglut`), SDL2, FFmpeg (4 libs), MPV, PulseAudio, Freetype, DBus,
  glslang+SPIRV-Cross (vendored), GLFW, and CEF + `libcef_dll_wrapper`**
  (`target_link_libraries`, lines 578-600). Splitting the CMake target
  to drop CEF/MPV/PulseAudio/SDL2 for a scene-only build is possible in
  principle (the scene/texture/shader/effect/particle code doesn't call into
  those) but is itself a nontrivial patch to a codebase we don't control, and
  every upstream commit would need re-diffing against our patch.

### B — PORT: bring pieces into our Rust/Vulkan worker

Priority order for unblocking the 46 refused scenes, with rough size
estimates from the upstream sources read above:

1. **TEXV/.tex decoder** (highest priority, smallest effort). Port of
   `TextureParser.cpp` (403 LOC) + `Texture.h` (177 LOC) ≈ **500-600 LOC** in
   Rust: magic parsing, LZ4 frame decompress (the `lz4` or `lz4_flex` crate
   already covers this — no need to port LZ4 itself), mipmap chain assembly,
   spritesheet/animation frame tables. **No DXT/BC7 CPU decoder needs
   porting** — Vulkan has native `VK_FORMAT_BC1_RGBA_UNORM_BLOCK` /
   `BC2`/`BC3_UNORM_BLOCK` / `BC7_UNORM_BLOCK` compressed formats, so the
   LZ4-decompressed DXT/BC7 blocks can go straight into a Vulkan compressed
   image the same way upstream hands them to
   `glCompressedTexImage2D` — this is a meaningfully *smaller* lift than
   writing a software block decompressor. The `stb_image`-via-`FIF_*` path
   (PNG/JPEG containers wrapped in the same `.tex` envelope) is already
   covered by our existing `image`-crate decoder in
   `crates/kwe-scene-renderer/src/textures.rs` — only the TEXV *envelope*
   (container/mipmap/LZ4/format-enum parsing) is new work, not raw pixel
   decoding.
2. **Model → material → texture resolution** (`ModelParser.cpp`, 33 LOC +
   `MaterialParser.cpp`, 127 LOC + associated `Data/Model/*.h` structs) ≈
   **300-400 LOC** to port the JSON schema walk (model → material path →
   passes → texture-slot maps → combos → constant shader values). This is
   mechanical JSON-to-struct work, not rendering logic, and is what actually
   turns "46 scenes are model layers we skip" into "resolved texture + shader
   references" — i.e. this is the piece that determines whether a model
   layer even has anything drawable once TEXV works.
3. **Shader preprocessing** (`ShaderUnit.cpp`, 725 LOC minus the ~150 lines
   that are glslang/SPIRV-Cross plumbing we'd replace) ≈ **400-500 LOC**:
   the `#include` resolver, `#require`/LightingV1 stub, combo/uniform
   comment-scraping, and the large HLSL-compat `#define` header
   (verbatim-portable, it's just string data). **GLSL→SPIR-V**: `glslang`
   has Rust bindings friction; more idiomatic in a Rust/Vulkan stack is
   `naga` or `shaderc-rs` for the GLSL→SPIR-V step, replacing
   `GLSLContext.cpp` (194 LOC) entirely rather than porting it — and unlike
   upstream, we would *skip* the SPIRV-Cross round-trip back to GLSL, since
   Vulkan consumes SPIR-V directly. Net new Rust code: preprocessor only,
   ~400-500 LOC; the SPIR-V step is a library swap, not a port.
4. **Effect passes / FBO composition** (`CPass.cpp`, 1114 LOC + `CFBO.cpp`,
   135 LOC + `EffectParser.cpp`, 117 LOC) ≈ **700-900 LOC** ported: this is
   the largest single piece, because `CPass.cpp` also carries per-pass
   texture-binding/override logic intertwined with the render call, which
   would need to be re-split against our own Vulkan render-graph/pass
   abstraction rather than copied as-is.
5. **Particle definitions + simulation** (`CParticle.cpp`, 2323 LOC +
   `CParticle.h`, ~200 LOC) ≈ **1500-2000 LOC** ported (the file includes a
   lot of per-operator-type boilerplate that compresses well in Rust with
   enums/match rather than one `EmitterFunc`/`OperatorFunc` per shape) — this
   is the single biggest port item by upstream LOC, but our own
   `crates/kwe-scene-renderer/src/particles.rs` already has 1129 LOC of
   particle machinery for the particle systems we *do* support today, so this
   is "extend an existing system to more emitter/operator kinds and read
   external particle-definition files" rather than a green-field port —
   likely smaller in practice than the raw upstream LOC suggests.

Total rough new/ported Rust for items 1-4 (the model-layer-unblocking path,
leaving full particle-file support for later): **~2000-2500 LOC**, well
inside the range of milestones already shipped in this repo (`scene.rs` alone
is 3399 LOC).

## 6. Build attempt on this machine

No system packages were installed (no `pacman -S`/`sudo` used). Pre-existing
package inventory on this machine (`pacman -Q`) already covers nearly every
upstream-listed dependency:

| Dependency | Status |
|---|---|
| glew 2.3.1 | present |
| glfw 3.5.1 | present |
| sdl2-compat 2.32.70 | present (provides SDL2 API) |
| ffmpeg 9.0.1 | present |
| mpv 0.41.0 | present (provides `libmpv.so`/`mpv.pc`, so `--assets-dir`-style `find_package(MPV)` succeeds) |
| cef 151.3.24 | present, but **irrelevant** — upstream's `CMakeLists.txt` ignores the system CEF and downloads its own pinned `135.0.17` binary unconditionally (`DownloadCEF`, `CMakeLists.txt:113-114`) |
| lz4 1.10.0 | present |
| pipewire 1.6.8 (+ pulse compat) | present; satisfies the `PulseAudio` `find_package` via `libpulse` |
| glm 1.0.3, freeglut 3.8.0 | present |
| libpulse 17.0 | present |
| cmake 4.4.2, mesa 26.2.1, libglvnd 1.7.0 | present |
| `freeimage` | **not packaged**, but **not actually needed** — see §3, the code never links real FreeImage |
| dedicated `pulseaudio` daemon package | not packaged, but `libpulse` (client lib) is present and this machine runs PipeWire's PulseAudio-compatible server, which is what `find_package(PulseAudio)` and the runtime client actually need |

`cmake -S . -B build -DCMAKE_BUILD_TYPE=Release` (all git submodules
initialized first: `glslang-WallpaperEngine`, `SPIRV-Cross-WallpaperEngine`,
`json`, `stb`, `kissfft`, `quickjs`, `argparse`, `Catch2`, `MimeTypes`):

- **Configures successfully.** Every `find_package`/`pkg_check_modules` call
  resolved against the pre-existing package set above (X11, OpenGL, GLEW,
  DBus, GLUT, ZLIB, MPV, LZ4, FFmpeg's 4 libs, PulseAudio, Freetype all
  reported `Found`). Wayland support (`wayland-cursor`,
  `wayland-protocols`, `egl`, `wayland-egl`) and X11/Xrandr/Xxf86vm both
  detected — both backends compile in.
- The only slow/unusual step is the **unconditional CEF binary download**
  (~360 MB from `cef-builds.spotifycdn.com`), which is not a "missing
  dependency" in the failure sense but is a mandatory network fetch baked
  into configure that has nothing to do with our scene-rendering interest —
  it exists purely for the `content.web` (CEF) wallpaper type upstream also
  supports.
- Build step (`cmake --build build -j16`, 16 cores, capped at 10 min): **succeeded
  cleanly, exit 0, zero compiler errors** (`grep -ic error` on the full build
  log returns 0), well inside the time budget. Final artifact:
  `build/output/linux-wallpaperengine`, a 41 KB dynamically-linked
  `linux-wallpaperengine-lib.so`-dependent PIE ELF64 executable (the bulk of
  the 3.4 GB build tree is the downloaded CEF distribution — `libcef.so`
  alone is ~1.4 GB — plus object files; the actual project code compiles to a
  small binary + shared lib). This confirms the upstream project is not just
  theoretically buildable but concretely builds end-to-end against this
  machine's ordinary (non-AUR, non-manually-curated) package set, with the
  sole non-system dependency being the auto-fetched CEF blob. No attempt was
  made to run the binary against the live desktop, per the task's scope.

## 7. Licensing obligations

- **Option A (link against `linux-wallpaperengine-lib`)**: linking (static or
  dynamic, same-process or via a helper binary invoked as a subprocess with
  a stable IPC boundary) against GPL-3.0-or-later code is the strict case
  GPL was written for. If our worker process statically or dynamically links
  the library in the same address space, **that worker binary as a whole
  must be GPL-3.0-or-later** (or compatible), source must be offered to
  anyone we distribute the binary to, and all upstream copyright/license
  notices must be preserved. A cleaner boundary is a **separate GPL
  subprocess** communicating over a well-defined protocol (stdin/stdout,
  a socket, or our existing frame-file mechanism) with no shared address
  space and no static linking — under the "mere aggregation"/"separate and
  independent programs" reading of GPLv3 §5, this is the standard way
  GPL and non-GPL components coexist in one product (this is exactly the
  arrangement our own repo already uses for
  `crates/kwe-scene-renderer` and `crates/kwe-web-renderer` as independent
  supervised worker binaries; a `linux-wallpaperengine`-derived worker binary
  would join that model as another GPL sibling process, not require the
  Rust daemon or other workers to become GPL). Either way, our project or the
  worker-in-question must ship under GPL-3.0-or-later terms (or a
  GPL-compatible license) for any code that is *part of* the linked binary,
  and we must be prepared to offer corresponding source on distribution.
- **Option B (port logic into our Rust worker)**: porting/rewriting
  algorithms and data-format knowledge (the TEXV container layout, the shader
  `#define` compatibility shims, the particle emitter/operator model) based
  on reading GPL source is **not automatically "clean-room"** — courts and
  the FSF's own guidance treat a close structural/textual port as a
  derivative work even when renamed and re-typed, especially for the
  shader-compat header (verbatim `#define` text) and the binary-format
  parsing logic where there's essentially one obvious way to write it once
  you've read theirs. The safe posture for Option B: either (a) treat the
  ported crate/module as GPL-3.0-or-later itself (isolate it in its own
  crate so the license boundary is a crate boundary, matching how
  `kwe-scene-renderer` is already a separate binary from the daemon), or
  (b) reimplement strictly from the **public, third-party-documented**
  Wallpaper Engine `.tex`/scene.json format (much of which is independently
  documented by the RePKG project and community wikis, credited by
  upstream's own README "Special Thanks") rather than from reading
  Almamu's C++ line-by-line, and be able to point to that independent
  source for each ported piece. Given the practical reality that this
  review itself was produced by reading Almamu's source closely (this
  document cites exact file:line locations), any Option-B port that follows
  from it should be treated as GPL-derived and licensed accordingly unless
  a deliberate clean-room re-derivation from independent documentation is
  done first.
- Either option requires: preserving upstream copyright notices for any
  copied/closely-derived text (the shader compat header, TEXV struct
  layouts), a `NOTICE`/`THIRD_PARTY_LICENSES` entry crediting Almamu and
  linux-wallpaperengine, and — if we distribute binaries — a source offer
  covering the GPL-covered component(s).
- The permissively-licensed vendored deps (glslang/SPIRV-Cross/stb/json/
  MimeTypes/kissfft/argparse/quickjs) impose no copyleft even under Option A;
  only Almamu's own GPL-3.0-or-later `src/` code and its "you must be GPL to
  link this" property matter here.

## Recommendation: **B (port), scoped to items 1-2 first, treated as GPL-licensed code in an isolated crate**

Justification:

- The single biggest blocker for our 46 refused scenes is the **TEXV
  container + model→material→texture resolution** (§5B items 1-2, ~800-1000
  LOC combined) — small, self-contained, format-parsing work with no OpenGL
  dependency to strip out, unlike everything else in the upstream codebase.
- Option A's dependency graph (GLEW/GLUT/SDL2/FFmpeg/MPV/PulseAudio/DBus/CEF)
  is entirely disproportionate to what we need (draw a scene into a
  BGRA buffer) — even a "gutted" wrap still links a library built to also
  drive audio playback, video decode, and a full Chromium instance, none of
  which our scene worker touches; every upstream release would force us to
  re-validate that our patched, gutted CMake target still builds.
- Our render backend is Vulkan, not OpenGL — Option A's `CTexture`/`CFBO`/
  `CPass` all speak raw OpenGL 3.3 calls directly; wrapping them offscreen
  still leaves us maintaining a second GPU API's driver stack (Mesa's GL
  path) in addition to our Vulkan one, purely to feed pixels back into our
  own compositor.
- Option B lets us keep everything in one process, one GPU API, one frame
  protocol — matching the architecture the rest of this repo already commits
  to (supervised single-purpose worker binaries, no OpenGL anywhere else in
  the stack).
- The GLSL→SPIR-V step (item 3) is *better* served by a library swap
  (`naga`/`shaderc-rs`, already idiomatic in Rust/Vulkan) than by porting
  upstream's glslang+SPIRV-Cross round-trip, which exists only because
  upstream targets OpenGL and needs GLSL back out; we can skip that
  round-trip entirely and consume SPIR-V directly.

Top risks:

1. **Licensing posture of Option B.** As argued in §7, the port should be
   treated as GPL-3.0-or-later, isolated to its own crate, with a
   deliberate decision (before writing code) about whether our overall
   project/worker binary is prepared to carry that license, or whether we
   instead do a genuine clean-room re-derivation from public format
   documentation (RePKG, community wikis) rather than from this repo's
   source. This must be resolved before item 1 lands, not after.
2. **Model geometry/mesh loading was not traced in this pass** — the review
   budget went to texture/shader/effect/particle plumbing (the pieces most
   directly blocking the 46 refused scenes) and did not locate/read the
   `.json` model *mesh* (vertex/puppet) parser. If a meaningful fraction of
   the 46 scenes need real 3D mesh data (not just a textured quad with a
   material), the effort estimate in §5B item 2 understates the work; a
   follow-up pass should specifically locate and read the mesh-loading code
   before committing to a schedule.
3. **`BC7` texture format is unhandled even in upstream's own OpenGL path**
   (`CTexture::setupInternalFormat`, no `case` for `TextureFormat_BC7`,
   §3) — if any of our 46 scenes use BC7 textures, upstream's own code is
   not a working reference for that format either; Vulkan's
   `VK_FORMAT_BC7_UNORM_BLOCK` support means we aren't blocked, but we can't
   crib the OpenGL constant mapping for it since upstream doesn't have one.
4. **Effect/FBO composition (item 4) is the largest, most render-API-coupled
   port item** and the estimate above (700-900 LOC) assumes a reasonably
   direct mapping onto whatever pass/render-graph abstraction
   `crates/kwe-scene-renderer/src/vulkan.rs` (2928 LOC) already has; if our
   existing Vulkan layer's pass model doesn't cleanly support "read an
   arbitrary earlier FBO as a sampled texture in a later pass," this item
   grows.
5. **This review's cmake configure/build was performed in a scratch clone
   outside our repo, on this one machine's already-rich package set** — it
   demonstrates the upstream project *can* build here, not that any of its
   code compiles or behaves correctly once ported/adapted into our Rust/
   Vulkan stack; it's evidence for "the reference implementation exists and
   runs," not a validation of the port itself.

Concrete first slice:

- **Build**: a `wpengine-texv` Rust module (own file, clearly marked
  GPL-3.0-or-later-derived per §7, e.g. under `crates/kwe-scene-renderer/src/`
  or a new small crate) that parses the TEXV/TEXI/TEXBxxxx/TEXSxxxx container
  format per §3 (magic checks, format/flags/dimensions, mipmap chain with
  LZ4 decompress via `lz4_flex`, animation/spritesheet frame tables) and
  returns either raw pixel bytes (for `FIF_*`-tagged PNG/JPEG-style entries,
  routed through our existing `image`-crate decoder in `textures.rs`) or
  compressed block bytes + a `TextureFormat` enum (for DXT1/3/5/BC7/raw
  formats, to be uploaded as native Vulkan compressed image formats — no
  block decompression needed).
- **Acceptance test**: feed the module the `scene.pkg` at
  `/media/crushinator/steamapps/workshop/content/431960/1725674512/scene.pkg`
  (parsed via a small `PKGV` reader per §3's `PackageParser` notes — file
  list of name/offset/length, no container-level compression) — extract
  every `.tex` entry referenced by that scene's `scene.json`/model/material
  chain, decode each one, and assert: (a) the container/format/flags parse
  without error for every `.tex` found, (b) the decoded byte length matches
  `width * height * bytes_per_pixel` for uncompressed formats or the DXT
  block-size formula for compressed ones, (c) at least one previously-skipped
  model-layer object in that scene now resolves to a non-empty texture
  reference instead of being counted as a `renderer.scene.model_layer_skip`
  event (`docs/FEATURE_COMPATIBILITY.md` row `content.scene2d`) — i.e. the
  test proves the *decode* path works even before the full model→material→
  shader→draw chain is wired up, which is the honest, incrementally-testable
  slice given items 2-4 remain unbuilt.
