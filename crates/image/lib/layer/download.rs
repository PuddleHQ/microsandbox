//! Layer download and blob-cache management.

use std::{
    fs::File,
    io::{self, Read},
    path::{Path, PathBuf},
    time::Instant,
};

use oci_client::client::{BlobResponse, SizedStream};
use sha2::{Digest as Sha2Digest, Sha256};
use tokio::io::AsyncWriteExt;

use crate::{
    cache::{GlobalCache, lock::CacheLockGuard},
    digest::Digest,
    error::{ImageError, ImageResult},
    registry::retry::with_retries,
};

//--------------------------------------------------------------------------------------------------
// Constants
//--------------------------------------------------------------------------------------------------

/// Minimum byte delta between per-layer download progress updates.
const DOWNLOAD_PROGRESS_EMIT_BYTES: u64 = 256 * 1024;

//--------------------------------------------------------------------------------------------------
// Types
//--------------------------------------------------------------------------------------------------

/// A single OCI layer handle with download state.
pub(crate) struct Layer {
    /// Compressed layer digest (from manifest).
    pub digest: Digest,
    /// Cached paths derived from the global cache.
    tar_path: PathBuf,
    download_lock_path: PathBuf,
    part_path: PathBuf,
}

enum DownloadStart {
    Fresh,
    Resume(u64),
    Complete,
}

//--------------------------------------------------------------------------------------------------
// Methods
//--------------------------------------------------------------------------------------------------

impl Layer {
    /// Create a new layer handle.
    pub fn new(digest: Digest, cache: &GlobalCache) -> Self {
        Self {
            tar_path: cache.tar_path(&digest),
            download_lock_path: cache.download_lock_path(&digest),
            part_path: cache.part_path(&digest),
            digest,
        }
    }

    /// Path to the compressed tarball.
    pub fn tar_path_ref(&self) -> PathBuf {
        self.tar_path.clone()
    }

    /// Download the layer blob to the cache.
    ///
    /// Uses cross-process `flock()` to prevent races. Supports resumption
    /// via partial `.part` files.
    pub async fn download(
        &self,
        client: &oci_client::Client,
        image_ref: &oci_client::Reference,
        expected_size: Option<u64>,
        force: bool,
        progress: Option<&crate::progress::PullProgressSender>,
        layer_index: usize,
    ) -> ImageResult<()> {
        let started_at = Instant::now();
        let tar_path = &self.tar_path;
        let part_path = &self.part_path;

        // Acquire cross-process download lock (non-blocking on async executor).
        let _guard = CacheLockGuard::acquire_async(&self.download_lock_path).await?;

        if force {
            remove_file_if_exists(tar_path)?;
            remove_file_if_exists(part_path)?;
        }

        let digest_display = self.digest.to_string();
        let digest_str: std::sync::Arc<str> = digest_display.as_str().into();

        // Re-check after lock — another process may have completed the download.
        match tokio::fs::metadata(tar_path).await {
            Ok(meta)
                if expected_size.is_some_and(|expected| meta.len() == expected)
                    || (expected_size.is_none() && meta.len() > 0) =>
            {
                if let Some(p) = progress {
                    p.send(crate::progress::PullProgress::LayerDownloadComplete {
                        layer_index,
                        digest: digest_str,
                        downloaded_bytes: expected_size.unwrap_or(0),
                    });
                }
                tracing::debug!(
                    layer_index,
                    digest = %digest_display,
                    elapsed_ms = started_at.elapsed().as_millis(),
                    "layer download reused cached tarball"
                );
                return Ok(());
            }
            Ok(_) | Err(_) => {}
        }

        // Stream the blob to a .part file.
        let expected_hex = self.digest.hex();

        // Run download-start determination (may hash a large .part file) off the executor.
        let part_path_for_start = part_path.clone();
        let expected_hex_owned = expected_hex.to_string();
        let download_start = tokio::task::spawn_blocking(move || {
            determine_download_start(&part_path_for_start, expected_size, &expected_hex_owned)
        })
        .await
        .map_err(|e| ImageError::Io(io::Error::other(e)))??;
        if matches!(download_start, DownloadStart::Complete) {
            tokio::fs::rename(part_path, tar_path)
                .await
                .map_err(|e| ImageError::Cache {
                    path: tar_path.clone(),
                    source: e,
                })?;

            if let Some(p) = progress {
                p.send(crate::progress::PullProgress::LayerDownloadComplete {
                    layer_index,
                    digest: digest_str,
                    downloaded_bytes: expected_size.unwrap_or(0),
                });
            }

            tracing::debug!(
                layer_index,
                digest = %digest_display,
                elapsed_ms = started_at.elapsed().as_millis(),
                "layer download resumed from completed part file"
            );

            return Ok(());
        }

        let (mut stream, mut file, mut downloaded): (SizedStream, tokio::fs::File, u64) =
            match download_start {
                DownloadStart::Fresh => {
                    let stream = with_retries("layer blob", || async {
                        Ok(client
                            .pull_blob_stream(image_ref, digest_display.as_str())
                            .await?)
                    })
                    .await?;
                    let file = tokio::fs::OpenOptions::new()
                        .create(true)
                        .truncate(true)
                        .write(true)
                        .open(part_path)
                        .await
                        .map_err(|e| ImageError::Cache {
                            path: part_path.clone(),
                            source: e,
                        })?;
                    (stream, file, 0)
                }
                DownloadStart::Resume(offset) => {
                    let blob = with_retries("layer blob", || async {
                        Ok(client
                            .pull_blob_stream_partial(
                                image_ref,
                                digest_display.as_str(),
                                offset,
                                None,
                            )
                            .await?)
                    })
                    .await?;

                    match blob {
                        BlobResponse::Partial(stream) => {
                            let file = tokio::fs::OpenOptions::new()
                                .create(true)
                                .append(true)
                                .open(part_path)
                                .await
                                .map_err(|e| ImageError::Cache {
                                    path: part_path.clone(),
                                    source: e,
                                })?;
                            (stream, file, offset)
                        }
                        BlobResponse::Full(stream) => {
                            let file = tokio::fs::OpenOptions::new()
                                .create(true)
                                .truncate(true)
                                .write(true)
                                .open(part_path)
                                .await
                                .map_err(|e| ImageError::Cache {
                                    path: part_path.clone(),
                                    source: e,
                                })?;
                            (stream, file, 0)
                        }
                    }
                }
                DownloadStart::Complete => unreachable!(),
            };
        let mut last_progress_bytes = downloaded;

        // Compute SHA-256 incrementally during download — avoids re-reading
        // the entire blob from disk for post-download verification.
        // For resumed downloads, we must hash the existing bytes first.
        let mut hasher = if downloaded > 0 {
            let part_path = part_path.clone();
            tokio::task::spawn_blocking(move || hash_file_hasher(&part_path))
                .await
                .map_err(|e| ImageError::Io(io::Error::other(e)))??
        } else {
            Sha256::new()
        };

        use futures::StreamExt;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            hasher.update(&chunk);
            file.write_all(&chunk)
                .await
                .map_err(|e| ImageError::Cache {
                    path: part_path.clone(),
                    source: e,
                })?;
            downloaded += chunk.len() as u64;

            let should_emit_progress = downloaded.saturating_sub(last_progress_bytes)
                >= DOWNLOAD_PROGRESS_EMIT_BYTES
                || expected_size.is_some_and(|total| downloaded >= total);

            if should_emit_progress {
                if let Some(p) = progress {
                    p.send(crate::progress::PullProgress::LayerDownloadProgress {
                        layer_index,
                        digest: digest_str.clone(),
                        downloaded_bytes: downloaded,
                        total_bytes: expected_size,
                    });
                }
                last_progress_bytes = downloaded;
            }
        }
        file.flush().await.map_err(|e| ImageError::Cache {
            path: part_path.clone(),
            source: e,
        })?;
        drop(file);

        // Verify compressed digest from the incremental hash.
        let actual_hash = hex::encode(hasher.finalize());
        if actual_hash != expected_hex {
            let _ = tokio::fs::remove_file(part_path).await;
            return Err(ImageError::DigestMismatch {
                digest: digest_display,
                expected: expected_hex.to_string(),
                actual: actual_hash,
            });
        }

        // Atomic rename .part -> final.
        tokio::fs::rename(part_path, tar_path)
            .await
            .map_err(|e| ImageError::Cache {
                path: tar_path.clone(),
                source: e,
            })?;

        if let Some(p) = progress {
            p.send(crate::progress::PullProgress::LayerDownloadComplete {
                layer_index,
                digest: digest_str,
                downloaded_bytes: downloaded,
            });
        }

        tracing::debug!(
            layer_index,
            digest = %digest_display,
            downloaded_bytes = downloaded,
            elapsed_ms = started_at.elapsed().as_millis(),
            "layer download completed"
        );

        Ok(())
    }
}

//--------------------------------------------------------------------------------------------------
// Functions: Helpers
//--------------------------------------------------------------------------------------------------

/// Compute the SHA-256 hex digest of a file.
fn compute_sha256_file(path: &Path) -> ImageResult<String> {
    Ok(hex::encode(hash_file_hasher(path)?.finalize()))
}

fn hash_file_hasher(path: &Path) -> ImageResult<Sha256> {
    let mut file = File::open(path).map_err(|e| ImageError::Cache {
        path: path.to_path_buf(),
        source: e,
    })?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf).map_err(|e| ImageError::Cache {
            path: path.to_path_buf(),
            source: e,
        })?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher)
}

fn remove_file_if_exists(path: &Path) -> ImageResult<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(ImageError::Cache {
            path: path.to_path_buf(),
            source: err,
        }),
    }
}

fn determine_download_start(
    part_path: &Path,
    expected_size: Option<u64>,
    expected_hex: &str,
) -> ImageResult<DownloadStart> {
    let part_size = match std::fs::metadata(part_path) {
        Ok(meta) => meta.len(),
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(DownloadStart::Fresh),
        Err(err) => {
            return Err(ImageError::Cache {
                path: part_path.to_path_buf(),
                source: err,
            });
        }
    };

    if part_size == 0 {
        return Ok(DownloadStart::Fresh);
    }

    if let Some(expected) = expected_size {
        if part_size > expected {
            let _ = std::fs::remove_file(part_path);
            return Ok(DownloadStart::Fresh);
        }

        if part_size == expected {
            let actual_hash = compute_sha256_file(part_path)?;
            if actual_hash == expected_hex {
                return Ok(DownloadStart::Complete);
            }

            let _ = std::fs::remove_file(part_path);
            return Ok(DownloadStart::Fresh);
        }
    }

    Ok(DownloadStart::Resume(part_size))
}

//--------------------------------------------------------------------------------------------------
// Tests
//--------------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::{DownloadStart, determine_download_start, remove_file_if_exists};

    #[test]
    fn test_determine_download_start_returns_fresh_when_part_missing() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("layer.part");

        let start = determine_download_start(&path, Some(10), "deadbeef").unwrap();

        assert!(matches!(start, DownloadStart::Fresh));
    }

    #[test]
    fn test_determine_download_start_resumes_partial_file() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("layer.part");
        std::fs::write(&path, b"hello").unwrap();

        let start = determine_download_start(&path, Some(10), "deadbeef").unwrap();

        assert!(matches!(start, DownloadStart::Resume(5)));
    }

    #[test]
    fn test_determine_download_start_resets_oversized_part_file() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("layer.part");
        std::fs::write(&path, b"hello world").unwrap();

        let start = determine_download_start(&path, Some(5), "deadbeef").unwrap();

        assert!(matches!(start, DownloadStart::Fresh));
        assert!(!path.exists());
    }

    #[test]
    fn test_determine_download_start_marks_complete_when_hash_matches() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("layer.part");
        std::fs::write(&path, b"hello").unwrap();
        let digest = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";

        let start = determine_download_start(&path, Some(5), digest).unwrap();

        assert!(matches!(start, DownloadStart::Complete));
    }

    #[test]
    fn test_determine_download_start_restarts_when_full_part_hash_mismatches() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("layer.part");
        std::fs::write(&path, b"hello").unwrap();

        let start = determine_download_start(&path, Some(5), "deadbeef").unwrap();

        assert!(matches!(start, DownloadStart::Fresh));
        assert!(!path.exists());
    }

    #[test]
    fn test_remove_file_if_exists_deletes_existing_file() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("layer.tar.gz");
        std::fs::write(&path, b"cached").unwrap();

        remove_file_if_exists(&path).unwrap();

        assert!(!path.exists());
    }

    #[test]
    fn test_remove_file_if_exists_ignores_missing_file() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("missing.tar.gz");

        remove_file_if_exists(&path).unwrap();

        assert!(!path.exists());
    }

    /// A plain-HTTP registry that serves one blob, answering the first `failures` requests for
    /// it with a 500. Returns its `host:port`, the blob's digest and the count of blob requests.
    async fn flaky_blob_server(
        blob: &'static [u8],
        failures: usize,
    ) -> (
        String,
        String,
        std::sync::Arc<std::sync::atomic::AtomicUsize>,
    ) {
        use std::sync::atomic::Ordering;

        use sha2::{Digest as _, Sha256};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let host = listener.local_addr().unwrap().to_string();
        let digest = format!("sha256:{}", hex::encode(Sha256::digest(blob)));
        let requests = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = requests.clone();
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let counter = counter.clone();
                tokio::spawn(async move {
                    let mut head = Vec::new();
                    let mut byte = [0u8; 1];
                    while !head.ends_with(b"\r\n\r\n") {
                        match stream.read(&mut byte).await {
                            Ok(1) => head.push(byte[0]),
                            _ => return,
                        }
                    }
                    let path = String::from_utf8_lossy(&head)
                        .split(' ')
                        .nth(1)
                        .unwrap_or_default()
                        .to_owned();
                    let (status, body): (&str, &[u8]) = if !path.contains("/blobs/") {
                        ("404 Not Found", b"")
                    } else if counter.fetch_add(1, Ordering::SeqCst) < failures {
                        ("500 Internal Server Error", b"busy")
                    } else {
                        ("200 OK", blob)
                    };
                    let response = format!(
                        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = stream.write_all(response.as_bytes()).await;
                    let _ = stream.write_all(body).await;
                });
            }
        });
        (host, digest, requests)
    }

    #[tokio::test]
    async fn test_layer_download_retries_a_blob_request_that_failed_on_the_server() {
        const BLOB: &[u8] = b"layer bytes";
        let (host, digest, requests) = flaky_blob_server(BLOB, 2).await;
        let temp = tempdir().unwrap();
        let cache = crate::cache::GlobalCache::new(temp.path()).unwrap();
        let client = oci_client::Client::new(oci_client::client::ClientConfig {
            protocol: oci_client::client::ClientProtocol::HttpsExcept(vec![host.clone()]),
            ..Default::default()
        });
        let reference: oci_client::Reference = format!("{host}/app:1").parse().unwrap();
        let layer = super::Layer::new(digest.parse().unwrap(), &cache);

        layer
            .download(&client, &reference, Some(BLOB.len() as u64), false, None, 0)
            .await
            .unwrap();

        assert_eq!(std::fs::read(layer.tar_path_ref()).unwrap(), BLOB);
        assert_eq!(requests.load(std::sync::atomic::Ordering::SeqCst), 3);
    }
}
