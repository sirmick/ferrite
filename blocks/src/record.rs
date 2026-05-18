//! Shared file-recorder used by blocks that grow a live `record_path`
//! param.
//!
//! Blocks like [`crate::log_mag_u8::LogMagU8`] and
//! [`crate::channelizer::Channelizer`] can side-tee their output to disk
//! while the live pipeline keeps driving the UI. Each one holds a
//! [`Recorder`] keyed by `record_path` (set live via
//! `apply_live_params`); the helper takes care of the file open, the
//! `max_bytes` cap, and the flush. The sidecar JSON shape is per-block
//! — blocks know the format/rate metadata that's meaningful for their
//! own output, the helper doesn't try to be clever about it.

use std::{
    fs::OpenOptions,
    io::{BufWriter, Write},
    path::Path,
};

use anyhow::{anyhow, bail, Context, Result};

/// Append-only file recorder with an optional byte cap. Drops auto-flush
/// on `Drop` so a SIGKILL'd process still leaves a parseable file
/// (truncated to the last buffer flush). The cap is in *bytes* — each
/// block translates its own "max_seconds × rate × bytes_per_sample" math
/// once at open-time, so the helper stays type-agnostic.
pub struct Recorder {
    writer: Option<BufWriter<Box<dyn Write + Send>>>,
    bytes_written: u64,
    max_bytes: Option<u64>,
}

impl Recorder {
    /// Open `path` for write, truncating any existing file. `max_bytes
    /// = None` records until [`finalise`](Self::finalise) is called.
    pub fn open(path: &Path, max_bytes: Option<u64>) -> Result<Self> {
        if path.as_os_str().is_empty() {
            bail!("recorder: path is required");
        }
        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)
            .with_context(|| format!("open {}", path.display()))?;
        let writer: Box<dyn Write + Send> = Box::new(file);
        Ok(Self {
            // 64 KiB BufWriter — comfortably more than one tick's worth
            // of data on the highest-rate live-record path we expect
            // (Channelizer at 500 kS/s cf32 ≈ 4 MB/s, ~3 ms / 64 KiB
            // chunk), so the syscall rate stays manageable.
            writer: Some(BufWriter::with_capacity(64 * 1024, writer)),
            bytes_written: 0,
            max_bytes,
        })
    }

    /// Constructor parameterised over the writer for in-memory tests.
    pub fn from_writer(writer: Box<dyn Write + Send>, max_bytes: Option<u64>) -> Self {
        Self {
            writer: Some(BufWriter::with_capacity(64 * 1024, writer)),
            bytes_written: 0,
            max_bytes,
        }
    }

    /// Write up to `max_bytes - bytes_written` of `bytes`. Past the
    /// cap, returns `Ok(0)` and the writer auto-finalises so the file
    /// is durable even before the cap-fired stop fires from the block.
    pub fn write(&mut self, bytes: &[u8]) -> Result<usize> {
        if bytes.is_empty() {
            return Ok(0);
        }
        let allowed = match self.max_bytes {
            None => bytes.len() as u64,
            Some(cap) => cap
                .saturating_sub(self.bytes_written)
                .min(bytes.len() as u64),
        };
        if allowed == 0 {
            return Ok(0);
        }
        let writer = self
            .writer
            .as_mut()
            .ok_or_else(|| anyhow!("recorder: writer already finalised"))?;
        writer
            .write_all(&bytes[..allowed as usize])
            .context("recorder write")?;
        self.bytes_written = self.bytes_written.saturating_add(allowed);
        if self.is_capped() {
            // Cap fired exactly on this write — close the writer so a
            // crash-after-cap still leaves the bytes on disk. The block
            // will rewrite the sidecar on its own finalise tick.
            self.flush_and_close()?;
        }
        Ok(allowed as usize)
    }

    /// True when the cap has fired. The block uses this to know it
    /// should clear `record_path` and rewrite the sidecar.
    #[must_use]
    pub fn is_capped(&self) -> bool {
        match self.max_bytes {
            None => false,
            Some(cap) => self.bytes_written >= cap,
        }
    }

    #[must_use]
    pub fn bytes_written(&self) -> u64 {
        self.bytes_written
    }

    /// Flush the BufWriter and drop it. Idempotent. The file remains
    /// open at the OS level until the block drops the [`Recorder`]
    /// itself.
    pub fn finalise(&mut self) -> Result<()> {
        self.flush_and_close()
    }

    fn flush_and_close(&mut self) -> Result<()> {
        let Some(mut w) = self.writer.take() else {
            return Ok(());
        };
        w.flush().context("recorder flush")?;
        Ok(())
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        if let Err(e) = self.flush_and_close() {
            eprintln!("recorder: flush on drop failed: {e:#}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Recorder;
    use crate::test_support::{SharedBuf, SharedCursor};

    fn make(max_bytes: Option<u64>) -> (Recorder, SharedBuf) {
        let (w, shared) = SharedCursor::new();
        (Recorder::from_writer(Box::new(w), max_bytes), shared)
    }

    #[test]
    fn writes_through_when_uncapped() {
        let (mut r, shared) = make(None);
        assert_eq!(r.write(&[1, 2, 3, 4]).unwrap(), 4);
        assert_eq!(r.write(&[5, 6, 7, 8]).unwrap(), 4);
        r.finalise().unwrap();
        assert_eq!(*shared.borrow(), vec![1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(r.bytes_written(), 8);
        assert!(!r.is_capped());
    }

    #[test]
    fn cap_truncates_then_returns_zero() {
        let (mut r, shared) = make(Some(5));
        // First write fits.
        assert_eq!(r.write(&[1, 2, 3]).unwrap(), 3);
        assert!(!r.is_capped());
        // Second write straddles the cap — only 2 bytes land.
        assert_eq!(r.write(&[4, 5, 6, 7]).unwrap(), 2);
        assert!(r.is_capped());
        // Subsequent writes are no-ops.
        assert_eq!(r.write(&[8, 9]).unwrap(), 0);
        assert_eq!(*shared.borrow(), vec![1, 2, 3, 4, 5]);
        assert_eq!(r.bytes_written(), 5);
    }

    #[test]
    fn finalise_is_idempotent() {
        let (mut r, _) = make(None);
        r.write(&[1, 2, 3]).unwrap();
        r.finalise().unwrap();
        r.finalise().unwrap(); // no-op
    }

    #[test]
    fn writes_after_finalise_error() {
        let (mut r, _) = make(None);
        r.finalise().unwrap();
        assert!(r.write(&[1]).is_err());
    }
}
