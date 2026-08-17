# M1d-A task contract: renderer resource containment

## Goal and user-visible outcome

A broken or hostile wallpaper renderer must fail inside a bounded child-process
budget without exhausting the desktop session. The daemon must identify a
resource-limit failure, preserve a healthy active wallpaper while a bad
candidate is rolled back, and retain enough diagnostics to reproduce and
quarantine the offending wallpaper.

## Scope

In scope:

- per-renderer address-space, output-file, file-descriptor, process, and core
  dump limits applied before executing the renderer;
- bounded and configurable limits with conservative defaults suitable for the
  future Vulkan renderer;
- additive status diagnostics that report the effective renderer limits;
- a synthetic memory-pressure fault that proves allocation denial, bounded
  retry, quarantine, and active-renderer continuity;
- aggregate memory and task limits plus GPU-compatible hardening in the
  packaged systemd user service;
- verification without installing the service or touching the live Plasma
  session.

Out of scope:

- renderer performance or frame-protocol changes;
- CPU-time limits, because wallpapers are intentionally long-running;
- kernel OOM injection into the user's desktop session;
- seccomp policy generation before the renderer syscall surface is stable;
- input, audio responsiveness, Workshop downloads, or Plasma package loading.

## Acceptance and failure criteria

- Every renderer child receives limits before `exec`; failure to apply any
  limit aborts that launch.
- The default address-space ceiling leaves room for Vulkan driver mappings and
  can be reduced explicitly for deterministic integration tests.
- Core dumps are disabled, renderer-created files remain bounded above the
  frame protocol's 128 MiB maximum, and descriptor/process counts are finite.
- A memory-pressure candidate is denied by its process limit, reported as
  `resource_limit`, retried only within its budget, and quarantined while the
  active PID and frame file continue advancing.
- The systemd user unit grants its runtime and state directories explicitly,
  preserves GPU device access, and applies aggregate service limits.
- Rust tests, the complete supervisor smoke suite, and offline systemd unit
  verification pass. No operation touches the live Plasma session.

## Protocol, compatibility, and provenance

The newline-delimited daemon protocol remains version 1. Effective limit
fields and the `resource_limit` failure value are additive. Frame protocol v1
is unchanged. The implementation uses standard Linux `setrlimit(2)` and
systemd service directives; no source code is copied from upstream wallpaper
projects. Their isolation goals remain idea-level references documented in
`THIRD_PARTY.yml` and `docs/PROVENANCE.md`.
