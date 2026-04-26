use std::error::Error as StdError;
use std::fs::File;
use std::io::{Cursor, Read, Seek};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::task::{Context as TaskContext, Poll};

use anyhow::{Context, Result};
use aws_sdk_s3::primitives::{ByteStream, SdkBody};
use bytes::Bytes;
use http_body::{Body, Frame, SizeHint};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use uuid::Uuid;
use zip::ZipArchive;

use crate::types::{AppState, SourceArchive, SourceArchiveData};

use super::{MEMORY_ARCHIVE_THRESHOLD_BYTES, ZIP_ENTRY_READ_CHUNK_BYTES};

type BodyError = Box<dyn StdError + Send + Sync>;

pub(super) async fn download_source_zip(
    state: &AppState,
    bucket: &str,
    key: &str,
) -> Result<SourceArchive> {
    tracing::info!(bucket, key, "downloading source archive");

    let response = state
        .s3
        .get_object()
        .bucket(bucket)
        .key(key)
        .send()
        .await
        .with_context(|| format!("failed to download s3://{bucket}/{key}"))?;

    let content_length = response
        .content_length()
        .and_then(|size| usize::try_from(size).ok());
    if content_length.is_some_and(|size| size <= MEMORY_ARCHIVE_THRESHOLD_BYTES) {
        let mut body = response.body.into_async_read();
        let mut bytes = Vec::with_capacity(content_length.unwrap_or_default());
        body.read_to_end(&mut bytes)
            .await
            .context("failed to read source archive body into memory")?;

        tracing::info!(
            bucket,
            key,
            bytes = bytes.len(),
            "downloaded source archive into memory"
        );
        return Ok(SourceArchive::in_memory(bytes));
    }

    let archive_path = temporary_archive_path();
    let mut archive_file = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&archive_path)
        .await
        .with_context(|| {
            format!(
                "failed to create temporary source archive {}",
                archive_path.display()
            )
        })?;
    let mut body = response.body.into_async_read();

    if let Err(error) = tokio::io::copy(&mut body, &mut archive_file).await {
        let _ = tokio::fs::remove_file(&archive_path).await;
        return Err(error).context("failed to write source archive body to temporary file");
    }
    archive_file
        .flush()
        .await
        .context("failed to flush temporary source archive")?;

    Ok(SourceArchive::temporary_file(archive_path))
}

pub(super) fn open_zip_archive(archive: &SourceArchive) -> Result<ZipArchive<ArchiveReader>> {
    let reader = match archive.data.as_ref() {
        SourceArchiveData::InMemory(bytes) => ArchiveReader::Memory(Cursor::new(bytes.clone())),
        SourceArchiveData::TemporaryFile(path) => {
            let archive_file = File::open(path).with_context(|| {
                format!("failed to open temporary source archive {}", path.display())
            })?;
            ArchiveReader::File(archive_file)
        }
    };
    ZipArchive::new(reader).context("failed to open zip archive")
}

pub(super) fn zip_entry_body(
    archive: SourceArchive,
    entry_index: usize,
    content_length: u64,
) -> ByteStream {
    ByteStream::new(SdkBody::retryable(move || {
        zip_entry_sdk_body(archive.clone(), entry_index, content_length)
    }))
}

fn temporary_archive_path() -> PathBuf {
    let mut path = std::env::temp_dir();
    path.push(format!(
        "rust-bucket-deployment-source-{}.zip",
        Uuid::new_v4()
    ));
    path
}

fn zip_entry_sdk_body(archive: SourceArchive, entry_index: usize, content_length: u64) -> SdkBody {
    let (sender, receiver) = tokio::sync::mpsc::channel(1);

    tokio::task::spawn_blocking(move || {
        if let Err(error) = send_zip_entry_chunks(archive, entry_index, sender.clone()) {
            let _ = sender.blocking_send(Err(error));
        }
    });

    SdkBody::from_body_1_x(ReceiverBody {
        receiver: Mutex::new(receiver),
        content_length,
    })
}

fn send_zip_entry_chunks(
    archive: SourceArchive,
    entry_index: usize,
    sender: tokio::sync::mpsc::Sender<std::result::Result<Bytes, BodyError>>,
) -> std::result::Result<(), BodyError> {
    let mut zip = open_zip_archive(&archive).map_err(boxed_body_message)?;
    let mut entry = zip.by_index(entry_index).map_err(boxed_body_error)?;

    loop {
        let mut chunk = Vec::with_capacity(ZIP_ENTRY_READ_CHUNK_BYTES);
        let bytes_read = entry
            .by_ref()
            .take(ZIP_ENTRY_READ_CHUNK_BYTES as u64)
            .read_to_end(&mut chunk)
            .map_err(boxed_body_error)?;

        if bytes_read == 0 {
            break;
        }

        if sender.blocking_send(Ok(Bytes::from(chunk))).is_err() {
            break;
        }
    }

    Ok(())
}

fn boxed_body_error(error: impl StdError + Send + Sync + 'static) -> BodyError {
    Box::new(error)
}

fn boxed_body_message(error: anyhow::Error) -> BodyError {
    Box::new(std::io::Error::new(
        std::io::ErrorKind::Other,
        error.to_string(),
    ))
}

pub(super) enum ArchiveReader {
    Memory(Cursor<Arc<[u8]>>),
    File(File),
}

impl Read for ArchiveReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Self::Memory(reader) => std::io::Read::read(reader, buf),
            Self::File(reader) => std::io::Read::read(reader, buf),
        }
    }
}

impl Seek for ArchiveReader {
    fn seek(&mut self, pos: std::io::SeekFrom) -> std::io::Result<u64> {
        match self {
            Self::Memory(reader) => reader.seek(pos),
            Self::File(reader) => reader.seek(pos),
        }
    }
}

struct ReceiverBody {
    receiver: Mutex<tokio::sync::mpsc::Receiver<std::result::Result<Bytes, BodyError>>>,
    content_length: u64,
}

impl Body for ReceiverBody {
    type Data = Bytes;
    type Error = BodyError;

    fn poll_frame(
        self: std::pin::Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
    ) -> Poll<Option<std::result::Result<Frame<Self::Data>, Self::Error>>> {
        let mut receiver = self
            .receiver
            .lock()
            .expect("receiver body mutex should not be poisoned");

        match receiver.poll_recv(cx) {
            Poll::Ready(Some(Ok(bytes))) => Poll::Ready(Some(Ok(Frame::data(bytes)))),
            Poll::Ready(Some(Err(error))) => Poll::Ready(Some(Err(error))),
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }

    fn size_hint(&self) -> SizeHint {
        SizeHint::with_exact(self.content_length)
    }
}

#[cfg(test)]
mod tests {
    use std::fs::File;
    use std::io::Write;

    use zip::write::{SimpleFileOptions, ZipWriter};

    use super::{open_zip_archive, zip_entry_body};
    use crate::types::SourceArchive;

    #[tokio::test]
    async fn zip_entry_body_streams_entry_and_reports_exact_size() {
        let archive_path = write_test_zip(&[("index.html", b"hello world" as &[u8])]);
        let archive = SourceArchive::temporary_file(archive_path);

        let body = zip_entry_body(archive.clone(), 0, 11);

        assert_eq!(body.size_hint(), (11, Some(11)));
        assert_eq!(body.collect().await.unwrap().into_bytes(), "hello world");
    }

    #[tokio::test]
    async fn zip_entry_body_can_be_rebuilt_from_archive_path() {
        let archive_path = write_test_zip(&[("asset.txt", b"retryable body" as &[u8])]);
        let archive = SourceArchive::temporary_file(archive_path);

        let first = zip_entry_body(archive.clone(), 0, 14)
            .collect()
            .await
            .unwrap()
            .into_bytes();
        let second = zip_entry_body(archive.clone(), 0, 14)
            .collect()
            .await
            .unwrap()
            .into_bytes();

        assert_eq!(first, "retryable body");
        assert_eq!(second, "retryable body");
    }

    #[test]
    fn open_zip_archive_reads_in_memory_archive() {
        let archive_path = write_test_zip(&[("asset.txt", b"in memory" as &[u8])]);
        let bytes = std::fs::read(&archive_path).unwrap();
        std::fs::remove_file(archive_path).unwrap();
        let archive = SourceArchive::in_memory(bytes);

        let mut zip = open_zip_archive(&archive).unwrap();
        let mut entry = zip.by_index(0).unwrap();
        let mut body = String::new();
        std::io::Read::read_to_string(&mut entry, &mut body).unwrap();

        assert_eq!(body, "in memory");
    }

    fn write_test_zip(entries: &[(&str, &[u8])]) -> std::path::PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "rust-bucket-deployment-test-{}.zip",
            uuid::Uuid::new_v4()
        ));

        let file = File::create(&path).unwrap();
        let mut writer = ZipWriter::new(file);
        let options = SimpleFileOptions::default();

        for (name, bytes) in entries {
            writer.start_file(name, options).unwrap();
            writer.write_all(bytes).unwrap();
        }
        writer.finish().unwrap();

        path
    }
}
