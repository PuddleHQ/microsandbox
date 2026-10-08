//! Keeps the sandbox process out of its creator's terminal and console.
//!
//! The sandbox process hosts the VMM, so an interrupt meant for the creator must not reach it:
//! a terminal Ctrl-C (SIGINT to the foreground process group), a closed terminal (SIGHUP) or a
//! closed Windows console (`CTRL_CLOSE_EVENT`, which ends every process attached to the console)
//! would otherwise kill the VM before its owner could stop it. The owner stops an attached
//! sandbox itself, and the parent watchdog (Unix) or the kill-on-close job (Windows) still ends
//! the VM when the owner dies.

use std::process::Command;

use super::SpawnMode;

//--------------------------------------------------------------------------------------------------
// Constants
//--------------------------------------------------------------------------------------------------

/// Windows creation flags for a sandbox process.
///
/// Both modes get no console and a process group of their own. A detached sandbox also leaves
/// the creator's job, because it must outlive it; an attached one stays in reach of the job that
/// kills it when the owner dies.
#[cfg(windows)]
pub(crate) const fn creation_flags(mode: SpawnMode) -> u32 {
    use windows_sys::Win32::System::Threading::{
        CREATE_BREAKAWAY_FROM_JOB, CREATE_NEW_PROCESS_GROUP, DETACHED_PROCESS,
    };

    let isolated = DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP;
    match mode {
        SpawnMode::Attached => isolated,
        SpawnMode::Detached => isolated | CREATE_BREAKAWAY_FROM_JOB,
    }
}

//--------------------------------------------------------------------------------------------------
// Functions
//--------------------------------------------------------------------------------------------------

/// Detach the sandbox process from the creator's terminal or console. Call it before any other
/// `pre_exec` hook is added, so the hooks that follow already run in the new session.
pub(crate) fn isolate_from_launcher(cmd: &mut Command, mode: SpawnMode) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;

        cmd.creation_flags(creation_flags(mode));
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;

        // SAFETY: the hook only calls setsid and sigaction, which are async-signal-safe.
        unsafe {
            cmd.pre_exec(move || start_own_session(mode));
        }
    }
}

/// Runs in the forked child, so it uses only async-signal-safe calls.
#[cfg(unix)]
fn start_own_session(mode: SpawnMode) -> std::io::Result<()> {
    if unsafe { libc::setsid() } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // Only a detached sandbox outlives the terminal; an attached one ends through its watchdog
    // when the owner goes, so it keeps the default SIGHUP.
    if matches!(mode, SpawnMode::Detached) {
        let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
        action.sa_sigaction = libc::SIG_IGN;
        if unsafe { libc::sigemptyset(&mut action.sa_mask) } < 0 {
            return Err(std::io::Error::last_os_error());
        }
        if unsafe { libc::sigaction(libc::SIGHUP, &action, std::ptr::null_mut()) } < 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

//--------------------------------------------------------------------------------------------------
// Tests
//--------------------------------------------------------------------------------------------------

#[cfg(all(test, unix))]
mod unix_tests {
    use std::process::{Child, Stdio};

    use super::*;

    fn spawn_sleeper(mode: SpawnMode) -> Child {
        let mut cmd = Command::new("sleep");
        cmd.arg("30").stdin(Stdio::null());
        isolate_from_launcher(&mut cmd, mode);
        cmd.spawn().expect("spawn sleep")
    }

    fn stop(mut child: Child) {
        let _ = child.kill();
        let _ = child.wait();
    }

    /// Whether SIGHUP is ignored in `pid` (bit 0 of `SigIgn`).
    #[cfg(target_os = "linux")]
    fn ignores_sighup(pid: u32) -> bool {
        let status = std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap();
        let mask = status
            .lines()
            .find_map(|line| line.strip_prefix("SigIgn:"))
            .map(|hex| u64::from_str_radix(hex.trim(), 16).unwrap())
            .expect("SigIgn line");
        mask & (1 << (libc::SIGHUP - 1)) != 0
    }

    fn assert_own_session(child: &Child) {
        let pid = child.id() as libc::pid_t;
        let ours = unsafe { libc::getsid(0) };
        assert_ne!(
            unsafe { libc::getsid(pid) },
            ours,
            "child shares our session"
        );
        assert_eq!(
            unsafe { libc::getsid(pid) },
            pid,
            "child is not a session leader"
        );
        assert_eq!(
            unsafe { libc::getpgid(pid) },
            pid,
            "child is not its own group leader"
        );
    }

    /// A terminal's Ctrl-C goes to the foreground process group of the creator's session. The
    /// attached sandbox must be in neither.
    #[test]
    fn attached_sandbox_process_leaves_the_creators_session_and_group() {
        let child = spawn_sleeper(SpawnMode::Attached);
        assert_own_session(&child);
        #[cfg(target_os = "linux")]
        assert!(
            !ignores_sighup(child.id()),
            "attached keeps the default SIGHUP"
        );
        stop(child);
    }

    #[test]
    fn detached_sandbox_process_leaves_the_session_and_ignores_sighup() {
        let child = spawn_sleeper(SpawnMode::Detached);
        assert_own_session(&child);
        #[cfg(target_os = "linux")]
        assert!(
            ignores_sighup(child.id()),
            "detached survives a closed terminal"
        );
        stop(child);
    }

    /// The interrupt a terminal sends to the creator's group must leave the sandbox process alone,
    /// while a child that stayed in the group dies of it.
    #[test]
    fn a_signal_to_the_creators_group_misses_the_isolated_child_only() {
        // A group of our own stands in for the creator's: this test must not signal the harness.
        let mut creator = Command::new("sh");
        creator
            .args(["-c", "sleep 30 & sleep 30 & wait"])
            .stdin(Stdio::null());
        std::os::unix::process::CommandExt::process_group(&mut creator, 0);
        let creator = creator.spawn().expect("spawn creator");
        let group = creator.id() as libc::pid_t;

        // Started inside the creator's group, as the sandbox process is, then isolated.
        let mut isolated = Command::new("sleep");
        isolated.arg("30").stdin(Stdio::null());
        std::os::unix::process::CommandExt::process_group(&mut isolated, group);
        isolate_from_launcher(&mut isolated, SpawnMode::Attached);
        let mut isolated = isolated.spawn().expect("spawn isolated");
        let mut plain = Command::new("sleep");
        plain.arg("30").stdin(Stdio::null());
        std::os::unix::process::CommandExt::process_group(&mut plain, group);
        let mut plain = plain.spawn().expect("spawn plain");

        assert_eq!(unsafe { libc::kill(-group, libc::SIGINT) }, 0);
        let status = plain.wait().expect("wait plain");
        assert!(
            !status.success(),
            "the child left in the group should die of SIGINT"
        );
        std::thread::sleep(std::time::Duration::from_millis(200));
        assert!(
            isolated.try_wait().unwrap().is_none(),
            "the isolated child died of the group's SIGINT"
        );
        stop(isolated);
        stop(creator);
    }
}

/// These tests re-run the test binary as helper processes. A helper is this binary started with
/// `PROBE_ROLE` set and a filter that matches only `helper_entry`, which does nothing when the
/// variable is absent.
#[cfg(all(test, windows))]
mod windows_tests {
    use std::path::{Path, PathBuf};
    use std::process::Stdio;
    use std::time::{Duration, Instant};

    use windows_sys::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
    use windows_sys::Win32::System::Console::GetConsoleProcessList;
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject,
    };

    use super::*;
    use crate::runtime::handle::WindowsJob;

    const PROBE_ROLE: &str = "MSB_LAUNCHER_ISOLATION_PROBE_ROLE";
    const PROBE_OUT: &str = "MSB_LAUNCHER_ISOLATION_PROBE_OUT";
    const HELPER_FILTER: &str = "launcher_isolation::windows_tests::helper_entry";

    fn helper_command(role: &str, out: &Path) -> Command {
        let mut cmd = Command::new(std::env::current_exe().unwrap());
        cmd.args([HELPER_FILTER, "--nocapture", "--test-threads=1"])
            .env(PROBE_ROLE, role)
            .env(PROBE_OUT, out)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        cmd
    }

    fn wait_for_file(path: &Path) -> String {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            if let Ok(text) = std::fs::read_to_string(path)
                && text.ends_with('\n')
            {
                return text.trim().to_owned();
            }
            assert!(
                Instant::now() < deadline,
                "{} never appeared",
                path.display()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn console_process_count() -> u32 {
        let mut pids = [0u32; 8];
        unsafe { GetConsoleProcessList(pids.as_mut_ptr(), pids.len() as u32) }
    }

    /// Entry point of the helper processes; a no-op in a normal test run.
    #[test]
    fn helper_entry() {
        let Ok(role) = std::env::var(PROBE_ROLE) else {
            return;
        };
        let out = PathBuf::from(std::env::var(PROBE_OUT).unwrap());
        match role.as_str() {
            // Report how many processes share this one's console (0: it has none).
            "console" => std::fs::write(&out, format!("{}\n", console_process_count())).unwrap(),
            "sleeper" => {
                std::fs::write(&out, format!("{}\n", std::process::id())).unwrap();
                std::thread::sleep(Duration::from_secs(300));
            }
            // Runs as the creator with a console of its own, and reports how many processes share
            // the console of a plain child and of an attached sandbox process. A detached one is left out:
            // its breakaway flag is refused (access denied) inside a job that forbids breakaway, as
            // on a hosted runner. `creation_flags_isolate_both_modes...` covers its flags.
            "console-parent" => {
                let dir = out.parent().unwrap();
                let report = format!(
                    "self={} plain={} attached={}\n",
                    console_process_count(),
                    console_count_of_child(None, dir, "plain"),
                    console_count_of_child(Some(SpawnMode::Attached), dir, "attached"),
                );
                std::fs::write(&out, report).unwrap();
            }
            // Stands in for the creator: owns the kill-on-close job and starts the sandbox
            // process the way spawn_sandbox does, then stays alive until it is killed.
            "creator" => {
                let job = WindowsJob::new_kill_on_close().unwrap();
                let mut cmd = helper_command("sleeper", &out);
                isolate_from_launcher(&mut cmd, SpawnMode::Attached);
                let child = cmd.spawn().unwrap();
                job.assign_pid(child.id()).unwrap();
                std::thread::sleep(Duration::from_secs(300));
            }
            other => panic!("unknown role {other}"),
        }
    }

    /// The console process count a child reports, or the reason it could not be run.
    fn console_count_of_child(mode: Option<SpawnMode>, dir: &Path, name: &str) -> String {
        let out = dir.join(name);
        let mut cmd = helper_command("console", &out);
        if let Some(mode) = mode {
            isolate_from_launcher(&mut cmd, mode);
        }
        match cmd.status() {
            Ok(status) if status.success() => wait_for_file(&out),
            Ok(status) => format!("exit-{status}"),
            Err(err) => format!("spawn-failed-{err}").replace(' ', "_"),
        }
    }

    /// A close of the creator's console ends every process attached to it, so the sandbox
    /// process must have no console. The plain child proves the probe can see one.
    #[test]
    fn sandbox_process_has_no_console_where_a_plain_child_shares_the_creators() {
        use std::os::windows::process::CommandExt;
        use windows_sys::Win32::System::Threading::CREATE_NEW_CONSOLE;

        let dir = tempfile::tempdir().unwrap();
        let report = dir.path().join("report");
        // The test process may have no console (a service, a CI step), so the creator gets its own.
        let status = helper_command("console-parent", &report)
            .creation_flags(CREATE_NEW_CONSOLE)
            .status()
            .expect("run the console creator");
        assert!(status.success(), "console creator failed: {status}");
        let text = wait_for_file(&report);
        let count = |key: &str| -> u32 {
            text.split(' ')
                .find_map(|part| part.strip_prefix(key))
                .unwrap_or_else(|| panic!("no {key} in {text:?}"))
                .parse()
                .unwrap_or_else(|_| panic!("{key} is not a number in {text:?}"))
        };
        assert!(count("self=") > 0, "the creator has no console: {text}");
        assert!(
            count("plain=") > 0,
            "a plain child should share the creator's console: {text}"
        );
        assert_eq!(count("attached="), 0, "{text}");
    }

    #[test]
    fn creation_flags_isolate_both_modes_and_only_detached_breaks_away() {
        use windows_sys::Win32::System::Threading::{
            CREATE_BREAKAWAY_FROM_JOB, CREATE_NEW_PROCESS_GROUP, DETACHED_PROCESS,
        };

        let attached = creation_flags(SpawnMode::Attached);
        let detached = creation_flags(SpawnMode::Detached);
        for flags in [attached, detached] {
            assert_eq!(flags & DETACHED_PROCESS, DETACHED_PROCESS);
            assert_eq!(flags & CREATE_NEW_PROCESS_GROUP, CREATE_NEW_PROCESS_GROUP);
        }
        assert_eq!(attached & CREATE_BREAKAWAY_FROM_JOB, 0);
        assert_eq!(
            detached & CREATE_BREAKAWAY_FROM_JOB,
            CREATE_BREAKAWAY_FROM_JOB
        );
    }

    /// The Windows counterpart of the Unix parent watchdog: killing the creator outright (no
    /// chance to run any cleanup) must take the sandbox process with it.
    #[test]
    fn killing_the_creator_ends_the_sandbox_process() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("sandbox.pid");
        let mut creator = helper_command("creator", &pid_file)
            .spawn()
            .expect("spawn creator");
        let sandbox_pid: u32 = wait_for_file(&pid_file).parse().unwrap();

        let process = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, sandbox_pid) };
        assert!(!process.is_null(), "sandbox process is not running");
        assert_ne!(
            unsafe { WaitForSingleObject(process, 0) },
            WAIT_OBJECT_0,
            "sandbox process exited before the creator was killed"
        );

        creator.kill().expect("TerminateProcess the creator");
        let _ = creator.wait();
        let waited = unsafe { WaitForSingleObject(process, 20_000) };
        unsafe { CloseHandle(process) };
        assert_eq!(
            waited, WAIT_OBJECT_0,
            "sandbox process outlived its creator"
        );
    }
}
