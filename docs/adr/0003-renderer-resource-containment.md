# ADR 0003: layered renderer resource containment

- Status: accepted for Alpha M1d-A
- Date: 2026-08-16

## Decision

Apply bounded Linux `setrlimit(2)` values in each renderer child after fork and
before exec, and apply aggregate resource controls to the complete daemon unit
with systemd. Keep renderer crash, hang, protocol, and resource failures in the
existing candidate/retry/quarantine state machine.

Use a generous default virtual-address limit and a lower aggregate cgroup
resident-memory limit. Do not apply lifetime CPU-time limits to long-running
wallpapers. Do not enable systemd device isolation or executable-memory denial
until those controls are proven compatible with Vulkan drivers.

## Rationale

Per-process limits fail close to the renderer that requested a resource and
remain present when the daemon is started manually. systemd controls bound the
aggregate of the daemon, active worker, candidate, and brief handoff worker.
Neither layer alone covers both cases.

The deterministic test fault uses fallible virtual allocation and exits with a
reserved test code when the kernel denies it. This validates policy and
rollback without risking a desktop-wide OOM event.

## Provenance

This is an original implementation using documented Linux and systemd
interfaces. Upstream wallpaper projects informed the general isolation goal;
no code or wire format was copied or adapted.
