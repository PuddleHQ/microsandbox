//! Win32 name rules for share paths.
//!
//! The Win32 layer reads some names as something else than a file: `CON`, `NUL` and
//! `COM1` are devices (also with an extension, as in `CON.txt`), trailing dots and spaces
//! are dropped, and a file is also reachable under its 8.3 short name. A guest that picks
//! such a name would reach a host device, or a file under a name the share never listed.
//! The share refuses these names, and it resolves its root as a `\\?\` path, which
//! Win32 passes to the file system without reading names.

use std::ffi::OsString;
use std::io;
use std::path::{Component, Path, PathBuf, Prefix};

use super::{LINUX_EINVAL, LINUX_EPERM, host_error, linux_error};

/// Whether Win32 would read `name` as a device, a different name, or an invalid one.
pub(super) fn win32_name_refused(name: &str) -> bool {
    // A device name is the part before the first dot, minus trailing spaces: `con .txt` is `CON`.
    let stem = name
        .split('.')
        .next()
        .unwrap_or_default()
        .trim_end_matches(' ');
    let upper = stem.to_ascii_uppercase();
    let numbered_device = upper
        .strip_prefix("COM")
        .or_else(|| upper.strip_prefix("LPT"))
        .is_some_and(|suffix| {
            matches!(
                suffix,
                "0" | "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
            )
        });
    name.ends_with(['.', ' '])
        || name
            .chars()
            .any(|ch| ch.is_control() || "<>\"|?*".contains(ch))
        || matches!(
            upper.as_str(),
            "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
        )
        || numbered_device
}

/// Whether `name` has the shape of an 8.3 short name: one to six characters, `~`, digits,
/// and an extension of at most three characters (`PROGRA~1`, `LONGNA~2.TXT`).
fn has_short_name_shape(name: &str) -> bool {
    let (stem, extension) = match name.rsplit_once('.') {
        Some((stem, extension)) => (stem, extension),
        None => (name, ""),
    };
    if extension.chars().count() > 3 {
        return false;
    }
    let Some((base, digits)) = stem.rsplit_once('~') else {
        return false;
    };
    (1..=6).contains(&base.chars().count())
        && !digits.is_empty()
        && digits.bytes().all(|byte| byte.is_ascii_digit())
}

/// Refuse `name` when it reaches an existing entry of `parent` only through its short name.
///
/// A name that does not exist passes: a create makes its own file, and a lookup answers
/// not found. A name with the shape of a short name that is also the real name of an entry
/// passes, so a file the user really called `backup~1.txt` stays usable.
pub(super) fn refuse_short_name_alias(parent: &Path, name: &str) -> io::Result<()> {
    if !has_short_name_shape(name) || std::fs::symlink_metadata(parent.join(name)).is_err() {
        return Ok(());
    }
    for entry in std::fs::read_dir(parent).map_err(host_error)? {
        let entry = entry.map_err(host_error)?;
        if entry
            .file_name()
            .to_string_lossy()
            .eq_ignore_ascii_case(name)
        {
            return Ok(());
        }
    }
    Err(linux_error(LINUX_EPERM))
}

/// Turn the mount root into its `\\?\` form and refuse a root Win32 would read differently.
///
/// A path that already starts with `\\?\` is taken as written. Any other path is checked
/// for names Win32 would rewrite (`CON`, a trailing dot), made absolute, and given the
/// prefix, so every later lookup under the root is literal.
pub(super) fn verbatim_root(root_dir: &Path) -> io::Result<PathBuf> {
    let verbatim = matches!(
        root_dir.components().next(),
        Some(Component::Prefix(prefix)) if prefix.kind().is_verbatim()
    );
    let absolute = if verbatim {
        root_dir.to_path_buf()
    } else {
        for component in root_dir.components() {
            if let Component::Normal(part) = component
                && part.to_str().is_none_or(win32_name_refused)
            {
                return Err(linux_error(LINUX_EINVAL));
            }
        }
        std::path::absolute(root_dir).map_err(host_error)?
    };

    let mut result = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::Prefix(prefix) => {
                let mut text = OsString::new();
                match prefix.kind() {
                    Prefix::Disk(letter) => {
                        text.push(format!(r"\\?\{}:", char::from(letter)));
                    }
                    Prefix::UNC(server, share) => {
                        text.push(r"\\?\UNC\");
                        text.push(server);
                        text.push(r"\");
                        text.push(share);
                    }
                    Prefix::Verbatim(_) | Prefix::VerbatimDisk(_) | Prefix::VerbatimUNC(..) => {
                        text.push(prefix.as_os_str());
                    }
                    Prefix::DeviceNS(_) => return Err(linux_error(LINUX_EINVAL)),
                }
                result.push(text);
            }
            other => result.push(other.as_os_str()),
        }
    }
    Ok(result)
}
