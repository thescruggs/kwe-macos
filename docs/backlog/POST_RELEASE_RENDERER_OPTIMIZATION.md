# Deferred renderer optimization backlog

Decision recorded 2026-08-16: keep the initial renderer and frame protocol as
implemented through the first release. Optimization work begins after that
release unless measured performance violates a release safety or usability
gate.

Future tasks must start with profiling on the supported Intel Mesa and NVIDIA
lanes. Candidate work:

1. measure CPU copy time, compositor presentation latency, dropped frames,
   memory bandwidth, power use, and VRAM/RAM pressure at common resolutions;
2. establish per-resolution and per-refresh-rate performance budgets;
3. add negotiated DMA-BUF image and semaphore transport while retaining the
   mmap fallback;
4. eliminate avoidable format conversions and copies based on profiles;
5. tune frame pacing, inactive-output throttling, and pause behavior;
6. compare per-output and shared-renderer strategies for clone/span modes;
7. rerun crash, corruption, driver-reset, and fallback tests after every
   optimization.

No optimization may move parsing, rendering, media, or recovery work into
`plasmashell`.

