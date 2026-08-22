# Bug: applying a scene shows a blank (white/grey) background

- **Reported:** 2026-08-22 (user report: "when applying a scene I just had a
  white background")
- **Severity:** High user-facing — the apply *succeeds*, the desktop goes
  blank, and nothing anywhere tells the user why.
- **Status:** DIAGNOSED from the state the failing apply left behind. Not yet
  fixed.

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

## Fix directions (to be decided at fix time)

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
