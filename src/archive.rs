use std::task::{Context, Poll};

use bytes::{Buf, Bytes};
use futures_util::StreamExt;
use snafu::ResultExt;

use crate::target::ArchiveFormat;

/// Incrementally fed archive being scanned for the paths of the files it contains.
pub struct Archive {
    sender: tokio::sync::mpsc::Sender<Bytes>,
    task: tokio::task::JoinHandle<Result<Vec<Entry>, snafu::WhateverLocal>>,
}

impl Archive {
    pub fn new(format: ArchiveFormat) -> Self {
        let (sender, receiver) = tokio::sync::mpsc::channel(4);
        let reader = ChannelReader {
            receiver,
            leftover: Bytes::new(),
        };

        let task = tokio::task::spawn_local(async move {
            match format {
                ArchiveFormat::Tar => list_tar_entries(reader).await,
                ArchiveFormat::TarGz => {
                    list_tar_entries(async_compression::tokio::bufread::GzipDecoder::new(
                        tokio::io::BufReader::new(reader),
                    ))
                    .await
                }
                ArchiveFormat::TarXz => list_tar_entries(XzTarReader::new(reader)?).await,
                ArchiveFormat::TarZst => list_tar_entries(ZstdTarReader::new(reader)).await,
                ArchiveFormat::TarBz2 => {
                    list_tar_entries(async_compression::tokio::bufread::BzDecoder::new(
                        tokio::io::BufReader::new(reader),
                    ))
                    .await
                }
                ArchiveFormat::Zip => list_zip_entries(reader).await,
                ArchiveFormat::Gz | ArchiveFormat::Xz | ArchiveFormat::Zst | ArchiveFormat::Bz2 => {
                    unreachable!("`Archive` is not constructed for formats without entries")
                }
            }
        });

        Archive { sender, task }
    }

    pub async fn update(
        &mut self,
        bytes: Bytes,
        url: &reqwest::Url,
    ) -> Result<(), snafu::WhateverLocal> {
        self.sender
            .send(bytes)
            .await
            .with_whatever_context(|_| format!("failed to scan archive from {url}"))
    }

    pub async fn finish(self, url: &reqwest::Url) -> Result<Vec<Entry>, snafu::WhateverLocal> {
        drop(self.sender);

        self.task
            .await
            .with_whatever_context(|_| format!("failed to scan archive from {url}"))?
            .with_whatever_context(|_| format!("failed to scan archive from {url}"))
    }
}

/// An entry in an archive.
pub struct Entry {
    executable: bool,
    in_bin: bool,
    pub path: String,
}

impl Entry {
    fn new(mut path: String, executable: bool) -> Entry {
        if path.starts_with("./") {
            path.replace_range(..2, "");
        }

        Entry {
            in_bin: path.starts_with("bin/") || path.contains("/bin/") || path.ends_with(".exe"),
            executable,
            path,
        }
    }
}

impl std::borrow::Borrow<str> for Entry {
    fn borrow(&self) -> &str {
        &self.path
    }
}

fn finish_entries(vec: &mut Vec<Entry>) {
    // If at least one entry is executable, only keep executable entries. Otherwise, we may simply
    // be lacking the metadata necessary to know it's executable, so keep going.
    if vec.iter().any(|entry| entry.executable) {
        vec.retain(|x| x.executable);
        return;
    }

    // Otherwise, prefer entries in `bin/`.
    vec.sort_by(|a, b| (!a.in_bin, &a.path).cmp(&(!b.in_bin, &b.path)));
}

async fn list_tar_entries<R>(reader: R) -> Result<Vec<Entry>, snafu::WhateverLocal>
where
    R: tokio::io::AsyncRead + Unpin + Send,
{
    let archive = async_tar::Archive::new(reader);
    let mut entries = archive
        .entries()
        .whatever_context("failed to read tar archive")?;
    let mut results = Vec::new();

    loop {
        let entry = match entries.next().await {
            Some(Ok(entry)) => entry,
            Some(Err(_)) => {
                // A malformed/truncated tar may still yield a usable prefix of entries; only
                // treat this as fatal if nothing could be read at all.
                snafu::ensure_whatever!(!results.is_empty(), "failed to read tar archive");
                break;
            }
            None => break,
        };

        if !entry.header().entry_type().is_file() {
            continue;
        }
        let executable = entry.header().mode().is_ok_and(|mode| mode & 0o111 != 0);

        let path = entry
            .path()
            .whatever_context("failed to read tar entry path")?
            .to_string_lossy()
            .into_owned();

        results.push(Entry::new(path, executable));
    }

    finish_entries(&mut results);

    Ok(results)
}

async fn list_zip_entries<R>(reader: R) -> Result<Vec<Entry>, snafu::WhateverLocal>
where
    R: futures_util::io::AsyncRead + Unpin + Send,
{
    let mut zip =
        async_zip::base::read::stream::ZipFileReader::new(futures_util::io::BufReader::new(reader));
    let mut results = Vec::new();

    loop {
        let Some(entry) = zip
            .next_with_entry()
            .await
            .whatever_context("failed to read zip archive")?
        else {
            break;
        };

        let name = entry
            .reader()
            .entry()
            .filename()
            .as_str()
            .whatever_context("failed to read zip entry name")?
            .to_owned();

        if !name.ends_with('/') {
            let executable = entry
                .reader()
                .entry()
                .unix_permissions()
                .is_some_and(|mode| mode & 0o111 != 0);

            results.push(Entry::new(name, executable));
        }

        zip = entry
            .skip()
            .await
            .whatever_context("failed to read zip archive")?;
    }

    finish_entries(&mut results);

    Ok(results)
}

// -------------------------------------------------------------------------------------------------
// MARK: ChannelReader

/// A reader fed by chunks pushed through a channel, implementing both [`tokio::io::AsyncRead`]
/// (for [`async_tar`] / [`async_compression`]) and [`futures_util::io::AsyncRead`] (for [`async_zip`]).
struct ChannelReader {
    receiver: tokio::sync::mpsc::Receiver<Bytes>,
    leftover: Bytes,
}

impl ChannelReader {
    /// Fills `buf` with as many leftover/newly-received bytes as fit, returning the number of
    /// bytes written (`0` meaning the channel is closed and no bytes remain, i.e. EOF).
    fn poll_fill(&mut self, cx: &mut Context<'_>, buf: &mut [u8]) -> Poll<std::io::Result<usize>> {
        if self.leftover.is_empty() {
            match self.receiver.poll_recv(cx) {
                Poll::Ready(Some(bytes)) => self.leftover = bytes,
                Poll::Ready(None) => return Poll::Ready(Ok(0)),
                Poll::Pending => return Poll::Pending,
            }
        }

        let n = self.leftover.len().min(buf.len());

        buf[..n].copy_from_slice(&self.leftover[..n]);
        self.leftover.advance(n);

        Poll::Ready(Ok(n))
    }
}

impl tokio::io::AsyncRead for ChannelReader {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let unfilled = buf.initialize_unfilled();

        match self.poll_fill(cx, unfilled) {
            Poll::Ready(Ok(n)) => {
                buf.advance(n);
                Poll::Ready(Ok(()))
            }
            Poll::Ready(Err(err)) => Poll::Ready(Err(err)),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl futures_util::io::AsyncRead for ChannelReader {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<std::io::Result<usize>> {
        self.poll_fill(cx, buf)
    }
}

// -------------------------------------------------------------------------------------------------
// MARK: XzTarReader
//
// `xz` is a pure-Rust crate (its default `xz-core` backend is a c2rust transpile of liblzma), used
// instead of `async-compression`'s `xz` feature (which links a C-compiled liblzma via
// `liblzma-sys`) so this binary never depends on a working local C toolchain.

/// Adapts a push-based [`xz::stream::Stream`] decoder, fed compressed bytes read from a
/// [`ChannelReader`], into a `tokio::io::AsyncRead` of decompressed bytes.
struct XzTarReader {
    inner: ChannelReader,
    stream: xz::stream::Stream,
    /// Compressed bytes read from `inner` but not yet consumed by `stream`.
    input: Bytes,
    /// Set once `inner` has reached EOF, so we know to pass `Action::Finish` to `stream`.
    input_eof: bool,
}

impl XzTarReader {
    fn new(inner: ChannelReader) -> Result<Self, snafu::WhateverLocal> {
        let stream = xz::stream::Stream::new_stream_decoder(u64::MAX, 0)
            .whatever_context("failed to initialize xz decoder")?;

        Ok(XzTarReader {
            inner,
            stream,
            input: Bytes::new(),
            input_eof: false,
        })
    }
}

impl tokio::io::AsyncRead for XzTarReader {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();

        loop {
            // Refill `input` if we've consumed everything we had and the source isn't done.
            if this.input.is_empty() && !this.input_eof {
                let mut scratch = [0u8; 8 * 1024];

                match this.inner.poll_fill(cx, &mut scratch) {
                    Poll::Ready(Ok(0)) => this.input_eof = true,
                    Poll::Ready(Ok(n)) => this.input = Bytes::copy_from_slice(&scratch[..n]),
                    Poll::Ready(Err(err)) => return Poll::Ready(Err(err)),
                    Poll::Pending => return Poll::Pending,
                }
            }

            let action = if this.input_eof {
                xz::stream::Action::Finish
            } else {
                xz::stream::Action::Run
            };

            let before_in = this.stream.total_in();
            let before_out = this.stream.total_out();

            let status = this
                .stream
                .process(&this.input, buf.initialize_unfilled(), action)
                .map_err(std::io::Error::other)?;

            let consumed = (this.stream.total_in() - before_in) as usize;
            let produced = (this.stream.total_out() - before_out) as usize;

            this.input.advance(consumed);
            buf.advance(produced);

            if produced > 0 || status == xz::stream::Status::StreamEnd || buf.remaining() == 0 {
                return Poll::Ready(Ok(()));
            }

            // No output was produced, the stream isn't done, and the caller still has room: we
            // must need more input, so loop back around to refill `input`.
        }
    }
}

// -------------------------------------------------------------------------------------------------
// MARK: ZstdTarReader
//
// `ruzstd` is a pure-Rust zstd decoder, used instead of `async-compression`'s `zstd` feature
// (which links a C-compiled libzstd via `zstd-sys`) so this binary never depends on a working
// local C toolchain.

/// Largest possible zstd frame header: 4-byte magic number + up to 14 bytes of frame header
/// descriptor/fields. [`ruzstd`] needs at least this many bytes buffered before it can start
/// decoding (see [`ruzstd::decoding::FrameDecoder::decode_from_to`]).
const ZSTD_MAX_FRAME_HEADER_SIZE: usize = 18;

/// Adapts a pull-based [`ruzstd::decoding::FrameDecoder`], fed compressed bytes read from a
/// [`ChannelReader`], into a `tokio::io::AsyncRead` of decompressed bytes.
struct ZstdTarReader {
    inner: ChannelReader,
    decoder: ruzstd::decoding::FrameDecoder,
    /// Compressed bytes read from `inner` but not yet consumed by `decoder`.
    input: Vec<u8>,
    /// Set once `inner` has reached EOF.
    input_eof: bool,
}

impl ZstdTarReader {
    fn new(inner: ChannelReader) -> Self {
        ZstdTarReader {
            inner,
            decoder: ruzstd::decoding::FrameDecoder::new(),
            input: Vec::new(),
            input_eof: false,
        }
    }
}

impl tokio::io::AsyncRead for ZstdTarReader {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();

        loop {
            // `decode_from_to` needs at least a full frame header, and (once started) at least
            // one full block, to make progress; keep pulling more input until either happens or
            // the source is exhausted.
            if !this.input_eof
                && (this.input.len() < ZSTD_MAX_FRAME_HEADER_SIZE || this.input.len() < 128 * 1024)
            {
                let mut scratch = [0u8; 8 * 1024];

                match this.inner.poll_fill(cx, &mut scratch) {
                    Poll::Ready(Ok(0)) => this.input_eof = true,
                    Poll::Ready(Ok(n)) => this.input.extend_from_slice(&scratch[..n]),
                    Poll::Ready(Err(err)) => return Poll::Ready(Err(err)),
                    Poll::Pending => {
                        // We may already have enough buffered to make progress (e.g. a small
                        // final block); only propagate `Pending` if we have nothing to try yet.
                        if this.input.is_empty() {
                            return Poll::Pending;
                        }
                    }
                }

                continue;
            }

            let (read, written) = this
                .decoder
                .decode_from_to(&this.input, buf.initialize_unfilled())
                .map_err(std::io::Error::other)?;

            this.input.drain(..read);
            buf.advance(written);

            if written > 0 || buf.remaining() == 0 {
                return Poll::Ready(Ok(()));
            }

            if read == 0 && this.input_eof {
                // No full block/header available and the source is exhausted: nothing more to
                // decode.
                return Poll::Ready(Ok(()));
            }

            // `read == 0` means `input` didn't contain a full block yet; loop back to pull more.
        }
    }
}
