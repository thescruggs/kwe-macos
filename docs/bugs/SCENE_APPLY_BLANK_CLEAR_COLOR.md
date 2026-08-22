# Bug: applying a scene shows a blank (white/grey) background

- **Reported:** 2026-08-22 (user report: "when applying a scene I just had a
  white background")
- **Severity:** High user-facing — the apply *succeeds*, the desktop goes
  blank, and nothing anywhere tells the user why.
- **Status:** FIXED (2026-08-22, `77c6d3e` + review `e6fff36`, merged to
  `fix/qt611-gallery-delegates`). The diagnosis below was right about the
  silence and half right about the cause; confirming the scene at fix time
  turned up a second, larger defect. Both are fixed. See "Fix" at the end.

## Symptom

The user applied a scene wallpaper. The transaction completed (no error, no
rollback, no quarantine) and the desktop showed a flat, near-white background.

## Evidence

The assignment store (`~/.local/state/kwe/assignments-v1.json`) records the
last applied wallpaper:

```json
{"outputs": {"DP-1": {"wallpaper_id": "1725674512", "kind": "scene",
  "content": ".../workshop/content/431960/1725674512/scene.pkg",
  "width": 960, "height": 540, "fps": 30}}}
```

That is Workshop item 1725674512, "Aurora Borealis" (`"type": "scene"`).
The daemon log for that boot confirms a healthy promotion, not a failure:

```
12:08:26 kwe-daemon: event=renderer.promoted generation=2 wallpaper_id=1725674512
```

The promoted frames were kept as `~/.local/state/kwe/last-good-{a,b}.ppm`.
Both are 960x540 and **every one of their 518,400 pixels is exactly
`b2 b2 b2`** — a single flat colour, no content at all. `0xB2` = 178/255 =
0.698, i.e. the scene's own declared `clearcolor` of `0.7 0.7 0.7`.

So the renderer ran correctly and drew **nothing**: the composite is the clear
colour and only the clear colour.

## Root cause (high confidence, to confirm at fix time)

Scene layers that reference a **model** (`.json` model reference — the
scene3d path) are skipped at parse. `crates/kwe-scene-renderer/src/scene.rs:551`
does this *before* any validation, by design, under the skip-never-reject
policy, and `scene.rs:2306` (`model_json_references_skipped_as_m3h`) pins it as
deliberate: **scene3d is BETA_M3h, which is not implemented yet.** A scene
built entirely out of model-backed layers — which "Aurora Borealis" almost
certainly is — therefore registers zero drawable layers and composites to bare
clear colour.

Two distinct defects follow:

1. **No honesty signal whatsoever.** `SceneConfig` counts `text_layer_skips`,
   `video_layer_skips`, `particle_system_skips` and `particle_file_refs`, and
   the worker emits a one-time diagnostic for each (`main.rs:857`, `:881`).
   Model-layer skips are counted **nowhere** and logged nowhere. The renderer
   cannot tell the daemon it rendered an empty scene, the daemon cannot tell
   the manager, and the manager tells the user it worked.
2. **An empty composite is treated as a good frame.** The promotion gate
   accepts a scene that drew zero layers and stores it as `last-good`. A
   wallpaper that renders nothing at all is not a successful apply.

Confirming step for fix time: read the `scene.json` inside that `scene.pkg`
and count objects by kind, to prove every object is model-backed rather than
image/text/particle.

### What the confirming step actually found (2026-08-22)

The scene has 7 objects, and only 2 of them are the model layers the
diagnosis predicted:

| objects | shape | classified as (before the fix) |
|---|---|---|
| 2 | `"image": "models/….json"` | model layer — skipped, as diagnosed |
| 5 | `"image": null` + `"particle": "particles/presets/….json"` | **image layer with no texture** |

The five are the scene's particle systems (Shooting star, Magic sparkle x3,
Fireflies). `parse_objects` classified an object as an image layer whenever
it carried an `image` KEY, without looking at the value — and the Wallpaper
Engine editor writes `"image": null` on every particle object. So the
particle systems never registered as particle systems at all: they became
textureless image layers, and the scene lost its only non-model content.

That is a second defect, independent of the missing honesty signal, and it
is not specific to this wallpaper: **all 65 null-image objects in the
60-package local corpus are particle systems.**

Two further facts came out of the same census, and they set the honest
scope of what this build can render:

- The scene's actual background is `models/….json` → a 5.5 MB
  `materials/….tex`. TEXV textures are planned with M3h, so even with model
  layers parsed there would be nothing to decode.
- All 380 particle systems in the corpus reference an external particle
  definition file; none carries an inline material. The M3f parse registers
  such a system with defaults and never reads the file, so it has no
  texture and draws nothing.

Net: **46 of the 60 local scenes composite to bare clear colour.** The 14
that draw anything do it with text or decodable image layers.

## Fix directions (as written at diagnosis time)

1. Add `model_layer_skips` to the parse counts and a one-time worker
   diagnostic, matching the existing text/video/particle skip pattern.
2. Surface "this scene needs features this build does not have yet" through
   `renderer.status` / the `renderer_report` side file into the manager's apply
   result, so the UI can say *scene3d is not supported yet* instead of showing
   a blank desktop. This is the same honesty ladder as
   `FEATURE_COMPATIBILITY.md`.
3. Decide the policy for a zero-drawable-layer scene: preflight it as
   unsupported and refuse the apply (keeping the previous wallpaper), or apply
   it with a visible degraded state. Refusing at preflight is preferable — a
   blank desktop reads as a crash to the user.
4. Preflight should be able to answer this *before* the apply transaction runs:
   `preflight_scene` can already open the pkg (M3b), so it can count drawable
   objects.

Note this bug does not go away when M3h lands: any scene using a feature the
current build lacks hits the same silent-blank path. The honesty work (1-3) is
the durable fix; M3h just shrinks the set of scenes that trip it.

## Fix (2026-08-22)

Branch `beta-b2-scene-honesty`, worktree slice, implementation commit
`77c6d3e` + adversarial-review commit `e6fff36`, fast-forwarded onto
`fix/qt611-gallery-delegates`.

**1. Classification (the render defect).** `crates/kwe-core/src/sceneobjects.rs`
is new and owns the `objects` classification: an object is an image layer
only when its property-unwrapped `image` value is a STRING; otherwise the
`video`/`particle`/`text` keys decide, and an `image` key with a non-string
value and no other visual key still registers a textureless layer so a
script can reach it by name. The scene renderer's `parse_objects` now calls
that classifier instead of its own `contains_key` ladder, so the corpus's
65 null-image particle systems register as particle systems.

**2. Honesty (the silence).** The same classifier answers "can this object
draw in this build?", and both gates use it:

- `preflight_scene` (file lane) and `preflight_pkg` (package lane, which
  now decompresses the `scene.json` entry under the worker's own 16 MiB
  cap) refuse a scene that declares objects and can draw none of them,
  with a reason per missing feature: model layers, TEXV textures, external
  particle files, textureless layers. The refusal reaches the client as
  `invalid_params` from the existing StartSpec validation, so no worker
  spawns, no transaction runs, and the wallpaper on screen stays put.
- The worker re-checks the same STATIC rule before its first publish and
  exits 74 (`event=renderer.scene.no_drawable_content`) for scenes that
  never went through preflight. Static, not "did any texture upload
  succeed": a layer whose content fails to decode is a degraded layer, and
  degrading a layer never rejects a scene (the M3c/M3g skip-never-reject
  contract).
- `model_layer_skips` is counted and reported
  (`event=renderer.scene.model_layer_skip count=N`), matching the existing
  text/video/particle skip diagnostics.
- The manager maps the refusal to "This wallpaper needs features this
  version cannot render yet, so it was not applied (…). Your current
  wallpaper is unchanged." — a feature gap, not a rejected request.

Policy decision, per fix direction 3: **refuse, do not apply degraded to
blank.** One drawable object is enough to apply; zero is a refusal.

**Effect on the local corpus:** 46 of 60 scenes are now refused with a
named reason instead of applying as a flat rectangle, the reported
"Aurora Borealis" (1725674512) among them:

```
$ kwe preflight --path .../1725674512/scene.pkg
"scene draws nothing in this build: 2 model layer(s) need scene3d, which
 this build does not render yet; 5 particle system(s) reference external
 particle files, which this build does not read yet"
```

**Gates:** `./scripts/check.sh` green (fmt, clippy -D warnings, workspace
tests, cmake build, qmllint, diagnose, vulkan probe); `ctest` 8/8 including
`kwe-apply-client-test`; `scripts/smoke-scene.sh` green end to end with new
cases B2-a (model-only scene → preflight `invalid_params`, no worker),
B2-b (one drawable layer → applies, model skip reported), B2-d (standalone
model-only scene → exit 74, no frame published). The suite also fixes a
pre-existing flake it exposed: the standalone M3e lane sampled the frame
file as soon as it had a header — before the first publish — and read an
all-zero slot.

**Known limitations (deliberate, recorded in FEATURE_COMPATIBILITY.md):**

- The refusal has no error code of its own; the manager keys off the
  "draws nothing in this build" phrase in the `invalid_params` detail. A
  dedicated code would need a typed reason threaded through the supervisor.
- A scene whose declared-drawable content all fails to DECODE still applies
  and shows the clear colour: the static rule cannot see that, and the
  skip-never-reject contract says a broken layer degrades rather than
  rejects.
- Repeated exit-74 workers count toward `max_failures` and would eventually
  quarantine an unsupported wallpaper. Preflight refuses those scenes first,
  so the path is rare; a distinct "unsupported" failure kind is the clean
  fix if it ever shows up.
- This does not shrink when M3h lands — it shrinks the SET of scenes that
  trip it. The honesty ladder is the durable part.
