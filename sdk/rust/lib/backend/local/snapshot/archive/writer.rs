//! Async tar writer for snapshot export.
//!
//! The `tar` crate only writes to `std::io::Write`. The exporter streams into an async (often zstd) sink, so this writer emits the records itself: a header (preceded
//! by a GNU long-name record when the name does not fit), the data, and zero padding to the 512-byte block. Header encoding goes through `tar::Builder`, so the bytes
//! match what the `tar` crate writes for the same member.

use std::io;

use tar::Header;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};

//--------------------------------------------------------------------------------------------------
// Constants
//--------------------------------------------------------------------------------------------------

const BLOCK: u64 = 512;

/// Two zero blocks end a tar archive.
const END_OF_ARCHIVE: [u8; 1024] = [0; 1024];

//--------------------------------------------------------------------------------------------------
// Types
//--------------------------------------------------------------------------------------------------

/// Writes tar members to an async sink.
pub(super) struct TarWriter<W> {
    inner: W,
}

//--------------------------------------------------------------------------------------------------
// Methods
//--------------------------------------------------------------------------------------------------

impl<W: AsyncWrite + Unpin> TarWriter<W> {
    pub(super) fn new(inner: W) -> Self {
        Self { inner }
    }

    /// The sink, for records the exporter writes itself (old-GNU sparse members). Write whole 512-byte records only.
    pub(super) fn get_mut(&mut self) -> &mut W {
        &mut self.inner
    }

    /// Append one member named `path`. `header` must carry the data size; its path and checksum are set here.
    pub(super) async fn append_data<R: AsyncRead + Unpin>(
        &mut self,
        header: &mut Header,
        path: &str,
        mut data: R,
    ) -> io::Result<()> {
        let records = encode_header(header, path)?;
        self.inner.write_all(&records).await?;
        let written = tokio::io::copy(&mut data, &mut self.inner).await?;
        let padding = (BLOCK - written % BLOCK) % BLOCK;
        self.inner
            .write_all(&END_OF_ARCHIVE[..padding as usize])
            .await
    }

    /// Write the end-of-archive marker and return the sink.
    pub(super) async fn into_inner(mut self) -> io::Result<W> {
        self.inner.write_all(&END_OF_ARCHIVE).await?;
        Ok(self.inner)
    }
}

//--------------------------------------------------------------------------------------------------
// Functions
//--------------------------------------------------------------------------------------------------

/// Encode `header` for `path` as `tar::Builder` does: the header block, preceded by a GNU long-name record when `path` does not fit the name field.
fn encode_header(header: &mut Header, path: &str) -> io::Result<Vec<u8>> {
    let mut builder = tar::Builder::new(Vec::new());
    builder.append_data(header, path, io::empty())?;
    // Take the records before the builder's drop appends its end-of-archive marker.
    Ok(std::mem::take(builder.get_mut()))
}

//--------------------------------------------------------------------------------------------------
// Tests
//--------------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn sync_archive(members: &[(&str, &[u8])]) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        for (name, data) in members {
            let mut header = Header::new_gnu();
            header.set_mode(0o644);
            header.set_size(data.len() as u64);
            builder.append_data(&mut header, name, *data).unwrap();
        }
        builder.into_inner().unwrap()
    }

    async fn async_archive(members: &[(&str, &[u8])]) -> Vec<u8> {
        let mut writer = TarWriter::new(Vec::new());
        for (name, data) in members {
            let mut header = Header::new_gnu();
            header.set_mode(0o644);
            header.set_size(data.len() as u64);
            writer.append_data(&mut header, name, *data).await.unwrap();
        }
        writer.into_inner().await.unwrap()
    }

    #[tokio::test]
    async fn bytes_match_the_tar_crate_builder() {
        let long_name = format!("snapshots/{}/upper.ext4", "a".repeat(120));
        let members: [(&str, &[u8]); 4] = [
            ("archive.json", b"{}"),
            ("empty", b""),
            ("block", &[7u8; 512]),
            (&long_name, b"long name member"),
        ];
        let written = async_archive(&members).await;
        assert_eq!(written, sync_archive(&members));
        assert_eq!(written.len() % BLOCK as usize, 0);

        let mut archive = tar::Archive::new(written.as_slice());
        let names = archive
            .entries()
            .unwrap()
            .map(|entry| entry.unwrap().path_bytes().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            members
                .iter()
                .map(|(name, _)| name.as_bytes().to_vec())
                .collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn short_name_error_is_returned_without_writing() {
        let mut writer = TarWriter::new(Vec::new());
        let mut header = Header::new_gnu();
        header.set_size(0);
        let error = writer
            .append_data(&mut header, "../escape", tokio::io::empty())
            .await
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert!(writer.get_mut().is_empty());
    }
}
