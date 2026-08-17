# Alpha M1d-A renderer resource containment

M1d-A adds a second containment layer around every supervised renderer. The
daemon now applies Linux process limits before `exec`, reports the effective
policy in renderer status, and classifies a controlled allocation denial as a
stable `resource_limit` failure. A bad candidate still consumes only its own
retry budget and cannot replace a healthy active wallpaper.

## Effective policy

The production defaults are:

- 4096 MiB virtual address space per renderer;
- 160 MiB maximum output file, above frame protocol v1's 128 MiB mapping cap;
- 256 open file descriptors;
- 1024 UID-scoped processes inherited as `RLIMIT_NPROC`;
- zero-byte core dumps.

The address-space limit is deliberately higher than the aggregate resident
memory budget because Vulkan drivers commonly reserve large virtual ranges.
It is configurable with `--renderer-address-space-mib`; descriptor and process
ceilings are configurable with `--renderer-open-files` and
`--renderer-processes`. Invalid or unbounded values are rejected at startup.

The packaged systemd user unit adds an aggregate 768 MiB memory-pressure
threshold, 1 GiB hard resident-memory limit, 256 MiB swap limit, 300% CPU
quota, and 64-task ceiling. Runtime and state directories are explicitly
owned by the service. The unit intentionally does not enable
`PrivateDevices=` or `MemoryDenyWriteExecute=` because Vulkan needs DRM render
nodes and graphics drivers may need executable shader mappings.

## Safe verification

Run the complete isolated supervisor matrix:

```sh
scripts/smoke-supervisor.sh
```

The script launches its own daemon and workers beneath a temporary directory;
it does not install the user unit or communicate with Plasma. For the
memory-pressure case it lowers the renderer virtual-address limit to 384 MiB,
asks a candidate to reserve 1024 MiB, and verifies all three bounded retries
are reported as `resource_limit`. It also verifies the original active PID and
frame path survive the rollback.

After staging or installing the package, verify its final filesystem paths:

```sh
systemd-analyze --man=no verify --user packaging/systemd/kwe-daemon.service
```

Source-tree QA verifies the same directives with `ExecStart` pointed at the
built local daemon. Before packaging, direct verification of the shipped unit
correctly reports that `/usr/bin/kwe-daemon` is not installed yet.

This synthetic test exercises deterministic allocation denial, not a kernel
OOM kill. Kernel/cgroup OOM validation belongs in an isolated login or VM so
the development desktop is never deliberately placed under memory pressure.

## Remaining M1d work

Normalized pointer position is implemented in M1d-B, and M1e now provides the
thin Plasma display bridge in isolated staging. Mouse buttons, manager-driven
installation/output assignment, safe-mode restoration, and the authorized live
Plasma survival test remain disabled. Renderer performance work remains
deferred until after the initial feature release.
