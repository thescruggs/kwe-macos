// SPDX-License-Identifier: GPL-3.0-or-later
//! OS-specific primitives behind one small, dependency-free surface.
//!
//! Linux keeps the exact behavior the daemon and workers had before this
//! crate existed (parent-death signal, `PR_SET_NO_NEW_PRIVS`, `pipe2`,
//! `SOCK_CLOEXEC`, `SO_PEERCRED`, XDG directories). macOS supplies the
//! closest Darwin equivalent for each primitive; where Darwin has no
//! equivalent the substitute and its weaker guarantee are documented on the
//! function. Inside a `pre_exec` closure nothing here allocates on the
//! success path; the parent-check failure path builds an `io::Error` with a
//! static message (a small allocation), exactly as the call sites did before
//! this crate existed.
//!
//! macOS port note (docs/macos/MacOS-Port-Plan.md, MP-2): this is the seam
//! that keeps every other crate free of `#[cfg(target_os)]` sprawl.

use std::io;
use std::os::fd::RawFd;
use std::path::PathBuf;

/// The `resource` parameter type of `setrlimit(2)` for this libc.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
pub type RlimitResource = libc::__rlimit_resource_t;
/// The `resource` parameter type of `setrlimit(2)` for this libc.
#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
pub type RlimitResource = libc::c_int;

/// Address-space limit resource. Enforced on Linux. On macOS `RLIMIT_AS`
/// exists in the headers but `setrlimit` REFUSES it with `EINVAL` (measured
/// on macOS 14) and the kernel would not enforce it anyway; callers must
/// skip it there (`address_space_limit_enforced`) and pair containment
/// with a resident-set watchdog.
pub const RLIMIT_AS: RlimitResource = libc::RLIMIT_AS as RlimitResource;
pub const RLIMIT_FSIZE: RlimitResource = libc::RLIMIT_FSIZE as RlimitResource;
pub const RLIMIT_NOFILE: RlimitResource = libc::RLIMIT_NOFILE as RlimitResource;
pub const RLIMIT_NPROC: RlimitResource = libc::RLIMIT_NPROC as RlimitResource;
pub const RLIMIT_CORE: RlimitResource = libc::RLIMIT_CORE as RlimitResource;

/// Whether `RLIMIT_AS` can be set and is enforced on this platform
/// (Linux yes; Darwin refuses the call).
pub const fn address_space_limit_enforced() -> bool {
    cfg!(target_os = "linux")
}

/// Child-side containment for a freshly forked worker, to run inside a
/// `Command::pre_exec` closure after `setpgid(0, 0)`.
///
/// Linux: arms `PR_SET_PDEATHSIG` with `death_signal`, verifies the parent
/// is still `expected_parent` (closing the race where the parent died
/// between fork and the prctl), then sets `PR_SET_NO_NEW_PRIVS`.
///
/// macOS: Darwin has neither prctl. The parent check still runs; the
/// parent-death cover is provided from the CHILD side instead by
/// [`guard_parent_exit`], which every kwe worker calls at startup. There is
/// no no-new-privs equivalent; workers never exec setuid binaries, and the
/// web renderer relies on the browser's own sandbox plus `sandbox-exec`.
///
/// # Safety
/// Must be called only between fork and exec. Calls only
/// async-signal-safe functions; allocates only on the parent-check failure
/// path (a static-message `io::Error`).
pub unsafe fn child_pre_exec(expected_parent: libc::pid_t, death_signal: libc::c_int) -> io::Result<()> {
    unsafe { child_pre_exec_with(expected_parent, death_signal, Containment::Full) }
}

/// How much of the Linux containment [`child_pre_exec`] applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Containment {
    /// Parent-death signal, parent check, and `PR_SET_NO_NEW_PRIVS`
    /// (renderers, inspector, shader helper, audio worker).
    Full,
    /// Parent-death signal and parent check only. Used for children that
    /// are not kwe code and may legitimately gain privileges at exec
    /// (the audio worker's `pw-record`, which some distributions ship with
    /// file capabilities for realtime scheduling).
    ParentOnly,
}

/// [`child_pre_exec`] with an explicit containment level.
///
/// # Safety
/// Same contract as [`child_pre_exec`].
pub unsafe fn child_pre_exec_with(
    expected_parent: libc::pid_t,
    death_signal: libc::c_int,
    containment: Containment,
) -> io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        // SAFETY: prctl with these constants takes no pointers.
        if unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, death_signal, 0, 0, 0) } != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = death_signal;
    // SAFETY: getppid takes no arguments and cannot fail.
    if unsafe { libc::getppid() } != expected_parent {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "parent exited before child exec",
        ));
    }
    #[cfg(target_os = "linux")]
    {
        if containment == Containment::Full {
            // SAFETY: prctl with these constants takes no pointers.
            if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0 {
                return Err(io::Error::last_os_error());
            }
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = containment;
    Ok(())
}

/// Worker-side parent-death cover. Call once at the top of every worker
/// `main`. Linux: a no-op, `PR_SET_PDEATHSIG` from [`child_pre_exec`]
/// already covers it. macOS: spawns a detached thread that waits on a
/// kqueue `EVFILT_PROC`/`NOTE_EXIT` filter for the parent pid and then
/// delivers `signal` to this process; if the parent is already gone
/// (reparented to launchd, ppid 1) the signal is delivered immediately, and
/// if kqueue is unavailable the thread polls `getppid` every 500 ms.
pub fn guard_parent_exit(signal: libc::c_int) {
    #[cfg(target_os = "linux")]
    let _ = signal;
    #[cfg(target_os = "macos")]
    {
        // SAFETY: getppid takes no arguments and cannot fail.
        let parent = unsafe { libc::getppid() };
        if parent <= 1 {
            // SAFETY: signalling our own pid with a valid signal number.
            unsafe { libc::kill(libc::getpid(), signal) };
            return;
        }
        let spawned = std::thread::Builder::new()
            .name("kwe-parent-guard".into())
            .spawn(move || {
                if !wait_parent_exit_kqueue(parent) {
                    loop {
                        std::thread::sleep(std::time::Duration::from_millis(500));
                        // SAFETY: getppid takes no arguments and cannot fail.
                        if unsafe { libc::getppid() } != parent {
                            break;
                        }
                    }
                }
                // SAFETY: signalling our own pid with a valid signal number.
                unsafe { libc::kill(libc::getpid(), signal) };
            });
        if let Err(error) = spawned {
            eprintln!("event=worker.parent_guard_unavailable detail={error}");
        }
    }
}

/// Blocks until `parent` exits. Returns false when kqueue could not be
/// armed (the caller then falls back to polling).
#[cfg(target_os = "macos")]
fn wait_parent_exit_kqueue(parent: libc::pid_t) -> bool {
    // SAFETY: kqueue takes no arguments.
    let queue = unsafe { libc::kqueue() };
    if queue < 0 {
        return false;
    }
    let change = libc::kevent {
        ident: parent as libc::uintptr_t,
        filter: libc::EVFILT_PROC,
        flags: libc::EV_ADD | libc::EV_ENABLE | libc::EV_ONESHOT,
        fflags: libc::NOTE_EXIT,
        data: 0,
        udata: std::ptr::null_mut(),
    };
    // SAFETY: `change` is a valid kevent; no event list is requested here.
    let rc = unsafe { libc::kevent(queue, &change, 1, std::ptr::null_mut(), 0, std::ptr::null()) };
    if rc < 0 {
        // SAFETY: queue is a valid descriptor we own.
        unsafe { libc::close(queue) };
        // The parent may have exited between getppid and kevent (ESRCH).
        // SAFETY: getppid takes no arguments and cannot fail.
        return unsafe { libc::getppid() } != parent;
    }
    let mut event: libc::kevent = unsafe { std::mem::zeroed() };
    loop {
        // SAFETY: `event` is a valid out-buffer for exactly one kevent.
        let rc = unsafe { libc::kevent(queue, std::ptr::null(), 0, &mut event, 1, std::ptr::null()) };
        if rc > 0 {
            break;
        }
        if rc < 0 && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
            continue;
        }
        // SAFETY: getppid takes no arguments and cannot fail.
        if unsafe { libc::getppid() } != parent {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    // SAFETY: queue is a valid descriptor we own.
    unsafe { libc::close(queue) };
    true
}

/// An anonymous pipe with `FD_CLOEXEC` set on both ends
/// (`pipe2(O_CLOEXEC)` on Linux; `pipe` + `fcntl` on macOS, where the two
/// steps are not atomic against a concurrent fork in another thread —
/// acceptable because every kwe spawn goes through a single supervisor
/// thread).
pub fn pipe_cloexec() -> io::Result<[libc::c_int; 2]> {
    let mut fds = [0 as libc::c_int; 2];
    #[cfg(target_os = "linux")]
    {
        // SAFETY: fds is a valid 2-element buffer for pipe2 to fill.
        if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        // SAFETY: fds is a valid 2-element buffer for pipe to fill.
        if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        for fd in fds {
            if let Err(error) = set_cloexec(fd) {
                // SAFETY: both descriptors were just created and are owned here.
                unsafe {
                    libc::close(fds[0]);
                    libc::close(fds[1]);
                }
                return Err(error);
            }
        }
    }
    Ok(fds)
}

/// A connected `AF_UNIX`/`SOCK_STREAM` pair with `FD_CLOEXEC` on both ends.
pub fn socketpair_stream_cloexec() -> io::Result<[libc::c_int; 2]> {
    let mut fds = [0 as libc::c_int; 2];
    #[cfg(target_os = "linux")]
    let kind = libc::SOCK_STREAM | libc::SOCK_CLOEXEC;
    #[cfg(not(target_os = "linux"))]
    let kind = libc::SOCK_STREAM;
    // SAFETY: fds is a valid 2-element buffer for socketpair to fill.
    if unsafe { libc::socketpair(libc::AF_UNIX, kind, 0, fds.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    #[cfg(not(target_os = "linux"))]
    for fd in fds {
        if let Err(error) = set_cloexec(fd) {
            // SAFETY: both descriptors were just created and are owned here.
            unsafe {
                libc::close(fds[0]);
                libc::close(fds[1]);
            }
            return Err(error);
        }
    }
    Ok(fds)
}

/// Sets `FD_CLOEXEC` on an open descriptor.
pub fn set_cloexec(fd: RawFd) -> io::Result<()> {
    // SAFETY: F_GETFD/F_SETFD on a caller-provided descriptor.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: as above.
    if unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Credentials of the peer of a connected Unix stream socket.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PeerCredentials {
    pub pid: u32,
    pub uid: u32,
}

/// Peer credentials of a connected Unix stream socket: `SO_PEERCRED` on
/// Linux; `getpeereid` for the uid plus `LOCAL_PEERPID` for the pid on
/// macOS. `None` when the query fails.
pub fn peer_credentials(fd: RawFd) -> Option<PeerCredentials> {
    #[cfg(target_os = "linux")]
    {
        let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
        let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
        // SAFETY: `cred` is a valid mutable ucred buffer and `len` its bound.
        let rc = unsafe {
            libc::getsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                (&mut cred as *mut libc::ucred).cast(),
                &mut len,
            )
        };
        (rc == 0).then(|| PeerCredentials {
            pid: cred.pid as u32,
            uid: cred.uid,
        })
    }
    #[cfg(target_os = "macos")]
    {
        let mut uid: libc::uid_t = 0;
        let mut gid: libc::gid_t = 0;
        // SAFETY: valid out-pointers for getpeereid.
        if unsafe { libc::getpeereid(fd, &mut uid, &mut gid) } != 0 {
            return None;
        }
        let mut pid: libc::pid_t = 0;
        let mut len = std::mem::size_of::<libc::pid_t>() as libc::socklen_t;
        // SAFETY: `pid` is a valid mutable buffer and `len` its bound.
        let rc = unsafe {
            libc::getsockopt(
                fd,
                libc::SOL_LOCAL,
                libc::LOCAL_PEERPID,
                (&mut pid as *mut libc::pid_t).cast(),
                &mut len,
            )
        };
        Some(PeerCredentials {
            pid: if rc == 0 { pid as u32 } else { 0 },
            uid,
        })
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = fd;
        None
    }
}

/// Resident set size (bytes) of a live process, when the platform reports
/// it: `proc_pid_rusage` on macOS, `/proc/<pid>/statm` on Linux. The macOS
/// daemon uses it as the address-space budget's substitute because Darwin
/// refuses `RLIMIT_AS`; Linux keeps the rlimit and does not consult this.
pub fn resident_set_bytes(pid: libc::pid_t) -> Option<u64> {
    #[cfg(target_os = "macos")]
    {
        let mut info: libc::rusage_info_v2 = unsafe { std::mem::zeroed() };
        // SAFETY: `info` is a valid, writable rusage_info_v2 buffer and the
        // flavor names exactly that layout (Apple's rusage_info_t is `void*`,
        // hence the pointer cast).
        let rc = unsafe {
            libc::proc_pid_rusage(
                pid,
                libc::RUSAGE_INFO_V2,
                (&mut info as *mut libc::rusage_info_v2).cast::<libc::rusage_info_t>(),
            )
        };
        (rc == 0).then_some(info.ri_resident_size)
    }
    #[cfg(target_os = "linux")]
    {
        let statm = std::fs::read_to_string(format!("/proc/{pid}/statm")).ok()?;
        let pages: u64 = statm.split_whitespace().nth(1)?.parse().ok()?;
        // SAFETY: sysconf with a valid name has no preconditions.
        let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        (page_size > 0).then(|| pages * page_size as u64)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = pid;
        None
    }
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// Per-user runtime directory for sockets. Linux: `XDG_RUNTIME_DIR`
/// (required; `None` when unset). macOS: `XDG_RUNTIME_DIR` when set (smoke
/// scripts), otherwise `~/Library/Application Support` — the same
/// directory Qt's `QStandardPaths::RuntimeLocation` resolves to, so the
/// manager and the daemon agree on the socket path without configuration.
pub fn runtime_dir() -> Option<PathBuf> {
    if let Some(value) = std::env::var_os("XDG_RUNTIME_DIR") {
        return Some(PathBuf::from(value));
    }
    #[cfg(target_os = "macos")]
    {
        return home_dir().map(|home| home.join("Library/Application Support"));
    }
    #[cfg(not(target_os = "macos"))]
    {
        None
    }
}

/// Daemon state directory. Linux: `XDG_STATE_HOME/kwe` or
/// `~/.local/state/kwe`. macOS: `XDG_STATE_HOME/kwe` when set, otherwise
/// `~/Library/Application Support/kwe/state`.
pub fn state_dir() -> Option<PathBuf> {
    if let Some(value) = std::env::var_os("XDG_STATE_HOME") {
        return Some(PathBuf::from(value).join("kwe"));
    }
    let home = home_dir()?;
    #[cfg(target_os = "macos")]
    {
        Some(home.join("Library/Application Support/kwe/state"))
    }
    #[cfg(not(target_os = "macos"))]
    {
        Some(home.join(".local/state/kwe"))
    }
}

/// User data directory root (the parent of `kwe/`). Linux:
/// `XDG_DATA_HOME` or `~/.local/share`. macOS: `XDG_DATA_HOME` when set,
/// otherwise `~/Library/Application Support` (Qt's `GenericDataLocation`).
pub fn data_home() -> Option<PathBuf> {
    if let Some(value) = std::env::var_os("XDG_DATA_HOME") {
        return Some(PathBuf::from(value));
    }
    let home = home_dir()?;
    #[cfg(target_os = "macos")]
    {
        Some(home.join("Library/Application Support"))
    }
    #[cfg(not(target_os = "macos"))]
    {
        Some(home.join(".local/share"))
    }
}

/// Default Steam installation roots for this platform, in probe order.
/// Linux: `~/.local/share/Steam`, `~/.steam/steam`, `~/.steam/root`.
/// macOS: `~/Library/Application Support/Steam`. Callers prepend
/// `STEAM_ROOT`.
pub fn default_steam_roots(home: &std::path::Path) -> Vec<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        vec![home.join("Library/Application Support/Steam")]
    }
    #[cfg(not(target_os = "macos"))]
    {
        vec![
            home.join(".local/share/Steam"),
            home.join(".steam/steam"),
            home.join(".steam/root"),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pipe_is_cloexec_on_both_ends() {
        let [read_fd, write_fd] = pipe_cloexec().unwrap();
        for fd in [read_fd, write_fd] {
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
            assert!(flags >= 0);
            assert_ne!(flags & libc::FD_CLOEXEC, 0);
            unsafe { libc::close(fd) };
        }
    }

    #[test]
    fn socketpair_is_cloexec_on_both_ends() {
        let [a, b] = socketpair_stream_cloexec().unwrap();
        for fd in [a, b] {
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
            assert!(flags >= 0);
            assert_ne!(flags & libc::FD_CLOEXEC, 0);
            unsafe { libc::close(fd) };
        }
    }

    #[test]
    fn peer_credentials_report_our_own_uid_over_a_socketpair() {
        let [a, b] = socketpair_stream_cloexec().unwrap();
        let credentials = peer_credentials(a).expect("credentials");
        assert_eq!(credentials.uid, unsafe { libc::getuid() });
        assert_eq!(credentials.pid, std::process::id());
        unsafe {
            libc::close(a);
            libc::close(b);
        }
    }

    /// Runs each containment step the daemon applies between fork and exec
    /// in its own child, so a platform that refuses one names it. (macOS
    /// CI, 2026-09-04: every worker spawn failed with std's EINVAL
    /// placeholder, which hides which pre_exec step returned an error.)
    /// Assumes the ambient hard limits allow NOFILE 256 and NPROC 1024, as
    /// the daemon's defaults do; a tighter container would fail here first,
    /// which is the intended early warning.
    #[test]
    fn each_containment_step_succeeds_in_a_child() {
        use std::os::unix::process::CommandExt;
        use std::process::Command;

        fn set_limit(resource: RlimitResource, value: u64) -> io::Result<()> {
            let limit = libc::rlimit {
                rlim_cur: value as libc::rlim_t,
                rlim_max: value as libc::rlim_t,
            };
            if unsafe { libc::setrlimit(resource, &limit) } != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }
        let parent = std::process::id() as libc::pid_t;
        let mib = 1024_u64 * 1024;
        type Step = Box<dyn Fn() -> io::Result<()> + Send + Sync>;
        let steps: Vec<(&str, Step)> = vec![
            (
                "setpgid",
                Box::new(|| {
                    if unsafe { libc::setpgid(0, 0) } != 0 {
                        return Err(io::Error::last_os_error());
                    }
                    Ok(())
                }),
            ),
            (
                "child_pre_exec",
                Box::new(move || unsafe { child_pre_exec(parent, libc::SIGKILL) }),
            ),
            (
                "RLIMIT_AS",
                Box::new(move || {
                    if address_space_limit_enforced() {
                        set_limit(RLIMIT_AS, 4096 * mib)
                    } else {
                        Ok(())
                    }
                }),
            ),
            ("RLIMIT_FSIZE", Box::new(move || set_limit(RLIMIT_FSIZE, 160 * mib))),
            ("RLIMIT_NOFILE", Box::new(|| set_limit(RLIMIT_NOFILE, 256))),
            ("RLIMIT_NPROC", Box::new(|| set_limit(RLIMIT_NPROC, 1024))),
            ("RLIMIT_CORE", Box::new(|| set_limit(RLIMIT_CORE, 0))),
        ];
        let mut failures = Vec::new();
        for (name, step) in steps {
            let mut command = Command::new("true");
            // SAFETY: the step calls only async-signal-safe libc functions.
            unsafe {
                command.pre_exec(move || step());
            }
            match command.status() {
                Ok(status) if status.success() => {}
                Ok(status) => failures.push(format!("{name}: child exited {status}")),
                Err(error) => failures.push(format!(
                    "{name}: spawn error {error} (raw {:?})",
                    error.raw_os_error()
                )),
            }
        }
        assert!(failures.is_empty(), "containment steps refused: {failures:?}");
    }

    #[test]
    fn resident_set_of_this_process_is_reported() {
        let rss = resident_set_bytes(std::process::id() as libc::pid_t).expect("rss");
        assert!(rss > 64 * 1024, "implausible rss {rss}");
        assert_eq!(resident_set_bytes(i32::MAX), None);
    }

    #[test]
    fn steam_roots_are_platform_specific() {
        let roots = default_steam_roots(std::path::Path::new("/home/u"));
        assert!(!roots.is_empty());
        if cfg!(target_os = "macos") {
            assert_eq!(roots[0], PathBuf::from("/home/u/Library/Application Support/Steam"));
        } else {
            assert_eq!(roots[0], PathBuf::from("/home/u/.local/share/Steam"));
        }
    }
}
