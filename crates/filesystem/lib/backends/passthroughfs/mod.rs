//! Platform passthrough filesystem backends.
//!
//! The public passthrough API stays stable at this module path while the
//! implementation lives under platform-specific submodules. Shared helpers
//! such as quota accounting live beside the platform modules.

mod external;
mod owned;
pub(crate) mod quota;
#[cfg(any(target_os = "linux", windows))]
pub(crate) mod window;

/// Default for `PassthroughConfig::max_open_handles`. A guest's own tools stay far below this
/// (a build or a file watcher holds thousands); it only stops a loop that opens files forever.
pub(crate) const DEFAULT_MAX_OPEN_HANDLES: usize = 65_536;

#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

//--------------------------------------------------------------------------------------------------
// Re-Exports
//--------------------------------------------------------------------------------------------------

pub use external::ExternalCheckpointOptions;
pub(crate) use external::ExternalSingleFileIndex;
pub use owned::{OwnedDirectoryCheckpoint, OwnedDirectoryPayload, OwnedDirectorySnapshot};
#[cfg(unix)]
pub use unix::*;
#[cfg(windows)]
pub use windows::{HostPermissions, PassthroughConfig, PassthroughFs, StatVirtualization};
