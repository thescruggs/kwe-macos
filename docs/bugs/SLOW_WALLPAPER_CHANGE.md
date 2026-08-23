# Perf: wallpaper change feels slow (P1)

- **Reported:** 2026-08-22 (user, after F1 verification: "It's a bit slow on
  the wallpaper change, let's look at that later")
- **Status:** FILED, not measured. Triage notes only.

## Where the time can go (from the code, to confirm with timestamps)

1. `wallpaper.apply` transaction: fresh output enumeration (kscreen-doctor
   + one evaluateScript probe), preflight (scene.pkg decompression up to
   16 MiB), `renderer.start`, then **canary**: the candidate must publish
   ≥3 frames AND run for `--renderer-canary-ms` (default 1000 ms) before
   promotion; then the Plasma switch script + verification probe.
2. Renderer cold start: web = bwrap + chromium boot + first screencast
   frame (~0.6–1.5 s measured in M2); video = libmpv open + first decoded
   frame; scene = Vulkan device + texture uploads (+ libmpv per VideoLayer).
3. F1 made canvases larger (2560-wide): first-frame and per-frame cost grew
   ~4.5× versus 960x540 — worth re-measuring now.
4. The plugin polls the frame file at 33 ms and the display session polls
   `renderer.status` on its own interval; the handoff ack adds one round
   trip.

## How to measure

Add `event=apply.phase name=<step> ms=<n>` lines around each step of
`ApplyHandle::apply` (enumerate, preflight, start→first frame, canary,
switch, verify) and read them for video / web / scene on this machine.
Then decide: shorter canary for user applies (the canary exists for
crash containment — a user-initiated switch could use 250 ms), parallel
preflight, or keeping the previous renderer live during the handoff (it
already is: the old worker stays until promotion).
