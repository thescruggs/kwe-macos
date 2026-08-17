# Shared frame protocol v1

## Purpose and boundary

This is the portable fallback transport between an untrusted external renderer
and a small display client. Alpha M1a uses a mode-`0600` regular file beneath a
private user runtime directory. The producer maps it; consumers may map or use
positioned reads. On Arch this runtime directory is tmpfs-backed, so the
payload is shared memory without placing a parser, shader compiler, or renderer
in the display process. The M1e Plasma-facing consumer deliberately uses
bounded `pread` snapshots so a buggy worker truncating its mutable file cannot
deliver `SIGBUS` to `plasmashell` through a live mapping.

The consumer validates and copies a frame into its own image before display.
It never paints directly from a slot that the producer may reuse. If the
producer hangs, exits, or corrupts the header, the consumer retains the last
validated image and reports a non-color-only status.

## Wire layout

All static integers are little-endian. Version 1 supports little-endian targets
with lock-free 64-bit atomics. The header is exactly 64 bytes:

| Offset | Size | Field | Requirement |
|---:|---:|---|---|
| 0 | 8 | magic | `KWEFRM1\0` |
| 8 | 4 | version | `1` |
| 12 | 4 | header bytes | `64` |
| 16 | 8 | total file bytes | exactly `64 + 2 × stride × height` |
| 24 | 4 | width | `1..8192` |
| 28 | 4 | height | `1..8192` |
| 32 | 4 | stride | exactly `width × 4` in v1 |
| 36 | 4 | pixel format | `1`, BGRA8888 premultiplied |
| 40 | 4 | slot count | exactly `2` |
| 44 | 4 | reserved | zero |
| 48 | 8 | generation | aligned atomic seqlock counter |
| 56 | 4 | active slot | aligned atomic, `0` or `1` |
| 60 | 4 | producer state | starting `1`, running `2`, stopping `3`, failed `4` |

Two tightly packed frame slots immediately follow the header. The entire frame
file is capped at 512 MiB. The producer creates a new file with `O_NOFOLLOW`,
`O_CLOEXEC`, and `0600`; it refuses to replace an existing path. The consumer
also opens with `O_NOFOLLOW`, checks the descriptor is a regular file, and
validates the descriptor size before reading or mapping it.

## Publish and snapshot algorithm

The producer increments generation to an odd number, writes the inactive slot,
publishes its slot/state, then increments generation to an even number with
release ordering. The consumer:

1. acquire-loads an even generation;
2. validates all header fields and selects the active slot;
3. copies the bounded pixels to a private image;
4. acquire-loads generation again;
5. accepts only equal even values, retrying at most eight times in Rust.

The Qt consumer polls at approximately 30 Hz and reads the selected slot into
one private image with a bounded positioned read between the two generation
checks. Short reads, including concurrent truncation, are rejected without
dereferencing mutable shared memory. If no accepted generation arrives for 1.5
seconds, it changes to `Frozen` but keeps the last good image. Invalid magic,
dimensions, format, slot, stride, or size changes to `Invalid` and also keeps
that image.

## What v1 does not do

- It is a CPU-copy fallback, not the normal high-performance path.
- It does not carry pointer input, damage rectangles, color-space metadata,
  fences, or multi-plane formats.
- It does not authenticate a renderer; the future daemon creates the private
  runtime directory and passes only a validated transport to each worker.
- It is not installed into Plasma in M1a. M1e reuses the standalone preview as
  the fault harness for the staged thin bridge; live installation remains a
  separately authorized gate.

DMA-BUF plus external synchronization will be a separate negotiated transport.
The shared-memory path remains mandatory for recovery and unsupported drivers.
