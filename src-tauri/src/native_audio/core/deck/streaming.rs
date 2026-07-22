//! Streaming audio source — bridges async HTTP download with sync symphonia Read.
//!
//! The downloader appends chunks to an anonymous temporary file. The sync
//! reader blocks until requested bytes are available. This keeps the source
//! seekable for Symphonia without retaining an entire FLAC/M4A in RAM; the OS
//! removes the temporary file when the last handle closes.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};

use symphonia::core::io::MediaSource;

/// Shared state between the async downloader and the sync reader.
struct Inner {
    file: File,
    downloaded_len: u64,
    done: bool,
    /// True when the download was aborted due to an error (not a clean finish).
    /// The reader returns `ConnectionAborted` instead of normal EOF so the
    /// decoder can distinguish truncated streams from complete ones.
    aborted: bool,
    /// Total content length from the HTTP response (if known).
    content_length: Option<u64>,
}

/// Thread-safe, seekable spool. The downloader writes to it and the decoder
/// reads from it using independent logical positions under a short mutex.
///
/// When the reader side (StreamingReader) is dropped (e.g. track skipped),
/// the `abandoned` flag is set so the HTTP worker can stop promptly.
pub struct SharedBuffer {
    inner: Mutex<Inner>,
    condvar: Condvar,
    /// Set to `true` when the StreamingReader is dropped.
    abandoned: AtomicBool,
}

impl SharedBuffer {
    pub fn new(content_length: Option<u64>) -> io::Result<Arc<Self>> {
        let file = tempfile::tempfile()?;
        Ok(Arc::new(Self {
            inner: Mutex::new(Inner {
                file,
                downloaded_len: 0,
                done: false,
                aborted: false,
                content_length,
            }),
            condvar: Condvar::new(),
            abandoned: AtomicBool::new(false),
        }))
    }

    /// Append a chunk of bytes (called by the async downloader).
    pub fn push(&self, chunk: &[u8]) -> io::Result<()> {
        if self.abandoned.load(Ordering::Relaxed) {
            return Ok(());
        }
        let mut inner = self.inner.lock().unwrap();
        let write_at = inner.downloaded_len;
        inner.file.seek(SeekFrom::Start(write_at))?;
        inner.file.write_all(chunk)?;
        inner.downloaded_len = inner.downloaded_len.saturating_add(chunk.len() as u64);
        self.condvar.notify_all();
        Ok(())
    }

    /// Signal that the download is complete.
    pub fn finish(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.done = true;
        self.condvar.notify_all();
    }

    /// Signal a download error. The reader will return `ConnectionAborted`
    /// instead of normal EOF so the decoder can detect the truncation.
    pub fn abort(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.aborted = true;
        inner.done = true;
        self.condvar.notify_all();
    }

    pub fn is_abandoned(&self) -> bool {
        self.abandoned.load(Ordering::Relaxed)
    }

    /// Set the content length once known (from HTTP Content-Length header).
    pub fn set_content_length(&self, len: u64) {
        let mut inner = self.inner.lock().unwrap();
        inner.content_length = Some(len);
    }
}

/// Sync reader that blocks until data is available in the shared buffer.
/// Implements `Read + Seek + MediaSource` for symphonia.
pub struct StreamingReader {
    buffer: Arc<SharedBuffer>,
    pos: u64,
}

impl StreamingReader {
    pub fn new(buffer: Arc<SharedBuffer>) -> Self {
        Self { buffer, pos: 0 }
    }
}

impl Read for StreamingReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            let mut inner = self.buffer.inner.lock().unwrap();
            let available = inner.downloaded_len;

            if self.pos < available {
                let readable = available.saturating_sub(self.pos).min(buf.len() as u64) as usize;
                inner.file.seek(SeekFrom::Start(self.pos))?;
                let n = inner.file.read(&mut buf[..readable])?;
                self.pos += n as u64;
                return Ok(n);
            }

            if inner.done {
                if inner.aborted {
                    // Download was aborted — signal distinct error so the
                    // decoder can tell this apart from a normal end-of-file.
                    return Err(io::Error::new(
                        io::ErrorKind::ConnectionAborted,
                        "stream download aborted",
                    ));
                }
                // No more data coming — normal EOF
                return Ok(0);
            }

            // Wait for more data
            let _inner = self
                .buffer
                .condvar
                .wait_while(inner, |i| i.downloaded_len <= self.pos && !i.done)
                .unwrap();
        }
    }
}

impl Seek for StreamingReader {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let inner = self.buffer.inner.lock().unwrap();
        let len = inner.downloaded_len;
        let content_len = inner.content_length.unwrap_or(len);
        drop(inner);

        let new_pos = match pos {
            SeekFrom::Start(p) => p as i64,
            SeekFrom::Current(offset) => self.pos as i64 + offset,
            SeekFrom::End(offset) => content_len as i64 + offset,
        };

        if new_pos < 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "seek to negative position",
            ));
        }

        self.pos = new_pos as u64;
        Ok(self.pos)
    }
}

impl Drop for StreamingReader {
    fn drop(&mut self) {
        // Signal that the reader is done so the HTTP worker can cancel.
        self.buffer.abandoned.store(true, Ordering::Relaxed);
        // Wake any blocked push() or finish() waiting on the condvar
        self.buffer.condvar.notify_all();
    }
}

impl MediaSource for StreamingReader {
    fn is_seekable(&self) -> bool {
        // We can seek within the buffered region. Symphonia uses this for
        // format probing and container seeking.
        true
    }

    fn byte_len(&self) -> Option<u64> {
        self.buffer.inner.lock().unwrap().content_length
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_write_read() {
        let buf = SharedBuffer::new(None).unwrap();
        buf.push(b"hello").unwrap();
        buf.push(b" world").unwrap();
        buf.finish();

        let mut reader = StreamingReader::new(buf);
        let mut out = String::new();
        reader.read_to_string(&mut out).unwrap();
        assert_eq!(out, "hello world");
    }

    #[test]
    fn seek_within_buffer() {
        let buf = SharedBuffer::new(None).unwrap();
        buf.push(b"0123456789").unwrap();
        buf.finish();

        let mut reader = StreamingReader::new(buf);
        let mut out = [0u8; 3];

        reader.seek(SeekFrom::Start(5)).unwrap();
        reader.read_exact(&mut out).unwrap();
        assert_eq!(&out, b"567");

        reader.seek(SeekFrom::Start(0)).unwrap();
        reader.read_exact(&mut out).unwrap();
        assert_eq!(&out, b"012");
    }

    #[test]
    fn concurrent_write_read() {
        let buf = SharedBuffer::new(None).unwrap();
        let buf_clone = buf.clone();

        let writer = std::thread::spawn(move || {
            for i in 0..10 {
                std::thread::sleep(std::time::Duration::from_millis(5));
                buf_clone.push(format!("{}", i).as_bytes()).unwrap();
            }
            buf_clone.finish();
        });

        let mut reader = StreamingReader::new(buf);
        let mut out = String::new();
        reader.read_to_string(&mut out).unwrap();
        writer.join().unwrap();
        assert_eq!(out, "0123456789");
    }
}
