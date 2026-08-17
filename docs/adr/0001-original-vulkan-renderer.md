# ADR 0001: original isolated Vulkan renderer

- Status: accepted for Alpha development
- Date: 2026-08-16

## Decision

Implement the scene renderer as original Rust code using `ash` and Vulkan. Keep
it in an independently supervised process and transfer only validated frames to
a thin Plasma display bridge. Prefer DMA-BUF plus external semaphore/fence
synchronization, with a bounded shared-memory fallback.

Open Wallpaper Engine and other Linux compatibility projects are behavior,
format, UX, and failure-mode references only. They are not runtime backends,
fork bases, or sources of copied code. Any later code adaptation requires a
pinned revision and a new provenance review before implementation.

## Why

The existing projects demonstrate valuable concepts but also couple formats,
renderers, and desktop integration in ways that make failures difficult to
contain. An original renderer lets this project introduce capabilities in
small tested increments and quarantine failures by content hash, backend
version, GPU, and driver. The process boundary keeps a parser, shader compiler,
or GPU failure from sharing `plasmashell`'s process.

## Consequences

Initial compatibility will be narrower and reported honestly as
renderer-dependent. Scene format work needs synthetic fixtures and feature-level
capability evidence. GPL projects can still inform black-box behavior, but their
code cannot enter Apache-2.0 components without an explicit licensing change.

