//! Who is at the other end of one of the hub's sockets: a process the daemon started itself (a
//! program the agent ran) must not speak as the owner or as a device.

use super::*;

/// How far up the process tree a peer is traced.
pub(super) const MAX_ANCESTORS: usize = 64;

/// The parent of `pid`, asked of the system directly: a connection may trace up to
/// `MAX_ANCESTORS` of them, which as `ps` processes would be as many programs started.
#[cfg(target_os = "macos")]
pub(super) fn parent_of(pid: u32) -> Option<u32> {
    let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::zeroed();
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    // SAFETY: the buffer is a zeroed `proc_bsdinfo` of the size passed, which is what
    // PROC_PIDTBSDINFO fills; the result is read only when the whole struct was written.
    let written =
        unsafe { libc::proc_pidinfo(pid as libc::c_int, libc::PROC_PIDTBSDINFO, 0, info.as_mut_ptr().cast(), size) };
    if written != size {
        return None;
    }
    // SAFETY: fully written by the call above.
    Some(unsafe { info.assume_init() }.pbi_ppid)
}

/// The parent of `pid`, from `/proc/<pid>/stat`: the field after the state, which follows the
/// last `)` (a program's name may hold spaces and parentheses).
#[cfg(target_os = "linux")]
pub(super) fn parent_of(pid: u32) -> Option<u32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    stat.rsplit_once(')')?.1.split_whitespace().nth(1)?.parse().ok()
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub(super) fn parent_of(pid: u32) -> Option<u32> {
    let output = std::process::Command::new("ps").args(["-o", "ppid=", "-p", &pid.to_string()]).output().ok()?;
    String::from_utf8_lossy(&output.stdout).trim().parse().ok()
}

/// Whether `pid` was started, however indirectly, by `ancestor`.
pub(super) fn descends(pid: u32, ancestor: u32, parent_of: impl Fn(u32) -> Option<u32>) -> bool {
    let mut current = pid;
    for _ in 0..MAX_ANCESTORS {
        match parent_of(current) {
            Some(parent) if parent == ancestor => return true,
            Some(parent) if parent > 1 && parent != current => current = parent,
            _ => return false,
        }
    }
    false
}

/// The process at the other end of `stream`, when the system says.
pub(super) fn peer_of(stream: &UnixStream) -> Option<u32> {
    stream.peer_cred().ok().and_then(|cred| cred.pid()).and_then(|pid| u32::try_from(pid).ok())
}

/// Whether the process at the other end of `stream` was started, however indirectly, by this
/// one: a program the agent ran, which must not speak as the owner or as a device.
pub(crate) async fn started_by_this_process(stream: &UnixStream) -> bool {
    let own = std::process::id();
    match peer_of(stream) {
        Some(pid) if pid != own => {
            tokio::task::spawn_blocking(move || descends(pid, own, parent_of)).await.unwrap_or(false)
        }
        _ => false,
    }
}
