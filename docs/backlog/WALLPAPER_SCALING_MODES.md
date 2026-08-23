# Feature request: per-wallpaper scaling modes (stretch / fill / aspect)

- **Requested:** 2026-08-22 (user request)
- **Status:** IMPLEMENTED 2026-08-22 (F1, branch `beta-b4-apply-quarantine`,
  commit "feat(f1)"): everything under "Where it lands" below, plus the
  render-resolution decision (5): the daemon derives the canvas from the
  output geometry (aspect kept, long edge ≤ 2560) when the client omits
  `width`/`height`, and the mode is applied at BOTH layers — in the renderer
  (video: libmpv letterbox / panscan / keepaspect=no; scene: the declared
  scene rectangle is mapped onto the canvas by the mode — before F1 scene
  units were canvas pixels 1:1, so a 1920x1080 scene in a 960x540 canvas
  showed its centre quarter) and in the plugin (`FrameSurface.scaling`).
  Left open: a per-mode live pixel oracle in a smoke lane, and the
  letterbox colour (still black).

## Ask

Let the user choose how a wallpaper's frames map onto the output, the way
Plasma's own image wallpaper does:

| Mode | Behaviour |
|---|---|
| **Aspect** (fit) | Whole frame visible, aspect preserved, unused area filled. Today's only behaviour. |
| **Fill** (crop) | Aspect preserved, scaled to cover the output, overflow cropped. |
| **Stretch** | Scaled to the output exactly, aspect ignored. |

## Where it lands

**The scaling itself is one function.** `FrameItem::imageDestination()`
(`modules/org/kde/kwe/display/frameitem.cpp:359`) hardcodes
`target.scale(boundingRect().size(), Qt::KeepAspectRatio)` and centres the
result; `paint()` fills the rest with black. Fill is
`Qt::KeepAspectRatioByExpanding` plus clipping to `boundingRect()`; stretch is
`boundingRect()` itself. So the render change is small — the work is
everything around it:

1. **Pointer mapping must follow the same rectangle.** `updatePointer()`
   (`frameitem.cpp:381`) normalises the cursor against `imageDestination()`.
   Under Fill the destination is *larger* than the item, so the normalised
   coordinates must stay in 0..1 over the visible crop, and under Stretch the
   "outside the image" branch becomes unreachable. Scene and web wallpapers
   consume those coordinates, so getting this wrong silently misaligns
   interactive wallpapers.
2. **Persistence.** The mode is per output and must survive a restart:
   a `scaling` field in `assignments-v1.json` (schema addition — the store
   rejects unknown fields, so this is a versioned change), set through
   `wallpaper.apply` and reported by `wallpaper.assignments`.
3. **Transport.** The plugin learns the mode from the daemon over the display
   session, alongside `frameFile` — the frame protocol/DisplaySession carries
   it, the plugin does not read config directly.
4. **Manager UI.** A three-way selector in `WallpaperDetail.qml` beside the
   output picker, defaulting to Aspect, applied without re-running the whole
   apply transaction where possible.
5. **Render resolution is a separate question, worth deciding here.** The
   current apply pins `width: 960, height: 540` regardless of output geometry
   (observed on a 2926x823 output), so every mode is upscaling a 960x540 frame.
   Scaling modes make that visible. Whether the renderer should be asked for
   frames matched to the output — and what the cost is — should be settled
   alongside this, not after it.

## Notes

- The letterbox fill colour is currently hardcoded black (`paint()`); Aspect
  mode should probably let it follow the desktop theme, but that is a
  sub-decision, not a blocker.
- No new dependency, no daemon privilege, no renderer protocol break beyond
  the additive assignment field.
- Parity note: this is a `FEATURE_COMPATIBILITY.md` row of its own — the
  original offers the same three modes.
