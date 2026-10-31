//! Best-effort filesystem teardown before guest poweroff.
//!
//! agentd is effectively init: nothing else unmounts filesystems before the kernel powers off, so without this pass every graceful stop leaves block-backed
//! filesystems (notably the OCI overlay upper) with a dirty jbd2 journal (`EXT4_FEATURE_INCOMPAT_RECOVER` set) that the next mount or offline tool must replay.
//! Teardown is strictly best-effort: a sandbox must always power off, so every step logs and continues on failure, and the whole pass is bounded by a deadline.

use std::fs::File;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStringExt;
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use nix::mount::{self, MntFlags, MsFlags};

//--------------------------------------------------------------------------------------------------
// Constants
//--------------------------------------------------------------------------------------------------

/// Budget for the teardown pass after the processes are gone.
///
/// With [`GUEST_TERM_GRACE`] it must stay comfortably inside `microsandbox_protocol::NORMAL_SHUTDOWN_FLUSH_TIMEOUT`: a host lifetime policy
/// hard-kills the VMM when that window expires, so a slow teardown would turn a graceful stop into the very unclean shutdown it is trying to prevent.
const TEARDOWN_DEADLINE: Duration = Duration::from_millis(1000);

/// How long the guest's processes get to exit after their SIGTERM before the rest are killed.
const GUEST_TERM_GRACE: Duration = microsandbox_protocol::GUEST_TERM_GRACE;

const _: () = assert!(
    GUEST_TERM_GRACE.as_millis() + TEARDOWN_DEADLINE.as_millis()
        < microsandbox_protocol::NORMAL_SHUTDOWN_FLUSH_TIMEOUT.as_millis(),
    "the grace and the teardown must fit in the host's shutdown window",
);

/// Most processes still running at the end of [`GUEST_TERM_GRACE`] that the log names.
const LEFT_NAMED: usize = 16;

/// Sub-budget for waiting on killed processes to exit and release their open files.
const PROCESS_REAP_BUDGET: Duration = Duration::from_millis(300);

/// Poll interval while reaping killed processes.
const PROCESS_REAP_POLL: Duration = Duration::from_millis(5);

/// Filesystem types with no backing store worth flushing or detaching at poweroff.
const PSEUDO_FSTYPES: &[&str] = &[
    "autofs",
    "binfmt_misc",
    "bpf",
    "cgroup",
    "cgroup2",
    "configfs",
    "debugfs",
    "devpts",
    "devtmpfs",
    "efivarfs",
    "fusectl",
    "hugetlbfs",
    "mqueue",
    "nsfs",
    "proc",
    "pstore",
    "ramfs",
    "rootfs",
    "securityfs",
    "selinuxfs",
    "sysfs",
    "tmpfs",
    "tracefs",
];

/// Directory fd pinned to the root of the OCI overlay upper filesystem.
///
/// After the root pivot (`MS_MOVE` + chroot) the upper ext4 mount lives under the old, shadowed root: it has no reachable path and does not appear in
/// `/proc/self/mounts`, so the only way to address it at shutdown is through a fd captured at mount time, resolved via `/proc/self/fd/N`.
static UPPER_FS_DIR: OnceLock<File> = OnceLock::new();

//--------------------------------------------------------------------------------------------------
// Types
//--------------------------------------------------------------------------------------------------

/// One real (non-pseudo) mount from `/proc/self/mounts`.
#[derive(Debug, PartialEq, Eq)]
struct MountPoint {
    path: PathBuf,
    fstype: String,
}

//--------------------------------------------------------------------------------------------------
// Functions
//--------------------------------------------------------------------------------------------------

/// Captures a fd to the OCI overlay upper filesystem root so teardown can still reach it after the root pivot hides its path.
///
/// Best effort: on failure the upper simply keeps today's behavior (dirty journal at stop, replayed on next use).
pub fn register_upper_fs(path: &str) {
    match File::open(path) {
        Ok(file) => {
            let _ = UPPER_FS_DIR.set(file);
        }
        Err(err) => eprintln!("agentd: teardown: failed to pin upper fs dir {path}: {err}"),
    }
}

/// Flushes and tears down filesystems ahead of guest poweroff so block-backed mounts reach a clean terminal state (unmounted or read-only with the
/// journal checkpointed).
///
/// `end_remaining_processes` should be true only when agentd is PID 1 and about to power the kernel off: every other process then gets a SIGTERM and
/// up to [`GUEST_TERM_GRACE`] to exit, and is killed after that; lingering processes with write-open files would otherwise make every read-only
/// remount fail with `EBUSY`. Never fails — the caller must proceed to poweroff regardless.
pub fn teardown_filesystems(end_remaining_processes: bool) {
    // SAFETY: sync(2) has no failure modes and takes no arguments.
    unsafe { libc::sync() };

    if end_remaining_processes {
        end_processes();
    }

    let deadline = Instant::now() + TEARDOWN_DEADLINE;

    match std::fs::read_to_string("/proc/self/mounts") {
        Ok(table) => {
            for mount_point in teardown_plan(&table) {
                if Instant::now() >= deadline {
                    eprintln!("agentd: teardown: deadline reached, skipping remaining mounts");
                    break;
                }
                detach_or_remount_readonly(&mount_point.path);
            }
        }
        Err(err) => eprintln!("agentd: teardown: failed to read /proc/self/mounts: {err}"),
    }

    remount_upper_readonly(deadline);

    // SAFETY: sync(2) has no failure modes and takes no arguments.
    unsafe { libc::sync() };
}

/// Parses `/proc/self/mounts` into the teardown order: real filesystems only, reversed so children are handled before their parents and the root
/// mount comes last.
fn teardown_plan(proc_mounts: &str) -> Vec<MountPoint> {
    let mut plan: Vec<MountPoint> = proc_mounts
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let _device = fields.next()?;
            let path = fields.next()?;
            let fstype = fields.next()?;
            if PSEUDO_FSTYPES.contains(&fstype) {
                return None;
            }
            Some(MountPoint {
                path: unescape_mount_path(path),
                fstype: fstype.to_string(),
            })
        })
        .collect();
    plan.reverse();
    plan
}

/// Decodes the octal escapes (`\040` for space, etc.) the kernel uses for special characters in `/proc/mounts` paths.
fn unescape_mount_path(escaped: &str) -> PathBuf {
    let bytes = escaped.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 3 < bytes.len() {
            let octal = &bytes[i + 1..i + 4];
            if octal.iter().all(|b| (b'0'..=b'7').contains(b)) {
                let value = octal
                    .iter()
                    .fold(0u32, |acc, b| acc * 8 + u32::from(b - b'0'));
                if let Ok(byte) = u8::try_from(value) {
                    out.push(byte);
                    i += 4;
                    continue;
                }
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    PathBuf::from(std::ffi::OsString::from_vec(out))
}

/// Unmounts a filesystem, falling back to a read-only remount when it is busy.
///
/// The busy case is expected for the overlay root (it hosts the running agentd) — remounting its superblock read-only is the correct terminal state,
/// and it also stops new writes from reaching the upper filesystem underneath.
fn detach_or_remount_readonly(path: &Path) {
    if mount::umount2(path, MntFlags::empty()).is_ok() {
        return;
    }
    if let Err(err) = mount::mount(
        None::<&str>,
        path,
        None::<&str>,
        MsFlags::MS_REMOUNT | MsFlags::MS_RDONLY,
        None::<&str>,
    ) {
        eprintln!(
            "agentd: teardown: {} left mounted read-write: {err}",
            path.display()
        );
    }
}

/// Remounts the pinned upper filesystem read-only via `/proc/self/fd`.
///
/// Remount — not unmount — is deliberate: overlayfs holds a private clone of the upper mount, so detaching our mount would not release the superblock
/// or flush anything. `MS_REMOUNT | MS_RDONLY` acts on the superblock itself, which makes ext4 checkpoint the jbd2 journal and clear
/// `EXT4_FEATURE_INCOMPAT_RECOVER`. Must run after the overlay root has gone read-only so no writers remain on the upper.
fn remount_upper_readonly(deadline: Instant) {
    let Some(file) = UPPER_FS_DIR.get() else {
        return;
    };
    if Instant::now() >= deadline {
        eprintln!("agentd: teardown: deadline reached, upper fs left mounted read-write");
        return;
    }
    let target = format!("/proc/self/fd/{}", file.as_raw_fd());
    if let Err(err) = mount::mount(
        None::<&str>,
        target.as_str(),
        None::<&str>,
        MsFlags::MS_REMOUNT | MsFlags::MS_RDONLY,
        None::<&str>,
    ) {
        eprintln!("agentd: teardown: upper fs left mounted read-write: {err}");
    }
}

/// Ends every other process the way a machine shuts down: one SIGTERM each (and a SIGCONT, so a stopped process can act on it), up to
/// [`GUEST_TERM_GRACE`] for them to exit, then SIGKILL for what is left.
///
/// The SIGTERM goes to every process, not only the exec sessions: a daemon that left the command that started it (an image's entrypoint such as
/// dockerd) loses state when it is killed outright. Each process gets one SIGTERM only, because some count them (dockerd quits without cleanup at the
/// third). `kill(-1)` never signals the calling process or PID 1, so agentd survives. As PID 1, agentd is the parent of every orphan, so "no child
/// left" means no process is left.
fn end_processes() {
    // SAFETY: kill(2) with pid -1 is well-defined; failure (e.g. no processes left) is benign.
    unsafe {
        libc::kill(-1, libc::SIGTERM);
        libc::kill(-1, libc::SIGCONT);
    }
    if reap_children_until(Instant::now() + GUEST_TERM_GRACE) {
        return;
    }
    eprintln!(
        "agentd: teardown: still running {}s after SIGTERM, killing: {}",
        GUEST_TERM_GRACE.as_secs(),
        left_processes()
    );
    // SAFETY: as above.
    unsafe { libc::kill(-1, libc::SIGKILL) };
    reap_children_until(Instant::now() + PROCESS_REAP_BUDGET);
}

/// Reaps exited children until none is left (`true`) or `deadline` passes with some still running (`false`).
fn reap_children_until(deadline: Instant) -> bool {
    loop {
        // SAFETY: waitpid(2) with WNOHANG and a null status pointer is well-defined.
        let ret = unsafe { libc::waitpid(-1, ptr::null_mut(), libc::WNOHANG) };
        if ret > 0 {
            continue;
        }
        if ret == 0 {
            // Children remain but none exited yet.
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(PROCESS_REAP_POLL);
            continue;
        }
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::EINTR) {
            continue;
        }
        // ECHILD: no child is left (another reaper may have collected some of them).
        return true;
    }
}

/// The user processes still running, as `<pid> <comm>` pairs (at most [`LEFT_NAMED`]), for the log.
fn left_processes() -> String {
    let Ok(dir) = std::fs::read_dir("/proc") else {
        return "(cannot read /proc)".to_owned();
    };
    let own = std::process::id();
    let mut left: Vec<String> = dir
        .flatten()
        .filter_map(|entry| entry.file_name().to_str()?.parse::<u32>().ok())
        .filter(|&pid| pid != own && pid != 1)
        .filter_map(|pid| {
            let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
            user_process_name(&stat).map(|comm| format!("{pid} {comm}"))
        })
        .collect();
    left.sort();
    let more = left.len().saturating_sub(LEFT_NAMED);
    left.truncate(LEFT_NAMED);
    let mut text = left.join(", ");
    if more > 0 {
        text.push_str(&format!(" and {more} more"));
    }
    text
}

/// The command name from a `/proc/<pid>/stat` line, or `None` for a kernel thread, a zombie or a line it cannot read.
fn user_process_name(stat: &str) -> Option<&str> {
    const PF_KTHREAD: u64 = 0x0020_0000;
    let open = stat.find('(')?;
    let close = stat.rfind(')')?;
    let comm = stat.get(open + 1..close)?;
    // After the name: state, ppid, pgrp, session, tty_nr, tpgid, flags.
    let mut fields = stat.get(close + 1..)?.split_whitespace();
    let state = fields.next()?;
    let flags: u64 = fields.nth(5)?.parse().ok()?;
    (state != "Z" && flags & PF_KTHREAD == 0).then_some(comm)
}

//--------------------------------------------------------------------------------------------------
// Tests
//--------------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// A realistic post-pivot guest mount table: overlay root, pseudo filesystems, the /.msb virtiofs runtime share, a virtiofs dir volume, an ext4
    /// disk volume, and tmpfs mounts.
    const GUEST_MOUNTS: &str = "\
overlay / overlay rw,relatime,lowerdir=/.msb/rootfs/lower,upperdir=/.msb/rootfs/upperfs/upper,workdir=/.msb/rootfs/upperfs/work 0 0
devtmpfs /dev devtmpfs rw,relatime,size=100k 0 0
proc /proc proc rw,nosuid,nodev,noexec,relatime 0 0
sysfs /sys sysfs rw,nosuid,nodev,noexec,relatime 0 0
cgroup2 /sys/fs/cgroup cgroup2 rw,nosuid,nodev,noexec,relatime 0 0
devpts /dev/pts devpts rw,nosuid,noexec,relatime 0 0
tmpfs /dev/shm tmpfs rw,nosuid,noexec,relatime 0 0
msb_runtime /.msb virtiofs rw,relatime 0 0
vol0 /data virtiofs rw,relatime 0 0
/dev/vdc /mnt/disk ext4 rw,relatime 0 0
tmpfs /tmp tmpfs rw,nosuid,nodev,relatime,mode=1777 0 0
";

    #[test]
    fn plan_filters_pseudo_filesystems_and_reverses_mount_order() {
        let plan = teardown_plan(GUEST_MOUNTS);
        let paths: Vec<&Path> = plan.iter().map(|m| m.path.as_path()).collect();
        assert_eq!(
            paths,
            vec![
                Path::new("/mnt/disk"),
                Path::new("/data"),
                Path::new("/.msb"),
                Path::new("/"),
            ]
        );
    }

    #[test]
    fn plan_orders_root_last_so_upper_remount_follows_overlay() {
        // The upper ext4 remount only succeeds once the overlay root is read-only, so the root mount must be the final entry of the walk.
        let plan = teardown_plan(GUEST_MOUNTS);
        let last = plan.last().expect("plan must not be empty");
        assert_eq!(last.path, Path::new("/"));
        assert_eq!(last.fstype, "overlay");
    }

    #[test]
    fn plan_unescapes_octal_mount_paths() {
        let plan = teardown_plan("/dev/vdc /mnt/my\\040disk ext4 rw,relatime 0 0\n");
        assert_eq!(plan[0].path, Path::new("/mnt/my disk"));
    }

    #[test]
    fn plan_keeps_literal_backslashes_that_are_not_octal_escapes() {
        let plan = teardown_plan(
            "/dev/vdc /mnt/we\\134ird ext4 rw 0 0\n/dev/vdd /mnt/tail\\04 ext4 rw 0 0\n",
        );
        assert_eq!(plan[1].path, Path::new("/mnt/we\\ird"));
        assert_eq!(plan[0].path, Path::new("/mnt/tail\\04"));
    }

    #[test]
    fn plan_skips_malformed_lines() {
        let plan = teardown_plan("garbage\n\n/dev/vda\n/dev/vdb /ok ext4 rw 0 0\n");
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].path, Path::new("/ok"));
    }

    #[test]
    fn a_user_process_is_named_by_its_stat_line() {
        // dockerd, sleeping, flags 0x400100 (PF_FORKNOEXEC | PF_RANDOMIZE); the name may hold spaces and parentheses.
        let stat = "412 (dockerd) S 1 412 412 0 -1 4194560 3012 0 0 0 31 18 0 0 20 0 14 0 214 1838108672 9984";
        assert_eq!(user_process_name(stat), Some("dockerd"));
        let odd = "77 (my (odd) name) R 1 77 77 0 -1 4194304 0 0 0 0 0 0 0 0 20 0 1 0 5 0 0";
        assert_eq!(user_process_name(odd), Some("my (odd) name"));
    }

    #[test]
    fn kernel_threads_zombies_and_bad_lines_are_not_user_processes() {
        // kworker: PF_KTHREAD (0x200000) in flags 0x4208060.
        let kthread = "23 (kworker/0:1) I 2 0 0 0 -1 69238880 0 0 0 0 0 0 0 0 20 0 1 0 9 0 0";
        assert_eq!(user_process_name(kthread), None);
        let zombie = "90 (sh) Z 1 90 90 0 -1 4194316 0 0 0 0 0 0 0 0 20 0 1 0 50 0 0";
        assert_eq!(user_process_name(zombie), None);
        assert_eq!(user_process_name("90 (sh) S 1"), None);
        assert_eq!(user_process_name("garbage"), None);
        assert_eq!(user_process_name(""), None);
    }

    #[test]
    fn the_grace_and_the_teardown_fit_in_the_host_window() {
        assert_eq!(GUEST_TERM_GRACE, Duration::from_secs(10));
        assert!(
            GUEST_TERM_GRACE + TEARDOWN_DEADLINE
                < microsandbox_protocol::NORMAL_SHUTDOWN_FLUSH_TIMEOUT
        );
    }
}
