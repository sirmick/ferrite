//! Test-only fixtures shared across block unit tests.
//!
//! `#[cfg(test)]`-gated, `pub(crate)` — compiled only for this crate's
//! own unit tests, never in a shipped build.

use std::cell::RefCell;
use std::io::{Cursor, Seek, SeekFrom, Write};
use std::rc::Rc;

/// Handle the test reads back to inspect bytes written through the
/// sink's `Box<dyn WriteSeek>`.
pub(crate) type SharedBuf = Rc<RefCell<Vec<u8>>>;

/// A `Write + Seek` that mirrors every byte written into a shared
/// `Vec<u8>` the test can inspect (the real sink owns its writer, so
/// the test can't reach inside it otherwise).
///
/// `Rc` is `!Send`, so the blanket `WriteSeek: Send` bound would reject
/// this. The fixture is only ever constructed and used on the test's
/// own thread — nothing crosses a thread boundary — so the
/// `unsafe impl Send` below is sound for this use.
pub(crate) struct SharedCursor {
    inner: Cursor<Vec<u8>>,
    shared: SharedBuf,
}

impl SharedCursor {
    /// Returns the cursor plus the shared buffer it mirrors into.
    pub(crate) fn new() -> (Self, SharedBuf) {
        let shared: SharedBuf = Rc::new(RefCell::new(Vec::new()));
        (
            Self {
                inner: Cursor::new(Vec::new()),
                shared: shared.clone(),
            },
            shared,
        )
    }
}

impl Write for SharedCursor {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.inner.write(buf)?;
        *self.shared.borrow_mut() = self.inner.get_ref().clone();
        Ok(n)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

impl Seek for SharedCursor {
    fn seek(&mut self, p: SeekFrom) -> std::io::Result<u64> {
        self.inner.seek(p)
    }
}

// Sound: see the type-level doc — single-threaded test use only.
unsafe impl Send for SharedCursor {}
