//! Tar stream ingestion into an in-memory `FileTree`.
//!
//! Reads an OCI layer tar stream (optionally compressed) and builds a `FileTree`
//! representing the layer's filesystem contents. Handles all OCI tar edge cases
//! including whiteouts, hardlinks, special files, and path validation.

use std::fmt;
use std::io::{ErrorKind, Read};
use std::pin::Pin;
use std::task::{Context, Poll};

use async_compression::tokio::bufread::{GzipDecoder, ZstdDecoder};
use sha2::{Digest as Sha2Digest, Sha256};
use tokio::io::{AsyncRead, AsyncReadExt, BufReader, ReadBuf};
use tokio_util::io::SyncIoBridge;

use crate::tree::{
    DataSpool, DeviceNode, DirectoryNode, FileData, FileTree, FileTreeError, InodeMetadata,
    RegularFileId, RegularFileNode, ResourceLimits, SPOOL_THRESHOLD, SymlinkNode, TreeNode, Xattr,
};

//--------------------------------------------------------------------------------------------------
// Constants
//--------------------------------------------------------------------------------------------------

/// Whiteout prefix used by OCI layers.
const WHITEOUT_PREFIX: &[u8] = b".wh.";

/// Opaque whiteout filename.
const OPAQUE_WHITEOUT: &[u8] = b".wh..wh..opq";

use crate::tree::{OPAQUE_XATTR_NAME, OPAQUE_XATTR_VALUE, WHITEOUT_MAJOR, WHITEOUT_MINOR};

/// Gzip magic bytes.
const GZIP_MAGIC: [u8; 2] = [0x1F, 0x8B];

/// Zstandard magic bytes.
const ZSTD_MAGIC: [u8; 4] = [0x28, 0xB5, 0x2F, 0xFD];

/// Maximum file-body read size used while ingesting tar entries, and the
/// read-ahead buffer between the tar parser and the async stream.
const ENTRY_READ_CHUNK_SIZE: usize = 64 * 1024;

/// Compressed input buffer size used before gzip/zstd decoders.
const COMPRESSED_INPUT_BUFFER_SIZE: usize = 256 * 1024;

/// Spool write batch size used after bounded decoder reads.
const SPOOL_WRITE_BUFFER_SIZE: usize = 1024 * 1024;

//--------------------------------------------------------------------------------------------------
// Types
//--------------------------------------------------------------------------------------------------

/// Compression format for a layer blob.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Compression {
    /// Uncompressed tar.
    None,
    /// Gzip-compressed tar.
    Gzip,
    /// Zstandard-compressed tar.
    Zstd,
}

/// Errors that can occur during tar ingestion.
#[derive(Debug)]
pub enum IngestError {
    /// Underlying I/O error.
    Io(std::io::Error),
    /// Tar entry path contains `..` components.
    PathTraversal(String),
    /// Tar entry path exceeds the maximum allowed length.
    PathTooLong(String),
    /// Tar entry path exceeds the maximum allowed depth.
    PathTooDeep(String),
    /// A single file exceeds the maximum allowed size.
    FileTooLarge(String),
    /// The cumulative size of all extracted data exceeds the limit.
    TotalSizeExceeded,
    /// The number of tar entries exceeds the limit.
    EntryCountExceeded,
    /// A symlink target exceeds the maximum allowed length.
    SymlinkTargetTooLong(String),
    /// A hardlink references a target that does not exist in the tree.
    HardlinkTarget(String),
    /// A tar entry is invalid or unsupported.
    InvalidEntry(String),
    /// FileTree insertion error.
    Tree(FileTreeError),
}

/// Result of parsing a whiteout filename.
enum WhiteoutKind<'a> {
    /// Not a whiteout; the entry should be inserted normally.
    None,
    /// Opaque whiteout — the parent directory gets an xattr.
    Opaque,
    /// Regular whiteout — replace with a char device node at the given name.
    File(&'a [u8]),
}

//--------------------------------------------------------------------------------------------------
// Methods
//--------------------------------------------------------------------------------------------------

impl Compression {
    /// Detect compression from an OCI media type string.
    pub fn from_media_type(media_type: &str) -> Self {
        if media_type.contains("gzip") {
            Compression::Gzip
        } else if media_type.contains("zstd") {
            Compression::Zstd
        } else {
            Compression::None
        }
    }

    /// Detect compression from magic bytes at the start of a stream.
    pub fn detect(magic: &[u8]) -> Self {
        if magic.len() >= 4 && magic[..4] == ZSTD_MAGIC {
            Compression::Zstd
        } else if magic.len() >= 2 && magic[..2] == GZIP_MAGIC {
            Compression::Gzip
        } else {
            Compression::None
        }
    }
}

//--------------------------------------------------------------------------------------------------
// Trait Implementations
//--------------------------------------------------------------------------------------------------

impl fmt::Display for IngestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            IngestError::Io(e) => write!(f, "I/O error: {e}"),
            IngestError::PathTraversal(p) => write!(f, "path traversal in tar: \"{p}\""),
            IngestError::PathTooLong(p) => write!(f, "path too long: \"{p}\""),
            IngestError::PathTooDeep(p) => write!(f, "path too deep: \"{p}\""),
            IngestError::FileTooLarge(p) => write!(f, "file too large: \"{p}\""),
            IngestError::TotalSizeExceeded => write!(f, "total extracted size exceeded"),
            IngestError::EntryCountExceeded => write!(f, "entry count exceeded"),
            IngestError::SymlinkTargetTooLong(p) => {
                write!(f, "symlink target too long: \"{p}\"")
            }
            IngestError::HardlinkTarget(p) => {
                write!(f, "hardlink target not found: \"{p}\"")
            }
            IngestError::InvalidEntry(msg) => write!(f, "invalid tar entry: {msg}"),
            IngestError::Tree(e) => write!(f, "file tree error: {e}"),
        }
    }
}

impl std::error::Error for IngestError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            IngestError::Io(e) => Some(e),
            IngestError::Tree(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for IngestError {
    fn from(e: std::io::Error) -> Self {
        IngestError::Io(e)
    }
}

impl From<FileTreeError> for IngestError {
    fn from(e: FileTreeError) -> Self {
        IngestError::Tree(e)
    }
}

//--------------------------------------------------------------------------------------------------
// Functions
//--------------------------------------------------------------------------------------------------

/// Ingest a decompressed tar stream into a `FileTree`.
///
/// If `spool` is provided, files larger than `SPOOL_THRESHOLD` are written
/// to the spool file instead of held in memory.
///
/// The tar parser is synchronous, so the stream is parsed on the blocking
/// pool and read through a [`SyncIoBridge`].
pub async fn ingest_tar<R: AsyncRead + Unpin + Send + 'static>(
    reader: R,
    limits: &ResourceLimits,
    spool: Option<DataSpool>,
) -> Result<FileTree, IngestError> {
    let (tree, _reader) = ingest_on_blocking_pool(reader, limits, spool).await?;
    Ok(tree)
}

/// Parse `reader` on the blocking pool and hand the reader back, so callers
/// can keep reading (and hashing) the bytes after the end-of-archive marker.
async fn ingest_on_blocking_pool<R: AsyncRead + Unpin + Send + 'static>(
    reader: R,
    limits: &ResourceLimits,
    spool: Option<DataSpool>,
) -> Result<(FileTree, R), IngestError> {
    let limits = limits.clone();
    let bridge = SyncIoBridge::new(reader);
    let task = tokio::task::spawn_blocking(move || {
        // Bytes left in the read-ahead buffer were already pulled through
        // `reader`, so dropping the buffer loses nothing a hashing reader saw.
        let mut archive = tar::Archive::new(std::io::BufReader::with_capacity(
            ENTRY_READ_CHUNK_SIZE,
            bridge,
        ));
        let tree = ingest_archive(&mut archive, &limits, spool)?;
        Ok((tree, archive.into_inner().into_inner().into_inner()))
    });
    match task.await {
        Ok(result) => result,
        Err(error) if error.is_panic() => std::panic::resume_unwind(error.into_panic()),
        Err(error) => Err(IngestError::Io(std::io::Error::other(error))),
    }
}

fn ingest_archive<R: Read>(
    archive: &mut tar::Archive<R>,
    limits: &ResourceLimits,
    mut spool: Option<DataSpool>,
) -> Result<FileTree, IngestError> {
    let mut tree = FileTree::new();
    let mut entry_count: u64 = 0;
    let mut total_size: u64 = 0;
    let mut saw_hardlink = false;
    let mut spool_read_buf = Vec::new();
    let mut spool_write_buf = Vec::new();

    let entries = archive.entries().map_err(IngestError::Io)?;

    for entry_result in entries {
        let mut entry = entry_result.map_err(IngestError::Io)?;

        entry_count += 1;
        if entry_count > limits.max_entry_count {
            return Err(IngestError::EntryCountExceeded);
        }

        let header = entry.header().clone();

        // Get the raw path bytes. Entry::path_bytes handles PAX extended headers.
        let raw_path = entry.path_bytes();
        let path = normalize_path(&raw_path, limits)?;

        // Skip empty paths (root directory entry `./` after stripping).
        let path = match path {
            Some(p) => p,
            None => continue,
        };

        let entry_type = header.entry_type();

        // Extract metadata from the header.
        let metadata = extract_metadata(&header);

        match entry_type {
            tar::EntryType::Link => {
                // Hardlink — look up the target in the tree and clone its data.
                let link_target_bytes = entry.link_name_bytes().ok_or_else(|| {
                    IngestError::InvalidEntry("hardlink with no target".to_string())
                })?;
                let target_path = normalize_path(&link_target_bytes, limits)?;
                let target_path = match target_path {
                    Some(p) => p,
                    None => {
                        return Err(IngestError::HardlinkTarget(
                            String::from_utf8_lossy(&link_target_bytes).into_owned(),
                        ));
                    }
                };

                handle_hardlink(&mut tree, &path, &target_path)?;
                saw_hardlink = true;
            }
            tar::EntryType::Directory => {
                let node = TreeNode::Directory(DirectoryNode {
                    metadata,
                    xattrs: Vec::new(),
                    entries: std::collections::BTreeMap::new(),
                });
                tree.insert(&path, node)?;
            }
            tar::EntryType::Symlink => {
                let link_target = entry.link_name_bytes().ok_or_else(|| {
                    IngestError::InvalidEntry("symlink with no target".to_string())
                })?;

                if link_target.len() > limits.max_symlink_target {
                    return Err(IngestError::SymlinkTargetTooLong(
                        String::from_utf8_lossy(&path).into_owned(),
                    ));
                }

                // Check for whiteout handling before inserting.
                let file_name = path_filename(&path);
                match classify_whiteout(file_name) {
                    WhiteoutKind::Opaque => {
                        // Opaque whiteout: add xattr to the parent directory.
                        apply_opaque_xattr(&mut tree, &path)?;
                    }
                    WhiteoutKind::File(real_name) => {
                        // Regular whiteout: insert a char device node.
                        let whiteout_path = replace_filename(&path, real_name);
                        let node = TreeNode::CharDevice(DeviceNode {
                            metadata,
                            major: WHITEOUT_MAJOR,
                            minor: WHITEOUT_MINOR,
                        });
                        tree.insert(&whiteout_path, node)?;
                    }
                    WhiteoutKind::None => {
                        let node = TreeNode::Symlink(SymlinkNode {
                            metadata,
                            target: link_target.into_owned(),
                        });
                        tree.insert(&path, node)?;
                    }
                }
            }
            tar::EntryType::Regular | tar::EntryType::Continuous | tar::EntryType::GNUSparse => {
                // The entry size honours a PAX `size` record and, for GNU
                // sparse members, is the expanded size the reader yields.
                let size = entry.size();
                if size > limits.max_file_size {
                    return Err(IngestError::FileTooLarge(
                        String::from_utf8_lossy(&path).into_owned(),
                    ));
                }
                total_size = total_size.saturating_add(size);
                if total_size > limits.max_total_size {
                    return Err(IngestError::TotalSizeExceeded);
                }

                // Check for whiteouts before reading the file body — whiteout
                // markers don't carry data, so reading would be wasted I/O.
                let file_name = path_filename(&path);
                match classify_whiteout(file_name) {
                    WhiteoutKind::Opaque => {
                        apply_opaque_xattr(&mut tree, &path)?;
                    }
                    WhiteoutKind::File(real_name) => {
                        let whiteout_path = replace_filename(&path, real_name);
                        let node = TreeNode::CharDevice(DeviceNode {
                            metadata,
                            major: WHITEOUT_MAJOR,
                            minor: WHITEOUT_MINOR,
                        });
                        tree.insert(&whiteout_path, node)?;
                    }
                    WhiteoutKind::None => {
                        let file_data = if size >= SPOOL_THRESHOLD
                            && let Some(spool) = spool.as_mut()
                        {
                            stream_entry_to_spool(
                                &mut entry,
                                size,
                                spool,
                                &mut spool_read_buf,
                                &mut spool_write_buf,
                            )?
                        } else {
                            FileData::Memory(read_entry_to_memory(&mut entry, size)?)
                        };

                        let node = TreeNode::RegularFile(RegularFileNode {
                            id: RegularFileId::new(),
                            metadata,
                            xattrs: Vec::new(),
                            data: file_data,
                            nlink: 1,
                        });
                        tree.insert(&path, node)?;
                    }
                }
            }
            tar::EntryType::Char => {
                let major = header.device_major().map_err(IngestError::Io)?.unwrap_or(0);
                let minor = header.device_minor().map_err(IngestError::Io)?.unwrap_or(0);
                let node = TreeNode::CharDevice(DeviceNode {
                    metadata,
                    major,
                    minor,
                });
                tree.insert(&path, node)?;
            }
            tar::EntryType::Block => {
                let major = header.device_major().map_err(IngestError::Io)?.unwrap_or(0);
                let minor = header.device_minor().map_err(IngestError::Io)?.unwrap_or(0);
                let node = TreeNode::BlockDevice(DeviceNode {
                    metadata,
                    major,
                    minor,
                });
                tree.insert(&path, node)?;
            }
            tar::EntryType::Fifo => {
                let node = TreeNode::Fifo(metadata);
                tree.insert(&path, node)?;
            }
            // GNU extensions and PAX headers are handled internally by the tar library.
            // Socket type is not a standard tar entry type but we handle it if encountered.
            other if other.as_byte() == 0o140 => {
                // Unix socket (type '`' = 0o140 = 96).
                let node = TreeNode::Socket(metadata);
                tree.insert(&path, node)?;
            }
            _ => {
                // Skip GNU long name/link, PAX headers, and other extension entries.
                // These are handled internally by the tar library when reading
                // subsequent entries.
            }
        }
    }

    if saw_hardlink {
        tree.refresh_regular_nlinks();
    }

    Ok(tree)
}

/// Ingest a compressed tar stream, automatically decompressing based on the
/// specified compression format.
/// Result of tar ingestion including the decompressed content hash.
pub struct IngestResult {
    /// The in-memory file tree built from the tar stream.
    pub tree: FileTree,
    /// SHA-256 hex digest of the decompressed tar stream (the OCI diff_id).
    pub uncompressed_digest: String,
}

pub async fn ingest_compressed_tar<R: AsyncRead + Unpin + Send + 'static>(
    reader: R,
    compression: Compression,
    limits: &ResourceLimits,
    spool_path: Option<&std::path::Path>,
) -> Result<IngestResult, IngestError> {
    let spool = spool_path
        .map(DataSpool::new)
        .transpose()
        .map_err(IngestError::Io)?;

    match compression {
        Compression::None => ingest_hashed(reader, limits, spool).await,
        Compression::Gzip => {
            let decoder = GzipDecoder::new(BufReader::with_capacity(
                COMPRESSED_INPUT_BUFFER_SIZE,
                reader,
            ));
            ingest_hashed(decoder, limits, spool).await
        }
        Compression::Zstd => {
            let decoder = ZstdDecoder::new(BufReader::with_capacity(
                COMPRESSED_INPUT_BUFFER_SIZE,
                reader,
            ));
            ingest_hashed(decoder, limits, spool).await
        }
    }
}

/// Ingest a decompressed stream and hash all of it, including the bytes
/// after the end-of-archive marker: the diff_id covers the full stream.
async fn ingest_hashed<R: AsyncRead + Unpin + Send + 'static>(
    reader: R,
    limits: &ResourceLimits,
    spool: Option<DataSpool>,
) -> Result<IngestResult, IngestError> {
    let (tree, mut hashing) =
        ingest_on_blocking_pool(HashingReader::new(reader), limits, spool).await?;
    drain_reader(&mut hashing).await?;
    Ok(IngestResult {
        tree,
        uncompressed_digest: hashing.hex_digest(),
    })
}

/// Read all remaining bytes from a reader to ensure the full stream is
/// consumed (and hashed, if wrapped in `HashingReader`). The tar parser
/// may stop before the EOF padding; the diff_id covers the full stream.
async fn drain_reader<R: AsyncRead + Unpin>(reader: &mut R) -> Result<(), IngestError> {
    let mut buf = vec![0u8; ENTRY_READ_CHUNK_SIZE];
    loop {
        let n = reader.read(&mut buf).await.map_err(IngestError::Io)?;
        if n == 0 {
            break;
        }
    }
    Ok(())
}

/// AsyncRead wrapper that computes SHA-256 of all data flowing through it.
struct HashingReader<R> {
    inner: R,
    hasher: Sha256,
}

impl<R> HashingReader<R> {
    fn new(inner: R) -> Self {
        Self {
            inner,
            hasher: Sha256::new(),
        }
    }

    fn hex_digest(self) -> String {
        hex::encode(self.hasher.finalize())
    }
}

impl<R: AsyncRead + Unpin> AsyncRead for HashingReader<R> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let before = buf.filled().len();
        let result = Pin::new(&mut self.inner).poll_read(cx, buf);
        if let Poll::Ready(Ok(())) = &result {
            let new_bytes = &buf.filled()[before..];
            if !new_bytes.is_empty() {
                self.hasher.update(new_bytes);
            }
        }
        result
    }
}

/// Normalize a raw tar path: strip leading `./` and `/`, reject `..`
/// components, and enforce length/depth limits.
///
/// Returns `Ok(None)` for empty paths (for example the root `./` or `/` entry
/// after stripping).
fn normalize_path(raw: &[u8], limits: &ResourceLimits) -> Result<Option<Vec<u8>>, IngestError> {
    let path = strip_dot_slash(raw);
    let path = strip_leading_slashes(path);

    // Strip trailing slashes.
    let path = strip_trailing_slashes(path);

    // Empty after stripping means this is the root entry.
    if path.is_empty() {
        return Ok(None);
    }

    // Reject `..` components.
    let mut depth: usize = 0;
    for component in path.split(|&b| b == b'/') {
        if component.is_empty() {
            continue;
        }
        if component == b".." {
            return Err(IngestError::PathTraversal(
                String::from_utf8_lossy(path).into_owned(),
            ));
        }
        depth += 1;
    }

    // Enforce path length limit.
    if path.len() > limits.max_path_length {
        return Err(IngestError::PathTooLong(
            String::from_utf8_lossy(path).into_owned(),
        ));
    }

    // Enforce path depth limit.
    if depth > limits.max_path_depth {
        return Err(IngestError::PathTooDeep(
            String::from_utf8_lossy(path).into_owned(),
        ));
    }

    Ok(Some(path.to_vec()))
}

/// Strip leading `./` prefix from a path.
fn strip_dot_slash(path: &[u8]) -> &[u8] {
    if path.starts_with(b"./") {
        &path[2..]
    } else if path == b"." {
        b""
    } else {
        path
    }
}

/// Strip trailing slashes from a path.
fn strip_trailing_slashes(path: &[u8]) -> &[u8] {
    let mut end = path.len();
    while end > 0 && path[end - 1] == b'/' {
        end -= 1;
    }
    &path[..end]
}

/// Strip all leading slashes from a path.
fn strip_leading_slashes(path: &[u8]) -> &[u8] {
    let mut start = 0;
    while start < path.len() && path[start] == b'/' {
        start += 1;
    }
    &path[start..]
}

/// Extract metadata from a tar header.
fn extract_metadata(header: &tar::Header) -> InodeMetadata {
    let uid = header.uid().unwrap_or(0) as u32;
    let gid = header.gid().unwrap_or(0) as u32;
    let mode = (header.mode().unwrap_or(0o644) & 0o7777) as u16;
    let mtime = header.mtime().unwrap_or(0);

    InodeMetadata {
        uid,
        gid,
        mode,
        mtime,
        mtime_nsec: 0,
    }
}

/// Get the filename component of a path (bytes after the last `/`).
fn path_filename(path: &[u8]) -> &[u8] {
    match path.iter().rposition(|&b| b == b'/') {
        Some(pos) => &path[pos + 1..],
        None => path,
    }
}

/// Get the parent portion of a path (bytes before the last `/`).
/// Returns an empty slice if there is no parent.
fn path_parent(path: &[u8]) -> &[u8] {
    match path.iter().rposition(|&b| b == b'/') {
        Some(pos) => &path[..pos],
        None => b"",
    }
}

/// Replace the filename component of a path with a new name.
fn replace_filename(path: &[u8], new_name: &[u8]) -> Vec<u8> {
    let parent = path_parent(path);
    if parent.is_empty() {
        new_name.to_vec()
    } else {
        let mut result = parent.to_vec();
        result.push(b'/');
        result.extend_from_slice(new_name);
        result
    }
}

/// Classify a filename as an OCI whiteout type.
///
/// OCI tar layers use two whiteout conventions:
/// - `.wh.<name>` — marks `<name>` as deleted in this layer
/// - `.wh..wh..opq` — marks the parent directory as opaque (hides all
///   entries from lower layers)
///
/// During ingestion these are converted to overlayfs-native representations:
/// `.wh.<name>` becomes a char device (0,0) inode, and `.wh..wh..opq`
/// becomes a `trusted.overlay.opaque=y` xattr on the parent directory.
fn classify_whiteout(filename: &[u8]) -> WhiteoutKind<'_> {
    if filename == OPAQUE_WHITEOUT {
        WhiteoutKind::Opaque
    } else if filename.starts_with(WHITEOUT_PREFIX) {
        let real_name = &filename[WHITEOUT_PREFIX.len()..];
        if real_name.is_empty() {
            WhiteoutKind::None
        } else {
            WhiteoutKind::File(real_name)
        }
    } else {
        WhiteoutKind::None
    }
}

/// Apply the opaque xattr to the parent directory of the given path.
fn apply_opaque_xattr(tree: &mut FileTree, path: &[u8]) -> Result<(), IngestError> {
    let parent = path_parent(path);

    // The parent is either the root (empty path) or a named directory.
    let dir = if parent.is_empty() {
        &mut tree.root
    } else {
        // Ensure the parent directory exists by inserting it if needed.
        match tree.get_mut(parent) {
            Some(TreeNode::Directory(dir)) => dir,
            _ => {
                // Create the parent directory, then retrieve it.
                let node = TreeNode::Directory(DirectoryNode::new(InodeMetadata::default()));
                tree.insert(parent, node)?;
                match tree.get_mut(parent) {
                    Some(TreeNode::Directory(dir)) => dir,
                    _ => {
                        return Err(IngestError::InvalidEntry(
                            "failed to create parent for opaque whiteout".to_string(),
                        ));
                    }
                }
            }
        }
    };

    // Add the opaque xattr if not already present.
    let already_has = dir
        .xattrs
        .iter()
        .any(|x| x.name == OPAQUE_XATTR_NAME && x.value == OPAQUE_XATTR_VALUE);

    if !already_has {
        dir.xattrs.push(Xattr {
            name: OPAQUE_XATTR_NAME.to_vec(),
            value: OPAQUE_XATTR_VALUE.to_vec(),
        });
    }

    Ok(())
}

/// Handle a hardlink entry by cloning the target's data to the new path.
fn handle_hardlink(
    tree: &mut FileTree,
    link_path: &[u8],
    target_path: &[u8],
) -> Result<(), IngestError> {
    let node = match tree.get_mut(target_path) {
        Some(TreeNode::RegularFile(f)) => {
            let new_nlink = f.nlink + 1;
            f.nlink = new_nlink;
            TreeNode::RegularFile(RegularFileNode {
                id: f.id,
                metadata: f.metadata.clone(),
                xattrs: f.xattrs.clone(),
                data: f.data.clone_ref(),
                nlink: new_nlink,
            })
        }
        Some(_) => {
            return Err(IngestError::HardlinkTarget(format!(
                "hardlink target is not a regular file: \"{}\"",
                String::from_utf8_lossy(target_path)
            )));
        }
        None => {
            return Err(IngestError::HardlinkTarget(
                String::from_utf8_lossy(target_path).into_owned(),
            ));
        }
    };

    tree.insert(link_path, node)?;

    Ok(())
}

/// Read an entry body into memory. The buffer grows with the bytes that
/// arrive; it is never sized from the declared size, which a hostile header
/// can set to anything.
fn read_entry_to_memory(entry: &mut impl Read, size: u64) -> Result<Vec<u8>, IngestError> {
    let mut data = Vec::new();
    entry
        .take(size)
        .read_to_end(&mut data)
        .map_err(IngestError::Io)?;
    if data.len() as u64 != size {
        return Err(IngestError::Io(std::io::Error::new(
            ErrorKind::UnexpectedEof,
            "failed to fill whole buffer",
        )));
    }
    Ok(data)
}

fn stream_entry_to_spool(
    entry: &mut impl Read,
    size: u64,
    spool: &mut DataSpool,
    read_buf: &mut Vec<u8>,
    write_buf: &mut Vec<u8>,
) -> Result<FileData, IngestError> {
    let offset = spool.current_offset();
    let mut remaining = size;

    if read_buf.len() != ENTRY_READ_CHUNK_SIZE {
        read_buf.resize(ENTRY_READ_CHUNK_SIZE, 0);
    }
    if write_buf.capacity() < SPOOL_WRITE_BUFFER_SIZE {
        write_buf.reserve(SPOOL_WRITE_BUFFER_SIZE - write_buf.capacity());
    }
    write_buf.clear();

    while remaining > 0 {
        let to_read = remaining.min(read_buf.len() as u64) as usize;
        entry
            .read_exact(&mut read_buf[..to_read])
            .map_err(IngestError::Io)?;

        if write_buf.len() + to_read > SPOOL_WRITE_BUFFER_SIZE && !write_buf.is_empty() {
            spool.write_chunk(write_buf).map_err(IngestError::Io)?;
            write_buf.clear();
        }

        write_buf.extend_from_slice(&read_buf[..to_read]);
        remaining -= to_read as u64;
    }

    if !write_buf.is_empty() {
        spool.write_chunk(write_buf).map_err(IngestError::Io)?;
        write_buf.clear();
    }

    Ok(spool.data_ref(offset, size))
}

//--------------------------------------------------------------------------------------------------
// Tests
//--------------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // ---- Path normalization tests ----

    #[test]
    fn normalize_strips_dot_slash_prefix() {
        let limits = ResourceLimits::default();
        let result = normalize_path(b"./foo/bar.txt", &limits).unwrap();
        assert_eq!(result, Some(b"foo/bar.txt".to_vec()));
    }

    #[test]
    fn normalize_strips_bare_dot() {
        let limits = ResourceLimits::default();
        let result = normalize_path(b".", &limits).unwrap();
        assert_eq!(result, None);
    }

    #[test]
    fn normalize_strips_dot_slash_only() {
        let limits = ResourceLimits::default();
        let result = normalize_path(b"./", &limits).unwrap();
        assert_eq!(result, None);
    }

    #[test]
    fn normalize_strips_absolute_path_prefix() {
        let limits = ResourceLimits::default();
        let result = normalize_path(b"/etc/passwd", &limits).unwrap();
        assert_eq!(result, Some(b"etc/passwd".to_vec()));
    }

    #[test]
    fn normalize_skips_bare_root_path() {
        let limits = ResourceLimits::default();
        let result = normalize_path(b"/", &limits).unwrap();
        assert_eq!(result, None);
    }

    #[test]
    fn normalize_rejects_dotdot() {
        let limits = ResourceLimits::default();
        let result = normalize_path(b"foo/../etc/passwd", &limits);
        assert!(matches!(result, Err(IngestError::PathTraversal(_))));
    }

    #[test]
    fn normalize_rejects_leading_dotdot() {
        let limits = ResourceLimits::default();
        let result = normalize_path(b"../etc/passwd", &limits);
        assert!(matches!(result, Err(IngestError::PathTraversal(_))));
    }

    #[test]
    fn normalize_allows_dotdot_in_filename() {
        // A file literally named "..foo" is fine, only bare ".." is rejected.
        let limits = ResourceLimits::default();
        let result = normalize_path(b"dir/..foo", &limits).unwrap();
        assert_eq!(result, Some(b"dir/..foo".to_vec()));
    }

    #[test]
    fn normalize_enforces_path_length() {
        let limits = ResourceLimits {
            max_path_length: 10,
            ..ResourceLimits::default()
        };
        let result = normalize_path(b"a/very/long/path/here", &limits);
        assert!(matches!(result, Err(IngestError::PathTooLong(_))));
    }

    #[test]
    fn normalize_enforces_path_depth() {
        let limits = ResourceLimits {
            max_path_depth: 2,
            ..ResourceLimits::default()
        };
        let result = normalize_path(b"a/b/c", &limits);
        assert!(matches!(result, Err(IngestError::PathTooDeep(_))));
    }

    #[test]
    fn normalize_strips_trailing_slash() {
        let limits = ResourceLimits::default();
        let result = normalize_path(b"./foo/bar/", &limits).unwrap();
        assert_eq!(result, Some(b"foo/bar".to_vec()));
    }

    // ---- Compression detection tests ----

    #[test]
    fn detect_gzip_magic() {
        assert_eq!(
            Compression::detect(&[0x1F, 0x8B, 0x08, 0x00]),
            Compression::Gzip
        );
    }

    #[test]
    fn detect_zstd_magic() {
        assert_eq!(
            Compression::detect(&[0x28, 0xB5, 0x2F, 0xFD, 0x00]),
            Compression::Zstd
        );
    }

    #[test]
    fn detect_none_for_unknown() {
        assert_eq!(
            Compression::detect(&[0x00, 0x00, 0x00, 0x00]),
            Compression::None
        );
    }

    #[test]
    fn detect_none_for_short_input() {
        assert_eq!(Compression::detect(&[0x1F]), Compression::None);
    }

    #[test]
    fn detect_zstd_takes_priority_over_partial_gzip() {
        // Zstd magic is checked first (4 bytes), then gzip (2 bytes).
        assert_eq!(
            Compression::detect(&[0x28, 0xB5, 0x2F, 0xFD]),
            Compression::Zstd
        );
    }

    #[test]
    fn from_media_type_gzip() {
        assert_eq!(
            Compression::from_media_type("application/vnd.oci.image.layer.v1.tar+gzip"),
            Compression::Gzip
        );
    }

    #[test]
    fn from_media_type_zstd() {
        assert_eq!(
            Compression::from_media_type("application/vnd.oci.image.layer.v1.tar+zstd"),
            Compression::Zstd
        );
    }

    #[test]
    fn from_media_type_plain() {
        assert_eq!(
            Compression::from_media_type("application/vnd.oci.image.layer.v1.tar"),
            Compression::None
        );
    }

    // ---- Whiteout classification tests ----

    #[test]
    fn classify_whiteout_opaque() {
        assert!(matches!(
            classify_whiteout(b".wh..wh..opq"),
            WhiteoutKind::Opaque
        ));
    }

    #[test]
    fn classify_whiteout_regular() {
        match classify_whiteout(b".wh.myfile") {
            WhiteoutKind::File(name) => assert_eq!(name, b"myfile"),
            _ => panic!("expected WhiteoutKind::File"),
        }
    }

    #[test]
    fn classify_whiteout_empty_name() {
        // `.wh.` alone with nothing after should not be treated as a whiteout file.
        assert!(matches!(classify_whiteout(b".wh."), WhiteoutKind::None));
    }

    #[test]
    fn classify_whiteout_normal_file() {
        assert!(matches!(
            classify_whiteout(b"regular_file.txt"),
            WhiteoutKind::None
        ));
    }

    // ---- Helper function tests ----

    #[test]
    fn path_filename_with_parent() {
        assert_eq!(path_filename(b"a/b/c.txt"), b"c.txt");
    }

    #[test]
    fn path_filename_no_parent() {
        assert_eq!(path_filename(b"file.txt"), b"file.txt");
    }

    #[test]
    fn path_parent_with_components() {
        assert_eq!(path_parent(b"a/b/c.txt"), b"a/b");
    }

    #[test]
    fn path_parent_single_component() {
        assert_eq!(path_parent(b"file.txt"), b"");
    }

    #[test]
    fn replace_filename_with_parent() {
        assert_eq!(
            replace_filename(b"dir/.wh.myfile", b"myfile"),
            b"dir/myfile"
        );
    }

    #[test]
    fn replace_filename_no_parent() {
        assert_eq!(replace_filename(b".wh.myfile", b"myfile"), b"myfile");
    }

    // ---- Integration tests using the sync `tar` crate to build test archives ----

    use ::tar as sync_tar;
    use async_compression::tokio::write::GzipEncoder;
    use tempfile::tempdir;
    use tokio::io::AsyncWriteExt;

    fn build_tar(build: impl FnOnce(&mut sync_tar::Builder<Vec<u8>>)) -> Vec<u8> {
        let mut builder = sync_tar::Builder::new(Vec::new());
        build(&mut builder);
        builder.into_inner().unwrap()
    }

    async fn gzip(data: &[u8]) -> Vec<u8> {
        let mut encoder = GzipEncoder::new(Vec::new());
        encoder.write_all(data).await.unwrap();
        encoder.shutdown().await.unwrap();
        encoder.into_inner()
    }

    #[tokio::test]
    async fn ingest_regular_file() {
        let data = build_tar(|b| {
            let content = b"hello world";
            let mut header = sync_tar::Header::new_gnu();
            header.set_path("foo.txt").unwrap();
            header.set_size(content.len() as u64);
            header.set_entry_type(sync_tar::EntryType::Regular);
            header.set_mode(0o644);
            header.set_uid(1000);
            header.set_gid(1000);
            header.set_mtime(1234567890);
            header.set_cksum();
            b.append(&header, &content[..]).unwrap();
        });

        let limits = ResourceLimits::default();
        let tree = ingest_tar(std::io::Cursor::new(data), &limits, None)
            .await
            .unwrap();

        match tree.get(b"foo.txt").unwrap() {
            TreeNode::RegularFile(f) => {
                assert_eq!(f.data, FileData::Memory(b"hello world".to_vec()));
                assert_eq!(f.metadata.uid, 1000);
                assert_eq!(f.metadata.gid, 1000);
                assert_eq!(f.metadata.mode, 0o644);
                assert_eq!(f.metadata.mtime, 1234567890);
                assert_eq!(f.nlink, 1);
            }
            _ => panic!("expected regular file"),
        }
    }

    #[tokio::test]
    async fn ingest_gzip_large_file_without_spool() {
        let content = (0..SPOOL_THRESHOLD as usize * 8 + 13)
            .map(|i| (i % 251) as u8)
            .collect::<Vec<_>>();
        let tar_data = build_tar(|b| {
            let mut header = sync_tar::Header::new_gnu();
            header.set_path("large.bin").unwrap();
            header.set_size(content.len() as u64);
            header.set_entry_type(sync_tar::EntryType::Regular);
            header.set_mode(0o644);
            header.set_cksum();
            b.append(&header, content.as_slice()).unwrap();
        });
        let data = gzip(&tar_data).await;

        let limits = ResourceLimits::default();
        let result =
            ingest_compressed_tar(std::io::Cursor::new(data), Compression::Gzip, &limits, None)
                .await
                .unwrap();

        match result.tree.get(b"large.bin").unwrap() {
            TreeNode::RegularFile(f) => {
                assert_eq!(f.data, FileData::Memory(content));
            }
            _ => panic!("expected regular file"),
        }
    }

    #[tokio::test]
    async fn ingest_large_file_spools_to_disk() {
        let content = vec![b'x'; SPOOL_THRESHOLD as usize + 1];
        let data = build_tar(|b| {
            let mut header = sync_tar::Header::new_gnu();
            header.set_path("large.bin").unwrap();
            header.set_size(content.len() as u64);
            header.set_entry_type(sync_tar::EntryType::Regular);
            header.set_mode(0o644);
            header.set_cksum();
            b.append(&header, content.as_slice()).unwrap();
        });

        let tempdir = tempdir().unwrap();
        let spool_path = tempdir.path().join("layer.spool");
        let spool = DataSpool::new(&spool_path).unwrap();
        let limits = ResourceLimits::default();
        let tree = ingest_tar(std::io::Cursor::new(data), &limits, Some(spool))
            .await
            .unwrap();

        match tree.get(b"large.bin").unwrap() {
            TreeNode::RegularFile(f) => {
                assert!(matches!(f.data, FileData::Spool { .. }));
                assert_eq!(f.data.read_all().unwrap(), content);
            }
            _ => panic!("expected regular file"),
        }
    }

    #[tokio::test]
    async fn ingest_multiple_large_files_spools_to_distinct_ranges() {
        let first = vec![b'a'; SPOOL_THRESHOLD as usize + 17];
        let second = vec![b'b'; SPOOL_THRESHOLD as usize * 2 + 31];
        let data = build_tar(|b| {
            for (path, content) in [
                ("first.bin", first.as_slice()),
                ("second.bin", second.as_slice()),
            ] {
                let mut header = sync_tar::Header::new_gnu();
                header.set_path(path).unwrap();
                header.set_size(content.len() as u64);
                header.set_entry_type(sync_tar::EntryType::Regular);
                header.set_mode(0o644);
                header.set_cksum();
                b.append(&header, content).unwrap();
            }
        });

        let tempdir = tempdir().unwrap();
        let spool_path = tempdir.path().join("layer.spool");
        let spool = DataSpool::new(&spool_path).unwrap();
        let limits = ResourceLimits::default();
        let tree = ingest_tar(std::io::Cursor::new(data), &limits, Some(spool))
            .await
            .unwrap();

        for (path, content) in [
            (b"first.bin".as_slice(), first),
            (b"second.bin".as_slice(), second),
        ] {
            match tree.get(path).unwrap() {
                TreeNode::RegularFile(f) => {
                    assert!(matches!(f.data, FileData::Spool { .. }));
                    assert_eq!(f.data.read_all().unwrap(), content);
                }
                _ => panic!("expected regular file"),
            }
        }
    }

    #[tokio::test]
    async fn ingest_directory() {
        let data = build_tar(|b| {
            let mut header = sync_tar::Header::new_gnu();
            header.set_path("mydir/").unwrap();
            header.set_size(0);
            header.set_entry_type(sync_tar::EntryType::Directory);
            header.set_mode(0o755);
            header.set_cksum();
            b.append(&header, &[] as &[u8]).unwrap();
        });

        let limits = ResourceLimits::default();
        let tree = ingest_tar(std::io::Cursor::new(data), &limits, None)
            .await
            .unwrap();

        match tree.get(b"mydir").unwrap() {
            TreeNode::Directory(d) => {
                assert_eq!(d.metadata.mode, 0o755);
            }
            _ => panic!("expected directory"),
        }
    }

    #[tokio::test]
    async fn ingest_symlink() {
        let data = build_tar(|b| {
            let mut header = sync_tar::Header::new_gnu();
            header.set_path("link").unwrap();
            header.set_size(0);
            header.set_entry_type(sync_tar::EntryType::Symlink);
            header.set_link_name("/usr/bin/target").unwrap();
            header.set_mode(0o777);
            header.set_cksum();
            b.append(&header, &[] as &[u8]).unwrap();
        });

        let limits = ResourceLimits::default();
        let tree = ingest_tar(std::io::Cursor::new(data), &limits, None)
            .await
            .unwrap();

        match tree.get(b"link").unwrap() {
            TreeNode::Symlink(s) => {
                assert_eq!(s.target, b"/usr/bin/target");
            }
            _ => panic!("expected symlink"),
        }
    }

    #[tokio::test]
    async fn ingest_hardlink() {
        let data = build_tar(|b| {
            // First: the original file.
            let content = b"shared data";
            let mut header = sync_tar::Header::new_gnu();
            header.set_path("original.txt").unwrap();
            header.set_size(content.len() as u64);
            header.set_entry_type(sync_tar::EntryType::Regular);
            header.set_mode(0o644);
            header.set_cksum();
            b.append(&header, &content[..]).unwrap();

            // Second: a hardlink to the original.
            let mut header = sync_tar::Header::new_gnu();
            header.set_path("hardlink.txt").unwrap();
            header.set_size(0);
            header.set_entry_type(sync_tar::EntryType::Link);
            header.set_link_name("original.txt").unwrap();
            header.set_cksum();
            b.append(&header, &[] as &[u8]).unwrap();
        });

        let limits = ResourceLimits::default();
        let tree = ingest_tar(std::io::Cursor::new(data), &limits, None)
            .await
            .unwrap();

        // Both should exist with the same data, inode identity, and nlink=2.
        let original = match tree.get(b"original.txt").unwrap() {
            TreeNode::RegularFile(f) => f,
            _ => panic!("expected regular file"),
        };
        let hardlink = match tree.get(b"hardlink.txt").unwrap() {
            TreeNode::RegularFile(f) => f,
            _ => panic!("expected regular file"),
        };

        assert_eq!(original.data, FileData::Memory(b"shared data".to_vec()));
        assert_eq!(hardlink.data, FileData::Memory(b"shared data".to_vec()));
        assert_eq!(original.id, hardlink.id);
        assert_eq!(original.nlink, 2);
        assert_eq!(hardlink.nlink, 2);
        assert_eq!(tree.total_data_size(), b"shared data".len() as u64);
    }

    #[tokio::test]
    async fn ingest_hardlink_chain_refreshes_link_counts() {
        let data = build_tar(|b| {
            let content = b"shared data";
            let mut header = sync_tar::Header::new_gnu();
            header.set_path("original.txt").unwrap();
            header.set_size(content.len() as u64);
            header.set_entry_type(sync_tar::EntryType::Regular);
            header.set_mode(0o644);
            header.set_cksum();
            b.append(&header, &content[..]).unwrap();

            for (link_name, target_name) in [
                ("hardlink-a.txt", "original.txt"),
                ("hardlink-b.txt", "hardlink-a.txt"),
            ] {
                let mut header = sync_tar::Header::new_gnu();
                header.set_path(link_name).unwrap();
                header.set_size(0);
                header.set_entry_type(sync_tar::EntryType::Link);
                header.set_link_name(target_name).unwrap();
                header.set_cksum();
                b.append(&header, &[] as &[u8]).unwrap();
            }
        });

        let limits = ResourceLimits::default();
        let tree = ingest_tar(std::io::Cursor::new(data), &limits, None)
            .await
            .unwrap();

        let ids = ["original.txt", "hardlink-a.txt", "hardlink-b.txt"].map(|path| {
            match tree.get(path.as_bytes()).unwrap() {
                TreeNode::RegularFile(f) => {
                    assert_eq!(f.nlink, 3);
                    f.id
                }
                _ => panic!("expected regular file"),
            }
        });

        assert_eq!(ids[0], ids[1]);
        assert_eq!(ids[0], ids[2]);
        assert_eq!(tree.total_data_size(), b"shared data".len() as u64);
    }

    #[tokio::test]
    async fn ingest_hardlink_missing_target() {
        let data = build_tar(|b| {
            let mut header = sync_tar::Header::new_gnu();
            header.set_path("bad_link.txt").unwrap();
            header.set_size(0);
            header.set_entry_type(sync_tar::EntryType::Link);
            header.set_link_name("nonexistent.txt").unwrap();
            header.set_cksum();
            b.append(&header, &[] as &[u8]).unwrap();
        });

        let limits = ResourceLimits::default();
        let result = ingest_tar(std::io::Cursor::new(data), &limits, None).await;
        assert!(matches!(result, Err(IngestError::HardlinkTarget(_))));
    }

    #[tokio::test]
    async fn ingest_whiteout_file() {
        let data = build_tar(|b| {
            // A whiteout marker for "deleted_file".
            let mut header = sync_tar::Header::new_gnu();
            header.set_path("dir/.wh.deleted_file").unwrap();
            header.set_size(0);
            header.set_entry_type(sync_tar::EntryType::Regular);
            header.set_mode(0o644);
            header.set_cksum();
            b.append(&header, &[] as &[u8]).unwrap();
        });

        let limits = ResourceLimits::default();
        let tree = ingest_tar(std::io::Cursor::new(data), &limits, None)
            .await
            .unwrap();

        // Should be inserted as a char device at "dir/deleted_file".
        match tree.get(b"dir/deleted_file").unwrap() {
            TreeNode::CharDevice(dev) => {
                assert_eq!(dev.major, 0);
                assert_eq!(dev.minor, 0);
            }
            _ => panic!("expected char device (whiteout)"),
        }

        // The .wh. file itself should not exist.
        assert!(tree.get(b"dir/.wh.deleted_file").is_none());
    }

    #[tokio::test]
    async fn ingest_opaque_whiteout() {
        let data = build_tar(|b| {
            // Create the parent directory first.
            let mut header = sync_tar::Header::new_gnu();
            header.set_path("mydir/").unwrap();
            header.set_size(0);
            header.set_entry_type(sync_tar::EntryType::Directory);
            header.set_mode(0o755);
            header.set_cksum();
            b.append(&header, &[] as &[u8]).unwrap();

            // Opaque whiteout.
            let mut header = sync_tar::Header::new_gnu();
            header.set_path("mydir/.wh..wh..opq").unwrap();
            header.set_size(0);
            header.set_entry_type(sync_tar::EntryType::Regular);
            header.set_mode(0o644);
            header.set_cksum();
            b.append(&header, &[] as &[u8]).unwrap();
        });

        let limits = ResourceLimits::default();
        let tree = ingest_tar(std::io::Cursor::new(data), &limits, None)
            .await
            .unwrap();

        // The parent directory should have the opaque xattr.
        match tree.get(b"mydir").unwrap() {
            TreeNode::Directory(d) => {
                assert!(
                    d.xattrs
                        .iter()
                        .any(|x| x.name == OPAQUE_XATTR_NAME && x.value == OPAQUE_XATTR_VALUE)
                );
            }
            _ => panic!("expected directory"),
        }
    }

    #[tokio::test]
    async fn ingest_accepts_absolute_path_in_tar() {
        let data = build_tar(|b| {
            let mut header = sync_tar::Header::new_gnu();
            header.set_size(0);
            header.set_entry_type(sync_tar::EntryType::Regular);
            header.set_mode(0o644);
            // Write path bytes directly into the GNU header name field
            // to bypass the tar crate's absolute-path rejection.
            let path_bytes = b"/etc/passwd";
            let gnu = header.as_gnu_mut().unwrap();
            gnu.name[..path_bytes.len()].copy_from_slice(path_bytes);
            gnu.name[path_bytes.len()] = 0;
            header.set_cksum();
            b.append(&header, &[] as &[u8]).unwrap();
        });

        let limits = ResourceLimits::default();
        let tree = ingest_tar(std::io::Cursor::new(data), &limits, None)
            .await
            .unwrap();
        assert!(matches!(
            tree.get(b"etc/passwd"),
            Some(TreeNode::RegularFile(_))
        ));
    }

    #[tokio::test]
    async fn ingest_accepts_absolute_hardlink_target() {
        let data = build_tar(|b| {
            let content = b"shared data";
            let mut header = sync_tar::Header::new_gnu();
            header.set_size(content.len() as u64);
            header.set_entry_type(sync_tar::EntryType::Regular);
            header.set_mode(0o644);
            let path_bytes = b"/nix/store/original.txt";
            let gnu = header.as_gnu_mut().unwrap();
            gnu.name[..path_bytes.len()].copy_from_slice(path_bytes);
            gnu.name[path_bytes.len()] = 0;
            header.set_cksum();
            b.append(&header, &content[..]).unwrap();

            let mut header = sync_tar::Header::new_gnu();
            header.set_size(0);
            header.set_entry_type(sync_tar::EntryType::Link);
            let path_bytes = b"/nix/store/link.txt";
            let link_bytes = b"/nix/store/original.txt";
            let gnu = header.as_gnu_mut().unwrap();
            gnu.name[..path_bytes.len()].copy_from_slice(path_bytes);
            gnu.name[path_bytes.len()] = 0;
            gnu.linkname[..link_bytes.len()].copy_from_slice(link_bytes);
            gnu.linkname[link_bytes.len()] = 0;
            header.set_cksum();
            b.append(&header, &[] as &[u8]).unwrap();
        });

        let limits = ResourceLimits::default();
        let tree = ingest_tar(std::io::Cursor::new(data), &limits, None)
            .await
            .unwrap();

        match tree.get(b"nix/store/link.txt").unwrap() {
            TreeNode::RegularFile(f) => {
                assert_eq!(f.data, FileData::Memory(b"shared data".to_vec()));
                assert_eq!(f.nlink, 2);
            }
            _ => panic!("expected regular file"),
        }
    }

    #[tokio::test]
    async fn ingest_entry_count_exceeded() {
        let data = build_tar(|b| {
            for i in 0..5 {
                let mut header = sync_tar::Header::new_gnu();
                header.set_path(format!("file{i}.txt")).unwrap();
                header.set_size(0);
                header.set_entry_type(sync_tar::EntryType::Regular);
                header.set_mode(0o644);
                header.set_cksum();
                b.append(&header, &[] as &[u8]).unwrap();
            }
        });

        let limits = ResourceLimits {
            max_entry_count: 3,
            ..ResourceLimits::default()
        };
        let result = ingest_tar(std::io::Cursor::new(data), &limits, None).await;
        assert!(matches!(result, Err(IngestError::EntryCountExceeded)));
    }

    #[tokio::test]
    async fn ingest_file_too_large() {
        let data = build_tar(|b| {
            let content = vec![0u8; 1024];
            let mut header = sync_tar::Header::new_gnu();
            header.set_path("big.bin").unwrap();
            header.set_size(content.len() as u64);
            header.set_entry_type(sync_tar::EntryType::Regular);
            header.set_mode(0o644);
            header.set_cksum();
            b.append(&header, &content[..]).unwrap();
        });

        let limits = ResourceLimits {
            max_file_size: 512,
            ..ResourceLimits::default()
        };
        let result = ingest_tar(std::io::Cursor::new(data), &limits, None).await;
        assert!(matches!(result, Err(IngestError::FileTooLarge(_))));
    }

    #[tokio::test]
    async fn ingest_dot_slash_prefix_stripped() {
        let data = build_tar(|b| {
            let content = b"data";
            let mut header = sync_tar::Header::new_gnu();
            header.set_path("./foo/bar.txt").unwrap();
            header.set_size(content.len() as u64);
            header.set_entry_type(sync_tar::EntryType::Regular);
            header.set_mode(0o644);
            header.set_cksum();
            b.append(&header, &content[..]).unwrap();
        });

        let limits = ResourceLimits::default();
        let tree = ingest_tar(std::io::Cursor::new(data), &limits, None)
            .await
            .unwrap();

        // Should be accessible without the ./ prefix.
        assert!(tree.get(b"foo/bar.txt").is_some());
    }

    #[tokio::test]
    async fn ingest_root_entry_skipped() {
        let data = build_tar(|b| {
            // Root directory entry `./`
            let mut header = sync_tar::Header::new_gnu();
            header.set_path("./").unwrap();
            header.set_size(0);
            header.set_entry_type(sync_tar::EntryType::Directory);
            header.set_mode(0o755);
            header.set_cksum();
            b.append(&header, &[] as &[u8]).unwrap();

            // A regular file.
            let content = b"data";
            let mut header = sync_tar::Header::new_gnu();
            header.set_path("./file.txt").unwrap();
            header.set_size(content.len() as u64);
            header.set_entry_type(sync_tar::EntryType::Regular);
            header.set_mode(0o644);
            header.set_cksum();
            b.append(&header, &content[..]).unwrap();
        });

        let limits = ResourceLimits::default();
        let tree = ingest_tar(std::io::Cursor::new(data), &limits, None)
            .await
            .unwrap();

        // The root entry should not appear as a named node.
        // Only the file should exist.
        assert_eq!(tree.node_count(), 1);
        assert!(tree.get(b"file.txt").is_some());
    }

    // ---- Hostile layers ----
    //
    // Layers come from registries we do not control. These archives are built
    // record by record so the paths, sizes and link names are exactly what an
    // attacker would write, without the builder's own validation.

    /// A GNU header with raw `name` and `linkname` bytes and the given size.
    fn raw_header(
        kind: sync_tar::EntryType,
        name: &[u8],
        link: Option<&[u8]>,
        size: u64,
    ) -> sync_tar::Header {
        let mut header = sync_tar::Header::new_gnu();
        header.set_entry_type(kind);
        header.set_mode(0o644);
        header.set_size(size);
        let gnu = header.as_gnu_mut().unwrap();
        gnu.name[..name.len()].copy_from_slice(name);
        if let Some(link) = link {
            gnu.linkname[..link.len()].copy_from_slice(link);
        }
        header.set_cksum();
        header
    }

    fn raw_entry(
        b: &mut sync_tar::Builder<Vec<u8>>,
        kind: sync_tar::EntryType,
        name: &[u8],
        link: Option<&[u8]>,
        data: &[u8],
    ) {
        let header = raw_header(kind, name, link, data.len() as u64);
        b.append(&header, data).unwrap();
    }

    fn raw_file(b: &mut sync_tar::Builder<Vec<u8>>, name: &[u8], data: &[u8]) {
        raw_entry(b, sync_tar::EntryType::Regular, name, None, data);
    }

    /// A PAX extended header (`x`) carrying `records` for the next member.
    fn pax_header(b: &mut sync_tar::Builder<Vec<u8>>, records: &[(&str, &[u8])]) {
        let mut data = Vec::new();
        for (key, value) in records {
            let body = 1 + key.len() + 1 + value.len() + 1;
            let mut len = body + 1;
            while format!("{len}").len() + body != len {
                len += 1;
            }
            data.extend_from_slice(format!("{len} {key}=").as_bytes());
            data.extend_from_slice(value);
            data.push(b'\n');
        }
        raw_entry(
            b,
            sync_tar::EntryType::XHeader,
            b"PaxHeaders/x",
            None,
            &data,
        );
    }

    /// A GNU long-name (`L`) record naming the next member.
    fn gnu_long_name(b: &mut sync_tar::Builder<Vec<u8>>, name: &[u8]) {
        let mut data = name.to_vec();
        data.push(0);
        raw_entry(
            b,
            sync_tar::EntryType::GNULongName,
            b"././@LongLink",
            None,
            &data,
        );
    }

    async fn ingest(data: Vec<u8>) -> Result<FileTree, IngestError> {
        ingest_tar(std::io::Cursor::new(data), &ResourceLimits::default(), None).await
    }

    fn file_bytes(tree: &FileTree, path: &[u8]) -> Vec<u8> {
        match tree.get(path) {
            Some(TreeNode::RegularFile(f)) => f.data.read_all().unwrap(),
            _ => panic!("expected regular file at {}", String::from_utf8_lossy(path)),
        }
    }

    #[tokio::test]
    async fn hostile_dotdot_file_is_refused() {
        let data = build_tar(|b| raw_file(b, b"../x", b"escape"));
        assert!(matches!(
            ingest(data).await,
            Err(IngestError::PathTraversal(_))
        ));
    }

    #[tokio::test]
    async fn hostile_dotdot_inside_path_is_refused() {
        let data = build_tar(|b| raw_file(b, b"a/b/../../../etc/passwd", b"escape"));
        assert!(matches!(
            ingest(data).await,
            Err(IngestError::PathTraversal(_))
        ));
    }

    #[tokio::test]
    async fn hostile_dot_slash_dotdot_is_refused() {
        let data = build_tar(|b| raw_file(b, b"./../x", b"escape"));
        assert!(matches!(
            ingest(data).await,
            Err(IngestError::PathTraversal(_))
        ));
    }

    #[tokio::test]
    async fn hostile_dotdot_directory_is_refused() {
        let data = build_tar(|b| raw_entry(b, sync_tar::EntryType::Directory, b"../d/", None, b""));
        assert!(matches!(
            ingest(data).await,
            Err(IngestError::PathTraversal(_))
        ));
    }

    #[tokio::test]
    async fn hostile_dotdot_symlink_is_refused() {
        let data = build_tar(|b| {
            raw_entry(
                b,
                sync_tar::EntryType::Symlink,
                b"../link",
                Some(b"/etc/passwd"),
                b"",
            )
        });
        assert!(matches!(
            ingest(data).await,
            Err(IngestError::PathTraversal(_))
        ));
    }

    #[tokio::test]
    async fn hostile_dotdot_whiteout_is_refused() {
        let data = build_tar(|b| raw_file(b, b"../.wh.victim", b""));
        assert!(matches!(
            ingest(data).await,
            Err(IngestError::PathTraversal(_))
        ));
    }

    #[tokio::test]
    async fn hostile_dotdot_hardlink_target_is_refused() {
        let data = build_tar(|b| {
            raw_file(b, b"a", b"data");
            raw_entry(
                b,
                sync_tar::EntryType::Link,
                b"b",
                Some(b"../etc/shadow"),
                b"",
            );
        });
        assert!(matches!(
            ingest(data).await,
            Err(IngestError::PathTraversal(_))
        ));
    }

    #[tokio::test]
    async fn hostile_dotdot_in_gnu_long_name_is_refused() {
        let name = format!("{}/../../../../etc/cron.d/x", "a".repeat(120));
        let data = build_tar(|b| {
            gnu_long_name(b, name.as_bytes());
            raw_file(b, b"innocent", b"escape");
        });
        assert!(matches!(
            ingest(data).await,
            Err(IngestError::PathTraversal(_))
        ));
    }

    #[tokio::test]
    async fn hostile_dotdot_in_pax_path_is_refused() {
        let data = build_tar(|b| {
            pax_header(b, &[("path", b"../../etc/x")]);
            raw_file(b, b"innocent", b"escape");
        });
        assert!(matches!(
            ingest(data).await,
            Err(IngestError::PathTraversal(_))
        ));
    }

    #[tokio::test]
    async fn hostile_absolute_paths_stay_inside_the_layer() {
        let data = build_tar(|b| {
            raw_file(b, b"/etc/shadow", b"one");
            raw_file(b, b"//root/.ssh/authorized_keys", b"two");
        });
        let tree = ingest(data).await.unwrap();
        assert_eq!(file_bytes(&tree, b"etc/shadow"), b"one");
        assert_eq!(file_bytes(&tree, b"root/.ssh/authorized_keys"), b"two");
        assert_eq!(tree.node_count(), 5);
    }

    #[tokio::test]
    async fn hostile_symlink_target_is_stored_not_followed() {
        let data = build_tar(|b| {
            raw_entry(
                b,
                sync_tar::EntryType::Symlink,
                b"passwd",
                Some(b"../../../../etc/passwd"),
                b"",
            )
        });
        let tree = ingest(data).await.unwrap();
        match tree.get(b"passwd") {
            Some(TreeNode::Symlink(link)) => assert_eq!(link.target, b"../../../../etc/passwd"),
            _ => panic!("expected symlink"),
        }
        assert_eq!(tree.node_count(), 1);
    }

    #[tokio::test]
    async fn hostile_write_through_symlink_is_refused() {
        let data = build_tar(|b| {
            raw_entry(b, sync_tar::EntryType::Symlink, b"etc", Some(b"/etc"), b"");
            raw_file(b, b"etc/passwd", b"root::0:0::/:/bin/sh");
        });
        assert!(matches!(
            ingest(data).await,
            Err(IngestError::Tree(FileTreeError::NotADirectory(_)))
        ));
    }

    #[tokio::test]
    async fn hostile_directory_below_symlink_is_refused() {
        let data = build_tar(|b| {
            raw_entry(b, sync_tar::EntryType::Symlink, b"up", Some(b"../.."), b"");
            raw_entry(b, sync_tar::EntryType::Directory, b"up/sub/", None, b"");
        });
        assert!(matches!(
            ingest(data).await,
            Err(IngestError::Tree(FileTreeError::NotADirectory(_)))
        ));
    }

    #[tokio::test]
    async fn hostile_symlink_over_populated_directory_is_refused() {
        let data = build_tar(|b| {
            raw_file(b, b"dir/file", b"data");
            raw_entry(b, sync_tar::EntryType::Symlink, b"dir", Some(b"/etc"), b"");
        });
        assert!(matches!(
            ingest(data).await,
            Err(IngestError::Tree(FileTreeError::EntryExists(_)))
        ));
    }

    #[tokio::test]
    async fn hostile_hardlink_to_host_file_is_refused() {
        let data = build_tar(|b| {
            raw_entry(
                b,
                sync_tar::EntryType::Link,
                b"shadow",
                Some(b"/etc/shadow"),
                b"",
            )
        });
        assert!(matches!(
            ingest(data).await,
            Err(IngestError::HardlinkTarget(_))
        ));
    }

    #[tokio::test]
    async fn hostile_hardlink_to_symlink_is_refused() {
        let data = build_tar(|b| {
            raw_entry(
                b,
                sync_tar::EntryType::Symlink,
                b"link",
                Some(b"/etc/shadow"),
                b"",
            );
            raw_entry(b, sync_tar::EntryType::Link, b"hard", Some(b"link"), b"");
        });
        assert!(matches!(
            ingest(data).await,
            Err(IngestError::HardlinkTarget(_))
        ));
    }

    #[tokio::test]
    async fn hostile_hardlink_to_directory_is_refused() {
        let data = build_tar(|b| {
            raw_entry(b, sync_tar::EntryType::Directory, b"dir/", None, b"");
            raw_entry(b, sync_tar::EntryType::Link, b"hard", Some(b"dir"), b"");
        });
        assert!(matches!(
            ingest(data).await,
            Err(IngestError::HardlinkTarget(_))
        ));
    }

    #[tokio::test]
    async fn hostile_pax_size_larger_than_ustar_size_is_honoured() {
        // The ustar field says 0; a reader that trusts it would parse the body
        // as the next header.
        let data = build_tar(|b| {
            pax_header(b, &[("size", b"5")]);
            let header = raw_header(sync_tar::EntryType::Regular, b"smuggled", None, 0);
            b.get_mut().extend_from_slice(header.as_bytes());
            b.get_mut().extend_from_slice(b"hello");
            b.get_mut().extend_from_slice(&[0; 507]);
            raw_file(b, b"next", b"after");
        });
        let tree = ingest(data).await.unwrap();
        assert_eq!(file_bytes(&tree, b"smuggled"), b"hello");
        assert_eq!(file_bytes(&tree, b"next"), b"after");
    }

    #[tokio::test]
    async fn hostile_pax_size_smaller_than_ustar_size_is_honoured() {
        // The ustar field claims 1024 bytes, which would swallow the next member.
        let data = build_tar(|b| {
            pax_header(b, &[("size", b"3")]);
            let header = raw_header(sync_tar::EntryType::Regular, b"short", None, 1024);
            b.get_mut().extend_from_slice(header.as_bytes());
            b.get_mut().extend_from_slice(b"abc");
            b.get_mut().extend_from_slice(&[0; 509]);
            raw_file(b, b"next", b"after");
        });
        let tree = ingest(data).await.unwrap();
        assert_eq!(file_bytes(&tree, b"short"), b"abc");
        assert_eq!(file_bytes(&tree, b"next"), b"after");
    }

    #[tokio::test]
    async fn hostile_pax_size_counts_against_the_file_limit() {
        let data = build_tar(|b| {
            pax_header(b, &[("size", b"4096")]);
            let header = raw_header(sync_tar::EntryType::Regular, b"big", None, 0);
            b.get_mut().extend_from_slice(header.as_bytes());
            b.get_mut().extend_from_slice(&[0; 4096]);
        });
        let limits = ResourceLimits {
            max_file_size: 1024,
            ..ResourceLimits::default()
        };
        let result = ingest_tar(std::io::Cursor::new(data), &limits, None).await;
        assert!(matches!(result, Err(IngestError::FileTooLarge(_))));
    }

    #[tokio::test]
    async fn hostile_huge_declared_size_is_not_reserved_up_front() {
        // 1 EiB declared, 5 bytes present. Reserving the declared size would
        // abort the process before the short read is noticed.
        let data = build_tar(|b| {
            let header = raw_header(sync_tar::EntryType::Regular, b"huge", None, 1 << 60);
            b.get_mut().extend_from_slice(header.as_bytes());
            b.get_mut().extend_from_slice(b"hello");
        });
        let limits = ResourceLimits {
            max_file_size: u64::MAX,
            max_total_size: u64::MAX,
            ..ResourceLimits::default()
        };
        let result = ingest_tar(std::io::Cursor::new(data), &limits, None).await;
        assert!(matches!(result, Err(IngestError::Io(e)) if e.kind() == ErrorKind::UnexpectedEof));
    }

    #[tokio::test]
    async fn hostile_huge_declared_size_with_spool_fails_cleanly() {
        let data = build_tar(|b| {
            let header = raw_header(sync_tar::EntryType::Regular, b"huge", None, 1 << 60);
            b.get_mut().extend_from_slice(header.as_bytes());
            b.get_mut().extend_from_slice(&[1; 4096]);
        });
        let limits = ResourceLimits {
            max_file_size: u64::MAX,
            max_total_size: u64::MAX,
            ..ResourceLimits::default()
        };
        let tempdir = tempdir().unwrap();
        let spool = DataSpool::new(&tempdir.path().join("layer.spool")).unwrap();
        let result = ingest_tar(std::io::Cursor::new(data), &limits, Some(spool)).await;
        assert!(matches!(result, Err(IngestError::Io(e)) if e.kind() == ErrorKind::UnexpectedEof));
    }

    #[tokio::test]
    async fn hostile_total_size_is_checked_before_reading() {
        let data = build_tar(|b| {
            raw_file(b, b"a", &[1; 600]);
            raw_file(b, b"b", &[2; 600]);
        });
        let limits = ResourceLimits {
            max_total_size: 1000,
            ..ResourceLimits::default()
        };
        let result = ingest_tar(std::io::Cursor::new(data), &limits, None).await;
        assert!(matches!(result, Err(IngestError::TotalSizeExceeded)));
    }

    #[tokio::test]
    async fn hostile_truncated_body_is_an_error() {
        let mut data = build_tar(|b| raw_file(b, b"file", &[7; 2048]));
        data.truncate(512 + 1000);
        assert!(matches!(ingest(data).await, Err(IngestError::Io(_))));
    }

    #[tokio::test]
    async fn hostile_symlink_target_over_limit_is_refused() {
        let target = "t/".repeat(40);
        let data = build_tar(|b| {
            let mut header = sync_tar::Header::new_gnu();
            header.set_entry_type(sync_tar::EntryType::Symlink);
            header.set_size(0);
            b.append_link(&mut header, "link", &target).unwrap();
        });
        let limits = ResourceLimits {
            max_symlink_target: 16,
            ..ResourceLimits::default()
        };
        let result = ingest_tar(std::io::Cursor::new(data), &limits, None).await;
        assert!(matches!(result, Err(IngestError::SymlinkTargetTooLong(_))));
    }

    #[tokio::test]
    async fn hostile_device_nodes_keep_major_and_minor() {
        let data = build_tar(|b| {
            for (kind, name, major, minor) in [
                (sync_tar::EntryType::Char, "dev/null", 1, 3),
                (sync_tar::EntryType::Block, "dev/sda1", 8, 1),
                (sync_tar::EntryType::Char, "dev/big", 4095, 1_048_575),
            ] {
                let mut header = sync_tar::Header::new_gnu();
                header.set_path(name).unwrap();
                header.set_entry_type(kind);
                header.set_size(0);
                header.set_device_major(major).unwrap();
                header.set_device_minor(minor).unwrap();
                header.set_cksum();
                b.append(&header, &[] as &[u8]).unwrap();
            }
        });
        let tree = ingest(data).await.unwrap();
        match tree.get(b"dev/null") {
            Some(TreeNode::CharDevice(dev)) => assert_eq!((dev.major, dev.minor), (1, 3)),
            _ => panic!("expected char device"),
        }
        match tree.get(b"dev/sda1") {
            Some(TreeNode::BlockDevice(dev)) => assert_eq!((dev.major, dev.minor), (8, 1)),
            _ => panic!("expected block device"),
        }
        match tree.get(b"dev/big") {
            Some(TreeNode::CharDevice(dev)) => {
                assert_eq!((dev.major, dev.minor), (4095, 1_048_575))
            }
            _ => panic!("expected char device"),
        }
    }

    #[tokio::test]
    async fn hostile_gnu_long_names_and_link_targets_are_read() {
        let long_dir = "d".repeat(90);
        let long_path = format!("{long_dir}/{}", "f".repeat(90));
        let long_target = format!("/opt/{}", "t".repeat(150));
        let data = build_tar(|b| {
            let mut header = sync_tar::Header::new_gnu();
            header.set_entry_type(sync_tar::EntryType::Regular);
            header.set_size(4);
            b.append_data(&mut header, &long_path, &b"long"[..])
                .unwrap();

            let mut header = sync_tar::Header::new_gnu();
            header.set_entry_type(sync_tar::EntryType::Symlink);
            header.set_size(0);
            b.append_link(&mut header, "link", &long_target).unwrap();

            let mut header = sync_tar::Header::new_gnu();
            header.set_entry_type(sync_tar::EntryType::Link);
            header.set_size(0);
            b.append_link(&mut header, "hard", &long_path).unwrap();
        });
        let tree = ingest(data).await.unwrap();
        assert_eq!(file_bytes(&tree, long_path.as_bytes()), b"long");
        assert_eq!(file_bytes(&tree, b"hard"), b"long");
        match tree.get(b"link") {
            Some(TreeNode::Symlink(link)) => assert_eq!(link.target, long_target.as_bytes()),
            _ => panic!("expected symlink"),
        }
    }

    /// An old-GNU sparse member (`S`) of `real_size` bytes with one data run.
    fn gnu_sparse(
        b: &mut sync_tar::Builder<Vec<u8>>,
        name: &str,
        real_size: u64,
        offset: u64,
        run: &[u8],
    ) {
        fn octal(field: &mut [u8; 12], value: u64) {
            field.copy_from_slice(format!("{value:011o}\0").as_bytes());
        }
        let mut header = sync_tar::Header::new_gnu();
        header.set_path(name).unwrap();
        header.set_entry_type(sync_tar::EntryType::GNUSparse);
        header.set_mode(0o644);
        header.set_size(run.len() as u64);
        let gnu = header.as_gnu_mut().unwrap();
        octal(&mut gnu.realsize, real_size);
        octal(&mut gnu.sparse[0].offset, offset);
        octal(&mut gnu.sparse[0].numbytes, run.len() as u64);
        // GNU tar ends the map with an empty run at the real size.
        octal(&mut gnu.sparse[1].offset, real_size);
        octal(&mut gnu.sparse[1].numbytes, 0);
        header.set_cksum();
        b.append(&header, run).unwrap();
    }

    #[tokio::test]
    async fn hostile_gnu_sparse_member_is_expanded() {
        let data = build_tar(|b| gnu_sparse(b, "sparse.img", 10_000, 4096, b"data"));
        let tree = ingest(data).await.unwrap();
        let mut expected = vec![0u8; 10_000];
        expected[4096..4100].copy_from_slice(b"data");
        assert_eq!(file_bytes(&tree, b"sparse.img"), expected);
    }

    #[tokio::test]
    async fn hostile_gnu_sparse_real_size_counts_against_the_file_limit() {
        // One archived byte claiming an 8 GiB file: the expanded size is what
        // lands in the tree, so it is what the limit applies to.
        let data = build_tar(|b| gnu_sparse(b, "bomb.img", (1 << 33) - 1, 0, b"x"));
        let limits = ResourceLimits {
            max_file_size: 1 << 20,
            ..ResourceLimits::default()
        };
        let result = ingest_tar(std::io::Cursor::new(data), &limits, None).await;
        assert!(matches!(result, Err(IngestError::FileTooLarge(_))));
    }
}
