//! Safe Rust wrapper around the curated fldigi modem cores.
//!
//! ## Two backends, one public API (link-vs-bridge)
//!
//! `FldigiModem` presents the same surface on every target; the
//! backend differs by `cfg(target_arch)`:
//!
//! * **native** (`backend_native`) — the C++ shim in
//!   `shim/fldigi_shim.cxx`, statically linked (`build.rs`). Plain
//!   `extern "C"`, raw-pointer + drain ABI.
//! * **wasm32** (`backend_wasm`) — the *same* curated C++, built once
//!   by Emscripten into a sibling `fldigi.wasm` (full libc++ +
//!   exceptions). The ABI is declared as **wasm-bindgen module
//!   imports with JS-native types** (`&str`, `&[f32]`, `String`,
//!   `Vec<f32>`): wasm-bindgen marshals Rust↔JS for us, so the symbols
//!   land in the `wbg` namespace it owns — no raw-pointer `env`
//!   imports, no wasm-memory access from JS, no import-object surgery.
//!   The bridge JS (`js/fldigi_bridge.js`) only talks to the
//!   Emscripten module.
//!
//! Both are **pull/drain, not callbacks**: a function pointer can't
//! cross the two wasm modules' tables/memories. The C side accumulates
//! decoded output; the host drains after each `rx`. One modem per
//! block; single-threaded, so no locking.

/// One decoded image segment (visual modes — Phase 3; ABI reserved so
/// native + bridge stay in lockstep).
#[derive(Clone, Debug)]
pub struct ImageRow {
    pub px: Vec<u8>,
    pub row: i32,
    pub kind: i32,
}

/// One scope frame (operator tuning aid). `mode` is fldigi's
/// `Digiscope::scope_mode` discriminant.
#[derive(Clone, Debug)]
pub struct ScopeFrame {
    pub data: Vec<f32>,
    pub mode: i32,
}

// ---------------------------------------------------------------------
// Native backend: static C++ link, raw-pointer pull/drain ABI.
// ---------------------------------------------------------------------
#[cfg(not(target_arch = "wasm32"))]
mod backend {
    use super::{ImageRow, ScopeFrame};
    use std::ffi::CString;
    use std::os::raw::{c_char, c_int};

    #[repr(C)]
    pub struct Raw {
        _p: [u8; 0],
    }
    pub type Handle = *mut Raw;
    pub const NULL: Handle = std::ptr::null_mut();
    pub fn is_null(h: Handle) -> bool {
        h.is_null()
    }

    extern "C" {
        fn fldigi_modem_create(mode: *const c_char, sample_rate: c_int) -> Handle;
        fn fldigi_modem_rx(m: Handle, audio: *const f32, n: c_int) -> c_int;
        fn fldigi_modem_set_param(m: Handle, key: *const c_char, value: f64);
        fn fldigi_modem_inject_rsid(m: Handle, id: *const c_char);
        fn fldigi_modem_drain_text(m: Handle, out: *mut c_char, cap: c_int) -> c_int;
        fn fldigi_modem_drain_status(m: Handle, out: *mut c_char, cap: c_int) -> c_int;
        fn fldigi_modem_drain_rsid(m: Handle, out: *mut c_char, cap: c_int) -> c_int;
        fn fldigi_modem_drain_scope(
            m: Handle,
            out: *mut f32,
            cap: c_int,
            mode: *mut c_int,
        ) -> c_int;
        fn fldigi_modem_drain_image(
            m: Handle,
            out: *mut u8,
            cap: c_int,
            row: *mut c_int,
            kind: *mut c_int,
        ) -> c_int;
        fn fldigi_modem_destroy(m: Handle);
    }

    pub fn create(mode: &str, sample_rate: u32) -> Handle {
        let Ok(c_mode) = CString::new(mode) else {
            return NULL;
        };
        // SAFETY: c_mode outlives the call; C side owns the handle.
        unsafe { fldigi_modem_create(c_mode.as_ptr(), sample_rate as c_int) }
    }
    pub fn rx(h: Handle, audio: &[f32]) {
        // SAFETY: h live; audio is exactly len f32.
        unsafe { fldigi_modem_rx(h, audio.as_ptr(), audio.len() as c_int) };
    }
    pub fn set_param(h: Handle, key: &str, v: f64) {
        if let Ok(k) = CString::new(key) {
            unsafe { fldigi_modem_set_param(h, k.as_ptr(), v) };
        }
    }
    pub fn inject_rsid(h: Handle, id: &str) {
        if let Ok(s) = CString::new(id) {
            unsafe { fldigi_modem_inject_rsid(h, s.as_ptr()) };
        }
    }

    fn drain_bytes(
        h: Handle,
        f: unsafe extern "C" fn(Handle, *mut c_char, c_int) -> c_int,
    ) -> Vec<u8> {
        const CAP: usize = 4096;
        let mut out = Vec::new();
        let mut buf = [0u8; CAP];
        loop {
            let n = unsafe { f(h, buf.as_mut_ptr().cast::<c_char>(), CAP as c_int) };
            if n <= 0 {
                break;
            }
            let n = (n as usize).min(CAP);
            out.extend_from_slice(&buf[..n]);
            if n < CAP {
                break;
            }
        }
        out
    }

    pub fn drain_text(h: Handle) -> String {
        String::from_utf8_lossy(&drain_bytes(h, fldigi_modem_drain_text)).into_owned()
    }
    pub fn drain_status(h: Handle) -> Vec<String> {
        String::from_utf8_lossy(&drain_bytes(h, fldigi_modem_drain_status))
            .lines()
            .map(str::to_string)
            .collect()
    }
    pub fn drain_rsid(h: Handle) -> Vec<String> {
        String::from_utf8_lossy(&drain_bytes(h, fldigi_modem_drain_rsid))
            .lines()
            .map(str::to_string)
            .collect()
    }
    pub fn drain_scope(h: Handle) -> Option<ScopeFrame> {
        const CAP: usize = 8192;
        let mut buf = vec![0.0f32; CAP];
        let mut mode: c_int = 0;
        let n = unsafe { fldigi_modem_drain_scope(h, buf.as_mut_ptr(), CAP as c_int, &mut mode) };
        if n <= 0 {
            return None;
        }
        buf.truncate((n as usize).min(CAP));
        Some(ScopeFrame {
            data: buf,
            mode: mode as i32,
        })
    }
    pub fn drain_image(h: Handle) -> Vec<ImageRow> {
        const CAP: usize = 1 << 16;
        let mut buf = vec![0u8; CAP];
        let mut out = Vec::new();
        loop {
            let (mut row, mut kind): (c_int, c_int) = (0, 0);
            let n = unsafe {
                fldigi_modem_drain_image(h, buf.as_mut_ptr(), CAP as c_int, &mut row, &mut kind)
            };
            if n <= 0 {
                break;
            }
            out.push(ImageRow {
                px: buf[..(n as usize).min(CAP)].to_vec(),
                row: row as i32,
                kind: kind as i32,
            });
        }
        out
    }
    pub fn destroy(h: Handle) {
        // SAFETY: h returned by create(), not yet destroyed.
        unsafe { fldigi_modem_destroy(h) };
    }
}

// ---------------------------------------------------------------------
// wasm32 backend: Emscripten sibling module via wasm-bindgen imports.
// JS-native marshalling — wasm-bindgen copies Rust↔JS, the bridge JS
// only talks to the Emscripten module.
// ---------------------------------------------------------------------
#[cfg(target_arch = "wasm32")]
mod backend {
    use super::{ImageRow, ScopeFrame};
    use wasm_bindgen::prelude::*;

    pub type Handle = u32; // Emscripten pointer; 0 = failure/null
    pub const NULL: Handle = 0;
    pub fn is_null(h: Handle) -> bool {
        h == 0
    }

    #[wasm_bindgen(module = "/js/fldigi_bridge.js")]
    extern "C" {
        fn fldigi_create(mode: &str, sample_rate: u32) -> u32;
        fn fldigi_rx(h: u32, audio: &[f32]);
        fn fldigi_set_param(h: u32, key: &str, value: f64);
        fn fldigi_drain_text(h: u32) -> String;
        fn fldigi_drain_status(h: u32) -> String;
        fn fldigi_drain_scope(h: u32) -> Vec<f32>;
        fn fldigi_scope_mode(h: u32) -> i32;
        fn fldigi_destroy(h: u32);
    }

    pub fn create(mode: &str, sample_rate: u32) -> Handle {
        fldigi_create(mode, sample_rate)
    }
    pub fn rx(h: Handle, audio: &[f32]) {
        fldigi_rx(h, audio);
    }
    pub fn set_param(h: Handle, key: &str, v: f64) {
        fldigi_set_param(h, key, v);
    }
    pub fn drain_text(h: Handle) -> String {
        fldigi_drain_text(h)
    }
    pub fn drain_status(h: Handle) -> Vec<String> {
        fldigi_drain_status(h).lines().map(str::to_string).collect()
    }
    pub fn drain_rsid(_h: Handle) -> Vec<String> {
        // RSID auto-mode-detect closes through the *server* control
        // plane (like AFC); the browser-half fldigi never self-switches,
        // so no bridge ABI is added here.
        Vec::new()
    }
    pub fn inject_rsid(_h: Handle, _id: &str) {
        // Test seam — native only.
    }
    pub fn drain_scope(h: Handle) -> Option<ScopeFrame> {
        let data = fldigi_drain_scope(h);
        if data.is_empty() {
            return None;
        }
        Some(ScopeFrame {
            data,
            mode: fldigi_scope_mode(h),
        })
    }
    pub fn drain_image(_h: Handle) -> Vec<ImageRow> {
        // Visual modes are Phase 3; bridge defers the image plane.
        Vec::new()
    }
    pub fn destroy(h: Handle) {
        fldigi_destroy(h);
    }
}

/// RAII wrapper over one fldigi modem instance (target-agnostic).
pub struct FldigiModem {
    h: backend::Handle,
}

// Single-threaded use; the C core is not Sync, but the runtime drives
// one block on one thread. `Send` lets the block move between ticks.
unsafe impl Send for FldigiModem {}

impl FldigiModem {
    /// Construct a modem by fldigi mode id (e.g. `"rtty45"`) at the
    /// given audio sample rate (Hz). `None` if the mode is unknown,
    /// the core failed to init, or (wasm) the Emscripten module is
    /// absent (then the preset is simply unavailable in-browser).
    pub fn new(mode: &str, sample_rate: u32) -> Option<Self> {
        let h = backend::create(mode, sample_rate);
        if backend::is_null(h) {
            return None;
        }
        Some(Self { h })
    }

    /// Feed mono audio (f32). Decoded output accumulates backend-side —
    /// drain with [`Self::take_text`] / [`Self::take_scope`].
    pub fn rx(&mut self, audio: &[f32]) {
        if !audio.is_empty() {
            backend::rx(self.h, audio);
        }
    }

    /// Set a mode operational parameter (baud, shift, afc…). Ignored
    /// if the key is unknown for the mode.
    pub fn set_param(&mut self, key: &str, value: f64) {
        backend::set_param(self.h, key, value);
    }

    /// Drain decoded text accumulated since the last call.
    pub fn take_text(&mut self) -> String {
        backend::drain_text(self.h)
    }

    /// Drain status lines accumulated since the last call.
    pub fn take_status(&mut self) -> Vec<String> {
        backend::drain_status(self.h)
    }

    /// Drain RSID detections (make_modem id strings) since the last
    /// call. Non-empty only when RSID is enabled (set_param
    /// "RECEIVERSID") and a Reed-Solomon mode-ID was decoded.
    pub fn take_rsid(&mut self) -> Vec<String> {
        backend::drain_rsid(self.h)
    }

    /// TEST-ONLY: queue an RSID detection (`id` = a make_modem id), as
    /// if cRsId had decoded it. Drains via [`Self::take_rsid`]. Lets
    /// e2e exercise the switch path without fldigi's RS encoder
    /// (upstream-tested). No-op on wasm.
    pub fn inject_rsid(&mut self, id: &str) {
        backend::inject_rsid(self.h, id);
    }

    /// Drain the most recent scope frame, if any (0 or 1 — coalesced
    /// to the latest, which is what a tuning scope wants).
    pub fn take_scope(&mut self) -> Vec<ScopeFrame> {
        backend::drain_scope(self.h).into_iter().collect()
    }

    /// Drain decoded image rows (visual modes — Phase 3; empty until).
    pub fn take_images(&mut self) -> Vec<ImageRow> {
        backend::drain_image(self.h)
    }
}

impl Drop for FldigiModem {
    fn drop(&mut self) {
        if !backend::is_null(self.h) {
            backend::destroy(self.h);
            self.h = backend::NULL;
        }
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;

    /// Link gate + smoke: constructs the vendored RTTY modem through
    /// the native ABI, feeds a second of silence, drains, tears down.
    #[test]
    fn rtty_create_rx_destroy_links_and_runs() {
        let mut m = FldigiModem::new("rtty45", 8000).expect("rtty45 modem constructs");
        m.rx(&vec![0.0f32; 8000]);
        let _ = m.take_text();
        let _ = m.take_scope();
        let _ = m.take_status();
        let _ = m.take_images();
        drop(m);
    }

    #[test]
    fn unknown_mode_is_none() {
        assert!(FldigiModem::new("definitely-not-a-mode", 8000).is_none());
    }
}
