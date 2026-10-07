//! Log file management with rotation for capturing VM console output.

use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use crate::RuntimeResult;

//--------------------------------------------------------------------------------------------------
// Constants
//--------------------------------------------------------------------------------------------------

/// Maximum number of rotated log files to keep.
const MAX_ROTATED_FILES: u32 = 3;

/// Default wait before a failed rotation is tried again.
const DEFAULT_RETRY_INTERVAL: Duration = Duration::from_secs(5);

/// Suffix of the file that holds a full log while its rotation is unfinished.
const PENDING_SUFFIX: &str = "rotating";

//--------------------------------------------------------------------------------------------------
// Types
//--------------------------------------------------------------------------------------------------

/// A simple rotating log writer.
///
/// Writes to a log file and rotates when the file exceeds `max_bytes`.
/// Rotated files are renamed with a numeric suffix (e.g., `vm.log.1`).
///
/// A failed rotation step never drops data and never costs an older log:
/// the writer keeps appending to the current file, remembers that the
/// rotation is unfinished, retries it after [`DEFAULT_RETRY_INTERVAL`], and
/// queues one [`RotationEvent`] per failure episode for the caller to report
/// ([`RotatingLog::take_rotation_event`]). On Windows a rotation fails when
/// another program holds the log open without `FILE_SHARE_DELETE`.
///
/// Rotation moves the full log aside first (to `<log>.rotating`), so the
/// step that can fail because of another program runs before any older file
/// moves. Older files only ever move into an empty slot (apart from the one
/// that rotates out), which makes every step safe to repeat.
pub struct RotatingLog {
    /// Path to the current log file.
    path: PathBuf,

    /// Open file handle for writing.
    file: File,

    /// Maximum file size in bytes before rotation.
    max_bytes: u64,

    /// Bytes written to the current file.
    written: u64,

    /// The last rotation attempt failed, or its last steps are still to do.
    unfinished: bool,

    /// `file` is the handle of the full log, now named `<log>.rotating`:
    /// the fresh log could not be opened yet.
    detached: bool,

    /// Earliest time of the next rotation attempt after a failure.
    next_retry: Instant,

    /// Wait between attempts after a failure.
    retry_interval: Duration,

    /// A failure episode is going on (reported, not yet recovered).
    failing: bool,

    /// Event for the caller, not taken yet.
    event: Option<RotationEvent>,
}

/// What the caller should report about rotation, once per change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RotationEvent {
    /// A rotation step failed. The log keeps growing past its limit and the
    /// rotation is retried.
    Failed(String),

    /// A rotation finished after an earlier failure.
    Recovered,
}

//--------------------------------------------------------------------------------------------------
// Methods
//--------------------------------------------------------------------------------------------------

impl RotatingLog {
    /// Create a new rotating log writer.
    ///
    /// The log file is created at `<log_dir>/<prefix>.log`.
    pub fn new(log_dir: &Path, prefix: &str, max_bytes: u64) -> RuntimeResult<Self> {
        fs::create_dir_all(log_dir)?;

        let path = log_dir.join(format!("{prefix}.log"));
        let written = path.metadata().map(|m| m.len()).unwrap_or(0);
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        let mut log = Self {
            path,
            file,
            max_bytes,
            written,
            unfinished: false,
            detached: false,
            next_retry: Instant::now(),
            retry_interval: DEFAULT_RETRY_INTERVAL,
            failing: false,
            event: None,
        };
        // A full log left aside by a process that died mid-rotation.
        log.unfinished = log.pending_path().exists();
        Ok(log)
    }

    /// Set the wait between rotation attempts after a failure.
    pub fn set_retry_interval(&mut self, interval: Duration) {
        self.retry_interval = interval;
    }

    /// Write data to the log file, rotating if necessary.
    ///
    /// A failed rotation does not fail the write: the data goes to the
    /// current file and the failure is queued as a [`RotationEvent`].
    pub fn write(&mut self, data: &[u8]) -> RuntimeResult<()> {
        let full = self.written + data.len() as u64 > self.max_bytes;
        if (full || self.unfinished) && Instant::now() >= self.next_retry {
            match self.rotate(full) {
                Ok(()) => {
                    self.unfinished = false;
                    if self.failing {
                        self.failing = false;
                        self.event = Some(RotationEvent::Recovered);
                    }
                }
                Err(err) => {
                    self.unfinished = true;
                    self.next_retry = Instant::now() + self.retry_interval;
                    if !self.failing {
                        self.failing = true;
                        self.event = Some(RotationEvent::Failed(err.to_string()));
                    }
                }
            }
        }

        self.file.write_all(data)?;
        self.written += data.len() as u64;
        Ok(())
    }

    /// Take the rotation event queued by the last writes, if any.
    pub fn take_rotation_event(&mut self) -> Option<RotationEvent> {
        self.event.take()
    }

    /// Append a line to the log file without ever rotating (for reports
    /// about the log itself).
    pub fn write_notice(&mut self, line: &str) -> RuntimeResult<()> {
        let mut bytes = line.as_bytes().to_vec();
        if !bytes.ends_with(b"\n") {
            bytes.push(b'\n');
        }
        self.file.write_all(&bytes)?;
        self.written += bytes.len() as u64;
        Ok(())
    }

    /// Flush the log file.
    pub fn flush(&mut self) -> RuntimeResult<()> {
        self.file.flush()?;
        Ok(())
    }
}

//--------------------------------------------------------------------------------------------------
// Methods: Helpers
//--------------------------------------------------------------------------------------------------

impl RotatingLog {
    fn pending_path(&self) -> PathBuf {
        PathBuf::from(format!("{}.{PENDING_SUFFIX}", self.path.display()))
    }

    fn rotated_path(&self, index: u32) -> PathBuf {
        PathBuf::from(format!("{}.{index}", self.path.display()))
    }

    /// Run the rotation steps that are still to do (`full`: the current log
    /// is over the limit and has to be rotated too). Every step either does
    /// nothing or moves a file into an empty slot, so a failure at any point
    /// loses nothing and the next call carries on.
    fn rotate(&mut self, full: bool) -> RuntimeResult<()> {
        self.file.flush()?;

        // 1. A full log left aside by an earlier attempt goes first: moving
        //    the current log onto it would destroy it.
        if !self.detached && self.pending_path().exists() {
            self.finish_pending()?;
            if !full {
                return Ok(());
            }
        }

        // 2. Move the current log aside. This is the step another program
        //    can block; nothing else has moved yet when it fails.
        if !self.detached {
            fs::rename(&self.path, self.pending_path())?;
            self.detached = true;
        }

        // 3. Open the fresh log. If that fails, put the full log back so the
        //    writes continue in a file at the expected path.
        match OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        {
            Ok(file) => {
                self.file = file;
                self.written = 0;
                self.detached = false;
            }
            Err(err) => {
                if !self.path.exists() && fs::rename(self.pending_path(), &self.path).is_ok() {
                    self.detached = false;
                }
                return Err(err.into());
            }
        }

        // 4. Move the older files up and the full log to `.1`.
        self.finish_pending()
    }

    /// Move `<log>.rotating` to `<log>.1`, making room for it without
    /// overwriting anything that still counts.
    fn finish_pending(&self) -> RuntimeResult<()> {
        // First empty slot among `.1` ..= `.{MAX_ROTATED_FILES + 1}`. When
        // all are taken the oldest rotates out, like the final rename of
        // any rotation.
        let last = MAX_ROTATED_FILES + 1;
        let free = match (1..=last).find(|i| !self.rotated_path(*i).exists()) {
            Some(free) => free,
            None => {
                fs::rename(self.rotated_path(last - 1), self.rotated_path(last))?;
                last - 1
            }
        };

        for i in (1..free).rev() {
            fs::rename(self.rotated_path(i), self.rotated_path(i + 1))?;
        }
        fs::rename(self.pending_path(), self.rotated_path(1))?;
        Ok(())
    }
}

//--------------------------------------------------------------------------------------------------
// Tests
//--------------------------------------------------------------------------------------------------

#[cfg(test)]
pub(crate) mod test_support {
    use std::path::Path;

    /// Makes renaming a log file fail, like another program would:
    /// on Windows by holding the file open without `FILE_SHARE_DELETE`, on
    /// Unix by taking the write permission off its directory.
    pub(crate) struct RotationBlocker {
        #[cfg(windows)]
        _holder: std::fs::File,
        #[cfg(unix)]
        dir: std::path::PathBuf,
        #[cfg(unix)]
        mode: u32,
    }

    impl RotationBlocker {
        /// `None` when this process can't be blocked (Unix: running as root).
        pub(crate) fn new(log: &Path) -> Option<Self> {
            #[cfg(windows)]
            {
                use std::os::windows::fs::OpenOptionsExt;

                // FILE_SHARE_READ | FILE_SHARE_WRITE, no FILE_SHARE_DELETE.
                let holder = std::fs::OpenOptions::new()
                    .read(true)
                    .share_mode(1 | 2)
                    .open(log)
                    .unwrap();
                Some(Self { _holder: holder })
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;

                let dir = log.parent().unwrap().to_path_buf();
                let mode = std::fs::metadata(&dir).unwrap().permissions().mode();
                std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o555)).unwrap();
                let blocker = Self {
                    dir: dir.clone(),
                    mode,
                };
                // Root ignores the permission bits.
                if std::fs::File::create(dir.join("probe")).is_ok() {
                    let _ = std::fs::remove_file(dir.join("probe"));
                    return None;
                }
                Some(blocker)
            }
        }
    }

    #[cfg(unix)]
    impl Drop for RotationBlocker {
        fn drop(&mut self) {
            use std::os::unix::fs::PermissionsExt;

            let _ = std::fs::set_permissions(&self.dir, std::fs::Permissions::from_mode(self.mode));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{test_support::RotationBlocker, *};

    fn read(dir: &Path, name: &str) -> Vec<u8> {
        fs::read(dir.join(name)).unwrap_or_default()
    }

    fn exists(dir: &Path, name: &str) -> bool {
        dir.join(name).exists()
    }

    #[test]
    fn test_rotation_shifts_files_and_keeps_four() {
        let dir = tempfile::tempdir().unwrap();
        let mut log = RotatingLog::new(dir.path(), "t", 10).unwrap();
        for c in b'a'..=b'g' {
            log.write(&[c; 8]).unwrap();
        }
        assert_eq!(read(dir.path(), "t.log"), [b'g'; 8]);
        assert_eq!(read(dir.path(), "t.log.1"), [b'f'; 8]);
        assert_eq!(read(dir.path(), "t.log.4"), [b'c'; 8]);
        assert!(!exists(dir.path(), "t.log.5"));
        assert!(!exists(dir.path(), "t.log.rotating"));
        assert_eq!(log.take_rotation_event(), None);
    }

    #[test]
    fn test_failed_rotation_keeps_writing_reports_once_and_retries() {
        let dir = tempfile::tempdir().unwrap();
        let mut log = RotatingLog::new(dir.path(), "t", 10).unwrap();
        log.set_retry_interval(Duration::from_millis(20));
        // Older logs that must survive the failure.
        log.write(&[b'a'; 8]).unwrap();
        log.write(&[b'b'; 8]).unwrap();
        log.write(&[b'c'; 8]).unwrap();
        assert_eq!(read(dir.path(), "t.log.1"), [b'b'; 8]);
        assert_eq!(read(dir.path(), "t.log.2"), [b'a'; 8]);

        let Some(blocker) = RotationBlocker::new(&dir.path().join("t.log")) else {
            eprintln!("cannot block renames here (root?); skipped");
            return;
        };
        for _ in 0..5 {
            log.write(&[b'd'; 8]).unwrap();
            std::thread::sleep(Duration::from_millis(25));
        }
        // One report for the whole episode, nothing dropped, nothing shifted.
        assert!(matches!(
            log.take_rotation_event(),
            Some(RotationEvent::Failed(_))
        ));
        assert_eq!(log.take_rotation_event(), None);
        assert_eq!(
            read(dir.path(), "t.log"),
            [&[b'c'; 8][..], &[b'd'; 40]].concat()
        );
        assert_eq!(read(dir.path(), "t.log.1"), [b'b'; 8]);
        assert_eq!(read(dir.path(), "t.log.2"), [b'a'; 8]);
        assert!(!exists(dir.path(), "t.log.3"));

        // Unblocked: the next write after the interval rotates and says so.
        drop(blocker);
        std::thread::sleep(Duration::from_millis(25));
        log.write(&[b'e'; 8]).unwrap();
        assert_eq!(log.take_rotation_event(), Some(RotationEvent::Recovered));
        assert_eq!(read(dir.path(), "t.log"), [b'e'; 8]);
        assert_eq!(
            read(dir.path(), "t.log.1"),
            [&[b'c'; 8][..], &[b'd'; 40]].concat()
        );
        assert_eq!(read(dir.path(), "t.log.2"), [b'b'; 8]);
        assert_eq!(read(dir.path(), "t.log.3"), [b'a'; 8]);
    }

    #[test]
    fn test_leftover_rotating_file_is_finished_without_loss() {
        let dir = tempfile::tempdir().unwrap();
        // A process died after moving the full log aside and shifting some
        // files: slot `.1` is empty, `.4` is taken.
        fs::write(dir.path().join("t.log.2"), b"A").unwrap();
        fs::write(dir.path().join("t.log.3"), b"B").unwrap();
        fs::write(dir.path().join("t.log.4"), b"C").unwrap();
        fs::write(dir.path().join("t.log.rotating"), b"E").unwrap();
        let mut log = RotatingLog::new(dir.path(), "t", 100).unwrap();
        log.write(b"x").unwrap();
        assert_eq!(read(dir.path(), "t.log.1"), b"E");
        assert_eq!(read(dir.path(), "t.log.2"), b"A");
        assert_eq!(read(dir.path(), "t.log.3"), b"B");
        assert_eq!(read(dir.path(), "t.log.4"), b"C");
        assert_eq!(read(dir.path(), "t.log"), b"x");
        assert!(!exists(dir.path(), "t.log.rotating"));
    }

    #[test]
    fn test_leftover_rotating_file_with_all_slots_taken_expires_only_the_oldest() {
        let dir = tempfile::tempdir().unwrap();
        for (i, c) in [(1, "1"), (2, "2"), (3, "3"), (4, "4")] {
            fs::write(dir.path().join(format!("t.log.{i}")), c).unwrap();
        }
        fs::write(dir.path().join("t.log.rotating"), b"E").unwrap();
        let mut log = RotatingLog::new(dir.path(), "t", 100).unwrap();
        log.write(b"x").unwrap();
        assert_eq!(read(dir.path(), "t.log.1"), b"E");
        assert_eq!(read(dir.path(), "t.log.2"), b"1");
        assert_eq!(read(dir.path(), "t.log.3"), b"2");
        assert_eq!(read(dir.path(), "t.log.4"), b"3");
    }
}
