# ADR 0002: double-buffered mmap fallback

- Status: accepted for Alpha M1a
- Date: 2026-08-16

## Decision

Use a versioned, double-buffered shared file as the first and permanent fallback
frame transport. Renderers stay in independent processes and may map the file.
Consumers validate a 64-byte fixed header, copy only a stable
seqlock-protected slot, and retain the last validated private image after
transport failure. M1e refines the Plasma-facing consumer to use bounded
positioned reads instead of mapping renderer-mutable storage, preventing a
worker truncation from delivering `SIGBUS` to `plasmashell`.

The protocol is original project code. Waywallen Display is an idea-level
reference for the broader external-daemon/thin-display boundary and future
DMA-BUF direction; this wire layout and implementation do not reproduce its
protocol or source.

## Consequences

The CPU copy costs memory bandwidth, but it is simple to exercise under faults
and works without GPU sharing support. Two 3840×2160 BGRA slots consume about
63.3 MiB plus the consumer's private image. A hard 512 MiB mapping limit and
8192-pixel dimension limit prevent unbounded allocations.

DMA-BUF will later be preferred when modifier and synchronization negotiation
succeeds. Failure to negotiate must fall back here rather than failing the
desktop or moving rendering into `plasmashell`.
