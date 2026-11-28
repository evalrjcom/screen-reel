//! ScreenReel v1.4.0 — lightweight screen recorder + editor (Rust + FLTK)
//! File: screenreel_v1.4.0.rs
//!
//! Design summary
//! --------------
//! * Capture: X11 MIT-SHM (fallback XGetImage) on Linux, GDI BitBlt on Windows, with
//!   cursor compositing (XFixes / DrawIconEx).
//! * Video codec: MJPEG (every frame is an independent key frame). Frames are
//!   encoded once, at capture time, by a worker pool. All editing (cut, freeze,
//!   insert-freeze) and export only copy JPEG bytes: no video re-encode, ever.
//! * Audio: tracks are PCM i16 @ 48 kHz held in Arc'd buffers. Adding, swapping,
//!   removing, muting, gain and narration re-record are sample-exact and instant.
//! * Export: native Matroska muxer (V_MJPEG + A_PCM/INT/LIT). Optional ffmpeg
//!   paths: lossless MOV remux (-c copy) or H.264/AAC MP4 (re-encode, labelled; GPU
//!   encoder when available).
//! * Effects (v1.4.0): overlays, cursor/click effects, zoom and webcam PiP are
//!   rendered only on the frames they affect; unaffected frames stay byte copies.
//!
//! Changelog
//! ---------
//! 1.4.0  Live voice chain while recording (RNNoise-style noise reduction via nnnoiseless,
//!        voice EQ presets, noise gate, de-esser, opto compressor; also as an offline
//!        "Voice FX" pass). Auto-duck system audio under the voice. Input events per frame
//!        (.ev: cursor, clicks, "a key was typed", never which key) drive click rings,
//!        cursor highlight and Auto zoom (smooth zoom/pan that follows clicks and typing).
//!        Webcam picture-in-picture (V4L2 / Media Foundation; circle, rounded, square;
//!        soft-oval background blur; mirror). Annotations: text callouts, arrows, blur and
//!        pixelate boxes, step counters, zoom regions; undoable, ripple with edits, saved
//!        as ov=/fx= lines. Effects render only on the frames they touch (parallel, q90
//!        4:4:4); every other frame is still byte-copied. Hardware H.264 for MP4 (NVENC,
//!        QSV, AMF, VAAPI, probed once, libx264 fallback and automatic retry).
//!        Robustness: audio spilled to disk while recording and media flushed ~1/s, so a
//!        crash or power cut is recoverable on next start (torn tail dropped); autosave
//!        every 60 s with restore-on-open; streaming resampler (no stop-time spike);
//!        Normalize and Compressor run as background jobs with progress; undo history
//!        capped by memory (1.5 GiB of distinct audio); window fits small screens;
//!        audio-device error logging rate-limited. Clippy-clean.
//! 1.3.1  Audit release (all changes tagged `v1.3.1 FIX` / `v1.3.1 PERF` inline). Crash fixes:
//!        timeline Ctrl+wheel zoom on short clips (f64::clamp min>max panic), poisoned
//!        mutexes no longer cascade panics. Data safety: crash-safe Save ordering (tracks
//!        staged, project.txt swapped, stale files removed last), narration finalized on
//!        quit, export temp files removed on every error path, Compact discarded if the
//!        edit changed meanwhile, exports may not overwrite the project's own files, NaN
//!        gains rejected on load. Correctness: decode_audio track filter precedence and
//!        sample-buffer spec changes, recordings reset dead-air/FX state like Open/New,
//!        narration start no longer leaves an empty track on mic failure (one undo step),
//!        ruler labels past 1 h. Efficiency: Save skips unchanged track WAVs, WAV export
//!        streams (O(1) memory) with progress+cancel, ffmpeg runs cancellable with live
//!        progress, dead-air scan skips JPEG decode while audio is audible. Clippy-clean.
//! 1.3.0  Opto compressor (CL 1B-style: soft-knee optical cell with program-dependent
//!        release + tube make-up stage) with presets, transfer curve and A/B audition.
//!        Normalize levels: BS.1770 gated LUFS per track to a target with a look-ahead
//!        -1 dBFS limiter. Output peak meter; soft -1 dBFS ceiling replaces hard clipping
//!        in playback/export. UI: all controls in one grouped dark dock under the preview.
//! 1.2.0  MOV/MP4 fix: ffmpeg is found beyond PATH (env override, saved choice, exe dir,
//!        WinGet/Scoop/Chocolatey/Program Files) with a Locate dialog; CREATE_NO_WINDOW.
//!        New project (button + Ctrl+N) with unsaved-changes guard (also on Open).
//!        Double-Space returns to 0; Ctrl+X cuts. Dead-air finder (silent + static) with
//!        one-step removal; Compact project (lossless media rewrite to reclaim disk).
//!        Speed up selection: Seg::Fast (lossless frame skipping) + WSOLA pitch-preserving
//!        audio compression; project keys seg=F and media=.
//! 1.1.0  System audio ("System audio" checkbox): records what the computer plays to
//!        its own stereo "System" track alongside the mic (WASAPI loopback on Windows,
//!        default-sink monitor via native Pulse/PipeWire stream on Linux). Linux mic and
//!        system capture use a native Pulse record stream (fixes ~2 s bursts, dropped
//!        audio and lost tail with cpal's Pulse backend); gap filling keeps audio in sync
//!        across idle output and pause/resume; tail trimmed to the exact stop time.
//!        Preview playback latency on Pulse cut from ~2 s to ~40 ms and the playhead now
//!        follows the audible sample. --selftest ... sys.
//! 1.0.0  Initial release: full/region capture, mic, timeline, cut/freeze,
//!        narration re-record, track add/swap/remove/shift, MKV/MOV/MP4/WAV export,
//!        project save/load, --selftest harness.

#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering::*};
use std::sync::mpsc::{self, Receiver, Sender, TrySendError};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, Sample, SizedSample};
use fltk::{
    app, button::{Button, CheckButton}, dialog, draw, enums::{Align, Color, Cursor, Event, Font, FrameType, Key, Shortcut},
    frame::Frame, group::Flex, image::RgbImage, menu::Choice, misc::Spinner, prelude::*,
    valuator::{HorNiceSlider, HorValueSlider}, window::Window,
};

// Every in-file version reference derives from these two constants (bump together).
const VERSION: &str = "1.4.0";
const FILE_TAG: &str = "screenreel_v1.4.0";
const APP_NAME: &str = "ScreenReel";
const AR: u32 = 48_000; // canonical project audio rate
const FPS_CHOICES: [u32; 8] = [10, 15, 20, 24, 25, 30, 50, 60]; // all divide 48000 exactly
const UNDO_MAX: usize = 64;
const MAX_JPEG: u32 = 64 << 20; // sanity bound per frame when loading projects
const MAX_AUDIO_FRAMES: usize = AR as usize * 60 * 60 * 4; // 4 h import cap (memory guard)

type Res<T> = Result<T, String>;
/// (channels, interleaved 48 kHz samples). v1.3.1: alias for clippy::type_complexity.
type PcmTrack = (u16, Vec<i16>);
fn es<E: std::fmt::Display>(e: E) -> String { e.to_string() }

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Rect { x: i32, y: i32, w: i32, h: i32 }

// =====================================================================================
// Screen capture backends
// =====================================================================================

/// Alpha-blend a premultiplied ARGB cursor image onto a BGRA frame.
// v1.3.1 FIX (clippy): 8 plain scalars read clearer than a one-off struct here.
#[allow(clippy::too_many_arguments)]
fn blend_cursor(buf: &mut [u8], w: usize, h: usize, ox: i32, oy: i32, cw: usize, ch: usize, px: &[u32]) {
    if px.len() < cw * ch { return; }
    for cy in 0..ch {
        let y = oy + cy as i32;
        if y < 0 || y >= h as i32 { continue; }
        for cx in 0..cw {
            let x = ox + cx as i32;
            if x < 0 || x >= w as i32 { continue; }
            let p = px[cy * cw + cx];
            let a = p >> 24;
            if a == 0 { continue; }
            let i = (y as usize * w + x as usize) * 4;
            let inv = 255 - a;
            for (k, sh) in [(0usize, 0u32), (1, 8), (2, 16)] {
                let v = ((p >> sh) & 0xff) + (buf[i + k] as u32 * inv + 127) / 255;
                buf[i + k] = v.min(255) as u8;
            }
        }
    }
}

#[cfg(target_os = "linux")]
mod cap {
    use super::*;
    use x11rb::connection::{Connection, RequestConnection};
    use x11rb::protocol::shm::ConnectionExt as _;
    use x11rb::protocol::xfixes::ConnectionExt as _;
    use x11rb::protocol::xproto::{ConnectionExt as _, ImageFormat, ImageOrder};
    use x11rb::rust_connection::RustConnection;

    fn connect() -> Res<(RustConnection, usize)> {
        x11rb::connect(None).map_err(|e| format!(
            "Cannot connect to the X server ({e}). On Wayland run under an X11 session or XWayland with DISPLAY set."))
    }

    pub fn virtual_screen() -> Res<Rect> {
        let (c, n) = connect()?;
        let s = &c.setup().roots[n];
        Ok(Rect { x: 0, y: 0, w: s.width_in_pixels as i32, h: s.height_in_pixels as i32 })
    }

    struct Shm { seg: u32, ptr: *mut u8 }

    pub struct Grabber { conn: RustConnection, root: u32, pub r: Rect, shm: Option<Shm>, xfixes: bool }
    // SAFETY: the raw SHM pointer is owned exclusively by this struct and only used
    // by the thread that owns the Grabber.
    unsafe impl Send for Grabber {}

    impl Grabber {
        pub fn new(r: Rect, cursor: bool) -> Res<Self> {
            let (conn, n) = connect()?;
            let setup = conn.setup();
            let scr = &setup.roots[n];
            let depth = scr.root_depth;
            let bpp32 = setup.pixmap_formats.iter().any(|f| f.depth == depth && f.bits_per_pixel == 32);
            if !(depth == 24 || depth == 32) || !bpp32 || setup.image_byte_order != ImageOrder::LSB_FIRST {
                return Err(format!("Unsupported X visual (depth {depth}); need 24/32-bit TrueColor LSB"));
            }
            let (sw, sh) = (scr.width_in_pixels as i32, scr.height_in_pixels as i32);
            let x = r.x.clamp(0, sw - 1);
            let y = r.y.clamp(0, sh - 1);
            let r = Rect { x, y, w: r.w.min(sw - x), h: r.h.min(sh - y) };
            if r.w < 16 || r.h < 16 { return Err("Capture region too small".into()); }
            let root = scr.root;
            let xfixes = cursor && conn.extension_information(x11rb::protocol::xfixes::X11_EXTENSION_NAME)
                .ok().flatten().is_some()
                && conn.xfixes_query_version(4, 0).ok().and_then(|c| c.reply().ok()).is_some();
            let mut g = Grabber { conn, root, r, shm: None, xfixes };
            g.shm = g.try_shm();
            Ok(g)
        }

        fn try_shm(&self) -> Option<Shm> {
            self.conn.extension_information(x11rb::protocol::shm::X11_EXTENSION_NAME).ok()??;
            self.conn.shm_query_version().ok()?.reply().ok()?;
            let size = self.r.w as usize * self.r.h as usize * 4;
            unsafe {
                let id = libc::shmget(libc::IPC_PRIVATE, size, libc::IPC_CREAT | 0o600);
                if id < 0 { return None; }
                let p = libc::shmat(id, std::ptr::null(), 0);
                if p as isize == -1 {
                    libc::shmctl(id, libc::IPC_RMID, std::ptr::null_mut());
                    return None;
                }
                let seg = self.conn.generate_id().ok();
                // check() round-trips so the server has attached before we mark for removal.
                let attached = seg.and_then(|s| self.conn.shm_attach(s, id as u32, false).ok()?.check().ok()).is_some();
                // Mark for deletion now: freed automatically when both sides detach (crash-safe, no leak).
                libc::shmctl(id, libc::IPC_RMID, std::ptr::null_mut());
                if !attached { libc::shmdt(p); return None; }
                Some(Shm { seg: seg?, ptr: p as *mut u8 })
            }
        }

        pub fn uses_shm(&self) -> bool { self.shm.is_some() }

        /// Grab one BGRA frame into `out` (resized as needed, capacity reused).
        pub fn grab(&mut self, out: &mut Vec<u8>) -> Res<()> {
            let (w, h) = (self.r.w as usize, self.r.h as usize);
            let need = w * h * 4;
            out.resize(need, 0);
            // Pipeline the cursor request with the image request (one round trip total).
            let cur = if self.xfixes { self.conn.xfixes_get_cursor_image().ok() } else { None };
            if let Some(s) = &self.shm {
                self.conn.shm_get_image(self.root, self.r.x as i16, self.r.y as i16, w as u16, h as u16,
                    !0, ImageFormat::Z_PIXMAP.into(), s.seg, 0).map_err(es)?.reply().map_err(es)?;
                // SAFETY: segment is exactly `need` bytes and the server wrote it before replying.
                unsafe { std::ptr::copy_nonoverlapping(s.ptr, out.as_mut_ptr(), need) };
            } else {
                let rep = self.conn.get_image(ImageFormat::Z_PIXMAP, self.root, self.r.x as i16, self.r.y as i16,
                    w as u16, h as u16, !0).map_err(es)?.reply().map_err(es)?;
                if rep.data.len() < need { return Err("short X image".into()); }
                out.copy_from_slice(&rep.data[..need]);
            }
            if let Some(c) = cur {
                if let Ok(ci) = c.reply() {
                    blend_cursor(out, w, h, ci.x as i32 - ci.xhot as i32 - self.r.x, ci.y as i32 - ci.yhot as i32 - self.r.y,
                        ci.width as usize, ci.height as usize, &ci.cursor_image);
                }
            }
            Ok(())
        }
    }

    impl Drop for Grabber {
        fn drop(&mut self) {
            if let Some(s) = self.shm.take() {
                let _ = self.conn.shm_detach(s.seg);
                let _ = self.conn.flush();
                unsafe { libc::shmdt(s.ptr as *const libc::c_void) };
            }
        }
    }

    pub fn exclude_from_capture<W: WindowExt>(_w: &W) {}
    pub fn set_dpi_aware() {}
}

#[cfg(windows)]
mod cap {
    use super::*;
    use std::ptr::null_mut;
    use windows_sys::Win32::Graphics::Gdi::*;
    use windows_sys::Win32::UI::WindowsAndMessaging::*;

    pub fn set_dpi_aware() {
        use windows_sys::Win32::UI::HiDpi::*;
        // Physical-pixel coordinates for capture; harmless if FLTK already set it.
        unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
    }

    pub fn virtual_screen() -> Res<Rect> {
        unsafe {
            let r = Rect { x: GetSystemMetrics(SM_XVIRTUALSCREEN), y: GetSystemMetrics(SM_YVIRTUALSCREEN),
                w: GetSystemMetrics(SM_CXVIRTUALSCREEN), h: GetSystemMetrics(SM_CYVIRTUALSCREEN) };
            if r.w <= 0 || r.h <= 0 { return Err("GetSystemMetrics failed".into()); }
            Ok(r)
        }
    }

    pub struct Grabber { screen: HDC, mem: HDC, bmp: HBITMAP, old: HGDIOBJ, bits: *mut u8, pub r: Rect,
        cursor: bool, hot: (HCURSOR, i32, i32) }
    // SAFETY: GDI handles are created and used only by the owning capture thread.
    unsafe impl Send for Grabber {}

    impl Grabber {
        pub fn new(r: Rect, cursor: bool) -> Res<Self> {
            let vs = virtual_screen()?;
            let x = r.x.clamp(vs.x, vs.x + vs.w - 1);
            let y = r.y.clamp(vs.y, vs.y + vs.h - 1);
            let r = Rect { x, y, w: r.w.min(vs.x + vs.w - x), h: r.h.min(vs.y + vs.h - y) };
            if r.w < 16 || r.h < 16 { return Err("Capture region too small".into()); }
            unsafe {
                let screen = GetDC(null_mut());
                if screen.is_null() { return Err("GetDC failed".into()); }
                let mem = CreateCompatibleDC(screen);
                if mem.is_null() { ReleaseDC(null_mut(), screen); return Err("CreateCompatibleDC failed".into()); }
                let mut bi: BITMAPINFO = std::mem::zeroed();
                bi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
                bi.bmiHeader.biWidth = r.w;
                bi.bmiHeader.biHeight = -r.h; // top-down
                bi.bmiHeader.biPlanes = 1;
                bi.bmiHeader.biBitCount = 32;
                bi.bmiHeader.biCompression = BI_RGB;
                let mut bits: *mut core::ffi::c_void = null_mut();
                let bmp = CreateDIBSection(mem, &bi, DIB_RGB_COLORS, &mut bits, null_mut(), 0);
                if bmp.is_null() || bits.is_null() {
                    DeleteDC(mem); ReleaseDC(null_mut(), screen);
                    return Err("CreateDIBSection failed".into());
                }
                let old = SelectObject(mem, bmp);
                Ok(Grabber { screen, mem, bmp, old, bits: bits as *mut u8, r, cursor, hot: (null_mut(), 0, 0) })
            }
        }

        pub fn uses_shm(&self) -> bool { false }

        pub fn grab(&mut self, out: &mut Vec<u8>) -> Res<()> {
            let (w, h) = (self.r.w as usize, self.r.h as usize);
            let need = w * h * 4;
            out.resize(need, 0);
            unsafe {
                // SRCCOPY only: CAPTUREBLT makes the cursor flicker and is not needed under DWM.
                if BitBlt(self.mem, 0, 0, self.r.w, self.r.h, self.screen, self.r.x, self.r.y, SRCCOPY) == 0 {
                    return Err("BitBlt failed (secure desktop / UAC?)".into());
                }
                if self.cursor {
                    let mut ci: CURSORINFO = std::mem::zeroed();
                    ci.cbSize = std::mem::size_of::<CURSORINFO>() as u32;
                    if GetCursorInfo(&mut ci) != 0 && (ci.flags & CURSOR_SHOWING) != 0 && !ci.hCursor.is_null() {
                        if self.hot.0 != ci.hCursor {
                            // Cache hotspot per cursor handle; free the bitmaps GetIconInfo allocates.
                            let mut ii: ICONINFO = std::mem::zeroed();
                            if GetIconInfo(ci.hCursor, &mut ii) != 0 {
                                if !ii.hbmMask.is_null() { DeleteObject(ii.hbmMask); }
                                if !ii.hbmColor.is_null() { DeleteObject(ii.hbmColor); }
                                self.hot = (ci.hCursor, ii.xHotspot as i32, ii.yHotspot as i32);
                            }
                        }
                        DrawIconEx(self.mem, ci.ptScreenPos.x - self.r.x - self.hot.1, ci.ptScreenPos.y - self.r.y - self.hot.2,
                            ci.hCursor, 0, 0, 0, null_mut(), DI_NORMAL);
                    }
                }
                GdiFlush();
                std::ptr::copy_nonoverlapping(self.bits, out.as_mut_ptr(), need);
            }
            Ok(())
        }
    }

    impl Drop for Grabber {
        fn drop(&mut self) {
            unsafe {
                SelectObject(self.mem, self.old);
                DeleteObject(self.bmp);
                DeleteDC(self.mem);
                ReleaseDC(null_mut(), self.screen);
            }
        }
    }

    /// Keep the recording control window out of the captured video (Win10 2004+).
    pub fn exclude_from_capture<W: WindowExt>(w: &W) {
        unsafe { SetWindowDisplayAffinity(w.raw_handle() as _, WDA_EXCLUDEFROMCAPTURE) };
    }
}

#[cfg(not(any(target_os = "linux", windows)))]
mod cap {
    use super::*;
    pub fn virtual_screen() -> Res<Rect> { Err("unsupported platform".into()) }
    pub struct Grabber { pub r: Rect }
    impl Grabber {
        pub fn new(_r: Rect, _c: bool) -> Res<Self> { Err("unsupported platform".into()) }
        pub fn uses_shm(&self) -> bool { false }
        pub fn grab(&mut self, _o: &mut Vec<u8>) -> Res<()> { Err("unsupported".into()) }
    }
    pub fn exclude_from_capture<W: WindowExt>(_w: &W) {}
    pub fn set_dpi_aware() {}
}

// =====================================================================================
// v1.4.0 NEW: input events (cursor position, clicks, "a key was typed") for auto-zoom,
// cursor highlight and click rings.
// PRIVACY: only *that* a non-modifier key went down during a frame is recorded, never which
// key: the .ev stream cannot be used to reconstruct typed text or passwords.
// =====================================================================================

/// Per-frame input sample, 5 bytes on disk (`<base>.ev`, one entry per idx entry).
/// x/y are in *stored frame* pixels (after half-res), relative to the capture region.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Ev { x: i16, y: i16, f: u8 }

const EV_LDOWN: u8 = 1;   // left button held
const EV_RDOWN: u8 = 2;   // right button held
const EV_CLICK: u8 = 4;   // a button went down during this frame (edge, latched by the poller)
const EV_KEY: u8 = 8;     // a non-modifier key went down during this frame
const EV_VIS: u8 = 16;    // pointer inside the captured region

impl Ev {
    fn to_bytes(self) -> [u8; 5] {
        let (x, y) = (self.x.to_le_bytes(), self.y.to_le_bytes());
        [x[0], x[1], y[0], y[1], self.f]
    }
    fn from_bytes(b: &[u8]) -> Ev { Ev { x: i16::from_le_bytes([b[0], b[1]]), y: i16::from_le_bytes([b[2], b[3]]), f: b[4] } }
}

/// Poller -> capture thread. Edges are latched (OR-ed) between frames so a 50 ms click is
/// never lost at 10 fps; the capture thread swaps them out once per frame.
#[derive(Default)]
struct InputShared { x: std::sync::atomic::AtomicI32, y: std::sync::atomic::AtomicI32, held: AtomicU32, latch: AtomicU32, ok: AtomicBool }

impl InputShared {
    /// Sample for one frame, mapped into region-relative stored-frame coordinates.
    fn take(&self, r: Rect, half: bool) -> Ev {
        if !self.ok.load(Relaxed) { return Ev::default(); }
        let (sx, sy) = (self.x.load(Relaxed) - r.x, self.y.load(Relaxed) - r.y);
        let mut f = (self.held.load(Relaxed) as u8 & (EV_LDOWN | EV_RDOWN)) | self.latch.swap(0, Relaxed) as u8;
        if sx >= 0 && sy >= 0 && sx < r.w && sy < r.h { f |= EV_VIS; }
        let d = if half { 2 } else { 1 };
        Ev { x: (sx / d).clamp(i16::MIN as i32, i16::MAX as i32) as i16, y: (sy / d).clamp(i16::MIN as i32, i16::MAX as i32) as i16, f }
    }
}

/// 125 Hz input poller. Polling (not a global hook) needs no privileges, works under any WM,
/// and cannot block or slow down other applications' input.
struct InputPoller { stop: Arc<AtomicBool>, th: Option<JoinHandle<()>> }

impl InputPoller {
    fn start(sh: Arc<InputShared>) -> InputPoller {
        let stop = Arc::new(AtomicBool::new(false));
        let st = stop.clone();
        let th = thread::Builder::new().name("input".into()).spawn(move || {
            if let Err(e) = inp::run(&sh, &st) { eprintln!("[input] disabled: {e}"); }
            sh.ok.store(false, Relaxed);
        }).ok();
        InputPoller { stop, th }
    }
}
impl Drop for InputPoller {
    fn drop(&mut self) { self.stop.store(true, Relaxed); if let Some(t) = self.th.take() { let _ = t.join(); } }
}

#[cfg(target_os = "linux")]
mod inp {
    use super::*;
    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::ConnectionExt as _;

    pub fn run(sh: &InputShared, stop: &AtomicBool) -> Res<()> {
        let (c, n) = x11rb::connect(None).map_err(es)?;
        let root = c.setup().roots[n].root;
        // modifier keycodes (Shift/Ctrl/Alt/Super...) never count as "typing"
        let mut is_mod = [false; 256];
        if let Ok(m) = c.get_modifier_mapping().map_err(es)?.reply() {
            for &k in &m.keycodes { if k != 0 { is_mod[k as usize] = true; } }
        }
        let mut prev = [0u8; 32];
        let mut prev_btn = 0u32;
        while !stop.load(Relaxed) {
            let p = c.query_pointer(root).map_err(es)?;
            let k = c.query_keymap().map_err(es)?;
            let p = p.reply().map_err(es)?;
            let k = k.reply().map_err(es)?;
            let mask = u16::from(p.mask) as u32;
            let btn = (if mask & 0x100 != 0 { EV_LDOWN } else { 0 } | if mask & 0x400 != 0 { EV_RDOWN } else { 0 }) as u32;
            let mut latch = 0u32;
            if btn & !prev_btn != 0 { latch |= EV_CLICK as u32; }
            prev_btn = btn;
            for (i, (&now, &was)) in k.keys.iter().zip(prev.iter()).enumerate() {
                let down = now & !was;
                if down != 0 && (0..8).any(|b| down & (1 << b) != 0 && !is_mod[i * 8 + b]) { latch |= EV_KEY as u32; }
            }
            prev = k.keys;
            sh.x.store(p.root_x as i32, Relaxed);
            sh.y.store(p.root_y as i32, Relaxed);
            sh.held.store(btn, Relaxed);
            if latch != 0 { sh.latch.fetch_or(latch, Relaxed); }
            sh.ok.store(true, Relaxed);
            thread::sleep(Duration::from_millis(8));
        }
        Ok(())
    }
}

#[cfg(windows)]
mod inp {
    use super::*;
    use windows_sys::Win32::Foundation::POINT;
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;
    use windows_sys::Win32::UI::WindowsAndMessaging::GetCursorPos;

    pub fn run(sh: &InputShared, stop: &AtomicBool) -> Res<()> {
        let skip = |vk: u16| matches!(vk, VK_LBUTTON | VK_RBUTTON | VK_MBUTTON | VK_XBUTTON1 | VK_XBUTTON2
            | VK_SHIFT | VK_CONTROL | VK_MENU | VK_LSHIFT | VK_RSHIFT | VK_LCONTROL | VK_RCONTROL
            | VK_LMENU | VK_RMENU | VK_LWIN | VK_RWIN | VK_CAPITAL | VK_NUMLOCK | VK_SCROLL);
        let mut prev = [false; 256];
        let mut prev_btn = 0u32;
        while !stop.load(Relaxed) {
            let mut pt = POINT { x: 0, y: 0 };
            // SAFETY: plain Win32 queries on a stack POINT; no handles involved.
            if unsafe { GetCursorPos(&mut pt) } == 0 { thread::sleep(Duration::from_millis(50)); continue; }
            let down = |vk: u16| unsafe { GetAsyncKeyState(vk as i32) } as u16 & 0x8000 != 0;
            // swapped mouse buttons are already resolved by GetAsyncKeyState (logical buttons)
            let btn = (if down(VK_LBUTTON) { EV_LDOWN } else { 0 } | if down(VK_RBUTTON) { EV_RDOWN } else { 0 }) as u32;
            let mut latch = 0u32;
            if btn & !prev_btn != 0 { latch |= EV_CLICK as u32; }
            prev_btn = btn;
            for vk in 8u16..0xFF {
                if skip(vk) { continue; }
                let d = down(vk);
                if d && !prev[vk as usize] { latch |= EV_KEY as u32; }
                prev[vk as usize] = d;
            }
            sh.x.store(pt.x, Relaxed);
            sh.y.store(pt.y, Relaxed);
            sh.held.store(btn, Relaxed);
            if latch != 0 { sh.latch.fetch_or(latch, Relaxed); }
            sh.ok.store(true, Relaxed);
            thread::sleep(Duration::from_millis(8));
        }
        Ok(())
    }
}

#[cfg(not(any(target_os = "linux", windows)))]
mod inp {
    use super::*;
    pub fn run(_sh: &InputShared, _stop: &AtomicBool) -> Res<()> { Err("unsupported platform".into()) }
}

// =====================================================================================
// Pixel helpers
// =====================================================================================

/// 2x2 box downscale (BGRA). Used for "half resolution" capture.
fn half_bgra(src: &[u8], w: usize, h: usize, dst: &mut Vec<u8>) -> (usize, usize) {
    let (ow, oh) = (w / 2, h / 2);
    dst.resize(ow * oh * 4, 0);
    for y in 0..oh {
        let r0 = &src[(2 * y) * w * 4..];
        let r1 = &src[(2 * y + 1) * w * 4..];
        let d = &mut dst[y * ow * 4..(y + 1) * ow * 4];
        for x in 0..ow {
            let i = 8 * x;
            for c in 0..4 {
                let s = r0[i + c] as u32 + r0[i + 4 + c] as u32 + r1[i + c] as u32 + r1[i + 4 + c] as u32;
                d[4 * x + c] = ((s + 2) >> 2) as u8;
            }
        }
    }
    (ow, oh)
}

/// Fit (w,h) into (bw,bh) preserving aspect ratio.
fn fit(w: i32, h: i32, bw: i32, bh: i32) -> (i32, i32) {
    if w <= 0 || h <= 0 || bw <= 0 || bh <= 0 { return (0, 0); }
    let s = (bw as f64 / w as f64).min(bh as f64 / h as f64);
    (((w as f64 * s) as i32).max(1), ((h as f64 * s) as i32).max(1))
}

/// Downscale RGB with a 2x2 average around each sample point (cheap anti-aliasing).
fn scale_rgb(src: &[u8], w: usize, h: usize, ow: usize, oh: usize) -> Vec<u8> {
    if ow == w && oh == h { return src.to_vec(); }
    let mut out = vec![0u8; ow * oh * 3];
    let xs: Vec<usize> = (0..ow).map(|x| ((x * w) / ow).min(w.saturating_sub(2))).collect();
    for y in 0..oh {
        let sy = ((y * h) / oh).min(h.saturating_sub(2));
        let (r0, r1) = (sy * w * 3, (sy + 1).min(h - 1) * w * 3);
        let d = &mut out[y * ow * 3..(y + 1) * ow * 3];
        for (x, &sx) in xs.iter().enumerate() {
            let (a, b) = (r0 + sx * 3, r1 + sx * 3);
            for c in 0..3 {
                let s = src[a + c] as u32 + src[a + 3 + c] as u32 + src[b + c] as u32 + src[b + 3 + c] as u32;
                d[3 * x + c] = ((s + 2) >> 2) as u8;
            }
        }
    }
    out
}

fn bgra_to_rgb(src: &[u8], dim: bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(src.len() / 4 * 3);
    for p in src.chunks_exact(4) {
        if dim { out.extend_from_slice(&[p[2] / 3, p[1] / 3, p[0] / 3]); } else { out.extend_from_slice(&[p[2], p[1], p[0]]); }
    }
    out
}

fn decode_jpeg_rgb(data: &[u8]) -> Res<(Vec<u8>, usize, usize)> {
    use zune_core::colorspace::ColorSpace;
    use zune_core::options::DecoderOptions;
    let opts = DecoderOptions::default().jpeg_set_out_colorspace(ColorSpace::RGB)
        .set_max_width(16384).set_max_height(16384);
    let mut d = zune_jpeg::JpegDecoder::new_with_options(std::io::Cursor::new(data), opts);
    let px = d.decode().map_err(|e| format!("jpeg decode: {e:?}"))?;
    let info = d.info().ok_or("jpeg: no info")?;
    let (w, h) = (info.width as usize, info.height as usize);
    if px.len() < w * h * 3 { return Err("jpeg: short output".into()); }
    Ok((px, w, h))
}

// =====================================================================================
// Audio helpers
// =====================================================================================

/// Linear-interpolating stretch of interleaved i16 to exactly `target_frames` frames.
fn stretch(src: &[i16], ch: usize, target_frames: usize) -> Vec<i16> {
    let n = src.len() / ch.max(1);
    if n == target_frames { return src[..n * ch].to_vec(); }
    let mut out = vec![0i16; target_frames * ch];
    if n == 0 || target_frames == 0 { return out; }
    let step = if target_frames > 1 { (n - 1) as f64 / (target_frames - 1) as f64 } else { 0.0 };
    for i in 0..target_frames {
        let p = i as f64 * step;
        let i0 = (p as usize).min(n - 1);
        let i1 = (i0 + 1).min(n - 1);
        let f = (p - i0 as f64) as f32;
        for c in 0..ch {
            let a = src[i0 * ch + c] as f32;
            let b = src[i1 * ch + c] as f32;
            out[i * ch + c] = (a + (b - a) * f).round() as i16;
        }
    }
    out
}

fn resample(src: &[i16], ch: usize, from: u32, to: u32) -> Vec<i16> {
    if from == to || from == 0 { return src.to_vec(); }
    let n = src.len() / ch.max(1);
    let target = ((n as u128 * to as u128) / from as u128) as usize;
    stretch(src, ch, target)
}

/// Make recorded audio exactly `target` frames long. Small drift (<=3%, device clock vs
/// wall clock + start latency) is absorbed by stretching; larger mismatch pads/truncates.
fn fit_len(v: Vec<i16>, ch: usize, target: usize) -> Vec<i16> {
    let n = v.len() / ch;
    if n == target { return v; }
    let diff = (n as f64 - target as f64).abs() / target.max(1) as f64;
    if n > 0 && diff <= 0.03 { return stretch(&v, ch, target); }
    let mut v = v;
    v.resize(target * ch, 0);
    v
}

#[inline]
fn f2i(s: f32) -> i16 { (s.clamp(-1.0, 1.0) * 32767.0).round() as i16 }

fn write_wav(path: &Path, ch: u16, rate: u32, data: &[i16]) -> Res<()> {
    write_wav_with(path, ch, rate, data.len() as u64, |w| {
        let mut buf = Vec::with_capacity(64 * 1024);
        for chunk in data.chunks(32 * 1024) {
            buf.clear();
            for s in chunk { buf.extend_from_slice(&s.to_le_bytes()); }
            w.write_all(&buf).map_err(es)?;
        }
        Ok(())
    })
}

/// v1.3.1: streaming WAV writer. `fill` must write exactly `n_samples` i16 LE samples; the
/// header is written up front (size known), the file is staged as `.wav.tmp` and renamed
/// into place only on success. v1.3.0 left the `.tmp` behind on any write error.
fn write_wav_with(path: &Path, ch: u16, rate: u32, n_samples: u64, fill: impl FnOnce(&mut BufWriter<File>) -> Res<()>) -> Res<()> {
    let bytes = n_samples * 2;
    if bytes > (u32::MAX - 44) as u64 { return Err("WAV would exceed 4 GiB".into()); }
    let tmp = path.with_extension("wav.tmp");
    let r = (|| -> Res<()> {
        let mut w = BufWriter::with_capacity(1 << 20, File::create(&tmp).map_err(|e| format!("{}: {e}", tmp.display()))?);
        let mut h = Vec::with_capacity(44);
        h.extend_from_slice(b"RIFF");
        h.extend_from_slice(&(36 + bytes as u32).to_le_bytes());
        h.extend_from_slice(b"WAVEfmt ");
        h.extend_from_slice(&16u32.to_le_bytes());
        h.extend_from_slice(&1u16.to_le_bytes());
        h.extend_from_slice(&ch.to_le_bytes());
        h.extend_from_slice(&rate.to_le_bytes());
        h.extend_from_slice(&(rate * ch as u32 * 2).to_le_bytes());
        h.extend_from_slice(&(ch * 2).to_le_bytes());
        h.extend_from_slice(&16u16.to_le_bytes());
        h.extend_from_slice(b"data");
        h.extend_from_slice(&(bytes as u32).to_le_bytes());
        w.write_all(&h).map_err(es)?;
        fill(&mut w)?;
        w.flush().map_err(es)?;
        // v1.3.1 FIX: verify the producer wrote exactly what the header promised
        let len = w.get_ref().metadata().map_err(es)?.len();
        if len != 44 + bytes { return Err(format!("WAV size mismatch ({len} != {})", 44 + bytes)); }
        Ok(())
    })();
    match r {
        Ok(()) => std::fs::rename(&tmp, path).map_err(|e| { let _ = std::fs::remove_file(&tmp); format!("{}: {e}", path.display()) }),
        Err(e) => { let _ = std::fs::remove_file(&tmp); Err(e) }
    }
}

/// v1.3.1: true if `path` is exactly what `write_wav(path, ch, AR, data)` with `n` samples
/// would produce (44-byte PCM header, 48 kHz, s16, same size), so Save may skip rewriting it.
fn wav_is_canonical(path: &Path, ch: u16, n: usize) -> bool {
    let Ok(mut f) = File::open(path) else { return false };
    let mut h = [0u8; 44];
    if f.read_exact(&mut h).is_err() { return false; }
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    &h[0..4] == b"RIFF" && &h[8..16] == b"WAVEfmt " && h[20..22] == 1u16.to_le_bytes() && h[22..24] == ch.to_le_bytes()
        && h[24..28] == AR.to_le_bytes() && h[34..36] == 16u16.to_le_bytes() && &h[36..40] == b"data"
        && len == 44 + n as u64 * 2
}

/// Decode any supported audio (wav/mp3/flac/ogg/m4a/aac/mkv/mp4 audio) to 48 kHz i16,
/// mono or stereo. Returns (channels, interleaved samples).
fn decode_audio(path: &Path) -> Res<(u16, Vec<i16>)> {
    use symphonia::core::{audio::SampleBuffer, codecs::{DecoderOptions, CODEC_TYPE_NULL}, errors::Error as SE,
        formats::FormatOptions, io::MediaSourceStream, meta::MetadataOptions, probe::Hint};
    let f = File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mss = MediaSourceStream::new(Box::new(f), Default::default());
    let mut hint = Hint::new();
    if let Some(e) = path.extension().and_then(|e| e.to_str()) { hint.with_extension(e); }
    let probed = symphonia::default::get_probe()
        .format(&hint, mss, &FormatOptions::default(), &MetadataOptions::default())
        .map_err(|e| format!("unsupported audio file: {e}"))?;
    let mut fmt = probed.format;
    // v1.3.1 FIX: `a && b || c` parsed as `(a && b) || c`, so a NULL-codec (e.g. subtitle or
    // attachment) track that merely advertised a sample rate could be picked over real audio.
    let track = fmt.tracks().iter().find(|t| t.codec_params.codec != CODEC_TYPE_NULL
        && (t.codec_params.channels.is_some() || t.codec_params.sample_rate.is_some())).ok_or("no audio track found")?;
    let tid = track.id;
    let mut rate = track.codec_params.sample_rate.unwrap_or(0);
    // Some demuxers (e.g. MKV with PCM) omit max_frames_per_packet, which the PCM decoder requires.
    let mut params = track.codec_params.clone();
    if params.max_frames_per_packet.is_none() { params.with_max_frames_per_packet(1 << 18); }
    let mut dec = symphonia::default::get_codecs().make(&params, &DecoderOptions::default()).map_err(es)?;
    let mut out_ch = 0usize;
    let mut out: Vec<i16> = Vec::new();
    let mut sb: Option<SampleBuffer<i16>> = None;
    let mut sb_spec = None; // v1.3.1: spec the sample buffer was built for
    loop {
        let pkt = match fmt.next_packet() {
            Ok(p) => p,
            Err(SE::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(SE::ResetRequired) => break,
            Err(e) => return Err(es(e)),
        };
        if pkt.track_id() != tid { continue; }
        match dec.decode(&pkt) {
            Ok(buf) => {
                let spec = *buf.spec();
                if rate == 0 { rate = spec.rate; }
                let c = spec.channels.count().max(1);
                if out_ch == 0 { out_ch = if c >= 2 { 2 } else { 1 }; }
                // v1.3.1 FIX: also rebuild when the stream's spec changes mid-file (channel count
                // or rate switch in chained OGG / broadcast MP3); a buffer sized for the old
                // layout made copy_interleaved_ref panic or mis-interleave frames.
                if sb_spec != Some(spec) || sb.as_ref().map(|s| s.capacity() < buf.capacity() * c).unwrap_or(true) {
                    sb = Some(SampleBuffer::<i16>::new(buf.capacity() as u64, spec));
                    sb_spec = Some(spec);
                }
                let s = sb.as_mut().unwrap();
                s.copy_interleaved_ref(buf);
                for fr in s.samples().chunks_exact(c) {
                    if out_ch == 1 { out.push(fr[0]); } else { out.push(fr[0]); out.push(fr[if c > 1 { 1 } else { 0 }]); }
                }
                if out.len() / out_ch > MAX_AUDIO_FRAMES { return Err("audio longer than 4 hours".into()); }
            }
            Err(SE::DecodeError(_)) => continue, // skip corrupt packet, keep going
            Err(e) => return Err(es(e)),
        }
    }
    if out_ch == 0 || out.is_empty() { return Err("no audio decoded".into()); }
    if rate == 0 { return Err("unknown sample rate".into()); }
    Ok((out_ch as u16, resample(&out, out_ch, rate, AR)))
}

fn input_device_names() -> Vec<String> {
    let host = cpal::default_host();
    match host.input_devices() {
        Ok(it) => it.filter_map(|d| d.description().ok().map(|d| d.name().to_string())).collect(),
        Err(_) => vec![],
    }
}

fn find_input(name: Option<&str>) -> Res<cpal::Device> {
    let host = cpal::default_host();
    if let Some(n) = name {
        if let Ok(it) = host.input_devices() {
            for d in it {
                if d.description().ok().map(|x| x.name() == n).unwrap_or(false) { return Ok(d); }
            }
        }
    }
    host.default_input_device().ok_or_else(|| "no microphone / input device found".to_string())
}

/// System-audio ("what you hear") device for the cpal path.
/// Windows: WASAPI loopback - opening the default *output* device as an input enables it.
#[cfg(windows)]
fn find_loopback() -> Res<(cpal::Device, cpal::SupportedStreamConfig)> {
    let host = cpal::default_host();
    let dev = host.default_output_device().ok_or("no audio output device to capture from")?;
    let sc = dev.default_output_config().map_err(|e| format!("system audio config: {e}"))?;
    Ok((dev, sc))
}

/// Linux without a Pulse/PipeWire server: plain ALSA has no generic loopback.
#[cfg(not(windows))]
fn find_loopback() -> Res<(cpal::Device, cpal::SupportedStreamConfig)> {
    Err("system audio needs PulseAudio or PipeWire (no sound server found)".into())
}

/// Pause state + "active time" clock shared by the video capture thread and the audio
/// readers. `active()` = wall time since creation minus time spent paused; audio uses it
/// to place samples after silent stretches (idle loopback, pause/resume) without drift.
struct PauseClock { flag: AtomicBool, st: Mutex<(Instant, Duration, Option<Instant>)> } // (t0, paused total, paused since)

impl Default for PauseClock {
    fn default() -> Self { PauseClock { flag: AtomicBool::new(false), st: Mutex::new((Instant::now(), Duration::ZERO, None)) } }
}

impl PauseClock {
    #[inline] fn is(&self) -> bool { self.flag.load(Relaxed) }
    fn set(&self, p: bool) {
        let mut g = self.st.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        match (p, g.2) {
            (true, None) => g.2 = Some(now),
            (false, Some(since)) => { g.1 += now - since; g.2 = None; }
            _ => {}
        }
        self.flag.store(p, Relaxed); // updated under the lock: active() never sees a torn state
    }
    fn toggle(&self) -> bool { let p = !self.is(); self.set(p); p }
    fn active(&self, now: Instant) -> f64 {
        let g = self.st.lock().unwrap_or_else(|e| e.into_inner());
        let paused = g.1 + g.2.map(|s| now.saturating_duration_since(s)).unwrap_or_default();
        now.saturating_duration_since(g.0).saturating_sub(paused).as_secs_f64()
    }
}

/// Places captured chunks on the recorder's active clock and fills real gaps with silence.
///
/// Loopback sources deliver *nothing* while nothing is playing (WASAPI always; Pulse when
/// the sink is suspended), and any source can stall. Without care, audio after a silent
/// stretch slides earlier than the video. A chunk that started > 200 ms after the previous
/// chunk ended gets that gap inserted as silence. The placement is anchored at stream open,
/// so silence before the first sound is kept too. It re-anchors on every chunk, so slow
/// device-clock drift never accumulates into false gaps (fit_len absorbs drift at the end).
/// `end` (active-clock time of the last sample) lets `finish` trim to the exact stop time.
struct Placer { next: f64, rate: f64, end: Arc<Mutex<f64>> }

impl Placer {
    fn new(clock: &PauseClock, rate: u32) -> (Self, Arc<Mutex<f64>>) {
        let next = clock.active(Instant::now());
        let end = Arc::new(Mutex::new(next));
        (Placer { next, rate: rate.max(1) as f64, end: end.clone() }, end)
    }
    /// `start`: active-clock time of the chunk's first frame; `n`: frames in the chunk.
    /// Returns the silent frames to insert before the chunk.
    fn place(&mut self, start: f64, n: usize) -> usize {
        let gap = start - self.next;
        let fill = if gap > 0.2 { (gap.min(3600.0) * self.rate) as usize } else { 0 };
        self.next = start + n as f64 / self.rate;
        *self.end.lock().unwrap_or_else(|e| e.into_inner()) = self.next;
        fill
    }
}

/// Native PulseAudio / PipeWire-pulse record stream (Linux), over the pure-Rust `pulseaudio`
/// protocol crate (no libpulse, stays fully static).
///
/// Why not cpal here: cpal's Pulse backend either lets the server choose multi-second
/// fragments (audio arrives in bursts, timestamps unreliable, the buffered tail is lost at
/// stop), or, with a fixed buffer, caps the server queue at one fragment so any late read
/// overflows and drops audio. Like `parec`, this asks for small 20 ms fragments with the
/// server's maximum queue, and has the server convert to s16 / 48 kHz / 1-2 ch for us.
#[cfg(target_os = "linux")]
mod pulse_rec {
    use super::Res;
    use pulseaudio::protocol as pa;
    use std::ffi::CString;
    use std::io::{BufReader, Read};
    use std::net::Shutdown;
    use std::os::unix::net::UnixStream;
    use std::thread::{self, JoinHandle};
    use std::time::Duration;

    pub enum Src<'a> { Mic(Option<&'a str>), Monitor }

    pub struct Reader { ctl: UnixStream, th: Option<JoinHandle<()>> }

    impl Reader {
        /// Closing the socket unblocks the reader thread, which then exits.
        pub fn stop(mut self) { self.shutdown(); }
        fn shutdown(&mut self) {
            let _ = self.ctl.shutdown(Shutdown::Both);
            if let Some(t) = self.th.take() { let _ = t.join(); }
        }
    }
    impl Drop for Reader { fn drop(&mut self) { self.shutdown(); } }

    pub fn available() -> bool { pulseaudio::socket_path_from_env().map(|p| p.exists()).unwrap_or(false) }

    fn pe(e: impl std::fmt::Display) -> String { format!("pulse: {e}") }

    fn connect() -> Res<(BufReader<UnixStream>, u16)> {
        let path = pulseaudio::socket_path_from_env().ok_or("no PulseAudio/PipeWire socket")?;
        let s = UnixStream::connect(&path).map_err(|e| format!("connect {}: {e}", path.display()))?;
        // Handshake must not hang forever on a wedged server; cleared once streaming.
        s.set_read_timeout(Some(Duration::from_secs(3))).map_err(pe)?;
        let mut sock = BufReader::new(s);
        let cookie = pulseaudio::cookie_path_from_env().and_then(|p| std::fs::read(p).ok()).unwrap_or_default();
        let auth = pa::AuthParams { version: pa::MAX_VERSION, supports_shm: false, supports_memfd: false, cookie };
        pa::write_command_message(sock.get_mut(), 0, &pa::Command::Auth(auth), pa::MAX_VERSION).map_err(pe)?;
        let (_, reply) = pa::read_reply_message::<pa::AuthReply>(&mut sock, pa::MAX_VERSION).map_err(pe)?;
        let ver = pa::MAX_VERSION.min(reply.version);
        let mut props = pa::Props::new();
        props.set(pa::Prop::ApplicationName, CString::new(super::APP_NAME).unwrap_or_default());
        pa::write_command_message(sock.get_mut(), 1, &pa::Command::SetClientName(props), ver).map_err(pe)?;
        let _ = pa::read_reply_message::<pa::SetClientNameReply>(&mut sock, ver).map_err(pe)?;
        Ok((sock, ver))
    }

    /// Open a record stream delivering interleaved s16 at 48 kHz with `ch` channels.
    /// `on_data(samples)` runs on the reader thread for every fragment, in order.
    pub fn open(src: Src, ch: u8, mut on_data: impl FnMut(&[i16]) + Send + 'static) -> Res<Reader> {
        let (mut sock, ver) = connect()?;
        let (source_index, source_name) = match src {
            Src::Monitor => {
                let q = pa::GetSinkInfo { index: None, name: Some(pa::DEFAULT_SINK.to_owned()) };
                pa::write_command_message(sock.get_mut(), 2, &pa::Command::GetSinkInfo(q), ver).map_err(pe)?;
                let (_, sink) = pa::read_reply_message::<pa::SinkInfo>(&mut sock, ver).map_err(|e| format!("default output: {e}"))?;
                (Some(sink.monitor_source_index.ok_or("default output has no monitor source")?), None)
            }
            Src::Mic(Some(n)) => {
                pa::write_command_message(sock.get_mut(), 2, &pa::Command::GetSourceInfoList, ver).map_err(pe)?;
                let (_, list) = pa::read_reply_message::<pa::SourceInfoList>(&mut sock, ver).map_err(pe)?;
                let hit = list.iter().find(|s| {
                    s.description.as_ref().map(|d| d.to_string_lossy() == n).unwrap_or(false) || s.name.to_string_lossy() == n
                });
                match hit { Some(s) => (Some(s.index), None), None => (None, Some(pa::DEFAULT_SOURCE.to_owned())) }
            }
            Src::Mic(None) => (None, Some(pa::DEFAULT_SOURCE.to_owned())),
        };
        let frag = 48_000 / 50 * ch as u32 * 2; // 20 ms of s16
        let params = pa::RecordStreamParams {
            sample_spec: pa::SampleSpec { format: pa::SampleFormat::S16Le, channels: ch, sample_rate: super::AR },
            channel_map: if ch == 2 { pa::ChannelMap::stereo() } else { pa::ChannelMap::mono() },
            source_index, source_name,
            buffer_attr: pa::stream::BufferAttr { fragment_size: frag, ..Default::default() }, // max_length = server max
            flags: pa::stream::StreamFlags { adjust_latency: true, ..Default::default() },
            props: { let mut p = pa::Props::new(); p.set(pa::Prop::MediaName, CString::new("Screen recording").unwrap_or_default()); p },
            ..Default::default()
        };
        pa::write_command_message(sock.get_mut(), 3, &pa::Command::CreateRecordStream(params), ver).map_err(pe)?;
        let (_, st) = pa::read_reply_message::<pa::CreateRecordStreamReply>(&mut sock, ver).map_err(|e| format!("open record stream: {e}"))?;
        if st.sample_spec.format != pa::SampleFormat::S16Le || st.sample_spec.channels != ch || st.sample_spec.sample_rate != super::AR {
            return Err(format!("server gave unexpected format {:?}", st.sample_spec));
        }
        sock.get_ref().set_read_timeout(None).map_err(pe)?;
        let ctl = sock.get_ref().try_clone().map_err(pe)?;
        let th = thread::Builder::new().name("pulse-rec".into()).spawn(move || {
            let fb = 2 * ch as usize;         // bytes per frame (s16)
            let mut pend: Vec<u8> = Vec::new(); // received bytes not yet forming whole frames
            let mut smp: Vec<i16> = Vec::new();
            // v1.3.1 FIX (clippy while_let_loop): same behaviour, idiomatic form.
            while let Ok(d) = pa::read_descriptor(&mut sock) {
                if d.channel == u32::MAX {
                    // control message (overflow/started/moved/killed...): consume; stop if killed
                    match pa::Command::read_tag_prefixed(&mut sock, ver) {
                        Ok((_, pa::Command::RecordStreamKilled(_))) | Err(_) => break,
                        Ok(_) => continue,
                    }
                }
                let len = d.length as usize;
                if len > 16 << 20 { break; } // corrupt framing guard
                let old = pend.len();
                pend.resize(old + len, 0);
                if sock.read_exact(&mut pend[old..]).is_err() { break; }
                if d.channel != st.channel { pend.truncate(old); continue; }
                let whole = pend.len() / fb * fb;
                if whole == 0 { continue; }
                smp.clear();
                smp.extend(pend[..whole].chunks_exact(2).map(|p| i16::from_le_bytes([p[0], p[1]])));
                pend.drain(..whole);
                on_data(&smp);
            }
        }).map_err(pe)?;
        Ok(Reader { ctl, th: Some(th) })
    }
}

/// Which device an `AudioCapture` records from.
#[derive(Clone, Copy)]
enum AudioSrc<'a> { Mic(Option<&'a str>), System }

enum CapInner { Cpal(cpal::Stream), #[cfg(target_os = "linux")] Pulse(pulse_rec::Reader), None }

/// Streaming linear resampler (device rate -> AR), interleaved, state carried across chunks.
/// v1.4.0 PERF: v1.3.x buffered the whole take at the device rate and resampled it at stop
/// (2x peak memory + a stall on Stop); resampling on the fly lets audio go straight to disk.
struct LinRes { step: f64, t: f64, ch: usize, prev: Vec<f32>, primed: bool }

impl LinRes {
    fn new(from: u32, to: u32, ch: usize) -> Self {
        LinRes { step: from.max(1) as f64 / to.max(1) as f64, t: 0.0, ch: ch.max(1), prev: vec![0.0; ch.max(1)], primed: false }
    }
    fn passthrough(&self) -> bool { (self.step - 1.0).abs() < 1e-12 }
    /// Appends the resampled frames of `x` to `out`. Index 0 of the virtual input is the last
    /// frame of the previous chunk, so interpolation is seamless across chunk boundaries.
    fn push(&mut self, x: &[i16], out: &mut Vec<i16>) {
        let ch = self.ch;
        let n = x.len() / ch;
        if n == 0 { return; }
        if self.passthrough() { out.extend_from_slice(&x[..n * ch]); return; }
        if !self.primed { for (p, &v) in self.prev.iter_mut().zip(&x[..ch]) { *p = v as f32; } self.primed = true; }
        let at = |i: usize, c: usize, prev: &[f32]| if i == 0 { prev[c] } else { x[(i - 1) * ch + c] as f32 };
        while self.t <= n as f64 {
            let i0 = self.t as usize;
            let f = (self.t - i0 as f64) as f32;
            for c in 0..ch {
                let a = at(i0, c, &self.prev);
                let b = if i0 < n { at(i0 + 1, c, &self.prev) } else { a };
                out.push((a + (b - a) * f).round() as i16);
            }
            self.t += self.step;
        }
        self.t -= n as f64;
        for c in 0..ch { self.prev[c] = x[(n - 1) * ch + c] as f32; }
    }
}

/// v1.4.0: what the capture sink does with the audio besides keeping it in memory.
#[derive(Clone, Default)]
struct SinkOpts {
    /// Crash-safety spill: raw s16le @ AR, flushed about once a second (`rec.txt` gives ch).
    spill: Option<PathBuf>,
    /// Live voice processing (mic only): the take is recorded already polished.
    fx: Option<VoiceFx>,
}

/// Audio capture (mic or system loopback). Device callbacks only copy samples into a
/// channel; a dedicated sink thread resamples to 48 kHz, applies live voice FX and spills to
/// disk, so heavy DSP never runs on the real-time audio thread (no xruns).
/// Mic is mono; system audio is stereo. Linux uses a native Pulse/PipeWire stream when a
/// sound server is running (falls back to cpal/ALSA for the mic); Windows uses WASAPI.
struct AudioCapture { inner: CapInner, sink: Option<JoinHandle<Vec<i16>>>, ch: u16, end: Arc<Mutex<f64>>, fx_err: Arc<Mutex<Option<String>>> }

impl AudioCapture {
    fn start(src: AudioSrc, clock: Arc<PauseClock>, opts: SinkOpts) -> Res<Self> {
        let (inner, rx, rate, ch, end) = Self::open(src, clock)?;
        let fx_err = Arc::new(Mutex::new(None));
        let fe = fx_err.clone();
        let sink = thread::Builder::new().name("audio-sink".into()).spawn(move || Self::sink(rx, rate, ch, opts, fe)).map_err(es)?;
        Ok(AudioCapture { inner, sink: Some(sink), ch, end, fx_err })
    }

    fn sink(rx: Receiver<Vec<i16>>, rate: u32, ch: u16, opts: SinkOpts, err: Arc<Mutex<Option<String>>>) -> Vec<i16> {
        let mut rs = LinRes::new(rate, AR, ch as usize);
        // live FX only on mono (mic) capture
        let mut vc = opts.fx.filter(|f| f.active() && ch == 1).map(VoiceChain::new);
        let mut spill = opts.spill.as_ref().and_then(|p| match File::create(p) {
            Ok(f) => Some(BufWriter::with_capacity(256 << 10, f)),
            Err(e) => { *err.lock().unwrap_or_else(|e| e.into_inner()) = Some(format!("spill {}: {e}", p.display())); None }
        });
        let mut all: Vec<i16> = Vec::with_capacity(AR as usize * ch as usize * 60);
        let (mut a, mut b) = (Vec::new(), Vec::new());
        let mut since_flush = 0usize;
        let emit = |v: &[i16], all: &mut Vec<i16>, spill: &mut Option<BufWriter<File>>, since: &mut usize| {
            if v.is_empty() { return; }
            all.extend_from_slice(v);
            if let Some(w) = spill.as_mut() {
                let mut bytes = Vec::with_capacity(v.len() * 2);
                for s in v { bytes.extend_from_slice(&s.to_le_bytes()); }
                // a full disk must not kill the take: stop spilling, keep recording in memory
                if w.write_all(&bytes).is_err() { *spill = None; return; }
                *since += v.len();
                if *since >= AR as usize * ch as usize { *since = 0; if w.flush().is_err() { *spill = None; } }
            }
        };
        for chunk in rx.iter() {
            a.clear();
            rs.push(&chunk, &mut a);
            let out: &[i16] = match vc.as_mut() { Some(v) => { b.clear(); v.push(&a, &mut b); &b } None => &a };
            emit(out, &mut all, &mut spill, &mut since_flush);
        }
        if let Some(v) = vc.as_mut() { b.clear(); v.finish(&mut b); emit(&b, &mut all, &mut spill, &mut since_flush); }
        if let Some(mut w) = spill { let _ = w.flush(); }
        all
    }

    #[allow(clippy::type_complexity)]
    fn open(src: AudioSrc, clock: Arc<PauseClock>) -> Res<(CapInner, Receiver<Vec<i16>>, u32, u16, Arc<Mutex<f64>>)> {
        #[cfg(target_os = "linux")]
        if pulse_rec::available() {
            match Self::start_pulse(src, clock.clone()) {
                Ok(c) => return Ok(c),
                // System audio exists only via the sound server; the mic can still try ALSA.
                Err(e) if matches!(src, AudioSrc::System) => return Err(e),
                Err(e) => eprintln!("[mic] {e}; falling back to ALSA"),
            }
        }
        Self::start_cpal(src, clock)
    }

    #[cfg(target_os = "linux")]
    #[allow(clippy::type_complexity)]
    fn start_pulse(src: AudioSrc, clock: Arc<PauseClock>) -> Res<(CapInner, Receiver<Vec<i16>>, u32, u16, Arc<Mutex<f64>>)> {
        let (ch, psrc, what) = match src {
            AudioSrc::Mic(n) => (1u8, pulse_rec::Src::Mic(n), "mic"),
            AudioSrc::System => (2u8, pulse_rec::Src::Monitor, "system audio"),
        };
        let (tx, rx) = mpsc::channel();
        let (mut placer, end) = Placer::new(&clock, AR);
        let rate = AR as f64;
        let reader = pulse_rec::open(psrc, ch, move |s: &[i16]| {
            if clock.is() { return; }
            let n = s.len() / ch as usize;
            // fragments arrive ~as captured (20 ms, dedicated reader): start = now - duration
            let start = clock.active(Instant::now()) - n as f64 / rate;
            let fill = placer.place(start, n);
            let mut v = Vec::with_capacity((fill + n) * ch as usize);
            v.resize(fill * ch as usize, 0);
            v.extend_from_slice(s);
            let _ = tx.send(v);
        }).map_err(|e| format!("{what}: {e}"))?;
        Ok((CapInner::Pulse(reader), rx, AR, ch as u16, end))
    }

    #[allow(clippy::type_complexity)]
    fn start_cpal(src: AudioSrc, clock: Arc<PauseClock>) -> Res<(CapInner, Receiver<Vec<i16>>, u32, u16, Arc<Mutex<f64>>)> {
        let (what, dev, sc) = match src {
            AudioSrc::Mic(name) => {
                let dev = find_input(name)?;
                let sc = dev.default_input_config().map_err(|e| format!("mic config: {e}"))?;
                ("mic", dev, sc)
            }
            AudioSrc::System => { let (d, c) = find_loopback()?; ("system audio", d, c) }
        };
        let mut cfg = sc.config();
        cfg.buffer_size = cpal::BufferSize::Default;
        let out_ch: u16 = if matches!(src, AudioSrc::System) && cfg.channels >= 2 { 2 } else { 1 };
        let (tx, rx) = mpsc::channel();
        let (placer, end) = Placer::new(&clock, cfg.sample_rate);
        let a = (out_ch, what);
        let stream = match sc.sample_format() {
            cpal::SampleFormat::F32 => Self::build::<f32>(&dev, &cfg, a, tx, clock, placer),
            cpal::SampleFormat::I16 => Self::build::<i16>(&dev, &cfg, a, tx, clock, placer),
            cpal::SampleFormat::I32 => Self::build::<i32>(&dev, &cfg, a, tx, clock, placer),
            cpal::SampleFormat::U16 => Self::build::<u16>(&dev, &cfg, a, tx, clock, placer),
            cpal::SampleFormat::U8 => Self::build::<u8>(&dev, &cfg, a, tx, clock, placer),
            cpal::SampleFormat::I8 => Self::build::<i8>(&dev, &cfg, a, tx, clock, placer),
            cpal::SampleFormat::F64 => Self::build::<f64>(&dev, &cfg, a, tx, clock, placer),
            f => Err(format!("unsupported {what} sample format {f:?}")),
        }?;
        stream.play().map_err(|e| format!("{what} start: {e}"))?;
        Ok((CapInner::Cpal(stream), rx, cfg.sample_rate, out_ch, end))
    }

    fn build<T>(dev: &cpal::Device, cfg: &cpal::StreamConfig, (out_ch, what): (u16, &'static str),
                tx: Sender<Vec<i16>>, clock: Arc<PauseClock>, mut placer: Placer) -> Res<cpal::Stream>
    where T: SizedSample + Send + 'static, f32: FromSample<T> {
        let ch = (cfg.channels as usize).max(1);
        let oc = out_ch as usize;
        dev.build_input_stream::<T, _, _>(*cfg, move |data: &[T], info: &cpal::InputCallbackInfo| {
            if clock.is() { return; }
            let n = data.len() / ch;
            // Capture time of the first frame: stream-clock delay mapped onto Instant
            // (WASAPI reports hardware QPC positions, so this is accurate even for bursts).
            let ts = info.timestamp();
            let now = Instant::now();
            let delay = ts.callback.saturating_duration_since(ts.capture);
            let start = clock.active(now.checked_sub(delay).unwrap_or(now));
            let fill = placer.place(start, n);
            let mut v = Vec::with_capacity((fill + n) * oc);
            v.resize(fill * oc, 0);
            for fr in data.chunks_exact(ch) {
                if oc == 2 {
                    v.push(f2i(f32::from_sample(fr[0])));
                    v.push(f2i(f32::from_sample(fr[1])));
                } else {
                    let s: f32 = fr.iter().map(|&x| f32::from_sample(x)).sum::<f32>() / ch as f32;
                    v.push(f2i(s));
                }
            }
            let _ = tx.send(v);
        }, move |e| stream_err(what, &e), None).map_err(|e| format!("{what} open: {e}"))
    }

    /// Stop and return (channels, interleaved samples at 48 kHz). With `stop_at` (active-clock
    /// time the user pressed stop) the source gets a moment to deliver its last fragment and
    /// anything captured after that instant is trimmed off.
    fn finish(mut self, stop_at: Option<f64>) -> (u16, Vec<i16>) {
        if stop_at.is_some() { thread::sleep(Duration::from_millis(80)); }
        // Dropping the stream/reader drops the producer's Sender, which ends the sink loop.
        match std::mem::replace(&mut self.inner, CapInner::None) {
            CapInner::Cpal(s) => drop(s),
            #[cfg(target_os = "linux")]
            CapInner::Pulse(r) => r.stop(),
            CapInner::None => {}
        }
        let ch = self.ch;
        let mut out = self.sink.take().map(|h| h.join().unwrap_or_default()).unwrap_or_default();
        if let Some(s) = stop_at {
            let over = ((*self.end.lock().unwrap_or_else(|e| e.into_inner()) - s).max(0.0) * AR as f64) as usize;
            let keep = (out.len() / ch as usize).saturating_sub(over);
            out.truncate(keep * ch as usize);
        }
        (ch, out)
    }
    fn fx_error(&self) -> Option<String> { self.fx_err.lock().unwrap_or_else(|e| e.into_inner()).clone() }
}

// =====================================================================================
// v1.4.0 NEW: webcam capture (picture-in-picture source)
// The camera thread keeps only the latest JPEG; the recorder's writer stores one cam index
// entry per screen frame (`<base>.cam.idx`, 12 bytes, len 0 = no frame yet) and appends a
// camera JPEG to `<base>.cam.mjpg` only when the camera produced a new one (dedup by id).
// =====================================================================================

#[derive(Default)]
struct CamShared { latest: Mutex<Option<(u64, Arc<Vec<u8>>)>>, stop: AtomicBool, err: Mutex<Option<String>> }

struct Camera { sh: Arc<CamShared>, th: Option<JoinHandle<()>>, name: String }

impl Camera {
    /// Opens the device on the camera thread and waits (bounded) for it to report ready, so
    /// a missing/busy camera is reported before recording starts rather than silently empty.
    fn start(dev: Option<String>) -> Res<Camera> {
        let sh = Arc::new(CamShared::default());
        let (tx, rx) = mpsc::channel::<Res<String>>();
        let s2 = sh.clone();
        let th = thread::Builder::new().name("camera".into()).spawn(move || {
            let r = if std::env::var_os("SCREENREEL_FAKE_CAM").is_some() { cam_fake(&s2, &tx) } else { cam::run(dev.as_deref(), &s2, &tx) };
            if let Err(e) = r {
                let _ = tx.send(Err(e.clone()));
                *s2.err.lock().unwrap_or_else(|e| e.into_inner()) = Some(e);
            }
        }).map_err(es)?;
        match rx.recv_timeout(Duration::from_secs(8)) {
            Ok(Ok(name)) => Ok(Camera { sh, th: Some(th), name }),
            Ok(Err(e)) => { let _ = th.join(); Err(e) }
            Err(_) => { sh.stop.store(true, Relaxed); Err("webcam did not start within 8 s".into()) }
        }
    }
    fn error(&self) -> Option<String> { self.sh.err.lock().unwrap_or_else(|e| e.into_inner()).clone() }
}
impl Drop for Camera {
    fn drop(&mut self) { self.sh.stop.store(true, Relaxed); if let Some(t) = self.th.take() { let _ = t.join(); } }
}

/// Encode a camera frame for storage. Large sensors are halved (PiP never needs > ~960 px).
fn cam_jpeg(px: &[u8], w: usize, h: usize, ct: jpeg_encoder::ColorType, scratch: &mut Vec<u8>) -> Option<Vec<u8>> {
    let bpp = if matches!(ct, jpeg_encoder::ColorType::Rgb) { 3 } else { 4 };
    if w == 0 || h == 0 || px.len() < w * h * bpp || w > 8192 || h > 8192 { return None; }
    let (data, w, h): (&[u8], usize, usize) = if w > 960 && bpp == 4 {
        let (ow, oh) = half_bgra(px, w, h, scratch);
        (&scratch[..ow * oh * 4], ow, oh)
    } else { (&px[..w * h * bpp], w, h) };
    let mut out = Vec::with_capacity(w * h / 6);
    jpeg_encoder::Encoder::new(&mut out, 80).encode(data, w as u16, h as u16, ct).ok()?;
    Some(out)
}

fn cam_publish(sh: &CamShared, id: &mut u64, jpeg: Vec<u8>) {
    *id += 1;
    *sh.latest.lock().unwrap_or_else(|e| e.into_inner()) = Some((*id, Arc::new(jpeg)));
}

#[cfg(all(target_os = "linux", target_pointer_width = "64"))]
mod cam {
    //! V4L2 via raw ioctls (no libv4l / C deps; keeps the binary fully static). Struct
    //! layouts are the 64-bit kernel ABI (sizes are encoded in the ioctl numbers below).
    use super::*;
    use std::os::fd::AsRawFd;

    const QUERYCAP: u64 = 0x8068_5600; // _IOR('V', 0, v4l2_capability[104])
    const S_FMT: u64 = 0xc0d0_5605;    // _IOWR('V', 5, v4l2_format[208])
    const REQBUFS: u64 = 0xc014_5608;  // _IOWR('V', 8, v4l2_requestbuffers[20])
    const QUERYBUF: u64 = 0xc058_5609; // _IOWR('V', 9, v4l2_buffer[88])
    const QBUF: u64 = 0xc058_560f;
    const DQBUF: u64 = 0xc058_5611;
    const STREAMON: u64 = 0x4004_5612;
    const STREAMOFF: u64 = 0x4004_5613;
    const CAP_VIDEO_CAPTURE: u32 = 1;
    const CAP_STREAMING: u32 = 0x0400_0000;
    const CAP_DEVICE_CAPS: u32 = 0x8000_0000;
    const fn fourcc(s: &[u8; 4]) -> u32 { s[0] as u32 | (s[1] as u32) << 8 | (s[2] as u32) << 16 | (s[3] as u32) << 24 }
    const MJPG: u32 = fourcc(b"MJPG");
    const YUYV: u32 = fourcc(b"YUYV");

    fn ioctl(fd: i32, req: u64, arg: &mut [u8]) -> std::io::Result<()> {
        loop {
            // SAFETY: `arg` is a buffer of exactly the size encoded in `req`.
            let r = unsafe { libc::ioctl(fd, req as _, arg.as_mut_ptr()) };
            if r >= 0 { return Ok(()); }
            let e = std::io::Error::last_os_error();
            if e.kind() != std::io::ErrorKind::Interrupted { return Err(e); }
        }
    }
    fn u32_at(b: &[u8], o: usize) -> u32 { u32::from_le_bytes(b[o..o + 4].try_into().unwrap()) }
    fn put32(b: &mut [u8], o: usize, v: u32) { b[o..o + 4].copy_from_slice(&v.to_le_bytes()); }

    /// (device path, display name) of capture-capable /dev/video* nodes.
    pub fn list() -> Vec<(String, String)> {
        let mut v = vec![];
        for i in 0..16 {
            let path = format!("/dev/video{i}");
            let Ok(f) = std::fs::OpenOptions::new().read(true).write(true).open(&path) else { continue };
            let mut cap = [0u8; 104];
            if ioctl(f.as_raw_fd(), QUERYCAP, &mut cap).is_err() { continue; }
            let caps = if u32_at(&cap, 88) & CAP_DEVICE_CAPS != 0 { u32_at(&cap, 92) } else { u32_at(&cap, 88) };
            if caps & CAP_VIDEO_CAPTURE == 0 || caps & CAP_STREAMING == 0 { continue; } // metadata nodes etc.
            let card = String::from_utf8_lossy(&cap[16..48]).trim_end_matches('\0').trim().to_string();
            v.push((path.clone(), format!("{} ({path})", if card.is_empty() { "Camera" } else { &card })));
        }
        v
    }

    struct Map { ptr: *mut libc::c_void, len: usize }
    impl Drop for Map { fn drop(&mut self) { unsafe { libc::munmap(self.ptr, self.len); } } }

    pub fn run(dev: Option<&str>, sh: &CamShared, ready: &Sender<Res<String>>) -> Res<()> {
        let devs = list();
        let (path, name) = match dev {
            Some(d) => devs.iter().find(|(p, n)| p == d || n == d).cloned().ok_or_else(|| format!("webcam {d} not found"))?,
            None => devs.first().cloned().ok_or("no webcam found (/dev/video*)")?,
        };
        let f = std::fs::OpenOptions::new().read(true).write(true).custom_flags_nonblock().open(&path)
            .map_err(|e| format!("{path}: {e} (is it in use, or are you not in the 'video' group?)"))?;
        let fd = f.as_raw_fd();
        // Format: MJPG 640x480 (cheap: frames stored as-is), else YUYV converted here.
        let mut fmt = [0u8; 208];
        let mut pixfmt = 0;
        for want in [MJPG, YUYV] {
            fmt.fill(0);
            put32(&mut fmt, 0, 1); // V4L2_BUF_TYPE_VIDEO_CAPTURE
            put32(&mut fmt, 8, 640);
            put32(&mut fmt, 12, 480);
            put32(&mut fmt, 16, want);
            put32(&mut fmt, 20, 0); // V4L2_FIELD_ANY
            if ioctl(fd, S_FMT, &mut fmt).is_ok() && u32_at(&fmt, 16) == want { pixfmt = want; break; }
        }
        if pixfmt == 0 { return Err(format!("{name}: no MJPG or YUYV mode")); }
        let (w, h, bpl) = (u32_at(&fmt, 8) as usize, u32_at(&fmt, 12) as usize, u32_at(&fmt, 24) as usize);
        let mut rb = [0u8; 20];
        put32(&mut rb, 0, 4); put32(&mut rb, 4, 1); put32(&mut rb, 8, 1); // 4 buffers, capture, MMAP
        ioctl(fd, REQBUFS, &mut rb).map_err(|e| format!("{name}: REQBUFS {e}"))?;
        let nb = u32_at(&rb, 0).min(8) as usize;
        if nb == 0 { return Err(format!("{name}: driver gave no buffers")); }
        let mut maps = Vec::with_capacity(nb);
        for i in 0..nb {
            let mut b = [0u8; 88];
            put32(&mut b, 0, i as u32); put32(&mut b, 4, 1); put32(&mut b, 60, 1);
            ioctl(fd, QUERYBUF, &mut b).map_err(|e| format!("{name}: QUERYBUF {e}"))?;
            let (off, len) = (u32_at(&b, 64) as i64, u32_at(&b, 72) as usize);
            // SAFETY: mapping a driver buffer as described by QUERYBUF; unmapped by Map::drop.
            let p = unsafe { libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ, libc::MAP_SHARED, fd, off) };
            if p == libc::MAP_FAILED { return Err(format!("{name}: mmap failed")); }
            maps.push(Map { ptr: p, len });
            ioctl(fd, QBUF, &mut b).map_err(|e| format!("{name}: QBUF {e}"))?;
        }
        let mut ty = 1u32.to_le_bytes();
        ioctl(fd, STREAMON, &mut ty).map_err(|e| format!("{name}: STREAMON {e}"))?;
        let _ = ready.send(Ok(name.clone()));
        let (mut id, mut scratch, mut rgb) = (0u64, Vec::new(), Vec::new());
        let mut res = Ok(());
        while !sh.stop.load(Relaxed) {
            let mut pfd = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
            // SAFETY: one valid pollfd; 200 ms timeout keeps `stop` responsive.
            let r = unsafe { libc::poll(&mut pfd, 1, 200) };
            if r <= 0 { continue; }
            let mut b = [0u8; 88];
            put32(&mut b, 4, 1); put32(&mut b, 60, 1);
            if let Err(e) = ioctl(fd, DQBUF, &mut b) {
                if e.kind() == std::io::ErrorKind::WouldBlock { continue; }
                res = Err(format!("{name}: {e} (unplugged?)")); break;
            }
            let (i, used) = (u32_at(&b, 0) as usize, u32_at(&b, 8) as usize);
            if let Some(m) = maps.get(i) {
                // SAFETY: the driver filled `used` <= len bytes of this mapped buffer.
                let data = unsafe { std::slice::from_raw_parts(m.ptr as *const u8, used.min(m.len)) };
                if pixfmt == MJPG {
                    if data.len() > 4 && data[0] == 0xFF && data[1] == 0xD8 { cam_publish(sh, &mut id, data.to_vec()); }
                } else if data.len() >= bpl * h && bpl >= w * 2 {
                    yuyv_to_rgb(data, w, h, bpl, &mut rgb);
                    if let Some(j) = cam_jpeg(&rgb, w, h, jpeg_encoder::ColorType::Rgb, &mut scratch) { cam_publish(sh, &mut id, j); }
                }
            }
            if ioctl(fd, QBUF, &mut b).is_err() { res = Err(format!("{name}: QBUF failed")); break; }
        }
        let _ = ioctl(fd, STREAMOFF, &mut ty);
        drop(maps); // munmap before close
        res
    }

    fn yuyv_to_rgb(src: &[u8], w: usize, h: usize, bpl: usize, out: &mut Vec<u8>) {
        out.resize(w * h * 3, 0);
        let c = |v: f32| v.clamp(0.0, 255.0) as u8;
        for y in 0..h {
            let row = &src[y * bpl..y * bpl + w * 2];
            for (x2, q) in row.chunks_exact(4).enumerate() {
                let (u, v) = (q[1] as f32 - 128.0, q[3] as f32 - 128.0);
                for (k, yy) in [q[0], q[2]].into_iter().enumerate() {
                    let yy = (yy as f32 - 16.0) * 1.164;
                    let o = (y * w + x2 * 2 + k) * 3;
                    out[o] = c(yy + 1.596 * v);
                    out[o + 1] = c(yy - 0.392 * u - 0.813 * v);
                    out[o + 2] = c(yy + 2.017 * u);
                }
            }
        }
    }

    trait NonBlock { fn custom_flags_nonblock(&mut self) -> &mut Self; }
    impl NonBlock for std::fs::OpenOptions {
        fn custom_flags_nonblock(&mut self) -> &mut Self { use std::os::unix::fs::OpenOptionsExt; self.custom_flags(libc::O_NONBLOCK) }
    }
}

#[cfg(windows)]
mod cam {
    //! Media Foundation source reader with the built-in video processor: any camera format
    //! (NV12/YUY2/MJPG) is delivered as RGB32, which we JPEG-encode. Verified against the
    //! windows 0.61 bindings; COM is initialised (MTA) on the camera thread only.
    use super::*;
    use windows::core::PWSTR;
    use windows::Win32::Media::MediaFoundation::*;
    use windows::Win32::System::Com::{CoInitializeEx, CoTaskMemFree, COINIT_MULTITHREADED};

    struct MfGuard;
    impl Drop for MfGuard { fn drop(&mut self) { unsafe { let _ = MFShutdown(); } } }

    unsafe fn devices() -> windows::core::Result<Vec<(String, IMFActivate)>> {
        let mut attrs: Option<IMFAttributes> = None;
        MFCreateAttributes(&mut attrs, 1)?;
        let attrs = attrs.ok_or_else(|| windows::core::Error::from(windows::Win32::Foundation::E_FAIL))?;
        attrs.SetGUID(&MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE, &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID)?;
        let mut pp: *mut Option<IMFActivate> = std::ptr::null_mut();
        let mut n = 0u32;
        MFEnumDeviceSources(&attrs, &mut pp, &mut n)?;
        let mut v = Vec::new();
        if !pp.is_null() {
            for a in std::slice::from_raw_parts_mut(pp, n as usize) {
                if let Some(act) = a.take() {
                    let mut s = PWSTR::null();
                    let mut len = 0u32;
                    let name = if act.GetAllocatedString(&MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME, &mut s, &mut len).is_ok() {
                        let r = s.to_string().unwrap_or_else(|_| "Camera".into());
                        CoTaskMemFree(Some(s.0 as *const core::ffi::c_void));
                        r
                    } else { "Camera".into() };
                    v.push((name, act));
                }
            }
            CoTaskMemFree(Some(pp as *const core::ffi::c_void));
        }
        Ok(v)
    }

    pub fn list() -> Vec<(String, String)> {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            if MFStartup(MF_VERSION, MFSTARTUP_FULL).is_err() { return vec![]; }
            let _g = MfGuard;
            devices().map(|v| v.into_iter().map(|(n, _)| (n.clone(), n)).collect()).unwrap_or_default()
        }
    }

    pub fn run(dev: Option<&str>, sh: &CamShared, ready: &Sender<Res<String>>) -> Res<()> {
        let me = |e: windows::core::Error| format!("webcam: {}", e.message());
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            MFStartup(MF_VERSION, MFSTARTUP_FULL).map_err(me)?;
            let _g = MfGuard;
            let mut devs = devices().map_err(me)?;
            let pick = match dev { Some(d) => devs.iter().position(|(n, _)| n == d), None => (!devs.is_empty()).then_some(0) };
            let Some(pick) = pick else { return Err("no webcam found".into()) };
            let (name, act) = devs.swap_remove(pick);
            let source: IMFMediaSource = act.ActivateObject().map_err(me)?;
            let mut ra: Option<IMFAttributes> = None;
            MFCreateAttributes(&mut ra, 1).map_err(me)?;
            let ra = ra.ok_or("webcam: attributes")?;
            ra.SetUINT32(&MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING, 1).map_err(me)?;
            let reader = MFCreateSourceReaderFromMediaSource(&source, &ra).map_err(me)?;
            let stream = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
            let mt = MFCreateMediaType().map_err(me)?;
            mt.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video).map_err(me)?;
            mt.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_RGB32).map_err(me)?;
            reader.SetCurrentMediaType(stream, None, &mt).map_err(me)?;
            let cur = reader.GetCurrentMediaType(stream).map_err(me)?;
            let fs = cur.GetUINT64(&MF_MT_FRAME_SIZE).map_err(me)?;
            let (w, h) = ((fs >> 32) as usize, (fs & 0xffff_ffff) as usize);
            // stride < 0 = bottom-up rows; absent = top-down (MF default for RGB32 here)
            let stride = cur.GetUINT32(&MF_MT_DEFAULT_STRIDE).map(|s| s as i32).unwrap_or((w * 4) as i32);
            if w == 0 || h == 0 || w > 8192 || h > 8192 { return Err(format!("{name}: bad frame size {w}x{h}")); }
            let _ = ready.send(Ok(name.clone()));
            let (mut id, mut scratch, mut bgra) = (0u64, Vec::new(), vec![0u8; w * h * 4]);
            let mut res = Ok(());
            while !sh.stop.load(Relaxed) {
                let (mut flags, mut ts) = (0u32, 0i64);
                let mut sample: Option<IMFSample> = None;
                if let Err(e) = reader.ReadSample(stream, 0, None, Some(&mut flags), Some(&mut ts), Some(&mut sample)) { res = Err(me(e)); break; }
                if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 { res = Err(format!("{name}: stream ended (unplugged?)")); break; }
                let Some(sample) = sample else { continue };
                let Ok(buf) = sample.ConvertToContiguousBuffer() else { continue };
                let (mut p, mut len) = (std::ptr::null_mut::<u8>(), 0u32);
                if buf.Lock(&mut p, None, Some(&mut len)).is_err() || p.is_null() { continue; }
                let rowb = w * 4;
                let ok = (len as usize) >= stride.unsigned_abs() as usize * (h - 1) + rowb;
                if ok {
                    let src = std::slice::from_raw_parts(p, len as usize);
                    for y in 0..h {
                        let sy = if stride < 0 { h - 1 - y } else { y };
                        let o = sy * stride.unsigned_abs() as usize;
                        bgra[y * rowb..(y + 1) * rowb].copy_from_slice(&src[o..o + rowb]);
                    }
                }
                let _ = buf.Unlock();
                if ok {
                    for px in bgra.chunks_exact_mut(4) { px[3] = 255; } // X8 -> opaque
                    if let Some(j) = cam_jpeg(&bgra, w, h, jpeg_encoder::ColorType::Bgra, &mut scratch) { cam_publish(sh, &mut id, j); }
                }
            }
            let _ = source.Shutdown();
            let _ = act.ShutdownObject();
            res
        }
    }
}

#[cfg(not(any(all(target_os = "linux", target_pointer_width = "64"), windows)))]
mod cam {
    use super::*;
    pub fn list() -> Vec<(String, String)> { vec![] }
    pub fn run(_d: Option<&str>, _s: &CamShared, _r: &Sender<Res<String>>) -> Res<()> { Err("webcam not supported on this platform".into()) }
}

/// Synthetic camera for --selftest / CI (SCREENREEL_FAKE_CAM=1): a moving "head" on a
/// gradient at 15 fps, so the cam pipeline + PiP compositing can be verified headless.
fn cam_fake(sh: &CamShared, ready: &Sender<Res<String>>) -> Res<()> {
    let (w, h) = (320usize, 240usize);
    let _ = ready.send(Ok("Fake camera".into()));
    let (mut id, mut scratch, mut rgb, mut t) = (0u64, Vec::new(), vec![0u8; w * h * 3], 0f32);
    while !sh.stop.load(Relaxed) {
        let cx = w as f32 * (0.5 + 0.2 * t.sin());
        for y in 0..h {
            for x in 0..w {
                let o = (y * w + x) * 3;
                let (dx, dy) = ((x as f32 - cx) / 60.0, (y as f32 - 110.0) / 80.0);
                let head = dx * dx + dy * dy < 1.0;
                rgb[o] = if head { 230 } else { (x * 255 / w) as u8 };
                rgb[o + 1] = if head { 180 } else { (y * 255 / h) as u8 };
                rgb[o + 2] = if head { 150 } else { 90 };
            }
        }
        if let Some(j) = cam_jpeg(&rgb, w, h, jpeg_encoder::ColorType::Rgb, &mut scratch) { cam_publish(sh, &mut id, j); }
        t += 0.1;
        thread::sleep(Duration::from_millis(66));
    }
    Ok(())
}

// =====================================================================================
// Recorder: capture thread -> encoder pool -> ordered writer
// =====================================================================================

struct RecConfig {
    region: Rect, fps: u32, quality: u8, s444: bool, half: bool, cursor: bool,
    mic: Option<Option<String>>, // None = off; Some(None) = default device
    sys: bool,                   // also record system audio (loopback) to its own track
    cam: Option<Option<String>>, // v1.4.0: webcam (None = off; Some(None) = first camera)
    live_fx: Option<VoiceFx>,    // v1.4.0: voice chain applied to the mic while recording
    dir: PathBuf,
}

#[derive(Default)]
struct RecShared { stop: AtomicBool, paused: Arc<PauseClock>, written: AtomicU64, dups: AtomicU64, error: Mutex<Option<String>> }

/// v1.4.0: `Meta` carries the per-frame input sample and webcam frame for *every* seq
/// (real or duplicate) so the .ev / .cam.idx streams stay in exact lockstep with frames.idx.
type CamRef = (u64, Arc<Vec<u8>>);
enum WMsg { Frame(u64, Vec<u8>), Dup(u64), Meta(u64, Ev, Option<CamRef>), Done(u64) }

/// Name of the crash-recovery marker written when a recording starts and removed once the
/// recording has been saved as a project.
const REC_MARK: &str = "rec.txt";

/// Ordered output of the writer thread: frames.idx + .ev (+ .cam.*) written in lockstep.
struct MediaOut {
    data: BufWriter<File>, idx: BufWriter<File>, ev: BufWriter<File>,
    cam: Option<(BufWriter<File>, BufWriter<File>)>, off: u64, cam_off: u64, cam_last: Option<(u64, [u8; 12])>, n: u64,
}

impl MediaOut {
    fn create(dir: &Path, cam: bool) -> Res<MediaOut> {
        let mk = |n: &str, cap: usize| File::create(dir.join(n)).map(|f| BufWriter::with_capacity(cap, f)).map_err(|e| format!("{n}: {e}"));
        Ok(MediaOut {
            data: mk("frames.mjpg", 4 << 20)?, idx: mk("frames.idx", 64 << 10)?, ev: mk("frames.ev", 32 << 10)?,
            cam: if cam { Some((mk("frames.cam.mjpg", 1 << 20)?, mk("frames.cam.idx", 32 << 10)?)) } else { None },
            off: 0, cam_off: 0, cam_last: None, n: 0,
        })
    }
    fn entry(off: u64, len: u32) -> [u8; 12] {
        let mut e = [0u8; 12];
        e[..8].copy_from_slice(&off.to_le_bytes());
        e[8..].copy_from_slice(&len.to_le_bytes());
        e
    }
    fn append_jpeg(&mut self, v: &[u8]) -> Res<[u8; 12]> {
        self.data.write_all(v).map_err(es)?;
        let e = Self::entry(self.off, v.len() as u32);
        self.off += v.len() as u64;
        Ok(e)
    }
    /// One output frame: video index entry + its input sample + its webcam entry.
    fn put(&mut self, e: &[u8; 12], ev: Ev, cam: &Option<CamRef>, fps: u64) -> Res<()> {
        self.idx.write_all(e).map_err(es)?;
        self.ev.write_all(&ev.to_bytes()).map_err(es)?;
        if let Some((cd, ci)) = &mut self.cam {
            let ce = match cam {
                Some((id, j)) => match self.cam_last {
                    Some((lid, le)) if lid == *id => le,
                    _ => {
                        cd.write_all(j).map_err(es)?;
                        let e = Self::entry(self.cam_off, j.len() as u32);
                        self.cam_off += j.len() as u64;
                        self.cam_last = Some((*id, e));
                        e
                    }
                },
                None => [0u8; 12], // len 0: no camera frame (yet)
            };
            ci.write_all(&ce).map_err(es)?;
        }
        self.n += 1;
        // v1.4.0 crash safety: push everything to the OS about once a second, data before
        // indexes, so a crash/kill loses at most ~1 s and indexes never point past the data.
        if self.n % fps.max(1) == 0 { self.flush()?; }
        Ok(())
    }
    fn flush(&mut self) -> Res<()> {
        self.data.flush().map_err(es)?;
        if let Some((cd, _)) = &mut self.cam { cd.flush().map_err(es)?; }
        self.idx.flush().map_err(es)?;
        self.ev.flush().map_err(es)?;
        if let Some((_, ci)) = &mut self.cam { ci.flush().map_err(es)?; }
        Ok(())
    }
}

struct Recorder {
    pub sh: Arc<RecShared>, cap: Option<JoinHandle<()>>, workers: Vec<JoinHandle<()>>,
    writer: Option<JoinHandle<Res<u64>>>, mic: Option<AudioCapture>, sys: Option<AudioCapture>,
    pub fps: u32, pub dims: (u32, u32), pub dir: PathBuf, pub mic_error: Option<String>, pub sys_error: Option<String>, pub shm: bool,
    pub cam_error: Option<String>, camera: Option<Camera>, input: Option<InputPoller>, pub has_cam: bool,
}

/// Recorded media handed back by `Recorder::finish`.
struct RecOut { frames: u32, mic: Option<PcmTrack>, sys: Option<PcmTrack>, warn: Option<String> }

impl Recorder {
    fn start(cfg: RecConfig) -> Res<Recorder> {
        std::fs::create_dir_all(&cfg.dir).map_err(|e| format!("create {}: {e}", cfg.dir.display()))?;
        let sh = Arc::new(RecShared::default());
        let nworkers = thread::available_parallelism().map(|n| n.get()).unwrap_or(2).saturating_sub(1).clamp(1, 8);
        let (enc_tx, enc_rx) = mpsc::sync_channel::<(u64, Vec<u8>)>(nworkers * 2);
        let (w_tx, w_rx) = mpsc::sync_channel::<WMsg>(256);
        let (pool_tx, pool_rx) = mpsc::channel::<Vec<u8>>();
        let (ready_tx, ready_rx) = mpsc::channel::<Res<(Rect, bool)>>();
        let (go_tx, go_rx) = mpsc::channel::<bool>();

        // v1.4.0: webcam first (bounded wait) so a failure is known before frames start.
        let (camera, cam_error) = match &cfg.cam {
            None => (None, None),
            Some(d) => match Camera::start(d.clone()) { Ok(c) => (Some(c), None), Err(e) => (None, Some(e)) },
        };
        let cam_sh = camera.as_ref().map(|c| c.sh.clone());
        let has_cam = cam_sh.is_some();
        let input_sh = Arc::new(InputShared::default());
        let input = Some(InputPoller::start(input_sh.clone()));

        // ---- capture thread (owns the Grabber; platform handles never cross threads)
        let (fps, region, cursor, half) = (cfg.fps, cfg.region, cfg.cursor, cfg.half);
        let sh_c = sh.clone();
        let w_tx_c = w_tx.clone();
        let cap = thread::Builder::new().name("capture".into()).spawn(move || {
            let mut g = match cap::Grabber::new(region, cursor) {
                Ok(g) => { let _ = ready_tx.send(Ok((g.r, g.uses_shm()))); g }
                Err(e) => { let _ = ready_tx.send(Err(e)); return; }
            };
            if !go_rx.recv().unwrap_or(false) { return; }
            let gr = g.r;
            // per-frame metadata goes out *before* the frame/dup message for the same seq
            let meta = |seq: u64| WMsg::Meta(seq, input_sh.take(gr, half),
                cam_sh.as_ref().and_then(|c| c.latest.lock().unwrap_or_else(|e| e.into_inner()).clone()));
            let fdur = 1.0 / fps as f64;
            let mut seq: u64 = 0;
            let mut active = Duration::ZERO;
            let mut last = Instant::now();
            let mut fails = 0u32;
            while !sh_c.stop.load(Relaxed) {
                let now = Instant::now();
                if sh_c.paused.is() { last = now; thread::sleep(Duration::from_millis(10)); continue; }
                active += now - last;
                last = now;
                let due = (active.as_secs_f64() * fps as f64).floor() as u64 + 1;
                if seq >= due {
                    let wait = (seq as f64 * fdur - active.as_secs_f64()).clamp(0.001, 0.05);
                    thread::sleep(Duration::from_secs_f64(wait));
                    continue;
                }
                // Behind schedule: emit zero-cost duplicates so A/V stays in sync.
                while seq + 1 < due {
                    if w_tx_c.send(meta(seq)).is_err() || w_tx_c.send(WMsg::Dup(seq)).is_err() { return; }
                    sh_c.dups.fetch_add(1, Relaxed);
                    seq += 1;
                }
                if w_tx_c.send(meta(seq)).is_err() { return; }
                let mut buf = pool_rx.try_recv().unwrap_or_default();
                match g.grab(&mut buf) {
                    Ok(()) => {
                        fails = 0;
                        match enc_tx.try_send((seq, buf)) {
                            Ok(()) => {}
                            Err(TrySendError::Full(_)) => { // encoders saturated: drop, keep timing
                                sh_c.dups.fetch_add(1, Relaxed);
                                if w_tx_c.send(WMsg::Dup(seq)).is_err() { return; }
                            }
                            Err(TrySendError::Disconnected(_)) => return,
                        }
                    }
                    Err(e) => {
                        fails += 1;
                        if fails > fps * 5 {
                            *sh_c.error.lock().unwrap_or_else(|e| e.into_inner()) = Some(format!("capture failing: {e}"));
                            sh_c.stop.store(true, Relaxed);
                        }
                        if w_tx_c.send(WMsg::Dup(seq)).is_err() { return; }
                    }
                }
                seq += 1;
            }
            let _ = w_tx_c.send(WMsg::Done(seq));
        }).map_err(es)?;

        let (r, shm) = match ready_rx.recv_timeout(Duration::from_secs(10)) {
            Ok(Ok(x)) => x,
            Ok(Err(e)) => { let _ = cap.join(); return Err(e); }
            Err(_) => { sh.stop.store(true, Relaxed); let _ = go_tx.send(false); return Err("capture init timeout".into()); }
        };
        let (cw, chh) = (r.w as usize, r.h as usize);
        let dims = if cfg.half { (cw as u32 / 2, chh as u32 / 2) } else { (cw as u32, chh as u32) };

        // ---- encoder workers
        let enc_rx = Arc::new(Mutex::new(enc_rx));
        let mut workers = Vec::new();
        for i in 0..nworkers {
            let rx = enc_rx.clone();
            let wtx = w_tx.clone();
            let ptx = pool_tx.clone();
            let (q, s444, half) = (cfg.quality, cfg.s444, cfg.half);
            let sh_w = sh.clone();
            workers.push(thread::Builder::new().name(format!("enc{i}")).spawn(move || {
                let mut scratch = Vec::new();
                loop {
                    // v1.3.1 FIX: a panicked sibling worker must not poison-panic the whole pool
                    let msg = { let g = rx.lock().unwrap_or_else(|e| e.into_inner()); g.recv() };
                    let Ok((seq, buf)) = msg else { break };
                    let (data, w, h): (&[u8], usize, usize) = if half {
                        let (ow, oh) = half_bgra(&buf, cw, chh, &mut scratch);
                        (&scratch, ow, oh)
                    } else { (&buf, cw, chh) };
                    let mut out = Vec::with_capacity(w * h / 4);
                    let mut enc = jpeg_encoder::Encoder::new(&mut out, q);
                    enc.set_sampling_factor(if s444 { jpeg_encoder::SamplingFactor::F_1_1 } else { jpeg_encoder::SamplingFactor::F_2_2 });
                    let r = enc.encode(data, w as u16, h as u16, jpeg_encoder::ColorType::Bgra);
                    let _ = ptx.send(buf); // recycle the raw frame buffer
                    let m = match r {
                        Ok(()) => WMsg::Frame(seq, out),
                        Err(e) => { *sh_w.error.lock().unwrap_or_else(|e| e.into_inner()) = Some(format!("jpeg encode: {e}")); WMsg::Dup(seq) }
                    };
                    if wtx.send(m).is_err() { break; }
                }
            }).map_err(es)?);
        }
        drop(pool_tx);

        // ---- writer: re-orders and writes video idx + .ev + .cam in lockstep (MediaOut)
        let dir = cfg.dir.clone();
        let sh_wr = sh.clone();
        drop(w_tx);
        let wfps = cfg.fps as u64;
        let writer = thread::Builder::new().name("writer".into()).spawn(move || -> Res<u64> {
            let r = (|| -> Res<u64> {
                let mut mo = MediaOut::create(&dir, has_cam)?;
                let mut pending: BTreeMap<u64, Option<Vec<u8>>> = BTreeMap::new();
                let mut metas: BTreeMap<u64, (Ev, Option<CamRef>)> = BTreeMap::new();
                // Leading dups (before the first real frame) are written once that frame exists;
                // their metadata waits with them so all streams stay the same length.
                let mut leading: Vec<(Ev, Option<CamRef>)> = Vec::new();
                let mut next = 0u64;
                let mut last: Option<[u8; 12]> = None;
                let mut total: Option<u64> = None;
                let step = |ent: Option<Vec<u8>>, meta: (Ev, Option<CamRef>), mo: &mut MediaOut,
                                last: &mut Option<[u8; 12]>, leading: &mut Vec<(Ev, Option<CamRef>)>| -> Res<()> {
                    match ent {
                        Some(v) => {
                            let e = mo.append_jpeg(&v)?;
                            for (ev, c) in leading.drain(..) { mo.put(&e, ev, &c, wfps)?; }
                            mo.put(&e, meta.0, &meta.1, wfps)?;
                            *last = Some(e);
                        }
                        None => match last { Some(e) => mo.put(e, meta.0, &meta.1, wfps)?, None => leading.push(meta) },
                    }
                    Ok(())
                };
                loop {
                    match w_rx.recv() {
                        Ok(WMsg::Frame(s, v)) => { pending.insert(s, Some(v)); }
                        Ok(WMsg::Dup(s)) => { pending.insert(s, None); }
                        Ok(WMsg::Meta(s, ev, c)) => { metas.insert(s, (ev, c)); }
                        Ok(WMsg::Done(n)) => total = Some(n),
                        Err(_) => break,
                    }
                    while pending.contains_key(&next) && metas.contains_key(&next) {
                        let ent = pending.remove(&next).flatten();
                        let meta = metas.remove(&next).unwrap_or_default();
                        step(ent, meta, &mut mo, &mut last, &mut leading)?;
                        next += 1;
                        sh_wr.written.store(next, Relaxed);
                    }
                    if let Some(n) = total { if next >= n && pending.is_empty() { break; } }
                }
                // Channel closed with gaps (should not happen): fill as duplicates.
                let end = total.unwrap_or(next).max(pending.keys().next_back().map(|k| k + 1).unwrap_or(0));
                while next < end {
                    let ent = pending.remove(&next).flatten();
                    let meta = metas.remove(&next).unwrap_or_default();
                    step(ent, meta, &mut mo, &mut last, &mut leading)?;
                    next += 1;
                }
                if last.is_none() { return Err("no frames were captured".into()); }
                let written = next - leading.len() as u64; // leading dups without a successor are dropped
                mo.flush()?;
                Ok(written)
            })();
            if let Err(e) = &r { *sh_wr.error.lock().unwrap_or_else(|e| e.into_inner()) = Some(e.clone()); sh_wr.stop.store(true, Relaxed); }
            r
        }).map_err(es)?;

        // ---- audio (main thread: cpal streams may be !Send). Failures degrade to video-only
        // for that source and are reported, never abort the recording.
        let open = |want: bool, src: AudioSrc, opts: SinkOpts| -> (Option<AudioCapture>, Option<String>) {
            if !want { return (None, None); }
            match AudioCapture::start(src, sh.paused.clone(), opts) { Ok(c) => (Some(c), None), Err(e) => (None, Some(e)) }
        };
        let (mic, mic_error) = open(cfg.mic.is_some(), AudioSrc::Mic(cfg.mic.as_ref().and_then(|m| m.as_deref())),
            SinkOpts { spill: Some(cfg.dir.join("mic.raw")), fx: cfg.live_fx });
        let (sys, sys_error) = open(cfg.sys, AudioSrc::System, SinkOpts { spill: Some(cfg.dir.join("sys.raw")), fx: None });
        // v1.4.0: recovery marker. Everything needed to rebuild a project if we never get to
        // Stop (crash, power loss, killed): dimensions, fps and the raw audio layouts.
        let mark = format!("screenreel_rec=1\nversion={VERSION}\nwidth={}\nheight={}\nfps={}\nmic_ch={}\nsys_ch={}\ncam={}\n",
            dims.0, dims.1, cfg.fps, mic.as_ref().map(|m| m.ch).unwrap_or(0), sys.as_ref().map(|m| m.ch).unwrap_or(0), has_cam as u8);
        if let Err(e) = std::fs::write(cfg.dir.join(REC_MARK), mark) { eprintln!("[rec] {REC_MARK}: {e} (crash recovery unavailable)"); }
        let _ = go_tx.send(true);
        Ok(Recorder { sh, cap: Some(cap), workers, writer: Some(writer), mic, sys, fps: cfg.fps, dims, dir: cfg.dir,
                      mic_error, sys_error, shm, cam_error, camera, input, has_cam })
    }

    /// Stop everything, join threads, return the frame count and audio (channels, 48 kHz
    /// interleaved samples) fitted exactly to the video length.
    fn finish(mut self) -> Res<RecOut> {
        let stop_at = self.sh.paused.active(Instant::now());
        self.sh.stop.store(true, Relaxed);
        if let Some(c) = self.cap.take() { let _ = c.join(); }
        for w in self.workers.drain(..) { let _ = w.join(); }
        let cam_warn = self.camera.as_ref().and_then(|c| c.error());
        self.camera = None; // stop the camera + input poller now that capture has ended
        self.input = None;
        let frames = self.writer.take().map(|w| w.join().unwrap_or_else(|_| Err("writer panicked".into())))
            .unwrap_or(Err("no writer".into()))?;
        let frames = u32::try_from(frames).map_err(|_| "recording too long")?;
        let spf = (AR / self.fps) as usize;
        let target = frames as usize * spf;
        let mut warn = cam_warn.map(|e| format!("webcam stopped: {e}"));
        let mut fit = |c: AudioCapture| {
            if let Some(e) = c.fx_error() { warn.get_or_insert(e); }
            let (ch, v) = c.finish(Some(stop_at));
            (ch, fit_len(v, ch as usize, target))
        };
        let mic = self.mic.take().map(&mut fit);
        let sys = self.sys.take().map(&mut fit);
        Ok(RecOut { frames, mic, sys, warn })
    }
}

/// v1.4.0: after a recording (or recovery) is saved as a project, drop the crash-safety
/// files: the raw audio is now in track WAVs and the marker would re-offer recovery.
fn clear_rec_spill(dir: &Path) {
    for n in ["mic.raw", "sys.raw", REC_MARK] { let _ = std::fs::remove_file(dir.join(n)); }
}

/// v1.4.0: rebuild a project from an interrupted recording folder (has rec.txt, no
/// project.txt). Truncated tails are tolerated: the frame index is cut at the last entry
/// whose JPEG is fully on disk, .ev/.cam are cut to match, audio is fitted to the video.
fn recover_recording(dir: &Path) -> Res<Project> {
    let txt = std::fs::read_to_string(dir.join(REC_MARK)).map_err(|e| format!("{REC_MARK}: {e}"))?;
    let (mut w, mut h, mut fps, mut mic_ch, mut sys_ch, mut ok) = (0u32, 0u32, 0u32, 0u16, 0u16, false);
    for l in txt.lines() {
        let Some((k, v)) = l.split_once('=') else { continue };
        match k {
            "screenreel_rec" => ok = v == "1",
            "width" => w = v.parse().unwrap_or(0),
            "height" => h = v.parse().unwrap_or(0),
            "fps" => fps = v.parse().unwrap_or(0),
            "mic_ch" => mic_ch = v.parse().unwrap_or(0),
            "sys_ch" => sys_ch = v.parse().unwrap_or(0),
            _ => {}
        }
    }
    if !ok || !FPS_CHOICES.contains(&fps) || w == 0 || h == 0 || w > 16384 || h > 16384 || mic_ch > 2 || sys_ch > 2 {
        return Err("recovery marker is damaged".into());
    }
    // Trim the index to whole entries that point inside the data actually on disk.
    let dlen = std::fs::metadata(dir.join("frames.mjpg")).map_err(|e| format!("frames.mjpg: {e}"))?.len();
    let raw = std::fs::read(dir.join("frames.idx")).map_err(|e| format!("frames.idx: {e}"))?;
    let mut good = 0usize;
    for e in raw.chunks_exact(12) {
        let off = u64::from_le_bytes(e[..8].try_into().unwrap());
        let len = u32::from_le_bytes(e[8..].try_into().unwrap());
        if len == 0 || len > MAX_JPEG || off.saturating_add(len as u64) > dlen { break; }
        good += 1;
    }
    let evn = std::fs::metadata(dir.join("frames.ev")).map(|m| m.len() as usize / 5).unwrap_or(0);
    let camn = std::fs::metadata(dir.join("frames.cam.idx")).map(|m| m.len() as usize / 12).ok();
    // streams are written in lockstep; a crash can leave them a few entries apart
    let mut n = good;
    if evn > 0 { n = n.min(evn); }
    if let Some(c) = camn { n = n.min(c); }
    if n == 0 { return Err("no complete frames were saved before the interruption".into()); }
    let cut = |name: &str, sz: usize| -> Res<()> {
        let p = dir.join(name);
        if let Ok(f) = std::fs::OpenOptions::new().write(true).open(&p) { f.set_len((n * sz) as u64).map_err(|e| format!("{name}: {e}"))?; }
        Ok(())
    };
    cut("frames.idx", 12)?;
    if evn > 0 { cut("frames.ev", 5)?; }
    if camn.is_some() { cut("frames.cam.idx", 12)?; }
    let src = Arc::new(Source::open(dir, w, h, fps)?);
    let target = src.count() as usize * src.spf();
    let read_raw = |name: &str, ch: u16| -> Option<PcmTrack> {
        if ch == 0 { return None; }
        let b = std::fs::read(dir.join(name)).ok()?;
        let v: Vec<i16> = b.chunks_exact(2).map(|p| i16::from_le_bytes([p[0], p[1]])).collect();
        let whole = v.len() / ch as usize * ch as usize;
        Some((ch, fit_len(v[..whole].to_vec(), ch as usize, target)))
    };
    let mut tracks = Vec::new();
    if let Some((ch, m)) = read_raw("mic.raw", mic_ch) { tracks.push(Track { name: "Mic".into(), ch, data: Arc::new(m), gain: 1.0, muted: false }); }
    if let Some((ch, v)) = read_raw("sys.raw", sys_ch) { tracks.push(Track { name: "System".into(), ch, data: Arc::new(v), gain: 1.0, muted: false }); }
    let mut p = Project { edit: Edit { segs: vec![Seg::Src { start: 0, len: src.count() }], tracks, ..Default::default() }, src, undo: vec![], redo: vec![], dirty: true, saved: vec![] };
    p.save()?;
    clear_rec_spill(dir);
    Ok(p)
}

/// Interrupted recordings under the recordings folder (newest first).
fn find_interrupted() -> Vec<PathBuf> {
    let Some(base) = rec_dir().parent().map(Path::to_path_buf) else { return vec![] };
    let Ok(rd) = std::fs::read_dir(&base) else { return vec![] };
    let mut v: Vec<PathBuf> = rd.flatten().map(|e| e.path())
        .filter(|p| p.join(REC_MARK).is_file() && !p.join("project.txt").exists() && p.join("frames.idx").is_file()).collect();
    v.sort();
    v.reverse();
    v
}

// =====================================================================================
// Project model: source frames + edit decision list + audio tracks
// =====================================================================================

/// v1.4.0: `ev` = per-frame input samples (empty for pre-1.4 recordings), `cam` = webcam
/// frames (one index entry per screen frame; len 0 = none).
struct Source { dir: PathBuf, base: String, file: File, idx: Vec<(u64, u32)>, w: u32, h: u32, fps: u32, ev: Vec<Ev>, cam: Option<CamSrc> }

struct CamSrc { file: File, idx: Vec<(u64, u32)> }

/// Every file that belongs to one media generation `<base>.*`.
const MEDIA_EXTS: [&str; 5] = ["mjpg", "idx", "ev", "cam.mjpg", "cam.idx"];

#[cfg(unix)]
fn read_at(f: &File, buf: &mut [u8], off: u64) -> std::io::Result<()> { std::os::unix::fs::FileExt::read_exact_at(f, buf, off) }
#[cfg(windows)]
fn read_at(f: &File, mut buf: &mut [u8], mut off: u64) -> std::io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !buf.is_empty() {
        let n = f.seek_read(buf, off)?;
        if n == 0 { return Err(std::io::ErrorKind::UnexpectedEof.into()); }
        buf = &mut buf[n..];
        off += n as u64;
    }
    Ok(())
}

impl Source {
    fn open(dir: &Path, w: u32, h: u32, fps: u32) -> Res<Source> { Source::open_base(dir, "frames", w, h, fps) }
    /// `base` names the media pair <base>.mjpg/<base>.idx (changes after "Compact project").
    fn open_base(dir: &Path, base: &str, w: u32, h: u32, fps: u32) -> Res<Source> {
        if !safe_file_name(base) { return Err("bad media name".into()); }
        let file = File::open(dir.join(format!("{base}.mjpg"))).map_err(|e| format!("{base}.mjpg: {e}"))?;
        let flen = file.metadata().map_err(es)?.len();
        let mut raw = Vec::new();
        File::open(dir.join(format!("{base}.idx"))).map_err(|e| format!("{base}.idx: {e}"))?.read_to_end(&mut raw).map_err(es)?;
        let mut idx = Vec::with_capacity(raw.len() / 12);
        for e in raw.chunks_exact(12) {
            let off = u64::from_le_bytes(e[..8].try_into().unwrap());
            let len = u32::from_le_bytes(e[8..].try_into().unwrap());
            if len == 0 || len > MAX_JPEG || off.checked_add(len as u64).map(|end| end > flen).unwrap_or(true) {
                return Err("corrupt frame index (entry out of bounds)".into());
            }
            idx.push((off, len));
        }
        if idx.is_empty() { return Err("project has no frames".into()); }
        if idx.len() > u32::MAX as usize { return Err("too many frames".into()); }
        let n = idx.len();
        // v1.4.0: optional side streams. Missing = feature not recorded; a length mismatch
        // (older crash, hand edits) is padded/truncated rather than failing the whole project.
        let mut ev: Vec<Ev> = std::fs::read(dir.join(format!("{base}.ev"))).map(|b| b.chunks_exact(5).map(Ev::from_bytes).collect()).unwrap_or_default();
        if !ev.is_empty() { ev.resize(n, Ev::default()); }
        let cam = (|| -> Option<CamSrc> {
            let file = File::open(dir.join(format!("{base}.cam.mjpg"))).ok()?;
            let clen = file.metadata().ok()?.len();
            let raw = std::fs::read(dir.join(format!("{base}.cam.idx"))).ok()?;
            let mut idx: Vec<(u64, u32)> = raw.chunks_exact(12).map(|e| {
                let (o, l) = (u64::from_le_bytes(e[..8].try_into().unwrap()), u32::from_le_bytes(e[8..].try_into().unwrap()));
                // out-of-bounds entries degrade to "no camera frame" instead of failing
                if l > MAX_JPEG || o.checked_add(l as u64).map(|x| x > clen).unwrap_or(true) { (0, 0) } else { (o, l) }
            }).collect();
            if idx.iter().all(|e| e.1 == 0) { return None; }
            idx.resize(n, (0, 0));
            Some(CamSrc { file, idx })
        })();
        Ok(Source { dir: dir.to_path_buf(), base: base.to_string(), file, idx, w, h, fps, ev, cam })
    }
    fn count(&self) -> u32 { self.idx.len() as u32 }
    fn read(&self, i: u32) -> Res<Vec<u8>> {
        let (o, l) = *self.idx.get(i as usize).ok_or("frame index out of range")?;
        let mut b = vec![0u8; l as usize];
        read_at(&self.file, &mut b, o).map_err(es)?;
        Ok(b)
    }
    fn spf(&self) -> usize { (AR / self.fps) as usize }
    /// v1.4.0: input sample of source frame `i` (default = no data).
    fn ev(&self, i: u32) -> Ev { self.ev.get(i as usize).copied().unwrap_or_default() }
    fn has_ev(&self) -> bool { !self.ev.is_empty() }
    /// v1.4.0: webcam JPEG recorded with source frame `i`, walking back to the most recent
    /// earlier frame if the camera had not delivered yet / was late (bounded look-back).
    fn read_cam(&self, i: u32) -> Option<Vec<u8>> {
        let c = self.cam.as_ref()?;
        let i = (i as usize).min(c.idx.len().checked_sub(1)?);
        let (o, l) = (0..=i).rev().take(64).map(|k| c.idx[k]).find(|e| e.1 > 0)?;
        let mut b = vec![0u8; l as usize];
        read_at(&c.file, &mut b, o).ok()?;
        Some(b)
    }
    fn entry(&self, i: u32) -> Option<(u64, u32)> { self.idx.get(i as usize).copied() }

    /// Lossless compaction: write a new media pair holding each frame the edit actually uses
    /// exactly once, with one index entry per output frame (holds/speed-ups become repeated or
    /// skipped index entries, 12 bytes each). Returns (new Source, bytes before, bytes after).
    /// Crash-safe: new files are fully written + fsynced before project.txt points at them.
    fn compact(&self, edit: &Edit, progress: &AtomicU32) -> Res<(Source, u64, u64)> {
        let total = edit.total();
        if total == 0 { return Err("nothing to keep".into()); }
        let gen = (1..10_000).find(|g| !self.dir.join(format!("frames_c{g}.mjpg")).exists()).ok_or("too many compactions")?;
        let base = format!("frames_c{gen}");
        let (pm, pi) = (self.dir.join(format!("{base}.mjpg")), self.dir.join(format!("{base}.idx")));
        let (pe, pcm, pci) = (self.dir.join(format!("{base}.ev")), self.dir.join(format!("{base}.cam.mjpg")), self.dir.join(format!("{base}.cam.idx")));
        let res = (|| -> Res<u64> {
            let mut data = BufWriter::with_capacity(4 << 20, File::create(&pm).map_err(es)?);
            let mut idx = BufWriter::with_capacity(64 << 10, File::create(&pi).map_err(es)?);
            let mut seen: HashMap<u64, (u64, u32)> = HashMap::new(); // old offset -> new entry
            let mut off = 0u64;
            let mut buf = Vec::new();
            let mut f = 0u32;
            // v1.4.0: side streams follow the same output-frame mapping
            let mut evw = if self.has_ev() { Some(BufWriter::new(File::create(&pe).map_err(es)?)) } else { None };
            let mut camw = match &self.cam { Some(_) => Some((BufWriter::new(File::create(&pcm).map_err(es)?), BufWriter::new(File::create(&pci).map_err(es)?))), None => None };
            let mut cam_seen: HashMap<u64, (u64, u32)> = HashMap::new();
            let mut cam_off = 0u64;
            let mut prev_sf: Option<u32> = None;
            for s in &edit.segs {
                for k in 0..s.len() {
                    let sf = s.src_at(k);
                    if let Some(w) = &mut evw {
                        let mut e = self.ev(sf);
                        // a held/repeated frame must not replay its click ring over and over
                        if prev_sf == Some(sf) { e.f &= !(EV_CLICK | EV_KEY); }
                        w.write_all(&e.to_bytes()).map_err(es)?;
                    }
                    prev_sf = Some(sf);
                    if let (Some((cd, ci)), Some(c)) = (&mut camw, &self.cam) {
                        let (co, cl) = c.idx.get(sf as usize).copied().unwrap_or((0, 0));
                        let ne = if cl == 0 { (0, 0) } else { match cam_seen.get(&co) { Some(e) => *e, None => {
                            let mut b = vec![0u8; cl as usize];
                            read_at(&c.file, &mut b, co).map_err(es)?;
                            cd.write_all(&b).map_err(es)?;
                            let e = (cam_off, cl); cam_off += cl as u64; cam_seen.insert(co, e); e
                        } } };
                        ci.write_all(&ne.0.to_le_bytes()).map_err(es)?;
                        ci.write_all(&ne.1.to_le_bytes()).map_err(es)?;
                    }
                    let (o, l) = self.entry(sf).ok_or("edit references missing frame")?;
                    let e = match seen.get(&o) { Some(e) => *e, None => {
                        buf.resize(l as usize, 0);
                        read_at(&self.file, &mut buf, o).map_err(es)?;
                        data.write_all(&buf).map_err(es)?;
                        let e = (off, l); off += l as u64; seen.insert(o, e); e
                    } };
                    idx.write_all(&e.0.to_le_bytes()).map_err(es)?;
                    idx.write_all(&e.1.to_le_bytes()).map_err(es)?;
                    f += 1;
                    if f % 64 == 0 { progress.store((f as u64 * 1000 / total as u64) as u32, Relaxed); }
                }
            }
            data.into_inner().map_err(|e| e.to_string())?.sync_all().map_err(es)?;
            if let Some(w) = evw { w.into_inner().map_err(|e| e.to_string())?.sync_all().map_err(es)?; }
            if let Some((cd, ci)) = camw {
                cd.into_inner().map_err(|e| e.to_string())?.sync_all().map_err(es)?;
                ci.into_inner().map_err(|e| e.to_string())?.sync_all().map_err(es)?;
            }
            // index last: it is what makes the new generation "complete"
            idx.into_inner().map_err(|e| e.to_string())?.sync_all().map_err(es)?;
            Ok(off)
        })();
        let after = match res { Ok(n) => n, Err(e) => { for p in [&pm, &pi, &pe, &pcm, &pci] { let _ = std::fs::remove_file(p); } return Err(e); } };
        let before = self.file.metadata().map(|m| m.len()).unwrap_or(0);
        let ns = Source::open_base(&self.dir, &base, self.w, self.h, self.fps)?;
        Ok((ns, before, after))
    }
}

/// Edit-list segment. `Fast` plays source frames start + floor((ph+k)*sp/100) for output
/// frame k: `sp` is speed x100 (150 = 1.5x) and `ph` the phase, so splitting a Fast segment
/// anywhere keeps the exact same frame mapping on both sides (no drift, lossless merge).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Seg { Src { start: u32, len: u32 }, Hold { frame: u32, len: u32 }, Fast { start: u32, len: u32, sp: u32, ph: u32 } }

const SPEED_MIN: u32 = 101; // x100
const SPEED_MAX: u32 = 6400; // 64x (combined, after repeated speed-ups)

impl Seg {
    fn len(&self) -> u32 { match *self { Seg::Src { len, .. } | Seg::Hold { len, .. } | Seg::Fast { len, .. } => len } }
    fn src_at(&self, k: u32) -> u32 {
        match *self {
            Seg::Src { start, .. } => start + k,
            Seg::Hold { frame, .. } => frame,
            // u64 math: (ph+k)*sp cannot overflow; result is validated <= last source frame
            Seg::Fast { start, sp, ph, .. } => start + ((ph as u64 + k as u64) * sp as u64 / 100) as u32,
        }
    }
    fn sub(&self, a: u32, b: u32) -> Seg {
        match *self {
            Seg::Src { start, .. } => Seg::Src { start: start + a, len: b - a },
            Seg::Hold { frame, .. } => Seg::Hold { frame, len: b - a },
            Seg::Fast { start, sp, ph, .. } => Seg::Fast { start, len: b - a, sp, ph: ph + a },
        }
    }
    /// Highest source frame this segment touches (for load-time validation).
    fn max_src(&self) -> u64 {
        match *self {
            Seg::Src { start, len } => start as u64 + len.max(1) as u64 - 1,
            Seg::Hold { frame, .. } => frame as u64,
            Seg::Fast { start, len, sp, ph } => start as u64 + (ph as u64 + len.max(1) as u64 - 1) * sp as u64 / 100,
        }
    }
}

/// Pitch-preserving time compression (WSOLA) of interleaved PCM to exactly `out_frames`.
/// Window 20 ms, 50% overlap Hann OLA; each grain is aligned to the natural continuation
/// of the previous one by a coarse-to-fine cross-correlation search (±5 ms), so speech
/// stays intelligible without chipmunk pitch. O(n) in output length with a bounded search.
fn time_stretch(x: &[i16], ch: usize, out_frames: usize) -> Vec<i16> {
    let n_in = x.len() / ch.max(1);
    if out_frames == 0 || n_in == 0 { return vec![0; out_frames * ch]; }
    if out_frames >= n_in { let mut v = x[..n_in * ch].to_vec(); v.resize(out_frames * ch, 0); return v; }
    const W: usize = 960; // 20 ms @ 48 kHz
    const HS: usize = W / 2;
    const TOL: usize = 240; // ±5 ms search
    if n_in < W * 2 || out_frames < W {
        // too short for grains: nearest-frame decimation (still exact length)
        let mut v = Vec::with_capacity(out_frames * ch);
        for k in 0..out_frames { let i = k * n_in / out_frames; v.extend_from_slice(&x[i * ch..i * ch + ch]); }
        return v;
    }
    let mono = |i: usize| -> f32 { let b = i * ch; (0..ch).map(|c| x[b + c] as f32).sum::<f32>() / ch as f32 };
    let win: Vec<f32> = (0..W).map(|i| 0.5 - 0.5 * (std::f32::consts::TAU * i as f32 / W as f32).cos()).collect();
    let mut acc = vec![0f32; (out_frames + W) * ch];
    let mut wsum = vec![0f32; out_frames + W];
    let ratio = n_in as f64 / out_frames as f64;
    let mut prev: usize = 0; // input position of previous grain
    let mut k = 0usize;
    while k * HS < out_frames {
        let o = k * HS;
        let nominal = ((o as f64) * ratio) as usize;
        let pos = if k == 0 { 0 } else {
            // template = natural continuation of the previous grain
            let tpl = prev + HS;
            let lo = nominal.saturating_sub(TOL);
            let hi = (nominal + TOL).min(n_in.saturating_sub(W));
            if tpl + W > n_in || lo > hi { nominal.min(n_in.saturating_sub(W)) } else {
                let score = |c: usize, step: usize| -> f32 { let mut s = 0f32; let mut i = 0; while i < HS { s += mono(tpl + i) * mono(c + i); i += step; } s };
                let mut best = lo; let mut bs = f32::MIN;
                let mut c = lo; while c <= hi { let v = score(c, 4); if v > bs { bs = v; best = c; } c += 4; }
                let (l2, h2) = (best.saturating_sub(3).max(lo), (best + 3).min(hi));
                bs = f32::MIN; for c in l2..=h2 { let v = score(c, 1); if v > bs { bs = v; best = c; } }
                best
            }
        };
        let pos = pos.min(n_in.saturating_sub(W));
        // v1.3.1 FIX (clippy needless_range_loop): iterate the window directly.
        for (i, &w) in win.iter().enumerate() {
            let (si, di) = (pos + i, o + i);
            if si >= n_in || di >= out_frames + W { break; }
            for c in 0..ch { acc[di * ch + c] += x[si * ch + c] as f32 * w; }
            wsum[di] += w;
        }
        prev = pos;
        k += 1;
    }
    let mut out = Vec::with_capacity(out_frames * ch);
    for i in 0..out_frames {
        let w = if wsum[i] > 1e-3 { wsum[i] } else { 1.0 };
        for c in 0..ch { out.push((acc[i * ch + c] / w).round().clamp(-32768.0, 32767.0) as i16); }
    }
    out
}

#[derive(Clone)]
struct Track { name: String, ch: u16, data: Arc<Vec<i16>>, gain: f32, muted: bool }

impl Track {
    fn frames(&self) -> usize { self.data.len() / self.ch as usize }
}

#[derive(Clone, Default)]
/// v1.4.0: `ov` (annotations + zooms, output-frame time) and `fx` (cursor/webcam settings)
/// live in the edit so undo/redo and save cover them.
struct Edit { segs: Vec<Seg>, tracks: Vec<Track>, ov: Vec<Overlay>, fx: FxSettings }

impl Edit {
    fn total(&self) -> u32 { self.segs.iter().fold(0u32, |a, s| a.saturating_add(s.len())) }

    fn src_frame(&self, f: u32) -> Option<u32> {
        let mut acc = 0u32;
        for s in &self.segs {
            if f < acc + s.len() { return Some(s.src_at(f - acc)); }
            acc += s.len();
        }
        None
    }

    /// Ensure a segment boundary at output frame `f`; return index of the segment starting there.
    fn split(&mut self, f: u32) -> usize {
        let mut acc = 0u32;
        for i in 0..self.segs.len() {
            let l = self.segs[i].len();
            if f == acc { return i; }
            if f < acc + l {
                let s = self.segs[i];
                let k = f - acc;
                self.segs[i] = s.sub(0, k);
                self.segs.insert(i + 1, s.sub(k, l));
                return i + 1;
            }
            acc += l;
        }
        self.segs.len()
    }

    fn normalize(&mut self) {
        let mut out: Vec<Seg> = Vec::with_capacity(self.segs.len());
        for s in self.segs.drain(..) {
            if s.len() == 0 { continue; }
            if let Some(last) = out.last_mut() {
                match (*last, s) {
                    (Seg::Src { start: a, len: la }, Seg::Src { start: b, len: lb }) if a + la == b => { *last = Seg::Src { start: a, len: la + lb }; continue; }
                    (Seg::Hold { frame: a, len: la }, Seg::Hold { frame: b, len: lb }) if a == b => { *last = Seg::Hold { frame: a, len: la + lb }; continue; }
                    (Seg::Fast { start: a, len: la, sp: pa, ph: ha }, Seg::Fast { start: b, len: lb, sp: pb, ph: hb }) if a == b && pa == pb && ha + la == hb => {
                        *last = Seg::Fast { start: a, len: la + lb, sp: pa, ph: ha }; continue;
                    }
                    _ => {}
                }
            }
            out.push(s);
        }
        self.segs = out;
    }

    /// v1.4.0: move overlays with ripple edits; overlays squeezed to nothing are dropped.
    fn ov_remap(&mut self, m: impl Fn(u32) -> u32) {
        for o in &mut self.ov { o.start = m(o.start); o.end = m(o.end); }
        self.ov.retain(|o| o.end > o.start);
    }

    /// Ripple-delete output frames [a,b) from video and every audio track.
    fn cut(&mut self, a: u32, b: u32, spf: usize) {
        if b <= a { return; }
        self.ov_remap(|f| if f <= a { f } else if f >= b { f - (b - a) } else { a });
        let i = self.split(a);
        let j = self.split(b);
        self.segs.drain(i..j);
        self.normalize();
        for t in &mut self.tracks {
            let ch = t.ch as usize;
            let (s, e) = ((a as usize * spf * ch).min(t.data.len()), (b as usize * spf * ch).min(t.data.len()));
            if s < e {
                let mut v = Vec::with_capacity(t.data.len() - (e - s));
                v.extend_from_slice(&t.data[..s]);
                v.extend_from_slice(&t.data[e..]);
                t.data = Arc::new(v);
            }
        }
    }

    /// Insert `n` frames holding the frame at `at`; tracks get matching silence (ripple).
    fn insert_freeze(&mut self, at: u32, n: u32, spf: usize) {
        let Some(src) = self.src_frame(at).or_else(|| at.checked_sub(1).and_then(|p| self.src_frame(p))) else { return };
        let i = self.split(at);
        self.segs.insert(i, Seg::Hold { frame: src, len: n });
        self.normalize();
        self.ov_remap(|f| if f >= at { f.saturating_add(n) } else { f });
        for t in &mut self.tracks {
            let ch = t.ch as usize;
            let pos = at as usize * spf * ch;
            if pos < t.data.len() {
                let mut v = Vec::with_capacity(t.data.len() + n as usize * spf * ch);
                v.extend_from_slice(&t.data[..pos]);
                v.resize(pos + n as usize * spf * ch, 0);
                v.extend_from_slice(&t.data[pos..]);
                t.data = Arc::new(v);
            }
        }
    }

    /// Hold the first frame of [a,b) across the range; audio keeps playing (no ripple).
    fn freeze_range(&mut self, a: u32, b: u32) {
        if b <= a { return; }
        let Some(src) = self.src_frame(a) else { return };
        let i = self.split(a);
        let j = self.split(b);
        self.segs.drain(i..j);
        self.segs.insert(i, Seg::Hold { frame: src, len: b - a });
        self.normalize();
    }

    /// Shift a track by whole frames (positive = later).
    fn shift_track(&mut self, ti: usize, frames: i32, spf: usize) {
        let Some(t) = self.tracks.get_mut(ti) else { return };
        let n = frames.unsigned_abs() as usize * spf * t.ch as usize;
        let v = if frames >= 0 {
            let mut v = vec![0i16; n];
            v.extend_from_slice(&t.data);
            v
        } else { t.data[n.min(t.data.len())..].to_vec() };
        t.data = Arc::new(v);
    }

    /// Speed up output frames [a,b) by `sp`/100 (ripple). Video: each segment piece becomes a
    /// Fast segment (Hold pieces just get shorter). Audio: the range is WSOLA-compressed to the
    /// exact new length (or silenced when `mute`). Returns the new end frame.
    fn speed_range(&mut self, a: u32, b: u32, sp: u32, mute: bool, spf: usize) -> u32 {
        if b <= a || sp < SPEED_MIN { return b; }
        let i = self.split(a);
        let j = self.split(b);
        let mut new: Vec<Seg> = Vec::with_capacity(j - i);
        for s in &self.segs[i..j] {
            let l = s.len() as u64;
            let nl = (l * 100).div_ceil(sp as u64).max(1) as u32; // ceil (v1.3.1: div_ceil)
            match *s {
                Seg::Hold { frame, .. } => new.push(Seg::Hold { frame, len: nl }),
                Seg::Src { .. } | Seg::Fast { .. } => {
                    let (old_sp, last) = match *s { Seg::Fast { sp: o, .. } => (o, s.max_src()), _ => (100, s.max_src()) };
                    let csp = (old_sp as u64 * sp as u64 / 100).clamp(SPEED_MIN as u64, SPEED_MAX as u64) as u32;
                    let start = s.src_at(0);
                    let mut nl = (l * old_sp as u64).div_ceil(csp as u64).max(1) as u32; // v1.3.1: div_ceil
                    // never read past the original piece's last source frame
                    while nl > 1 && start as u64 + (nl as u64 - 1) * csp as u64 / 100 > last { nl -= 1; }
                    new.push(Seg::Fast { start, len: nl, sp: csp, ph: 0 });
                }
            }
        }
        let old_len = b - a;
        let new_len: u32 = new.iter().map(|s| s.len()).sum();
        self.segs.splice(i..j, new);
        self.normalize();
        self.ov_remap(|f| if f <= a { f } else if f >= b { f - old_len + new_len } else { a + ((f - a) as u64 * new_len as u64 / old_len as u64) as u32 });
        for t in &mut self.tracks {
            let ch = t.ch as usize;
            let (s0, e0) = ((a as usize * spf * ch).min(t.data.len()), (b as usize * spf * ch).min(t.data.len()));
            if s0 >= e0 { continue; }
            let avail = (e0 - s0) / ch;
            // partial coverage keeps the same proportion of the new range
            let out = (avail as u64 * new_len as u64 * spf as u64 / (old_len as u64 * spf as u64).max(1)) as usize;
            let mid = if mute { vec![0i16; out * ch] } else { time_stretch(&t.data[s0..e0], ch, out) };
            let mut v = Vec::with_capacity(t.data.len() - (e0 - s0) + mid.len());
            v.extend_from_slice(&t.data[..s0]);
            v.extend_from_slice(&mid);
            v.extend_from_slice(&t.data[e0..]);
            t.data = Arc::new(v);
        }
        a + new_len
    }

    /// Remove several output ranges (sorted, non-overlapping) in one pass, last first so
    /// earlier indices stay valid.
    fn cut_many(&mut self, ranges: &[(u32, u32)], spf: usize) {
        for &(a, b) in ranges.iter().rev() { self.cut(a, b, spf); }
    }

    /// Overwrite track samples starting at frame `at` (mono input, upmixed if needed);
    /// optionally silence up to `clear_to` frame.
    fn write_track(&mut self, ti: usize, at: u32, mono: &[i16], clear_to: Option<u32>, spf: usize) {
        let Some(t) = self.tracks.get_mut(ti) else { return };
        let ch = t.ch as usize;
        let start = at as usize * spf;
        let end = clear_to.map(|c| (c as usize * spf).max(start + mono.len())).unwrap_or(start + mono.len());
        let mut v: Vec<i16> = (*t.data).clone();
        if v.len() < end * ch { v.resize(end * ch, 0); }
        for i in start..end {
            let s = mono.get(i - start).copied().unwrap_or(0);
            for c in 0..ch { v[i * ch + c] = s; }
        }
        t.data = Arc::new(v);
    }
}

struct Project {
    src: Arc<Source>, edit: Edit, undo: Vec<Edit>, redo: Vec<Edit>, dirty: bool,
    /// v1.3.1 PERF: the sample buffer last written to `track_{i}.wav`, per index. A Weak (not
    /// an Arc) so it never keeps audio alive, and it pins the allocation so the pointer can't
    /// be reused (no ABA). Save skips a WAV whose buffer is unchanged: Ctrl+S on a long
    /// project no longer rewrites hundreds of MB when only an edit or a gain changed.
    saved: Vec<std::sync::Weak<Vec<i16>>>,
}

/// Max audio tracks accepted from a project file (v1.3.1: memory/DoS guard on load).
const MAX_TRACKS: usize = 64;

fn safe_file_name(s: &str) -> bool {
    !s.is_empty() && s.len() < 128 && !s.starts_with('.')
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.')
}

fn sanitize_name(s: &str) -> String {
    let t: String = s.chars().filter(|c| !c.is_control() && *c != '|').take(48).collect();
    if t.trim().is_empty() { "Track".into() } else { t }
}

impl Project {
    /// v1.3.1 FIX: crash-safe save. v1.3.0 overwrote track WAVs in place and deleted stale
    /// track files *before* project.txt was replaced, so a failure in between (disk full,
    /// power loss, AV lock on Windows) left a project.txt pointing at deleted or half-written
    /// tracks: the project would not open. Order now: (1) stage changed tracks as
    /// `track_N.wav.new`, (2) write project.txt.tmp, (3) promote staged tracks, (4) atomically
    /// swap project.txt, (5) only then remove stale files. Any error before (3) leaves the
    /// previous save fully intact and cleans up the staging files.
    /// project.txt body without the track lines (shared by save and autosave).
    fn header_text(&self) -> String {
        let mut txt = format!("screenreel_project=1\nversion={VERSION}\nwidth={}\nheight={}\nfps={}\nmedia={}\n", self.src.w, self.src.h, self.src.fps, self.src.base);
        for s in &self.edit.segs {
            txt += &match *s {
                Seg::Src { start, len } => format!("seg=S {start} {len}\n"),
                Seg::Hold { frame, len } => format!("seg=H {frame} {len}\n"),
                Seg::Fast { start, len, sp, ph } => format!("seg=F {start} {len} {sp} {ph}\n"),
            };
        }
        // v1.4.0: effects + overlays
        txt += &self.edit.fx.to_line();
        for o in &self.edit.ov { txt += &o.to_line(); }
        txt
    }

    /// v1.4.0: snapshot for the background autosave thread. Tracks whose buffer is the one
    /// last saved point at their canonical WAV; changed ones go to autosave_track_N.wav.
    fn autosave_job(&self) -> impl FnOnce() -> Res<()> + Send + 'static {
        let dir = self.src.dir.clone();
        let mut txt = self.header_text();
        let mut writes: Vec<(PathBuf, u16, Arc<Vec<i16>>)> = Vec::new();
        for (i, t) in self.edit.tracks.iter().enumerate() {
            let canon = format!("track_{i}.wav");
            let same = self.saved.get(i).map(|w| std::ptr::eq(w.as_ptr(), Arc::as_ptr(&t.data))).unwrap_or(false) && dir.join(&canon).is_file();
            let f = if same { canon } else { let f = format!("{AUTOSAVE_PREFIX}track_{i}.wav"); writes.push((dir.join(&f), t.ch, t.data.clone())); f };
            txt += &format!("track={}|{}|{}|{}\n", sanitize_name(&t.name), t.gain, t.muted as u8, f);
        }
        move || {
            for (path, ch, data) in &writes { write_wav(path, *ch, AR, data)?; }
            let tmp = dir.join(format!("{AUTOSAVE_TXT}.tmp"));
            std::fs::write(&tmp, &txt).map_err(es)?;
            std::fs::rename(&tmp, dir.join(AUTOSAVE_TXT)).map_err(es)
        }
    }

    fn save(&mut self) -> Res<()> {
        let dir = self.src.dir.clone();
        let mut txt = self.header_text();
        let mut keep = Vec::new();
        let mut staged: Vec<(PathBuf, PathBuf)> = Vec::new();
        let cleanup = |staged: &[(PathBuf, PathBuf)]| for (n, _) in staged { let _ = std::fs::remove_file(n); };
        for (i, t) in self.edit.tracks.iter().enumerate() {
            let f = format!("track_{i}.wav");
            let fin = dir.join(&f);
            // v1.3.1 PERF: unchanged buffer + file still present -> nothing to write
            let same = self.saved.get(i).map(|w| std::ptr::eq(w.as_ptr(), Arc::as_ptr(&t.data))).unwrap_or(false);
            if !(same && fin.is_file()) {
                let new = dir.join(format!("{f}.new"));
                if let Err(e) = write_wav(&new, t.ch, AR, &t.data) { cleanup(&staged); let _ = std::fs::remove_file(&new); return Err(e); }
                staged.push((new, fin));
            }
            // gain is always finite here (UI slider / clamped on load); `{}` of f32 round-trips
            txt += &format!("track={}|{}|{}|{}\n", sanitize_name(&t.name), t.gain, t.muted as u8, f);
            keep.push(f);
        }
        let tmp = dir.join("project.txt.tmp");
        if let Err(e) = std::fs::write(&tmp, &txt) { cleanup(&staged); let _ = std::fs::remove_file(&tmp); return Err(format!("project.txt: {e}")); }
        for (new, fin) in &staged {
            if let Err(e) = std::fs::rename(new, fin) { cleanup(&staged); let _ = std::fs::remove_file(&tmp); return Err(format!("{}: {e}", fin.display())); }
        }
        std::fs::rename(&tmp, dir.join("project.txt")).map_err(|e| format!("project.txt: {e}"))?;
        // Remove stale track files from earlier saves (and leftovers of interrupted ones),
        // now that project.txt no longer references them.
        if let Ok(rd) = std::fs::read_dir(&dir) {
            for e in rd.flatten() {
                let n = e.file_name().to_string_lossy().to_string();
                let stale = n.starts_with("track_") && (n.ends_with(".wav.new") || n.ends_with(".wav.tmp") || n.ends_with(".new.tmp")
                    || (n.ends_with(".wav") && !keep.contains(&n)));
                if stale { let _ = std::fs::remove_file(e.path()); }
            }
        }
        self.saved = self.edit.tracks.iter().map(|t| Arc::downgrade(&t.data)).collect();
        self.dirty = false;
        clear_autosave(&dir); // v1.4.0: the real save supersedes any autosave
        Ok(())
    }

    fn load(path: &Path) -> Res<Project> { Project::load_from(path, "project.txt") }

    /// `txtname`: "project.txt" or the autosave file (v1.4.0 recovery).
    fn load_from(path: &Path, txtname: &str) -> Res<Project> {
        let dir = if path.is_dir() { path.to_path_buf() } else { path.parent().ok_or("bad path")?.to_path_buf() };
        let txt = std::fs::read_to_string(dir.join(txtname)).map_err(|e| format!("{txtname}: {e}"))?;
        if txt.len() > 16 << 20 { return Err("project.txt too large".into()); }
        let (mut w, mut h, mut fps, mut magic) = (0u32, 0u32, 0u32, false);
        let mut media = String::from("frames");
        let mut segs = Vec::new();
        let mut tdefs = Vec::new();
        let (mut ov, mut fx) = (Vec::new(), FxSettings::default());
        for line in txt.lines() {
            let Some((k, v)) = line.split_once('=') else { continue };
            match k {
                "screenreel_project" => magic = v == "1",
                "width" => w = v.parse().map_err(es)?,
                "height" => h = v.parse().map_err(es)?,
                "fps" => fps = v.parse().map_err(es)?,
                "media" => { if !safe_file_name(v) { return Err("bad media name".into()); } media = v.to_string(); }
                "seg" => {
                    let p: Vec<&str> = v.split_whitespace().collect();
                    let num = |i: usize| -> Res<u32> { p.get(i).ok_or_else(|| format!("bad seg line: {line}"))?.parse().map_err(es) };
                    let seg = match (p.first().copied(), p.len()) {
                        (Some("S"), 3) => Seg::Src { start: num(1)?, len: num(2)? },
                        (Some("H"), 3) => Seg::Hold { frame: num(1)?, len: num(2)? },
                        (Some("F"), 5) => {
                            let sp = num(3)?;
                            if !(SPEED_MIN..=SPEED_MAX).contains(&sp) { return Err(format!("bad speed: {line}")); }
                            Seg::Fast { start: num(1)?, len: num(2)?, sp, ph: num(4)? }
                        }
                        _ => return Err(format!("bad seg line: {line}")),
                    };
                    if segs.len() >= 1 << 20 { return Err("too many segments".into()); }
                    segs.push(seg);
                }
                "fx" => fx = FxSettings::from_line(v),
                "ov" => {
                    if ov.len() >= MAX_OVERLAYS { return Err("too many overlays".into()); }
                    ov.push(Overlay::from_line(v)?);
                }
                "track" => {
                    let p: Vec<&str> = v.split('|').collect();
                    if p.len() != 4 || !safe_file_name(p[3]) { return Err(format!("bad track line: {line}")); }
                    let gain: f32 = p[1].parse().map_err(es)?;
                    // v1.3.1 FIX: "NaN"/"inf" parse fine and f32::clamp passes NaN through, which
                    // turned the whole mix into NaN (silence/garbage). Reject non-finite gains.
                    if !gain.is_finite() { return Err(format!("bad track gain: {line}")); }
                    if tdefs.len() >= MAX_TRACKS { return Err("too many tracks".into()); }
                    tdefs.push((p[0].to_string(), gain.clamp(0.0, 4.0), p[2] == "1", p[3].to_string()));
                }
                _ => {}
            }
        }
        if !magic { return Err("not a ScreenReel project".into()); }
        if !FPS_CHOICES.contains(&fps) || w == 0 || h == 0 || w > 16384 || h > 16384 { return Err("invalid project header".into()); }
        let src = Arc::new(Source::open_base(&dir, &media, w, h, fps)?);
        let n = src.count() as u64;
        let mut total = 0u64;
        for s in &segs {
            // u64 max_src: a hostile seg line cannot overflow into a "valid" frame
            if s.len() == 0 || s.max_src() >= n { return Err("segment references frames outside the recording".into()); }
            if let Seg::Fast { ph, .. } = *s { if ph > u32::MAX / 2 { return Err("bad segment phase".into()); } }
            total += s.len() as u64;
        }
        if total > u32::MAX as u64 / 2 { return Err("project too long".into()); }
        let mut tracks = Vec::new();
        let mut saved = Vec::new();
        for (i, (name, gain, muted, f)) in tdefs.into_iter().enumerate() {
            let (ch, data) = decode_audio(&dir.join(&f))?;
            let data = Arc::new(data);
            // v1.3.1 PERF: a track read from its canonical file need not be rewritten on save,
            // unless it was resampled/converted on load (not 48 kHz s16) -- then rewrite once.
            let canonical = f == format!("track_{i}.wav") && wav_is_canonical(&dir.join(&f), ch, data.len());
            saved.push(if canonical { Arc::downgrade(&data) } else { std::sync::Weak::new() });
            tracks.push(Track { name: sanitize_name(&name), ch, data, gain, muted });
        }
        let mut edit = Edit { segs, tracks, ov, fx };
        edit.normalize();
        let tot = edit.total();
        for o in &mut edit.ov { o.end = o.end.min(tot); }
        edit.ov.retain(|o| o.end > o.start);
        let dirty = txtname != "project.txt"; // recovered autosave: unsaved until Save
        Ok(Project { src, edit, undo: vec![], redo: vec![], dirty, saved })
    }
}

const AUTOSAVE_TXT: &str = "project.autosave.txt";
const AUTOSAVE_PREFIX: &str = "autosave_";

fn clear_autosave(dir: &Path) {
    let _ = std::fs::remove_file(dir.join(AUTOSAVE_TXT));
    let _ = std::fs::remove_file(dir.join(format!("{AUTOSAVE_TXT}.tmp")));
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let n = e.file_name().to_string_lossy().to_string();
            if n.starts_with(AUTOSAVE_PREFIX) && (n.ends_with(".wav") || n.ends_with(".tmp")) { let _ = std::fs::remove_file(e.path()); }
        }
    }
}

/// v1.4.0: an autosave newer than project.txt means the app quit/crashed with unsaved edits.
fn has_newer_autosave(dir: &Path) -> bool {
    let m = |n: &str| std::fs::metadata(dir.join(n)).and_then(|m| m.modified()).ok();
    match (m(AUTOSAVE_TXT), m("project.txt")) { (Some(a), Some(p)) => a > p, (Some(_), None) => true, _ => false }
}

// =====================================================================================
// Audio FX: CL 1B-style opto compressor + BS.1770 loudness normalization
// =====================================================================================

/// Opto compressor parameters (modelled on the behaviour of a Tube-Tech CL 1B: optical
/// gain cell with program-dependent two-stage release, soft knee, tube make-up stage).
#[derive(Clone, Copy, Debug, PartialEq)]
struct OptoParams { threshold_db: f32, ratio: f32, attack_ms: f32, release_s: f32, warmth: f32, makeup_db: f32 }

const OPTO_PRESETS: [(&str, OptoParams); 5] = [
    ("Voice - gentle 2:1", OptoParams { threshold_db: -20.0, ratio: 2.0, attack_ms: 10.0, release_s: 0.40, warmth: 0.35, makeup_db: 4.0 }),
    ("Voice - leveling 4:1", OptoParams { threshold_db: -26.0, ratio: 4.0, attack_ms: 5.0, release_s: 0.60, warmth: 0.40, makeup_db: 7.0 }),
    ("Broadcast 6:1", OptoParams { threshold_db: -30.0, ratio: 6.0, attack_ms: 2.0, release_s: 0.30, warmth: 0.50, makeup_db: 10.0 }),
    ("Warm glue 1.5:1", OptoParams { threshold_db: -16.0, ratio: 1.5, attack_ms: 30.0, release_s: 1.00, warmth: 0.60, makeup_db: 2.0 }),
    ("Peak tamer 10:1", OptoParams { threshold_db: -10.0, ratio: 10.0, attack_ms: 1.0, release_s: 0.15, warmth: 0.20, makeup_db: 2.0 }),
];

const OPTO_KNEE_DB: f32 = 8.0; // opto cells have a very soft knee

impl OptoParams {
    /// Static gain reduction (dB, >= 0) for a detector level, soft knee.
    fn static_gr(&self, lvl_db: f32) -> f32 {
        let over = lvl_db - self.threshold_db;
        let k = OPTO_KNEE_DB;
        let slope = 1.0 - 1.0 / self.ratio.max(1.0);
        if 2.0 * over <= -k { 0.0 } else if 2.0 * over < k { slope * (over + k / 2.0).powi(2) / (2.0 * k) } else { slope * over }
    }
    fn sanitized(mut self) -> Self {
        let fix = |v: f32, lo: f32, hi: f32, d: f32| if v.is_finite() { v.clamp(lo, hi) } else { d };
        self.threshold_db = fix(self.threshold_db, -60.0, 0.0, -20.0);
        self.ratio = fix(self.ratio, 1.0, 20.0, 2.0);
        self.attack_ms = fix(self.attack_ms, 0.5, 200.0, 10.0);
        self.release_s = fix(self.release_s, 0.05, 5.0, 0.4);
        self.warmth = fix(self.warmth, 0.0, 1.0, 0.3);
        self.makeup_db = fix(self.makeup_db, -12.0, 24.0, 0.0);
        self
    }
}

#[derive(Default, Clone, Copy, Debug)]
struct FxStats { max_gr_db: f32, avg_gr_db: f32, in_peak: f32, out_peak: f32 }

/// Offline opto compression of interleaved 48 kHz PCM. O(n), one log + one exp per frame.
/// Detector: stereo-linked mean-square (2 ms) -> soft-knee curve -> opto smoothing where the
/// release slows down the longer/deeper the cell has been driven (T4 "memory"), giving the
/// characteristic fast-then-slow, pump-free release. Then a tube make-up stage: asymmetric
/// tanh (adds mostly 2nd harmonic, scaled by `warmth`, unity small-signal gain) + DC blocker.
fn opto_compress(x: &[i16], ch: usize, p: OptoParams) -> (Vec<i16>, FxStats) {
    // v1.4.0: the offline compressor is now a thin wrapper over the streaming `Opto` state
    // machine, so the exact same sound is available live while recording (VoiceChain).
    let ch = ch.max(1);
    let mut o = Opto::new(p, ch);
    let mut out = Vec::with_capacity(x.len());
    let mut fin = vec![0f32; ch];
    let mut fout = vec![0f32; ch];
    for fr in x.chunks_exact(ch) {
        for (d, &v) in fin.iter_mut().zip(fr) { *d = v as f32 / 32768.0; }
        o.frame(&fin, &mut fout);
        out.extend(fout.iter().map(|&v| f2i(v)));
    }
    (out, o.stats())
}

/// Streaming opto compressor state (one instance per signal; `ch` channels, stereo-linked).
/// Per frame: one log10 and one powf. Math identical to the v1.3 offline implementation.
struct Opto {
    p: OptoParams, a_det: f32, a_att: f32, a_mem: f32, makeup: f32, drive: f32, bias: f32, tb: f32, norm: f32, a_dc: f32,
    ms: f32, gr: f32, mem: f32, dc: Vec<f32>, st: FxStats, gr_sum: f64, gr_n: u64,
}

impl Opto {
    fn new(p: OptoParams, ch: usize) -> Self {
        let p = p.sanitized();
        let fs = AR as f32;
        // tube stage: drive and bias grow with warmth; normalised so small signals pass at unity
        let drive = 1.0 + 2.5 * p.warmth;
        let bias = 0.22 * p.warmth;
        let tb = bias.tanh();
        Opto {
            p, a_det: 1.0 - (-1.0 / (0.002 * fs)).exp(), a_att: 1.0 - (-1.0 / (p.attack_ms * 0.001 * fs)).exp(),
            a_mem: 1.0 / (2.5 * fs), // cell memory builds/decays over ~2.5 s
            makeup: 10f32.powf(p.makeup_db / 20.0), drive, bias, tb, norm: drive * (1.0 - tb * tb),
            a_dc: 1.0 - (-std::f32::consts::TAU * 8.0 / fs).exp(), // 8 Hz DC blocker
            ms: 0.0, gr: 0.0, mem: 0.0, dc: vec![0.0; ch.max(1)], st: FxStats::default(), gr_sum: 0.0, gr_n: 0,
        }
    }
    #[inline]
    fn frame(&mut self, fr: &[f32], out: &mut [f32]) {
        let fs = AR as f32;
        let p = self.p;
        let mut sq = 0f32;
        for &f in fr { sq = sq.max(f * f); self.st.in_peak = self.st.in_peak.max(f.abs()); }
        self.ms += (sq - self.ms) * self.a_det;
        // +3 dB: report the detector in "peak-equivalent" dB for sine-like material
        let lvl = 10.0 * (self.ms + 1e-12).log10() + 3.01;
        let target = p.static_gr(lvl);
        if target > self.gr { self.gr += (target - self.gr) * self.a_att; } else {
            let tau = (p.release_s * (0.25 + 0.75 * self.mem)).max(0.04); // program-dependent release
            self.gr += (target - self.gr) / (tau * fs);
        }
        self.mem += ((self.gr / 8.0).min(1.0) - self.mem) * self.a_mem;
        if self.gr > 0.5 { self.gr_sum += self.gr as f64; self.gr_n += 1; }
        self.st.max_gr_db = self.st.max_gr_db.max(self.gr);
        let g = 10f32.powf(-self.gr / 20.0) * self.makeup;
        for (c, (&v, o)) in fr.iter().zip(out.iter_mut()).enumerate() {
            let y = v * g;
            let t = if p.warmth > 0.0 { ((y * self.drive + self.bias).tanh() - self.tb) / self.norm } else { y };
            self.dc[c] += (t - self.dc[c]) * self.a_dc;
            *o = t - self.dc[c];
            self.st.out_peak = self.st.out_peak.max(o.abs());
        }
    }
    fn stats(&self) -> FxStats {
        FxStats { avg_gr_db: if self.gr_n > 0 { (self.gr_sum / self.gr_n as f64) as f32 } else { 0.0 }, ..self.st }
    }
}

/// ITU-R BS.1770-4 K-weighting biquad (48 kHz coefficients), transposed direct form II.
#[derive(Clone, Copy)]
struct Biquad { b: [f64; 3], a: [f64; 2], z: [f64; 2] }
impl Biquad {
    fn new(b: [f64; 3], a: [f64; 2]) -> Self { Biquad { b, a, z: [0.0; 2] } }
    #[inline] fn run(&mut self, x: f64) -> f64 {
        let y = self.b[0] * x + self.z[0];
        self.z[0] = self.b[1] * x - self.a[0] * y + self.z[1];
        self.z[1] = self.b[2] * x - self.a[1] * y;
        y
    }
}

/// Integrated loudness (LUFS) of interleaved PCM with BS.1770 gating (400 ms blocks, 75 %
/// overlap, -70 LUFS absolute and -10 LU relative gates). A mono track is measured as the
/// dual-mono signal it becomes in the stereo mix, so mono mic and stereo system tracks end up
/// equally loud when normalised to the same target. None = silent (no block passes the gate).
fn loudness_lufs(x: &[i16], ch: usize) -> Option<f64> {
    let ch = ch.max(1);
    let n = x.len() / ch;
    let (blk, hop) = (AR as usize * 4 / 10, AR as usize / 10);
    if n < blk { return None; }
    let mk = || (Biquad::new([1.53512485958697, -2.69169618940638, 1.19839281085285], [-1.69065929318241, 0.73248077421585]),
                 Biquad::new([1.0, -2.0, 1.0], [-1.99004745483398, 0.99007225036621]));
    let mut f: Vec<(Biquad, Biquad)> = (0..ch).map(|_| mk()).collect();
    // per-100 ms sums of K-weighted squares (channel-summed), blocks = 4 consecutive hops
    let mut hops: Vec<f64> = Vec::with_capacity(n / hop + 1);
    let mut acc = 0f64;
    for i in 0..n {
        for (c, (s1, s2)) in f.iter_mut().enumerate() {
            let v = s2.run(s1.run(x[i * ch + c] as f64 / 32768.0));
            acc += v * v;
        }
        if (i + 1) % hop == 0 { hops.push(acc); acc = 0.0; }
    }
    let dual = if ch == 1 { 2.0 } else { 1.0 };
    let blocks: Vec<f64> = hops.windows(4).map(|w| w.iter().sum::<f64>() / blk as f64 * dual).collect();
    let lk = |z: f64| -0.691 + 10.0 * z.max(1e-20).log10();
    let abs: Vec<f64> = blocks.into_iter().filter(|&z| lk(z) > -70.0).collect();
    if abs.is_empty() { return None; }
    let rel = lk(abs.iter().sum::<f64>() / abs.len() as f64) - 10.0;
    let g: Vec<f64> = abs.into_iter().filter(|&z| lk(z) > rel).collect();
    if g.is_empty() { return None; }
    Some(lk(g.iter().sum::<f64>() / g.len() as f64))
}

/// Apply a linear gain, then a 2 ms look-ahead / 80 ms release peak limiter so no sample
/// exceeds `ceiling` (linear). Sliding-window minimum keeps it O(n). Returns the deepest
/// limiting applied in dB.
fn gain_with_limiter(x: &[i16], ch: usize, gain: f32, ceiling: f32) -> (Vec<i16>, f32) {
    let ch = ch.max(1);
    let n = x.len() / ch;
    let need: Vec<f32> = x.chunks_exact(ch).map(|fr| {
        let pk = fr.iter().map(|&v| (v as f32 / 32768.0 * gain).abs()).fold(0f32, f32::max);
        if pk > ceiling { ceiling / pk } else { 1.0 }
    }).collect();
    let la = (AR as usize * 2 / 1000).max(1);
    // forward sliding min over [i, i+la] using a monotonic deque
    let mut tgt = vec![1f32; n];
    let mut dq: std::collections::VecDeque<usize> = std::collections::VecDeque::new();
    for j in 0..n + la {
        if j < n { while dq.back().map(|&b| need[b] >= need[j]).unwrap_or(false) { dq.pop_back(); } dq.push_back(j); }
        if j >= la {
            let i = j - la;
            while dq.front().map(|&f| f < i).unwrap_or(false) { dq.pop_front(); }
            if let Some(&f) = dq.front() { tgt[i] = need[f]; }
        }
    }
    let a_att = 1.0 - (-1.0 / (la as f32 / 3.0)).exp();
    let a_rel = 1.0 - (-1.0 / (0.080 * AR as f32)).exp();
    let mut env = 1f32;
    let mut deepest = 1f32;
    let mut out = Vec::with_capacity(x.len());
    for (i, fr) in x.chunks_exact(ch).enumerate() {
        env += (tgt[i] - env) * if tgt[i] < env { a_att } else { a_rel };
        let g = env.min(need[i]); // hard guarantee on the sample itself
        deepest = deepest.min(g);
        for &v in fr { out.push(f2i(v as f32 / 32768.0 * gain * g)); }
    }
    (out, (-20.0 * deepest.max(1e-6).log10()).max(0.0))
}

/// Smooth safety ceiling for the stereo mix: transparent below -1 dBFS, then a tanh knee
/// that approaches (never exceeds) full scale. Replaces hard clipping on overs.
#[inline]
fn soft_ceiling(x: f32) -> f32 {
    const K: f32 = 0.891; // -1 dBFS
    let a = x.abs();
    if a <= K { x } else { x.signum() * (K + (1.0 - K) * ((a - K) / (1.0 - K)).tanh()) }
}

// =====================================================================================
// v1.4.0 Voice chain: RNNoise noise reduction, gate, voice EQ, de-esser, opto compressor.
// One streaming implementation serves both live recording (mic + narration, processed as
// the audio arrives) and offline "Voice FX…" on any track. All at 48 kHz mono.
// =====================================================================================

/// RBJ cookbook biquad designs at the project rate (f64 coefficients, normalised a0 = 1).
fn bq_design(kind: u8, f: f64, q: f64, db: f64) -> Biquad {
    let fs = AR as f64;
    let w0 = std::f64::consts::TAU * (f / fs).clamp(1e-5, 0.49);
    let (cw, sw) = (w0.cos(), w0.sin());
    let al = sw / (2.0 * q.max(1e-3));
    let a = 10f64.powf(db / 40.0);
    let (b0, b1, b2, a0, a1, a2) = match kind {
        0 => ((1.0 + cw) / 2.0, -(1.0 + cw), (1.0 + cw) / 2.0, 1.0 + al, -2.0 * cw, 1.0 - al), // high-pass
        1 => (1.0 + al * a, -2.0 * cw, 1.0 - al * a, 1.0 + al / a, -2.0 * cw, 1.0 - al / a),     // peaking
        2 => { // low shelf (S = 1)
            let s2 = 2.0 * a.sqrt() * al;
            (a * ((a + 1.0) - (a - 1.0) * cw + s2), 2.0 * a * ((a - 1.0) - (a + 1.0) * cw), a * ((a + 1.0) - (a - 1.0) * cw - s2),
             (a + 1.0) + (a - 1.0) * cw + s2, -2.0 * ((a - 1.0) + (a + 1.0) * cw), (a + 1.0) + (a - 1.0) * cw - s2)
        }
        _ => { // high shelf (S = 1)
            let s2 = 2.0 * a.sqrt() * al;
            (a * ((a + 1.0) + (a - 1.0) * cw + s2), -2.0 * a * ((a - 1.0) + (a + 1.0) * cw), a * ((a + 1.0) + (a - 1.0) * cw - s2),
             (a + 1.0) - (a - 1.0) * cw + s2, 2.0 * ((a - 1.0) - (a + 1.0) * cw), (a + 1.0) - (a - 1.0) * cw - s2)
        }
    };
    Biquad::new([b0 / a0, b1 / a0, b2 / a0], [a1 / a0, a2 / a0])
}

const EQ_PRESETS: [&str; 4] = ["Off", "Voice clarity", "Warm broadcast", "Bright (dull mic fix)"];

/// EQ preset chains. Every preset starts with an 80 Hz-ish high-pass (rumble, desk thumps).
fn eq_chain(preset: u8) -> Vec<Biquad> {
    const S: f64 = std::f64::consts::FRAC_1_SQRT_2;
    match preset {
        1 => vec![bq_design(0, 80.0, S, 0.0), bq_design(1, 250.0, 1.0, -3.0), bq_design(1, 3200.0, 0.9, 3.0), bq_design(3, 10000.0, S, 2.0)],
        2 => vec![bq_design(0, 70.0, S, 0.0), bq_design(2, 150.0, S, 2.0), bq_design(1, 400.0, 1.0, -2.0), bq_design(1, 3000.0, 1.0, 2.0), bq_design(3, 9000.0, S, -1.0)],
        3 => vec![bq_design(0, 90.0, S, 0.0), bq_design(1, 200.0, 1.0, 1.5), bq_design(1, 4500.0, 1.0, 4.0), bq_design(3, 11000.0, S, 3.0)],
        _ => vec![],
    }
}

/// Voice chain settings. Stored in the user config (`voice_fx`) so live recording and the
/// Voice FX dialog share them.
#[derive(Clone, Copy, Debug, PartialEq)]
struct VoiceFx {
    nr: bool,        // RNNoise neural noise reduction
    gate: bool,      // downward expander after NR
    gate_db: f32,    // gate threshold, dBFS
    eq: u8,          // index into EQ_PRESETS
    deess: f32,      // 0 = off .. 1 = strong
    comp: u8,        // 0 = off, k = OPTO_PRESETS[k - 1]
}

impl Default for VoiceFx {
    fn default() -> Self { VoiceFx { nr: true, gate: true, gate_db: -50.0, eq: 1, deess: 0.5, comp: 1 } }
}

impl VoiceFx {
    fn active(&self) -> bool { self.nr || self.gate || self.eq != 0 || self.deess > 0.0 || self.comp != 0 }
    fn sanitized(mut self) -> Self {
        if !self.gate_db.is_finite() { self.gate_db = -50.0; }
        self.gate_db = self.gate_db.clamp(-80.0, -10.0);
        self.deess = if self.deess.is_finite() { self.deess.clamp(0.0, 1.0) } else { 0.0 };
        self.eq = self.eq.min(EQ_PRESETS.len() as u8 - 1);
        self.comp = self.comp.min(OPTO_PRESETS.len() as u8);
        self
    }
    fn to_line(self) -> String {
        format!("nr={} gate={} gate_db={} eq={} deess={} comp={}", self.nr as u8, self.gate as u8, self.gate_db, self.eq, self.deess, self.comp)
    }
    fn from_line(l: &str) -> Self {
        let mut v = VoiceFx::default();
        for kv in l.split_whitespace() {
            let Some((k, x)) = kv.split_once('=') else { continue };
            match k {
                "nr" => v.nr = x == "1", "gate" => v.gate = x == "1",
                "gate_db" => if let Ok(f) = x.parse() { v.gate_db = f },
                "eq" => if let Ok(f) = x.parse() { v.eq = f },
                "deess" => if let Ok(f) = x.parse() { v.deess = f },
                "comp" => if let Ok(f) = x.parse() { v.comp = f },
                _ => {}
            }
        }
        v.sanitized()
    }
    fn summary(&self) -> String {
        let mut p = vec![];
        if self.nr { p.push("NR".to_string()); }
        if self.gate { p.push(format!("gate {:.0} dB", self.gate_db)); }
        if self.eq != 0 { p.push(EQ_PRESETS[self.eq as usize].to_string()); }
        if self.deess > 0.0 { p.push(format!("de-ess {:.0}%", self.deess * 100.0)); }
        if self.comp != 0 { p.push(OPTO_PRESETS[self.comp as usize - 1].0.to_string()); }
        if p.is_empty() { "off".into() } else { p.join(", ") }
    }
}

fn load_voice_fx() -> VoiceFx {
    config_dir().and_then(|d| std::fs::read_to_string(d.join("voice_fx")).ok()).map(|l| VoiceFx::from_line(l.trim())).unwrap_or_default()
}

fn save_voice_fx(v: VoiceFx) -> Res<()> {
    let d = config_dir().ok_or("no config directory")?;
    std::fs::create_dir_all(&d).map_err(es)?;
    let tmp = d.join("voice_fx.tmp");
    std::fs::write(&tmp, v.to_line()).map_err(es)?;
    std::fs::rename(&tmp, d.join("voice_fx")).map_err(es)
}

/// Gate: peak envelope with hysteresis (6 dB) and 120 ms hold; gain glides between unity and
/// a -30 dB floor (an expander, not a hard mute, so breaths and room tone don't "pump").
struct Gate { thr: f32, env: f32, g: f32, hold: u32, open: bool, a_env_att: f32, a_env_rel: f32, a_open: f32, a_close: f32 }

impl Gate {
    fn new(thr_db: f32) -> Self {
        let fs = AR as f32;
        let k = |ms: f32| 1.0 - (-1.0 / (ms * 0.001 * fs)).exp();
        Gate { thr: 10f32.powf(thr_db / 20.0), env: 0.0, g: 0.0316, hold: 0, open: false,
               a_env_att: k(1.0), a_env_rel: k(80.0), a_open: k(2.0), a_close: k(150.0) }
    }
    #[inline]
    fn run(&mut self, x: f32) -> f32 {
        let a = x.abs();
        self.env += (a - self.env) * if a > self.env { self.a_env_att } else { self.a_env_rel };
        if self.env > self.thr { self.open = true; self.hold = AR * 12 / 100; }
        else if self.env < self.thr * 0.5 { if self.hold > 0 { self.hold -= 1; } else { self.open = false; } }
        let tgt = if self.open { 1.0 } else { 0.0316 }; // -30 dB floor
        self.g += (tgt - self.g) * if tgt > self.g { self.a_open } else { self.a_close };
        x * self.g
    }
}

/// De-esser (wideband mode). A 5 kHz high-pass sidechain detects sibilance; when the high
/// band is both above threshold and dominates the full-band level (an "s", not a bright vowel)
/// the signal is turned down for the few ms the sibilant lasts. Wideband gain avoids the
/// phase cancellation a naive x - hp + g*hp band split suffers from IIR filter phase shift.
struct DeEss { hp: Biquad, env: f32, full: f32, g: f32, thr: f32, max_gr: f32, a_att: f32, a_rel: f32, a_g: f32 }

impl DeEss {
    fn new(amount: f32) -> Self {
        let fs = AR as f32;
        let k = |ms: f32| 1.0 - (-1.0 / (ms * 0.001 * fs)).exp();
        DeEss { hp: bq_design(0, 5000.0, std::f64::consts::FRAC_1_SQRT_2, 0.0), env: 0.0, full: 0.0, g: 1.0,
                thr: 10f32.powf((-30.0 + 12.0 * (1.0 - amount)) / 20.0), max_gr: 4.0 + 10.0 * amount,
                a_att: k(0.5), a_rel: k(40.0), a_g: k(1.0) }
    }
    #[inline]
    fn run(&mut self, x: f32) -> f32 {
        let h = self.hp.run(x as f64) as f32;
        let (a, f) = (h.abs(), x.abs());
        self.env += (a - self.env) * if a > self.env { self.a_att } else { self.a_rel };
        self.full += (f - self.full) * if f > self.full { self.a_att } else { self.a_rel };
        let dominance = self.env / self.full.max(1e-9); // ~1 for sibilants, << 1 for vowels
        let over_db = 20.0 * (self.env / self.thr).max(1e-9).log10();
        let w = ((dominance - 0.35) / 0.3).clamp(0.0, 1.0);
        let gr_db = (over_db * 0.7).clamp(0.0, self.max_gr) * w;
        let tgt = 10f32.powf(-gr_db / 20.0);
        self.g += (tgt - self.g) * self.a_g;
        x * self.g
    }
}

/// Streaming mono voice processor. `push` may be called with any chunk size; output lags by
/// at most one RNNoise frame and `finish` flushes so the total output length equals the input
/// length exactly (the RNNoise algorithmic delay is compensated, so tracks stay sample-aligned).
struct VoiceChain {
    nr: Option<Box<nnnoiseless::DenoiseState<'static>>>,
    pre: Vec<Biquad>, // EQ (high-pass first) runs before NR: RNNoise likes rumble-free input
    gate: Option<Gate>, ds: Option<DeEss>, opto: Option<Opto>,
    inbuf: Vec<f32>, skip: usize, n_in: u64, n_out: u64,
    fin: [f32; nnnoiseless::FRAME_SIZE], fout: [f32; nnnoiseless::FRAME_SIZE],
}

/// RNNoise output delay in samples (measured: see selftest "voice chain delay").
const NR_DELAY: usize = nnnoiseless::FRAME_SIZE;

impl VoiceChain {
    fn new(fx: VoiceFx) -> Self {
        let fx = fx.sanitized();
        VoiceChain {
            nr: if fx.nr { Some(nnnoiseless::DenoiseState::new()) } else { None },
            pre: eq_chain(fx.eq),
            gate: fx.gate.then(|| Gate::new(fx.gate_db)),
            ds: (fx.deess > 0.0).then(|| DeEss::new(fx.deess)),
            opto: (fx.comp != 0).then(|| Opto::new(OPTO_PRESETS[fx.comp as usize - 1].1, 1)),
            inbuf: Vec::with_capacity(nnnoiseless::FRAME_SIZE * 2),
            skip: if fx.nr { NR_DELAY } else { 0 }, n_in: 0, n_out: 0,
            fin: [0.0; nnnoiseless::FRAME_SIZE], fout: [0.0; nnnoiseless::FRAME_SIZE],
        }
    }

    fn block(&mut self, out: &mut Vec<i16>) {
        const N: usize = nnnoiseless::FRAME_SIZE;
        for (i, &v) in self.inbuf[..N].iter().enumerate() {
            let mut y = v as f64;
            for b in &mut self.pre { y = b.run(y); }
            self.fin[i] = y as f32;
        }
        self.inbuf.drain(..N);
        match &mut self.nr {
            // RNNoise works on i16-scaled floats
            Some(nr) => {
                for x in &mut self.fin { *x *= 32768.0; }
                nr.process_frame(&mut self.fout, &self.fin);
                for x in &mut self.fout { *x /= 32768.0; }
            }
            None => self.fout = self.fin,
        }
        let mut o1 = [0f32; 1];
        for i in 0..N {
            let mut y = self.fout[i];
            if let Some(g) = &mut self.gate { y = g.run(y); }
            if let Some(d) = &mut self.ds { y = d.run(y); }
            if let Some(c) = &mut self.opto { c.frame(&[y], &mut o1); y = o1[0]; }
            if self.skip > 0 { self.skip -= 1; continue; }
            out.push(f2i(y));
            self.n_out += 1;
        }
    }

    fn push(&mut self, x: &[i16], out: &mut Vec<i16>) {
        self.n_in += x.len() as u64;
        self.inbuf.extend(x.iter().map(|&v| v as f32 / 32768.0));
        while self.inbuf.len() >= nnnoiseless::FRAME_SIZE { self.block(out); }
    }

    /// Flush: feed silence until everything that went in has come out, then trim to length.
    fn finish(&mut self, out: &mut Vec<i16>) {
        let mut guard = 0;
        while self.n_out < self.n_in && guard < 8 {
            let pad = nnnoiseless::FRAME_SIZE - self.inbuf.len() % nnnoiseless::FRAME_SIZE;
            self.inbuf.extend(std::iter::repeat(0.0).take(pad));
            while self.inbuf.len() >= nnnoiseless::FRAME_SIZE { self.block(out); }
            guard += 1;
        }
        let extra = self.n_out.saturating_sub(self.n_in) as usize;
        out.truncate(out.len() - extra.min(out.len()));
        self.n_out -= extra as u64;
    }
}

/// Offline convenience: process a whole mono track (exact length). Stereo tracks are
/// processed per channel with independent chains.
fn voice_process(x: &[i16], ch: usize, fx: VoiceFx, progress: Option<&AtomicU32>, cancel: Option<&AtomicBool>) -> Res<Vec<i16>> {
    let ch = ch.max(1);
    let n = x.len() / ch;
    let mut chans: Vec<Vec<i16>> = Vec::with_capacity(ch);
    for c in 0..ch {
        let mono: Vec<i16> = x.chunks_exact(ch).map(|f| f[c]).collect();
        let mut vc = VoiceChain::new(fx);
        let mut out = Vec::with_capacity(n + nnnoiseless::FRAME_SIZE);
        for (k, blk) in mono.chunks(AR as usize).enumerate() {
            if cancel.map(|c| c.load(Relaxed)).unwrap_or(false) { return Err("cancelled".into()); }
            vc.push(blk, &mut out);
            if let Some(p) = progress { p.store((((c * n + (k + 1) * AR as usize).min((c + 1) * n)) as u64 * 1000 / (n * ch).max(1) as u64) as u32, Relaxed); }
        }
        vc.finish(&mut out);
        out.resize(n, 0);
        chans.push(out);
    }
    if ch == 1 { return Ok(chans.pop().unwrap_or_default()); }
    let mut v = Vec::with_capacity(n * ch);
    for i in 0..n { for c in &chans { v.push(c[i]); } }
    Ok(v)
}

/// Auto-duck parameters: music/system tracks dip by `depth_db` while voice is present.
#[derive(Clone, Copy, Debug)]
struct DuckParams { depth_db: f32, thr_db: f32, attack_ms: f32, release_ms: f32, hold_ms: f32 }

impl Default for DuckParams {
    fn default() -> Self { DuckParams { depth_db: 12.0, thr_db: -40.0, attack_ms: 80.0, release_ms: 600.0, hold_ms: 300.0 } }
}

const DUCK_BLOCK: usize = 480; // 10 ms gain-envelope resolution

/// Duck gain (linear) per 10 ms block from the summed voice tracks. 50 ms look-ahead so the
/// music is already down when the first syllable lands. O(n).
fn duck_envelope(voices: &[MixTrack], frames: usize, p: DuckParams) -> Vec<f32> {
    let nb = frames.div_ceil(DUCK_BLOCK);
    let thr = 10f32.powf(p.thr_db / 20.0);
    let mut act = vec![false; nb];
    for (b, a) in act.iter_mut().enumerate() {
        let mut e = 0f32;
        for i in b * DUCK_BLOCK..((b + 1) * DUCK_BLOCK).min(frames) {
            let (l, r) = mix_frame(voices, i);
            e += 0.5 * (l * l + r * r);
        }
        *a = (e / DUCK_BLOCK as f32).sqrt() > thr;
    }
    let la = 5; // blocks of look-ahead
    let hold = (p.hold_ms / 10.0).round() as usize;
    let depth = 10f32.powf(-p.depth_db.clamp(0.0, 40.0) / 20.0);
    let k = |ms: f32| 1.0 - (-10.0 / ms.max(10.0)).exp();
    let (a_att, a_rel) = (k(p.attack_ms), k(p.release_ms));
    let mut env = Vec::with_capacity(nb);
    let (mut g, mut h) = (1f32, 0usize);
    for b in 0..nb {
        let on = act[b..(b + la + 1).min(nb)].iter().any(|&x| x);
        if on { h = hold; } else { h = h.saturating_sub(1); }
        let tgt = if on || h > 0 { depth } else { 1.0 };
        g += (tgt - g) * if tgt < g { a_att } else { a_rel };
        env.push(g);
    }
    env
}

/// Apply a duck envelope to interleaved PCM, interpolating gain between block centres.
fn apply_duck(x: &[i16], ch: usize, env: &[f32]) -> Vec<i16> {
    let ch = ch.max(1);
    let mut out = Vec::with_capacity(x.len());
    for (i, fr) in x.chunks_exact(ch).enumerate() {
        let pos = i as f32 / DUCK_BLOCK as f32 - 0.5;
        let b0 = (pos.floor().max(0.0) as usize).min(env.len().saturating_sub(1));
        let b1 = (b0 + 1).min(env.len().saturating_sub(1));
        let t = (pos - b0 as f32).clamp(0.0, 1.0);
        let g = if env.is_empty() { 1.0 } else { env[b0] + (env[b1] - env[b0]) * t };
        out.extend(fr.iter().map(|&v| f2i(v as f32 / 32768.0 * g)));
    }
    out
}

// =====================================================================================
// v1.4.0 NEW: overlays (annotations, zoom/pan) + effect renderer
// Overlays live in `Edit` (so undo/redo covers them) in *output* frame time and normalized
// source-frame coordinates (0..1). Rendering happens only for frames that carry an effect:
// every other frame is still exported by copying its JPEG bytes (lossless, near-instant).
// =====================================================================================

#[derive(Clone, Debug, PartialEq)]
enum OvKind { Text { text: String }, Arrow, Blur, Pixelate, Step { n: u32 }, Zoom { scale: f32, follow: bool } }

/// x/y/w/h: box in normalized source coordinates. Arrow: tail (x,y) -> head (x+w, y+h).
#[derive(Clone, Debug, PartialEq)]
struct Overlay { start: u32, end: u32, x: f32, y: f32, w: f32, h: f32, color: u32, kind: OvKind }

const MAX_OVERLAYS: usize = 10_000;
const MAX_OV_TEXT: usize = 2_000;

fn esc_line(s: &str) -> String { s.replace('\\', "\\\\").replace('\n', "\\n").replace('\r', "") }
fn unesc_line(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    let mut it = s.chars();
    while let Some(c) = it.next() {
        if c == '\\' { match it.next() { Some('n') => o.push('\n'), Some(x) => o.push(x), None => {} } } else { o.push(c); }
    }
    o
}

impl Overlay {
    fn active(&self, f: u32) -> bool { f >= self.start && f < self.end }
    fn name(&self) -> &'static str {
        match self.kind { OvKind::Text { .. } => "Text", OvKind::Arrow => "Arrow", OvKind::Blur => "Blur", OvKind::Pixelate => "Pixelate",
            OvKind::Step { .. } => "Step", OvKind::Zoom { .. } => "Zoom" }
    }
    fn to_line(&self) -> String {
        let (k, extra) = match &self.kind {
            OvKind::Text { text } => ("T", format!("|{}", esc_line(text))),
            OvKind::Arrow => ("A", String::new()),
            OvKind::Blur => ("B", String::new()),
            OvKind::Pixelate => ("P", String::new()),
            OvKind::Step { n } => ("S", format!(" {n}")),
            OvKind::Zoom { scale, follow } => ("Z", format!(" {scale} {}", *follow as u8)),
        };
        format!("ov={k} {} {} {} {} {} {} {:06x}{extra}\n", self.start, self.end, self.x, self.y, self.w, self.h, self.color & 0xff_ffff)
    }
    /// Strict parser: every number must be finite and in range (hostile project files).
    fn from_line(v: &str) -> Res<Overlay> {
        let (head, text) = match v.split_once('|') { Some((a, b)) => (a, Some(b)), None => (v, None) };
        let p: Vec<&str> = head.split_whitespace().collect();
        let bad = || format!("bad overlay: {v}");
        if p.len() < 8 { return Err(bad()); }
        let u = |i: usize| -> Res<u32> { p.get(i).ok_or_else(bad)?.parse::<u32>().map_err(|_| bad()) };
        let f = |i: usize, lo: f32, hi: f32| -> Res<f32> {
            let x: f32 = p.get(i).ok_or_else(bad)?.parse().map_err(|_| bad())?;
            if x.is_finite() && (lo..=hi).contains(&x) { Ok(x) } else { Err(bad()) }
        };
        let kind = match (p[0], p.len()) {
            ("T", 8) => {
                let t = unesc_line(text.ok_or_else(bad)?);
                if t.chars().count() > MAX_OV_TEXT { return Err(bad()); }
                OvKind::Text { text: t }
            }
            ("A", 8) => OvKind::Arrow,
            ("B", 8) => OvKind::Blur,
            ("P", 8) => OvKind::Pixelate,
            ("S", 9) => OvKind::Step { n: u(8)?.min(999) },
            ("Z", 10) => OvKind::Zoom { scale: f(8, 1.1, 6.0)?, follow: p[9] == "1" },
            _ => return Err(bad()),
        };
        let o = Overlay { start: u(1)?, end: u(2)?, x: f(3, -1.0, 2.0)?, y: f(4, -1.0, 2.0)?, w: f(5, -2.0, 2.0)?, h: f(6, -2.0, 2.0)?,
            color: u32::from_str_radix(p[7], 16).map_err(|_| bad())? & 0xff_ffff, kind };
        if o.end <= o.start { return Err(bad()); }
        Ok(o)
    }
    /// Normalized box with positive extent (arrows keep their direction in w/h).
    fn bounds(&self) -> (f32, f32, f32, f32) {
        let (x0, x1) = if self.w < 0.0 { (self.x + self.w, self.x) } else { (self.x, self.x + self.w) };
        let (y0, y1) = if self.h < 0.0 { (self.y + self.h, self.y) } else { (self.y, self.y + self.h) };
        (x0, y0, x1, y1)
    }
}

/// Whole-video effect settings (also part of `Edit`, so undoable and saved).
#[derive(Clone, Copy, Debug, PartialEq)]
struct FxSettings { cursor_hl: bool, click_rings: bool, cam_show: bool, cam_pos: u8, cam_size: f32, cam_shape: u8, cam_blur: bool, cam_mirror: bool }

impl Default for FxSettings {
    fn default() -> Self { FxSettings { cursor_hl: false, click_rings: true, cam_show: true, cam_pos: 0, cam_size: 0.22, cam_shape: 0, cam_blur: false, cam_mirror: true } }
}

const CAM_POSITIONS: [&str; 4] = ["Bottom right", "Bottom left", "Top right", "Top left"];
const CAM_SHAPES: [&str; 3] = ["Circle", "Rounded", "Square"];

impl FxSettings {
    fn to_line(self) -> String {
        format!("fx={} {} {} {} {} {} {} {}\n", self.cursor_hl as u8, self.click_rings as u8, self.cam_show as u8, self.cam_pos,
            self.cam_size, self.cam_shape, self.cam_blur as u8, self.cam_mirror as u8)
    }
    fn from_line(v: &str) -> FxSettings {
        let p: Vec<&str> = v.split_whitespace().collect();
        let d = FxSettings::default();
        let b = |i: usize, def: bool| p.get(i).map(|s| *s == "1").unwrap_or(def);
        let sz = p.get(4).and_then(|s| s.parse::<f32>().ok()).filter(|x| x.is_finite()).unwrap_or(d.cam_size);
        FxSettings {
            cursor_hl: b(0, d.cursor_hl), click_rings: b(1, d.click_rings), cam_show: b(2, d.cam_show),
            cam_pos: p.get(3).and_then(|s| s.parse::<u8>().ok()).unwrap_or(0).min(3), cam_size: sz.clamp(0.1, 0.45),
            cam_shape: p.get(5).and_then(|s| s.parse::<u8>().ok()).unwrap_or(0).min(2), cam_blur: b(6, d.cam_blur), cam_mirror: b(7, d.cam_mirror),
        }
    }
}

fn smoothstep(t: f32) -> f32 { let t = t.clamp(0.0, 1.0); t * t * (3.0 - 2.0 * t) }

/// Everything needed to render output frame `f`, snapshotted from the edit (cheap to share
/// with the preview decoder and the export workers via Arc).
struct RenderCtx {
    src: Arc<Source>, map: Vec<u32>, ov: Vec<Overlay>, fx: FxSettings,
    zoom: Vec<[f32; 3]>, // per output frame: scale, cx, cy (empty = no zoom anywhere)
    ring_n: u32,
}

static FONT: std::sync::OnceLock<Option<fontdue::Font>> = std::sync::OnceLock::new();
fn font() -> Option<&'static fontdue::Font> {
    FONT.get_or_init(|| fontdue::Font::from_bytes(&include_bytes!("assets/LiberationSans-Bold.ttf")[..], fontdue::FontSettings::default()).ok()).as_ref()
}

impl RenderCtx {
    /// None when the edit has no effect that could change any frame (export = pure copy).
    fn new(src: &Arc<Source>, edit: &Edit) -> Option<Arc<RenderCtx>> {
        let fx = edit.fx;
        let evfx = src.has_ev() && (fx.cursor_hl || fx.click_rings);
        let camfx = fx.cam_show && src.cam.is_some();
        if edit.ov.is_empty() && !evfx && !camfx { return None; }
        let mut map = Vec::with_capacity(edit.total() as usize);
        for s in &edit.segs { for k in 0..s.len() { map.push(s.src_at(k)); } }
        let mut c = RenderCtx { src: src.clone(), map, ov: edit.ov.clone(), fx, zoom: vec![], ring_n: (src.fps / 2).max(3) };
        c.build_zoom();
        Some(Arc::new(c))
    }

    fn ev_at(&self, f: u32) -> Ev { self.map.get(f as usize).map(|&s| self.src.ev(s)).unwrap_or_default() }
    /// A click counts once: on a held/repeated source frame it would otherwise re-trigger.
    fn click_at(&self, g: u32) -> bool {
        let e = self.ev_at(g);
        e.f & EV_CLICK != 0 && e.f & EV_VIS != 0 && (g == 0 || self.map.get(g as usize - 1) != self.map.get(g as usize))
    }

    fn build_zoom(&mut self) {
        let zs: Vec<&Overlay> = self.ov.iter().filter(|o| matches!(o.kind, OvKind::Zoom { .. })).collect();
        if zs.is_empty() { return; }
        let n = self.map.len();
        let mut z = vec![[1.0f32, 0.5, 0.5]; n];
        let fps = self.src.fps as f32;
        let (sw, sh) = (self.src.w.max(1) as f32, self.src.h.max(1) as f32);
        // prefix sums of visible cursor positions for the follow window average
        let mut px = vec![(0f64, 0f64, 0u32); n + 1];
        for f in 0..n {
            let e = self.ev_at(f as u32);
            let v = e.f & EV_VIS != 0;
            let p = px[f];
            px[f + 1] = if v { (p.0 + e.x as f64 / sw as f64, p.1 + e.y as f64 / sh as f64, p.2 + 1) } else { p };
        }
        let win = (0.6 * fps) as usize;
        for o in zs {
            let OvKind::Zoom { scale, follow } = o.kind else { continue };
            let (s, e) = (o.start as usize, (o.end as usize).min(n));
            if e <= s { continue; }
            let ramp = ((0.5 * fps) as usize).min((e - s) / 2).max(1) as f32;
            let (bx0, by0, bx1, by1) = o.bounds();
            let (rcx, rcy) = ((bx0 + bx1) / 2.0, (by0 + by1) / 2.0);
            let mut last = (rcx, rcy);
            for (f, zf) in z.iter_mut().enumerate().take(e).skip(s) {
                let t = smoothstep(((f - s + 1) as f32 / ramp).min((e - f) as f32 / ramp));
                let (mut cx, mut cy) = (rcx, rcy);
                if follow {
                    let (a, b) = (f.saturating_sub(win), (f + win + 1).min(n));
                    let (sx, sy, c) = (px[b].0 - px[a].0, px[b].1 - px[a].1, px[b].2 - px[a].2);
                    // cursor not visible in the window: hold the last known centre
                    if c > 0 { last = ((sx / c as f64) as f32, (sy / c as f64) as f32); }
                    (cx, cy) = last;
                }
                let sc = 1.0 + (scale - 1.0) * t;
                if sc > zf[0] {
                    let half = 0.5 / sc;
                    let cx = (0.5 + (cx - 0.5) * t).clamp(half, 1.0 - half);
                    let cy = (0.5 + (cy - 0.5) * t).clamp(half, 1.0 - half);
                    *zf = [sc, cx, cy];
                }
            }
        }
        self.zoom = z;
    }

    fn zoom_at(&self, f: u32) -> [f32; 3] { self.zoom.get(f as usize).copied().unwrap_or([1.0, 0.5, 0.5]) }

    /// Does output frame `f` differ from its source JPEG?
    fn needs(&self, f: u32) -> bool {
        if (f as usize) >= self.map.len() { return false; }
        if self.fx.cam_show && self.src.cam.is_some() { return true; }
        if self.zoom_at(f)[0] > 1.001 { return true; }
        if self.ov.iter().any(|o| o.active(f) && !matches!(o.kind, OvKind::Zoom { .. })) { return true; }
        if self.src.has_ev() {
            if self.fx.cursor_hl && self.ev_at(f).f & EV_VIS != 0 { return true; }
            if self.fx.click_rings && (0..self.ring_n).any(|k| f >= k && self.click_at(f - k)) { return true; }
        }
        false
    }

    /// Render output frame `f` (RGB, source size) from the source JPEG.
    #[allow(non_snake_case)]
    fn render(&self, f: u32, jpeg: &[u8]) -> Res<(Vec<u8>, usize, usize)> {
        let (mut rgb, w, h) = decode_jpeg_rgb(jpeg)?;
        {
            let mut im = Img { px: &mut rgb, w, h };
            let (W, H) = (w as f32, h as f32);
            let act: Vec<&Overlay> = self.ov.iter().filter(|o| o.active(f)).collect();
            // 1. redaction first, so nothing drawn later is blurred away
            for o in &act {
                let (x0, y0, x1, y1) = o.bounds();
                let r = ((x0 * W) as i32, (y0 * H) as i32, (x1 * W).ceil() as i32, (y1 * H).ceil() as i32);
                match o.kind { OvKind::Blur => im.blur(r), OvKind::Pixelate => im.pixelate(r), _ => {} }
            }
            // 2. cursor emphasis (under the annotations)
            if self.src.has_ev() {
                let base = (0.028 * H).max(12.0);
                if self.fx.cursor_hl {
                    let e = self.ev_at(f);
                    if e.f & EV_VIS != 0 { im.disc(e.x as f32, e.y as f32, base * 1.25, [255, 214, 10], 0.38); }
                }
                if self.fx.click_rings {
                    for k in 0..self.ring_n {
                        if f < k || !self.click_at(f - k) { continue; }
                        let e = self.ev_at(f - k);
                        let p = (k + 1) as f32 / self.ring_n as f32;
                        let col = if e.f & EV_RDOWN != 0 { [255, 120, 60] } else { [56, 156, 255] };
                        im.ring(e.x as f32, e.y as f32, base * (0.5 + 1.6 * p), (0.0045 * H).max(2.5), col, 0.95 * (1.0 - p));
                    }
                }
            }
            // 3. annotations in creation order
            for o in &act {
                let c = o.color;
                let col = [(c >> 16) as u8, (c >> 8) as u8, c as u8];
                match &o.kind {
                    OvKind::Arrow => {
                        let (ax, ay, bx, by) = (o.x * W, o.y * H, (o.x + o.w) * W, (o.y + o.h) * H);
                        let t = (0.0065 * H).max(3.0);
                        im.arrow(ax + 1.5, ay + 1.5, bx + 1.5, by + 1.5, t, [0, 0, 0], 0.35); // soft shadow
                        im.arrow(ax, ay, bx, by, t, col, 1.0);
                    }
                    OvKind::Text { text } => {
                        let (x0, y0, x1, y1) = o.bounds();
                        im.callout(x0 * W, y0 * H, (x1 - x0) * W, (y1 - y0) * H, text, col);
                    }
                    OvKind::Step { n } => {
                        let (x0, y0, x1, y1) = o.bounds();
                        let d = ((x1 - x0) * W).min((y1 - y0) * H).max(16.0);
                        let (cx, cy) = ((x0 + x1) / 2.0 * W, (y0 + y1) / 2.0 * H);
                        im.disc(cx + 1.5, cy + 1.5, d / 2.0, [0, 0, 0], 0.3);
                        im.disc(cx, cy, d / 2.0, col, 1.0);
                        im.ring(cx, cy, d / 2.0 - d * 0.04, d * 0.06, [255, 255, 255], 0.9);
                        let s = n.to_string();
                        let fpx = d * if s.len() > 1 { 0.46 } else { 0.56 };
                        let tw = text_width(&s, fpx);
                        im.text(cx - tw / 2.0, cy - fpx * 0.36, &s, fpx, contrast(col));
                    }
                    _ => {}
                }
            }
        }
        // 4. zoom / pan (output stays at source size)
        let z = self.zoom_at(f);
        if z[0] > 1.001 {
            let (cw, ch) = (w as f32 / z[0], h as f32 / z[0]);
            let (x0, y0) = ((z[1] * w as f32 - cw / 2.0).clamp(0.0, w as f32 - cw), (z[2] * h as f32 - ch / 2.0).clamp(0.0, h as f32 - ch));
            rgb = crop_scale(&rgb, w, h, x0, y0, cw, ch);
        }
        // 5. webcam picture-in-picture, composited in output space (never zoomed)
        if self.fx.cam_show {
            if let Some(cj) = self.map.get(f as usize).and_then(|&s| self.src.read_cam(s)) {
                if let Ok((cam, cw, chh)) = decode_jpeg_rgb(&cj) { composite_cam(&mut rgb, w, h, &cam, cw, chh, &self.fx); }
            }
        }
        Ok((rgb, w, h))
    }

    /// Output frame as JPEG bytes: rendered + re-encoded when needed, else the source bytes.
    fn frame_jpeg(&self, f: u32) -> Res<Vec<u8>> {
        let sf = *self.map.get(f as usize).ok_or("timeline corrupt")?;
        let j = self.src.read(sf)?;
        if !self.needs(f) { return Ok(j); }
        let (rgb, w, h) = self.render(f, &j)?;
        let mut out = Vec::with_capacity(w * h / 3);
        let mut enc = jpeg_encoder::Encoder::new(&mut out, 90);
        enc.set_sampling_factor(jpeg_encoder::SamplingFactor::F_1_1); // crisp coloured text/lines
        enc.encode(&rgb, w as u16, h as u16, jpeg_encoder::ColorType::Rgb).map_err(|e| format!("jpeg encode: {e}"))?;
        Ok(out)
    }
}

fn contrast(c: [u8; 3]) -> [u8; 3] {
    let l = 0.299 * c[0] as f32 + 0.587 * c[1] as f32 + 0.114 * c[2] as f32;
    if l > 150.0 { [16, 18, 22] } else { [255, 255, 255] }
}

fn text_width(s: &str, px: f32) -> f32 {
    let Some(fo) = font() else { return 0.0 };
    s.chars().map(|c| fo.metrics(c, px).advance_width).sum()
}

/// Bilinear crop+scale of an RGB image back to its own size (zoom/pan).
fn crop_scale(src: &[u8], w: usize, h: usize, x0: f32, y0: f32, cw: f32, ch: f32) -> Vec<u8> {
    let mut out = vec![0u8; w * h * 3];
    let (sx, sy) = (cw / w as f32, ch / h as f32);
    // precompute column taps once per frame (O(w) instead of O(w*h) float work)
    let cols: Vec<(usize, usize, f32)> = (0..w).map(|ox| {
        let x = (x0 + (ox as f32 + 0.5) * sx - 0.5).clamp(0.0, (w - 1) as f32);
        let i = x as usize;
        (i, (i + 1).min(w - 1), x - i as f32)
    }).collect();
    for oy in 0..h {
        let y = (y0 + (oy as f32 + 0.5) * sy - 0.5).clamp(0.0, (h - 1) as f32);
        let j = y as usize;
        let (j1, fy) = ((j + 1).min(h - 1), y - j as f32);
        let (r0, r1) = (&src[j * w * 3..(j + 1) * w * 3], &src[j1 * w * 3..(j1 + 1) * w * 3]);
        let o = &mut out[oy * w * 3..(oy + 1) * w * 3];
        for (ox, &(i, i1, fx)) in cols.iter().enumerate() {
            for c in 0..3 {
                let a = r0[i * 3 + c] as f32 + (r0[i1 * 3 + c] as f32 - r0[i * 3 + c] as f32) * fx;
                let b = r1[i * 3 + c] as f32 + (r1[i1 * 3 + c] as f32 - r1[i * 3 + c] as f32) * fx;
                o[ox * 3 + c] = (a + (b - a) * fy + 0.5) as u8;
            }
        }
    }
    out
}

fn sample(img: &[u8], w: usize, h: usize, x: f32, y: f32) -> [f32; 3] {
    let x = x.clamp(0.0, (w - 1) as f32);
    let y = y.clamp(0.0, (h - 1) as f32);
    let (i, j) = (x as usize, y as usize);
    let (i1, j1) = ((i + 1).min(w - 1), (j + 1).min(h - 1));
    let (fx, fy) = (x - i as f32, y - j as f32);
    let p = |a: usize, b: usize, c: usize| img[(b * w + a) * 3 + c] as f32;
    let mut o = [0f32; 3];
    for (c, v) in o.iter_mut().enumerate() {
        let t = p(i, j, c) + (p(i1, j, c) - p(i, j, c)) * fx;
        let b = p(i, j1, c) + (p(i1, j1, c) - p(i, j1, c)) * fx;
        *v = t + (b - t) * fy;
    }
    o
}

/// Webcam PiP: "cover" crop into the shape, optional mirror and portrait background blur
/// (a soft head-and-shoulders ellipse stays sharp; no ML segmentation), white border.
fn composite_cam(out: &mut [u8], w: usize, h: usize, cam: &[u8], cw: usize, ch: usize, fx: &FxSettings) {
    if cw < 2 || ch < 2 || w < 32 || h < 32 { return; }
    let d = (fx.cam_size * w as f32).clamp(48.0, 0.6 * w.min(h) as f32);
    let (pw, ph) = if fx.cam_shape == 1 { (d * 4.0 / 3.0, d) } else { (d, d) };
    let (pw, ph) = (pw.min(w as f32 - 8.0), ph.min(h as f32 - 8.0));
    let m = (0.025 * w as f32).max(6.0);
    let (left, top) = (fx.cam_pos == 1 || fx.cam_pos == 3, fx.cam_pos >= 2);
    let ox = if left { m } else { w as f32 - m - pw };
    let oy = if top { m } else { h as f32 - m - ph };
    let blurred = if fx.cam_blur {
        let mut b = cam.to_vec();
        let r = (cw.max(ch) / 28).max(3) as i32;
        let mut im = Img { px: &mut b, w: cw, h: ch };
        im.box_blur((0, 0, cw as i32, ch as i32), r, 3);
        Some(b)
    } else { None };
    let s = (pw / cw as f32).max(ph / ch as f32); // cover
    let rad = if fx.cam_shape == 1 { ph * 0.12 } else { 0.0 };
    let border = (d * 0.018).max(2.0);
    let (x0, y0, x1, y1) = (ox.floor().max(0.0) as usize, oy.floor().max(0.0) as usize, ((ox + pw).ceil() as usize).min(w), ((oy + ph).ceil() as usize).min(h));
    for y in y0..y1 {
        for x in x0..x1 {
            let (u, v) = (x as f32 + 0.5 - ox, y as f32 + 0.5 - oy);
            // signed distance to the shape edge (negative inside)
            let sd = match fx.cam_shape {
                0 => ((u - pw / 2.0).powi(2) + (v - ph / 2.0).powi(2)).sqrt() - pw.min(ph) / 2.0,
                _ => {
                    let (qx, qy) = ((u - pw / 2.0).abs() - (pw / 2.0 - rad), (v - ph / 2.0).abs() - (ph / 2.0 - rad));
                    (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt() + qx.max(qy).min(0.0) - rad
                }
            };
            let cover = (0.5 - sd).clamp(0.0, 1.0);
            if cover <= 0.0 { continue; }
            let uu = if fx.cam_mirror { pw - u } else { u };
            let (cx, cy) = (cw as f32 / 2.0 + (uu - pw / 2.0) / s - 0.5, ch as f32 / 2.0 + (v - ph / 2.0) / s - 0.5);
            let mut c = sample(cam, cw, ch, cx, cy);
            if let Some(b) = &blurred {
                let (nx, ny) = ((cx / cw as f32 - 0.5) / 0.30, (cy / ch as f32 - 0.44) / 0.50);
                let keep = smoothstep((1.15 - (nx * nx + ny * ny).sqrt()) / 0.3);
                let bc = sample(b, cw, ch, cx, cy);
                for k in 0..3 { c[k] = bc[k] + (c[k] - bc[k]) * keep; }
            }
            let edge = smoothstep((sd + border + 0.5) / 1.0); // 0 inside, 1 in the border band
            for v in c.iter_mut() { *v += (250.0 - *v) * edge * 0.92; }
            let i = (y * w + x) * 3;
            for k in 0..3 { out[i + k] = (out[i + k] as f32 + (c[k] - out[i + k] as f32) * cover).clamp(0.0, 255.0) as u8; }
        }
    }
}

/// Minimal anti-aliased RGB painter for the annotation renderer.
struct Img<'a> { px: &'a mut [u8], w: usize, h: usize }

impl Img<'_> {
    #[inline]
    fn blend(&mut self, x: usize, y: usize, c: [u8; 3], a: f32) {
        if a <= 0.0 { return; }
        let i = (y * self.w + x) * 3;
        let a = a.min(1.0);
        for (p, &ck) in self.px[i..i + 3].iter_mut().zip(c.iter()) { *p = (*p as f32 + (ck as f32 - *p as f32) * a + 0.5) as u8; }
    }
    fn clip(&self, x0: f32, y0: f32, x1: f32, y1: f32) -> (usize, usize, usize, usize) {
        (x0.floor().max(0.0) as usize, y0.floor().max(0.0) as usize, (x1.ceil().max(0.0) as usize).min(self.w), (y1.ceil().max(0.0) as usize).min(self.h))
    }
    /// Shade every pixel in a box by `cov(px, py) -> alpha`.
    fn shade(&mut self, b: (f32, f32, f32, f32), c: [u8; 3], a: f32, cov: impl Fn(f32, f32) -> f32) {
        let (x0, y0, x1, y1) = self.clip(b.0, b.1, b.2, b.3);
        for y in y0..y1 { for x in x0..x1 { let v = cov(x as f32 + 0.5, y as f32 + 0.5); if v > 0.0 { self.blend(x, y, c, v * a); } } }
    }
    fn disc(&mut self, cx: f32, cy: f32, r: f32, c: [u8; 3], a: f32) {
        self.shade((cx - r - 1.0, cy - r - 1.0, cx + r + 1.0, cy + r + 1.0), c, a, |x, y| (r + 0.5 - ((x - cx).powi(2) + (y - cy).powi(2)).sqrt()).clamp(0.0, 1.0));
    }
    fn ring(&mut self, cx: f32, cy: f32, r: f32, t: f32, c: [u8; 3], a: f32) {
        self.shade((cx - r - t, cy - r - t, cx + r + t, cy + r + t), c, a, |x, y| {
            let d = (((x - cx).powi(2) + (y - cy).powi(2)).sqrt() - r).abs();
            (t / 2.0 + 0.5 - d).clamp(0.0, 1.0)
        });
    }
    #[allow(clippy::too_many_arguments)]
    fn arrow(&mut self, ax: f32, ay: f32, bx: f32, by: f32, t: f32, c: [u8; 3], a: f32) {
        let (dx, dy) = (bx - ax, by - ay);
        let len = (dx * dx + dy * dy).sqrt();
        if len < 2.0 { return; }
        let (ux, uy) = (dx / len, dy / len);
        let hl = (t * 4.5).min(len * 0.6);
        let hw = t * 2.6;
        let (sx, sy) = (bx - ux * hl * 0.9, by - uy * hl * 0.9); // shaft stops inside the head
        // shaft: distance to segment
        self.shade((ax.min(sx) - t, ay.min(sy) - t, ax.max(sx) + t, ay.max(sy) + t), c, a, |x, y| {
            let l2 = (sx - ax).powi(2) + (sy - ay).powi(2);
            let k = if l2 > 0.0 { (((x - ax) * (sx - ax) + (y - ay) * (sy - ay)) / l2).clamp(0.0, 1.0) } else { 0.0 };
            let d = ((x - ax - k * (sx - ax)).powi(2) + (y - ay - k * (sy - ay)).powi(2)).sqrt();
            (t / 2.0 + 0.5 - d).clamp(0.0, 1.0)
        });
        // head: triangle tip=b, base corners at b - u*hl +- n*hw
        let (bx0, by0) = (bx - ux * hl, by - uy * hl);
        let p = [(bx, by), (bx0 - uy * hw, by0 + ux * hw), (bx0 + uy * hw, by0 - ux * hw)];
        let (mnx, mny) = (p.iter().map(|q| q.0).fold(f32::MAX, f32::min), p.iter().map(|q| q.1).fold(f32::MAX, f32::min));
        let (mxx, mxy) = (p.iter().map(|q| q.0).fold(f32::MIN, f32::max), p.iter().map(|q| q.1).fold(f32::MIN, f32::max));
        let area = (p[1].0 - p[0].0) * (p[2].1 - p[0].1) - (p[1].1 - p[0].1) * (p[2].0 - p[0].0);
        let sgn = if area < 0.0 { -1.0 } else { 1.0 };
        self.shade((mnx - 1.0, mny - 1.0, mxx + 1.0, mxy + 1.0), c, a, |x, y| {
            let mut m = f32::MAX;
            for i in 0..3 {
                let (q0, q1) = (p[i], p[(i + 1) % 3]);
                let (ex, ey) = (q1.0 - q0.0, q1.1 - q0.1);
                let el = (ex * ex + ey * ey).sqrt().max(1e-3);
                m = m.min(sgn * (ex * (y - q0.1) - ey * (x - q0.0)) / el);
            }
            (m + 0.5).clamp(0.0, 1.0)
        });
    }
    #[allow(clippy::too_many_arguments)]
    fn round_rect(&mut self, x: f32, y: f32, w: f32, h: f32, r: f32, c: [u8; 3], a: f32) {
        let r = r.min(w / 2.0).min(h / 2.0).max(0.0);
        let (cx, cy) = (x + w / 2.0, y + h / 2.0);
        self.shade((x - 1.0, y - 1.0, x + w + 1.0, y + h + 1.0), c, a, |px, py| {
            let (qx, qy) = ((px - cx).abs() - (w / 2.0 - r), (py - cy).abs() - (h / 2.0 - r));
            let sd = (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt() + qx.max(qy).min(0.0) - r;
            (0.5 - sd).clamp(0.0, 1.0)
        });
    }
    #[allow(non_snake_case)]
    fn text(&mut self, x: f32, baseline_top: f32, s: &str, px: f32, c: [u8; 3]) {
        let Some(fo) = font() else { return };
        if px < 4.0 { return; }
        let asc = fo.horizontal_line_metrics(px).map(|m| m.ascent).unwrap_or(px * 0.9);
        let base = baseline_top + asc;
        let mut pen = x;
        for ch in s.chars() {
            let (m, bmp) = fo.rasterize(ch, px);
            let gx = (pen + m.xmin as f32).round() as i32;
            let gy = (base - m.height as f32 - m.ymin as f32).round() as i32;
            for j in 0..m.height {
                for i in 0..m.width {
                    let (X, Y) = (gx + i as i32, gy + j as i32);
                    if X < 0 || Y < 0 || X as usize >= self.w || Y as usize >= self.h { continue; }
                    let cov = bmp[j * m.width + i] as f32 / 255.0;
                    self.blend(X as usize, Y as usize, c, cov);
                }
            }
            pen += m.advance_width;
        }
    }
    /// Text callout: filled rounded box, text auto-sized to fit (multi-line, left aligned).
    fn callout(&mut self, x: f32, y: f32, w: f32, h: f32, text: &str, c: [u8; 3]) {
        if w < 8.0 || h < 8.0 { return; }
        self.round_rect(x + 2.0, y + 2.0, w, h, h.min(w) * 0.12, [0, 0, 0], 0.28);
        self.round_rect(x, y, w, h, h.min(w) * 0.12, c, 0.94);
        let lines: Vec<&str> = text.split('\n').collect();
        let pad = (h * 0.12).clamp(3.0, 18.0);
        let mut px = ((h - 2.0 * pad) / lines.len().max(1) as f32 / 1.2).max(4.0);
        let widest = lines.iter().map(|l| text_width(l, px)).fold(0.0f32, f32::max);
        if widest > w - 2.0 * pad && widest > 0.0 { px *= (w - 2.0 * pad) / widest; }
        let lh = px * 1.2;
        let top = y + (h - lh * lines.len() as f32) / 2.0;
        let tc = contrast(c);
        for (i, l) in lines.iter().enumerate() { self.text(x + pad, top + i as f32 * lh + px * 0.1, l, px, tc); }
    }
    fn rect_i(&self, r: (i32, i32, i32, i32)) -> Option<(usize, usize, usize, usize)> {
        let (x0, y0) = (r.0.max(0) as usize, r.1.max(0) as usize);
        let (x1, y1) = ((r.2.max(0) as usize).min(self.w), (r.3.max(0) as usize).min(self.h));
        (x1 > x0 + 1 && y1 > y0 + 1).then_some((x0, y0, x1, y1))
    }
    /// Separable running-sum box blur, `passes` times (3 passes ~ Gaussian), O(pixels).
    fn box_blur(&mut self, r: (i32, i32, i32, i32), rad: i32, passes: u32) {
        let Some((x0, y0, x1, y1)) = self.rect_i(r) else { return };
        let (bw, bh) = (x1 - x0, y1 - y0);
        let rad = rad.max(1) as usize;
        let mut line: Vec<[u32; 3]> = Vec::new();
        let mut tmp: Vec<[u8; 3]> = Vec::new();
        for _ in 0..passes {
            for horiz in [true, false] {
                let (outer, inner) = if horiz { (bh, bw) } else { (bw, bh) };
                for o in 0..outer {
                    let at = |k: usize| if horiz { (y0 + o) * self.w + x0 + k } else { (y0 + k) * self.w + x0 + o };
                    tmp.clear();
                    tmp.extend((0..inner).map(|k| { let i = at(k) * 3; [self.px[i], self.px[i + 1], self.px[i + 2]] }));
                    line.clear();
                    let mut acc = [0u32; 3];
                    line.push(acc);
                    for p in &tmp { for c in 0..3 { acc[c] += p[c] as u32; } line.push(acc); }
                    for k in 0..inner {
                        let (a, b) = (k.saturating_sub(rad), (k + rad + 1).min(inner));
                        let n = (b - a) as u32;
                        let i = at(k) * 3;
                        for (p, (&lb, &la)) in self.px[i..i + 3].iter_mut().zip(line[b].iter().zip(line[a].iter())) { *p = ((lb - la + n / 2) / n) as u8; }
                    }
                }
            }
        }
    }
    fn blur(&mut self, r: (i32, i32, i32, i32)) {
        let m = (r.2 - r.0).min(r.3 - r.1);
        // strong by design: this is redaction, not a look
        self.box_blur(r, (m / 5).clamp(8, 64), 3);
    }
    fn pixelate(&mut self, r: (i32, i32, i32, i32)) {
        let Some((x0, y0, x1, y1)) = self.rect_i(r) else { return };
        let b = ((x1 - x0).min(y1 - y0) / 8).clamp(10, 64);
        let mut by = y0;
        while by < y1 {
            let mut bx = x0;
            let ey = (by + b).min(y1);
            while bx < x1 {
                let ex = (bx + b).min(x1);
                let mut s = [0u64; 3];
                for y in by..ey { for x in bx..ex { let i = (y * self.w + x) * 3; for (sc, &p) in s.iter_mut().zip(&self.px[i..i + 3]) { *sc += p as u64; } } }
                let n = ((ey - by) * (ex - bx)) as u64;
                let avg = [(s[0] / n) as u8, (s[1] / n) as u8, (s[2] / n) as u8];
                for y in by..ey { for x in bx..ex { let i = (y * self.w + x) * 3; self.px[i..i + 3].copy_from_slice(&avg); } }
                bx = ex;
            }
            by = ey;
        }
    }
}

/// Auto zoom: one smooth zoom-and-follow per burst of clicking/typing. Activity closer than
/// 2.5 s together is one burst; each burst zooms from 0.6 s before to 1.5 s after it (min
/// 2 s); bursts less than 1 s apart are merged so the camera doesn't bounce.
fn auto_zoom(src: &Source, edit: &Edit, scale: f32) -> Vec<Overlay> {
    if !src.has_ev() { return vec![]; }
    let fps = src.fps as f32;
    let mut map = Vec::with_capacity(edit.total() as usize);
    for s in &edit.segs { for k in 0..s.len() { map.push(s.src_at(k)); } }
    let n = map.len() as u32;
    let act: Vec<u32> = (0..n).filter(|&f| {
        let e = src.ev(map[f as usize]);
        e.f & (EV_CLICK | EV_KEY) != 0 && e.f & EV_VIS != 0 && (f == 0 || map[f as usize - 1] != map[f as usize])
    }).collect();
    let (gap, pre, post, minl, merge) = ((2.5 * fps) as u32, (0.6 * fps) as u32, (1.5 * fps) as u32, (2.0 * fps) as u32, fps as u32);
    let mut bursts: Vec<(u32, u32)> = vec![];
    for &f in &act {
        match bursts.last_mut() { Some(b) if f - b.1 < gap => b.1 = f, _ => bursts.push((f, f)) }
    }
    let mut zones: Vec<(u32, u32)> = vec![];
    for (a, b) in bursts {
        let (mut s, mut e) = (a.saturating_sub(pre), (b + post).min(n));
        if e - s < minl { e = (s + minl).min(n); if e - s < minl { s = e.saturating_sub(minl); } }
        match zones.last_mut() { Some(z) if s <= z.1 + merge => z.1 = z.1.max(e), _ => zones.push((s, e)) }
    }
    let half = 0.5 / scale;
    zones.into_iter().filter(|z| z.1 > z.0).map(|(s, e)| Overlay { start: s, end: e, x: 0.5 - half, y: 0.5 - half, w: 2.0 * half, h: 2.0 * half,
        color: 0x3a8dde, kind: OvKind::Zoom { scale, follow: true } }).collect()
}

// =====================================================================================
// Mixing + playback engine
// =====================================================================================

#[derive(Clone)]
struct MixTrack { data: Arc<Vec<i16>>, ch: u16, gain: f32 }

fn mix_tracks(edit: &Edit, exclude: Option<usize>) -> Vec<MixTrack> { mix_tracks_with(edit, exclude, None) }

/// Like `mix_tracks`, but track `over.0` plays `over.1` instead of its stored samples
/// (used to audition an FX result before it is applied; the override is always audible).
fn mix_tracks_with(edit: &Edit, exclude: Option<usize>, over: Option<&(usize, Arc<Vec<i16>>)>) -> Vec<MixTrack> {
    edit.tracks.iter().enumerate()
        .filter(|(i, t)| Some(*i) != exclude && (over.map(|o| o.0 == *i).unwrap_or(false) || (!t.muted && t.gain > 0.0)))
        .map(|(i, t)| match over {
            Some((oi, d)) if *oi == i => MixTrack { data: d.clone(), ch: t.ch, gain: t.gain.max(0.01) },
            _ => MixTrack { data: t.data.clone(), ch: t.ch, gain: t.gain },
        }).collect()
}

#[inline]
fn mix_frame(ts: &[MixTrack], i: usize) -> (f32, f32) {
    let (mut l, mut r) = (0f32, 0f32);
    for t in ts {
        if t.ch == 1 {
            if let Some(&s) = t.data.get(i) { let v = s as f32 * t.gain; l += v; r += v; }
        } else if let (Some(&a), Some(&b)) = (t.data.get(2 * i), t.data.get(2 * i + 1)) {
            l += a as f32 * t.gain; r += b as f32 * t.gain;
        }
    }
    (l / 32768.0, r / 32768.0)
}

/// Mix `n` frames from `start` into stereo i16 LE bytes.
fn mix_bytes(ts: &[MixTrack], start: usize, n: usize, out: &mut Vec<u8>) {
    out.clear();
    for i in start..start + n {
        let (l, r) = mix_frame(ts, i);
        out.extend_from_slice(&f2i(soft_ceiling(l)).to_le_bytes());
        out.extend_from_slice(&f2i(soft_ceiling(r)).to_le_bytes());
    }
}

const NO_SEEK: u64 = u64::MAX;

struct PlayShared { playing: AtomicBool, pos: AtomicU64, seek: AtomicU64, end: AtomicU64, mix: Mutex<Vec<MixTrack>>, meter: [AtomicU32; 2] }

impl PlayShared {
    fn new() -> Self { PlayShared { playing: AtomicBool::new(false), pos: AtomicU64::new(0), seek: AtomicU64::new(NO_SEEK), end: AtomicU64::new(0), mix: Mutex::new(vec![]), meter: [AtomicU32::new(0), AtomicU32::new(0)] } }
    fn seek_to(&self, p: u64) { self.pos.store(p, Relaxed); self.seek.store(p, Release); }
}

/// Preview playback buffer. cpal's PulseAudio backend with `BufferSize::Default` gets a
/// ~2 s server buffer, so preview audio lagged the picture by ~2 s (fatal for syncing
/// narration). A fixed 20 ms period (40 ms double buffer) keeps it tight; other backends'
/// defaults are already short. Remaining device latency is compensated in `build_out`.
fn playback_buffer() -> cpal::BufferSize {
    #[cfg(target_os = "linux")]
    if cpal::default_host().id() == cpal::HostId::PulseAudio { return cpal::BufferSize::Fixed(AR / 50); }
    cpal::BufferSize::Default
}

fn start_output(ps: Arc<PlayShared>) -> Res<cpal::Stream> {
    let host = cpal::default_host();
    let dev = host.default_output_device().ok_or("no audio output device")?;
    let want = cpal::StreamConfig { channels: 2, sample_rate: AR, buffer_size: playback_buffer() };
    let s = match build_out::<f32>(&dev, &want, ps.clone()) {
        Ok(s) => s,
        Err(_) => {
            let sc = dev.default_output_config().map_err(es)?;
            let cfg = sc.config();
            match sc.sample_format() {
                cpal::SampleFormat::F32 => build_out::<f32>(&dev, &cfg, ps)?,
                cpal::SampleFormat::I16 => build_out::<i16>(&dev, &cfg, ps)?,
                cpal::SampleFormat::I32 => build_out::<i32>(&dev, &cfg, ps)?,
                cpal::SampleFormat::U16 => build_out::<u16>(&dev, &cfg, ps)?,
                f => return Err(format!("unsupported output format {f:?}")),
            }
        }
    };
    s.play().map_err(es)?;
    Ok(s)
}

fn build_out<T>(dev: &cpal::Device, cfg: &cpal::StreamConfig, ps: Arc<PlayShared>) -> Res<cpal::Stream>
where T: SizedSample + FromSample<f32> + Send + 'static {
    let ch = (cfg.channels as usize).max(1);
    let step = AR as f64 / cfg.sample_rate.max(1) as f64;
    let mut frac = 0f64;
    let mut pos: u64 = 0;
    let mut start: u64 = 0;
    let cfg_rate = cfg.sample_rate.max(1) as f64;
    dev.build_output_stream::<T, _, _>(*cfg, move |data: &mut [T], info: &cpal::OutputCallbackInfo| {
        let sk = ps.seek.swap(NO_SEEK, Acquire);
        if sk != NO_SEEK { pos = sk; frac = 0.0; start = sk; } // `start`: never report before the seek point
        let silent = |d: &mut [T]| d.iter_mut().for_each(|x| *x = T::EQUILIBRIUM);
        if !ps.playing.load(Relaxed) { silent(data); return; }
        // try_lock: never block the real-time thread; a busy lock costs one silent buffer.
        let Ok(ts) = ps.mix.try_lock() else { silent(data); return };
        let end = ps.end.load(Relaxed);
        let (mut pk_l, mut pk_r) = (0f32, 0f32);
        for fr in data.chunks_mut(ch) {
            if pos >= end { fr.iter_mut().for_each(|x| *x = T::EQUILIBRIUM); ps.playing.store(false, Relaxed); continue; }
            let (l0, r0) = mix_frame(&ts, pos as usize);
            let (l, r) = if frac > 0.0 && step != 1.0 {
                let (l1, r1) = mix_frame(&ts, pos as usize + 1);
                let f = frac as f32;
                (l0 + (l1 - l0) * f, r0 + (r1 - r0) * f)
            } else { (l0, r0) };
            let (l, r) = (soft_ceiling(l), soft_ceiling(r));
            pk_l = pk_l.max(l.abs()); pk_r = pk_r.max(r.abs());
            if ch == 1 { fr[0] = T::from_sample((l + r) * 0.5); } else {
                fr[0] = T::from_sample(l); fr[1] = T::from_sample(r);
                for x in fr.iter_mut().skip(2) { *x = T::EQUILIBRIUM; }
            }
            frac += step;
            let adv = frac.floor();
            pos += adv as u64;
            frac -= adv;
        }
        // Publish the position being *heard* now, not the one just queued: back off by the
        // time until this buffer's last frame plays, so the picture matches the sound.
        let ts = info.timestamp();
        let lead = ts.playback.saturating_duration_since(ts.callback).as_secs_f64()
            + (data.len() / ch) as f64 / cfg_rate;
        let heard = pos.saturating_sub((lead.min(5.0) * AR as f64) as u64);
        // level meter: positive f32 bit patterns order like u32, so fetch_max is a float max
        ps.meter[0].fetch_max(pk_l.to_bits(), Relaxed);
        ps.meter[1].fetch_max(pk_r.to_bits(), Relaxed);
        ps.pos.store(heard.max(start.min(pos)), Relaxed);
    }, |e| stream_err("out", &e), None).map_err(es)
}

// =====================================================================================
// Matroska (MKV) muxer: V_MJPEG + A_PCM/INT/LIT, clusters of 1 s, cues
// =====================================================================================

mod mkv {
    pub fn id(id: u32) -> Vec<u8> {
        let b = id.to_be_bytes();
        let z = b.iter().position(|&x| x != 0).unwrap_or(3);
        b[z..].to_vec()
    }
    pub fn vsize(n: u64) -> Vec<u8> {
        let mut l = 1usize;
        while l < 8 && n >= (1u64 << (7 * l)) - 1 { l += 1; }
        let x = n | (1u64 << (7 * l));
        (0..l).rev().map(|i| (x >> (8 * i)) as u8).collect()
    }
    pub fn el(i: u32, payload: &[u8]) -> Vec<u8> {
        let mut v = id(i);
        v.extend(vsize(payload.len() as u64));
        v.extend_from_slice(payload);
        v
    }
    pub fn uint(i: u32, x: u64) -> Vec<u8> {
        let b = x.to_be_bytes();
        let z = b.iter().position(|&c| c != 0).unwrap_or(7);
        el(i, &b[z..])
    }
    pub fn uint8(i: u32, x: u64) -> Vec<u8> { el(i, &x.to_be_bytes()) } // fixed width (patchable)
    pub fn f64e(i: u32, x: f64) -> Vec<u8> { el(i, &x.to_be_bytes()) }
    pub fn s(i: u32, x: &str) -> Vec<u8> { el(i, x.as_bytes()) }
    pub fn cat(parts: &[Vec<u8>]) -> Vec<u8> { parts.concat() }
}

/// Background job (export, dead-air scan, compaction). Progress is 0..=1000.
struct Job { progress: Arc<AtomicU32>, cancel: Arc<AtomicBool>, handle: JoinHandle<Res<JobOut>>, label: String }

enum JobOut {
    Msg(String),
    /// Dead-air ranges plus the edit generation they were computed for.
    DeadAir { ranges: Vec<(u32, u32)>, gen: u64 },
    /// v1.3.1: `gen` = edit generation the compaction was computed for.
    Compact { src: Source, before: u64, after: u64, gen: u64 },
    /// v1.4.0: processed track samples (index, data, reset gain) for edit generation `gen`.
    Tracks { changes: Vec<(usize, Arc<Vec<i16>>, bool)>, msg: String, gen: u64 },
}

/// Dead-air scan parameters (output-frame units after conversion).
#[derive(Clone, Copy)]
struct DeadCfg { min_frames: u32, pad_frames: u32, /** full-scale 0..1 */ silence_peak: f32, px_diff: u8, still_frac: f64 }

impl Default for DeadCfg {
    // -45 dBFS peak counts as silence; >0.05 % of sampled pixels changing by >40/255
    // relative to the start of the still run counts as motion (cursor drift alone does not).
    fn default() -> Self { DeadCfg { min_frames: 60, pad_frames: 6, silence_peak: 0.0056, px_diff: 40, still_frac: 0.0005 } }
}

/// Find output-frame ranges where the audible mix is silent AND the picture is static.
/// Runs on a worker thread; reads JPEGs only when the index entry actually changed
/// (the recorder stores unchanged screens as repeated index entries, so idle stretches
/// cost nothing) and compares to the run's anchor frame on a 4-px grid (O(pixels/16)).
fn find_dead_air(src: &Source, edit: &Edit, cfg: DeadCfg, progress: &AtomicU32, cancel: &AtomicBool) -> Res<Vec<(u32, u32)>> {
    let total = edit.total();
    let spf = src.spf();
    let mix = mix_tracks(edit, None);
    let silent = |f: u32| -> bool {
        let b = f as usize * spf;
        (b..b + spf).all(|i| { let (l, r) = mix_frame(&mix, i); l.abs() < cfg.silence_peak && r.abs() < cfg.silence_peak })
    };
    let differs = |a: &[u8], b: &[u8], w: usize, h: usize| -> bool {
        let (mut n, mut changed) = (0u64, 0u64);
        let mut y = 0; while y < h { let mut x = 0; while x < w {
            let i = (y * w + x) * 3;
            let d = (0..3).map(|c| a[i + c].abs_diff(b[i + c])).max().unwrap_or(0);
            if d > cfg.px_diff { changed += 1; }
            n += 1; x += 4; } y += 4; }
        changed as f64 > n as f64 * cfg.still_frac
    };
    let mut out = Vec::new();
    let mut run_start: Option<u32> = None;
    let mut anchor: Option<(u64, Vec<u8>, usize, usize)> = None; // (offset, rgb, w, h)
    // flat iteration over segments avoids O(n*segs) src_frame lookups
    let frames: Box<dyn Iterator<Item = u32>> = Box::new(edit.segs.iter().flat_map(|s| (0..s.len()).map(move |k| s.src_at(k))));
    let close = |out: &mut Vec<(u32, u32)>, a: u32, b: u32| {
        // keep `pad` frames of context on each side so cuts never feel abrupt
        let (a2, b2) = (if a == 0 { 0 } else { a + cfg.pad_frames }, if b >= total { total } else { b.saturating_sub(cfg.pad_frames) });
        if b2 > a2 && b2 - a2 >= cfg.min_frames.max(1) { out.push((a2, b2)); }
    };
    // v1.3.1 FIX (clippy explicit_counter_loop): enumerate instead of a manual counter.
    for (f, sf) in frames.enumerate() {
        let f = f as u32;
        if f % 256 == 0 {
            if cancel.load(Relaxed) { return Err("scan cancelled".into()); }
            progress.store((f as u64 * 1000 / total.max(1) as u64) as u32, Relaxed);
        }
        // v1.3.1 PERF: test the (cheap) audio first. While anything is audible the frame can't
        // be dead, so skip the JPEG decode and drop the anchor; v1.3.0 decoded every changed
        // frame of narrated, screen-active footage only to discard the result.
        let quiet = silent(f);
        if !quiet { anchor = None; }
        let (off, _) = src.entry(sf).ok_or("missing frame")?;
        let still = quiet && match &anchor {
            Some((ao, ..)) if *ao == off => true,
            Some((_, argb, w, h)) => {
                let (rgb, w2, h2) = decode_jpeg_rgb(&src.read(sf)?)?;
                let s = w2 == *w && h2 == *h && !differs(argb, &rgb, w2, h2);
                if !s { anchor = Some((off, rgb, w2, h2)); }
                s
            }
            None => { let (rgb, w, h) = decode_jpeg_rgb(&src.read(sf)?)?; anchor = Some((off, rgb, w, h)); false }
        };
        let dead = still; // `still` already implies `quiet`
        match (dead, run_start) {
            (true, None) => run_start = Some(f),
            (false, Some(a)) => { close(&mut out, a, f); run_start = None; }
            _ => {}
        }
    }
    if let Some(a) = run_start { close(&mut out, a, total); }
    progress.store(1000, Relaxed);
    Ok(out)
}

/// Export the timeline as MKV. `span` maps this job's 0..100 % into a slice of the shared
/// progress bar (v1.3.1: MOV/MP4 run MKV + ffmpeg as two phases of one bar).
fn export_mkv(path: &Path, src: &Arc<Source>, edit: &Edit, progress: &AtomicU32, cancel: &AtomicBool, span: (u32, u32)) -> Res<()> {
    let tmp = path.with_extension("mkv.part");
    let r = export_mkv_inner(path, &tmp, src, edit, progress, cancel, span);
    // v1.3.1 FIX: v1.3.0 removed the .part file only on cancel; a write error (disk full,
    // unreadable frame) left a multi-GB orphan next to the target. Clean up on every error.
    if r.is_err() { let _ = std::fs::remove_file(&tmp); }
    r
}

fn export_mkv_inner(path: &Path, tmp: &Path, src: &Arc<Source>, edit: &Edit, progress: &AtomicU32, cancel: &AtomicBool, span: (u32, u32)) -> Res<()> {
    use mkv::*;
    let total = edit.total();
    if total == 0 { return Err("timeline is empty".into()); }
    let prog = |f: u32| span.0 + ((span.1 - span.0) as u64 * f as u64 / total as u64) as u32;
    let fps = src.fps;
    let spf = src.spf();
    let ts = mix_tracks(edit, None);
    let has_audio = !edit.tracks.is_empty();
    let mut w = BufWriter::with_capacity(8 << 20, File::create(tmp).map_err(|e| format!("{}: {e}", tmp.display()))?);
    let uid = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(1) | 1;

    let ebml = el(0x1A45DFA3, &cat(&[uint(0x4286, 1), uint(0x42F7, 1), uint(0x42F2, 4), uint(0x42F3, 8),
        s(0x4282, "matroska"), uint(0x4287, 4), uint(0x4285, 2)]));
    w.write_all(&ebml).map_err(es)?;
    let mut seg_hdr = id(0x18538067);
    let seg_size_pos = (ebml.len() + seg_hdr.len()) as u64;
    seg_hdr.extend_from_slice(&[0x01, 0, 0, 0, 0, 0, 0, 0]); // 8-byte size, patched at end
    w.write_all(&seg_hdr).map_err(es)?;
    let seg_start = seg_size_pos + 8;

    let dur_ms = total as f64 * 1000.0 / fps as f64;
    let info = el(0x1549A966, &cat(&[uint(0x2AD7B1, 1_000_000), f64e(0x4489, dur_ms),
        s(0x4D80, FILE_TAG), s(0x5741, FILE_TAG)]));
    let vtrack = el(0xAE, &cat(&[uint(0xD7, 1), uint(0x73C5, uid), uint(0x83, 1), uint(0x9C, 0),
        uint(0x23E383, 1_000_000_000 / fps as u64), s(0x86, "V_MJPEG"),
        el(0xE0, &cat(&[uint(0xB0, src.w as u64), uint(0xBA, src.h as u64)]))]));
    let mut tracks_payload = vtrack;
    if has_audio {
        tracks_payload.extend(el(0xAE, &cat(&[uint(0xD7, 2), uint(0x73C5, uid ^ 0x5A5A), uint(0x83, 2), uint(0x9C, 0),
            s(0x86, "A_PCM/INT/LIT"), s(0x22B59C, "und"),
            el(0xE1, &cat(&[f64e(0xB5, AR as f64), uint(0x9F, 2), uint(0x6264, 16)]))])));
    }
    let tracks = el(0x1654AE6B, &tracks_payload);
    // SeekHead with fixed-width positions so the Cues position can be patched in place.
    let mk_seekhead = |info_p: u64, tracks_p: u64, cues_p: u64| el(0x114D9B74, &cat(&[
        el(0x4DBB, &cat(&[el(0x53AB, &id(0x1549A966)), uint8(0x53AC, info_p)])),
        el(0x4DBB, &cat(&[el(0x53AB, &id(0x1654AE6B)), uint8(0x53AC, tracks_p)])),
        el(0x4DBB, &cat(&[el(0x53AB, &id(0x1C53BB6B)), uint8(0x53AC, cues_p)])),
    ]));
    let sh_len = mk_seekhead(0, 0, 0).len() as u64;
    let info_p = sh_len;
    let tracks_p = info_p + info.len() as u64;
    w.write_all(&mk_seekhead(info_p, tracks_p, 0)).map_err(es)?;
    w.write_all(&info).map_err(es)?;
    w.write_all(&tracks).map_err(es)?;
    let mut pos = tracks_p + tracks.len() as u64; // relative to seg_start

    let mut cues = Vec::new();
    let mut cache: Option<(u32, Vec<u8>)> = None;
    // v1.4.0: frames carrying an effect are rendered + re-encoded (in parallel, one cluster
    // at a time); all other frames are still copied byte-for-byte from the source.
    let rctx = RenderCtx::new(src, edit);
    let nthreads = thread::available_parallelism().map(|n| n.get()).unwrap_or(2).clamp(1, 16);
    let mut rendered: HashMap<u32, Vec<u8>> = HashMap::new();
    let mut abuf = Vec::with_capacity(spf * 4);
    let mut f = 0u32;
    while f < total {
        if cancel.load(Relaxed) { return Err("export cancelled".into()); } // caller removes the .part
        let c_ms = f as u64 * 1000 / fps as u64;
        let c_end = (f + fps).min(total);
        let mut body = uint(0xE7, c_ms);
        rendered.clear();
        if let Some(rc) = &rctx {
            let todo: Vec<u32> = (f..c_end).filter(|&x| rc.needs(x)).collect();
            if !todo.is_empty() {
                let chunk = todo.len().div_ceil(nthreads);
                let res: Vec<Res<Rendered>> = thread::scope(|sc| {
                    let hs: Vec<_> = todo.chunks(chunk).map(|part| sc.spawn(move || {
                        part.iter().map(|&x| rc.frame_jpeg(x).map(|j| (x, j))).collect::<Res<Vec<_>>>()
                    })).collect();
                    hs.into_iter().map(|h| h.join().unwrap_or_else(|_| Err("render thread panicked".into()))).collect()
                });
                for r in res { rendered.extend(r?); }
            }
        }
        for fr in f..c_end {
            let jpg: &Vec<u8> = match rendered.get(&fr) {
                Some(j) => j,
                None => {
                    let sf = edit.src_frame(fr).ok_or("timeline corrupt")?;
                    if cache.as_ref().map(|c| c.0 != sf).unwrap_or(true) { cache = Some((sf, src.read(sf)?)); }
                    &cache.as_ref().unwrap().1
                }
            };
            let rel = (fr as u64 * 1000 / fps as u64 - c_ms) as i16;
            let mut blk = vec![0x81];
            blk.extend_from_slice(&rel.to_be_bytes());
            blk.push(0x80);
            blk.extend_from_slice(jpg);
            body.extend(el(0xA3, &blk));
            if has_audio {
                mix_bytes(&ts, fr as usize * spf, spf, &mut abuf);
                let mut ab = vec![0x82];
                ab.extend_from_slice(&rel.to_be_bytes());
                ab.push(0x80);
                ab.extend_from_slice(&abuf);
                body.extend(el(0xA3, &ab));
            }
        }
        let cl = el(0x1F43B675, &body);
        w.write_all(&cl).map_err(es)?;
        cues.extend(el(0xBB, &cat(&[uint(0xB3, c_ms), el(0xB7, &cat(&[uint(0xF7, 1), uint(0xF1, pos)]))])));
        pos += cl.len() as u64;
        f = c_end;
        progress.store(prog(f), Relaxed);
    }
    let cues_p = pos;
    let cues_el = el(0x1C53BB6B, &cues);
    w.write_all(&cues_el).map_err(es)?;
    pos += cues_el.len() as u64;
    let mut file = w.into_inner().map_err(|e| es(e.error()))?;
    // Patch segment size and Cues position.
    file.seek(SeekFrom::Start(seg_size_pos)).map_err(es)?;
    let mut sz = [0u8; 8];
    sz[0] = 0x01;
    sz[1..].copy_from_slice(&pos.to_be_bytes()[1..]);
    file.write_all(&sz).map_err(es)?;
    file.seek(SeekFrom::Start(seg_start)).map_err(es)?;
    file.write_all(&mk_seekhead(info_p, tracks_p, cues_p)).map_err(es)?;
    file.sync_all().map_err(es)?;
    drop(file);
    std::fs::rename(tmp, path).map_err(es)?;
    Ok(())
}

/// v1.3.1 PERF/FIX: streams the mix straight to disk in 1 s blocks. v1.3.0 built the whole
/// stereo mix in RAM first (a 2 h project = ~1.4 GB), had no progress and ignored Cancel.
fn export_wav(path: &Path, src: &Source, edit: &Edit, progress: &AtomicU32, cancel: &AtomicBool) -> Res<()> {
    let n = edit.total() as usize * src.spf();
    if n == 0 { return Err("timeline is empty".into()); }
    let ts = mix_tracks(edit, None);
    write_wav_with(path, 2, AR, n as u64 * 2, |w| {
        let mut buf = Vec::with_capacity(AR as usize * 4);
        let mut i = 0usize;
        while i < n {
            if cancel.load(Relaxed) { return Err("export cancelled".into()); }
            let k = (AR as usize).min(n - i);
            mix_bytes(&ts, i, k, &mut buf);
            w.write_all(&buf).map_err(es)?;
            i += k;
            progress.store((i as u64 * 1000 / n as u64) as u32, Relaxed);
        }
        Ok(())
    })
}

// ---- ffmpeg discovery -----------------------------------------------------------------
// v1.1.0 only tried `ffmpeg` on PATH, so on Windows (where winget/scoop/manual installs often
// do not touch the PATH of an already-running Explorer) MOV/MP4 were permanently greyed out.
// Discovery order: $SCREENREEL_FFMPEG, saved user choice, next to our exe, PATH, then the
// well-known Windows package-manager shim folders. Candidates are checked by file existence
// first (cheap) and only the first hit is executed (`-version`) to confirm it works.

fn ffmpeg_exe_name() -> &'static str { if cfg!(windows) { "ffmpeg.exe" } else { "ffmpeg" } }

fn config_dir() -> Option<PathBuf> {
    #[cfg(windows)] { std::env::var_os("APPDATA").map(|a| PathBuf::from(a).join(APP_NAME)) }
    #[cfg(not(windows))] {
        std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
            .map(|d| d.join("screenreel"))
    }
}

fn saved_ffmpeg() -> Option<PathBuf> {
    let t = std::fs::read_to_string(config_dir()?.join("ffmpeg_path")).ok()?;
    let p = PathBuf::from(t.trim());
    if p.as_os_str().is_empty() || t.len() > 4096 { None } else { Some(p) }
}

fn save_ffmpeg_choice(p: &Path) -> Res<()> {
    let d = config_dir().ok_or("no config directory")?;
    std::fs::create_dir_all(&d).map_err(es)?;
    let tmp = d.join("ffmpeg_path.tmp");
    std::fs::write(&tmp, p.to_string_lossy().as_bytes()).map_err(es)?;
    std::fs::rename(&tmp, d.join("ffmpeg_path")).map_err(es)
}

fn ffmpeg_cmd(exe: &Path) -> std::process::Command {
    let mut c = std::process::Command::new(exe);
    #[cfg(windows)] {
        use std::os::windows::process::CommandExt;
        c.creation_flags(0x0800_0000); // CREATE_NO_WINDOW: no console flash from the GUI app
    }
    c.stdin(std::process::Stdio::null());
    c
}

/// True if `exe` runs and prints an ffmpeg banner (bounded: ffmpeg -version exits at once).
fn ffmpeg_works(exe: &Path) -> bool {
    ffmpeg_cmd(exe).arg("-version").stderr(std::process::Stdio::null()).output()
        .map(|o| o.status.success() && o.stdout.starts_with(b"ffmpeg version")).unwrap_or(false)
}

fn ffmpeg_candidates() -> Vec<PathBuf> {
    let name = ffmpeg_exe_name();
    let mut v: Vec<PathBuf> = Vec::new();
    if let Some(e) = std::env::var_os("SCREENREEL_FFMPEG") { v.push(PathBuf::from(e)); }
    if let Some(p) = saved_ffmpeg() { v.push(p); }
    if let Some(d) = std::env::current_exe().ok().and_then(|e| e.parent().map(Path::to_path_buf)) { v.push(d.join(name)); }
    if let Some(path) = std::env::var_os("PATH") { for d in std::env::split_paths(&path) { v.push(d.join(name)); } }
    #[cfg(windows)] {
        let env = |k: &str| std::env::var_os(k).map(PathBuf::from);
        if let Some(l) = env("LOCALAPPDATA") { v.push(l.join(r"Microsoft\WinGet\Links").join(name)); }
        if let Some(u) = env("USERPROFILE") { v.push(u.join(r"scoop\shims").join(name)); }
        if let Some(pd) = env("ProgramData") { v.push(pd.join(r"chocolatey\bin").join(name)); }
        for pf in ["ProgramFiles", "ProgramFiles(x86)"] { if let Some(p) = env(pf) { v.push(p.join(r"ffmpeg\bin").join(name)); } }
        v.push(PathBuf::from(r"C:\ffmpeg\bin").join(name));
    }
    #[cfg(not(windows))] { for d in ["/usr/bin", "/usr/local/bin", "/opt/homebrew/bin", "/snap/bin"] { v.push(Path::new(d).join(name)); } }
    v
}

fn find_ffmpeg() -> Option<PathBuf> {
    let mut tried = std::collections::HashSet::new();
    ffmpeg_candidates().into_iter()
        .filter(|p| p.is_file() && tried.insert(p.clone()))
        .find(|p| ffmpeg_works(p))
}

/// Run ffmpeg on an exported MKV. Args are passed as argv (no shell → no injection).
/// v1.3.1 FIX/PERF: v1.3.0 blocked in `Command::output()`, so Cancel did nothing during the
/// (long) H.264 encode and the bar sat at 100 %. Now the child is polled every 50 ms, killed
/// on cancel, and `-progress pipe:1` drives the bar over `span`. stdout and stderr are drained
/// on helper threads (no pipe-buffer deadlock); only the stderr tail is kept for the message.
// ---- v1.4.0: hardware H.264 for MP4 export ----------------------------------------
// Order = preference. An encoder being *listed* by `ffmpeg -encoders` only means ffmpeg was
// built with it; the GPU/driver may still be missing. Each candidate is therefore probed once
// with a tiny synthetic encode (10 s timeout, child killed on expiry) and the first that works
// is cached for the session. libx264 is always the final fallback.
// SCREENREEL_ENCODER=<name> forces one (e.g. libx264 to opt out of GPU encoding).
const HW_ENCODERS: &[&str] = &["h264_nvenc", "h264_qsv", "h264_amf", "h264_vaapi"];
const VAAPI_DEV: &str = "/dev/dri/renderD128";

/// (pre-input args, output video args) for an H.264 encoder at roughly CRF-18 quality.
fn h264_args(enc: &str) -> (Vec<&'static str>, Vec<&'static str>) {
    const SCALE: &str = "scale=trunc(iw/2)*2:trunc(ih/2)*2";
    match enc {
        "h264_nvenc" => (vec![], vec!["-c:v", "h264_nvenc", "-preset", "p4", "-rc", "vbr", "-cq", "19", "-b:v", "0", "-pix_fmt", "yuv420p", "-vf", SCALE]),
        "h264_qsv" => (vec![], vec!["-c:v", "h264_qsv", "-preset", "veryfast", "-global_quality", "20", "-pix_fmt", "nv12", "-vf", SCALE]),
        "h264_amf" => (vec![], vec!["-c:v", "h264_amf", "-quality", "balanced", "-rc", "cqp", "-qp_i", "18", "-qp_p", "20", "-pix_fmt", "nv12", "-vf", SCALE]),
        "h264_vaapi" => (vec!["-vaapi_device", VAAPI_DEV],
                         vec!["-c:v", "h264_vaapi", "-qp", "20", "-vf", "scale=trunc(iw/2)*2:trunc(ih/2)*2,format=nv12,hwupload"]),
        _ => (vec![], vec!["-c:v", "libx264", "-preset", "veryfast", "-crf", "18", "-pix_fmt", "yuv420p", "-vf", SCALE]),
    }
}

/// Run a short ffmpeg command with a hard deadline; true on clean exit.
fn ffmpeg_ok_within(c: &mut std::process::Command, limit: Duration) -> bool {
    let Ok(mut ch) = c.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null()).spawn() else { return false };
    let t = Instant::now();
    loop {
        match ch.try_wait() {
            Ok(Some(st)) => return st.success(),
            Ok(None) if t.elapsed() < limit => thread::sleep(Duration::from_millis(20)),
            _ => { let _ = ch.kill(); let _ = ch.wait(); return false; }
        }
    }
}

/// Pick the H.264 encoder for MP4 export (probed once per process).
fn pick_h264(exe: &Path) -> String {
    static PICK: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    PICK.get_or_init(|| {
        if let Ok(e) = std::env::var("SCREENREEL_ENCODER") {
            let e = e.trim();
            if e == "libx264" || HW_ENCODERS.contains(&e) { return e.to_string(); }
        }
        let listed = ffmpeg_cmd(exe).args(["-hide_banner", "-encoders"]).stdin(std::process::Stdio::null()).output()
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).unwrap_or_default();
        for &enc in HW_ENCODERS {
            if !listed.split_whitespace().any(|w| w == enc) { continue; }
            if enc == "h264_vaapi" && (!cfg!(target_os = "linux") || !Path::new(VAAPI_DEV).exists()) { continue; }
            if enc == "h264_amf" && !cfg!(windows) { continue; }
            let (pre, v) = h264_args(enc);
            let mut c = ffmpeg_cmd(exe);
            c.args(["-hide_banner", "-loglevel", "error"]).args(&pre)
                .args(["-f", "lavfi", "-i", "testsrc2=s=320x240:r=30:d=0.3"]).args(&v).args(["-f", "null", "-"]);
            if ffmpeg_ok_within(&mut c, Duration::from_secs(10)) { return enc.to_string(); }
        }
        "libx264".to_string()
    }).clone()
}

/// Convert the MKV to MOV (remux) or MP4 (H.264 re-encode). Returns a short note on the
/// video encoder used ("" for a remux). A hardware encoder that fails mid-run is retried once
/// with libx264 so a flaky driver never costs the user the export.
#[allow(clippy::too_many_arguments)]
fn ffmpeg_convert(exe: &Path, input: &Path, output: &Path, lossless_remux: bool, dur_us: u64,
                  progress: &AtomicU32, cancel: &AtomicBool, span: (u32, u32)) -> Res<String> {
    if lossless_remux { return ffmpeg_run(exe, input, output, None, dur_us, progress, cancel, span).map(|_| String::new()); }
    let enc = pick_h264(exe);
    match ffmpeg_run(exe, input, output, Some(&enc), dur_us, progress, cancel, span) {
        Ok(()) => Ok(enc),
        Err(e) if enc != "libx264" && !cancel.load(Relaxed) => {
            ffmpeg_run(exe, input, output, Some("libx264"), dur_us, progress, cancel, span)
                .map(|_| format!("libx264 ({enc} failed: {e})"))
        }
        Err(e) => Err(e),
    }
}

#[allow(clippy::too_many_arguments)]
fn ffmpeg_run(exe: &Path, input: &Path, output: &Path, h264: Option<&str>, dur_us: u64,
              progress: &AtomicU32, cancel: &AtomicBool, span: (u32, u32)) -> Res<()> {
    use std::io::BufRead;
    let mut c = ffmpeg_cmd(exe);
    let (pre, v) = h264.map(h264_args).unwrap_or_default();
    c.args(["-hide_banner", "-loglevel", "error", "-nostats", "-progress", "pipe:1", "-y"]).args(&pre).arg("-i").arg(input);
    if h264.is_none() {
        c.args(["-c", "copy", "-f", "mov"]);
    } else {
        c.args(&v).args(["-c:a", "aac", "-b:a", "192k", "-movflags", "+faststart", "-f", "mp4"]);
    }
    c.arg(output).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped());
    let mut child = c.spawn().map_err(|e| format!("ffmpeg: {e}"))?;
    let done_us = Arc::new(AtomicU64::new(0));
    let so = child.stdout.take().map(|o| {
        let d = done_us.clone();
        thread::spawn(move || {
            for l in std::io::BufReader::new(o).lines().map_while(Result::ok) {
                // out_time_us (newer ffmpeg) / out_time_ms (older: also microseconds)
                if let Some(v) = l.strip_prefix("out_time_us=").or_else(|| l.strip_prefix("out_time_ms=")) {
                    if let Ok(x) = v.trim().parse::<u64>() { d.store(x, Relaxed); }
                }
            }
        })
    });
    let se = child.stderr.take().map(|mut e| thread::spawn(move || {
        let mut v = Vec::new();
        let _ = e.read_to_end(&mut v);
        if v.len() > 8192 { v.drain(..v.len() - 8192); }
        v
    }));
    let status = loop {
        if cancel.load(Relaxed) {
            let _ = child.kill();
            let _ = child.wait();
            let _ = std::fs::remove_file(output);
            return Err("export cancelled".into());
        }
        match child.try_wait() {
            Ok(Some(st)) => break st,
            Ok(None) => {
                let f = (done_us.load(Relaxed) as f64 / dur_us.max(1) as f64).clamp(0.0, 1.0);
                progress.store(span.0 + ((span.1 - span.0) as f64 * f) as u32, Relaxed);
                thread::sleep(Duration::from_millis(50));
            }
            Err(e) => { let _ = child.kill(); return Err(format!("ffmpeg: {e}")); }
        }
    };
    if let Some(h) = so { let _ = h.join(); }
    let err = se.and_then(|h| h.join().ok()).unwrap_or_default();
    if !status.success() {
        let _ = std::fs::remove_file(output); // v1.3.1 FIX: no truncated MP4 left behind
        let err = String::from_utf8_lossy(&err);
        return Err(format!("ffmpeg failed: {}", err.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("unknown error")));
    }
    progress.store(span.1, Relaxed);
    Ok(())
}

// =====================================================================================
// Background JPEG decoder for the preview
// =====================================================================================

/// Frames re-encoded by one export worker: (output frame, JPEG).
type Rendered = Vec<(u32, Vec<u8>)>;

/// v1.4.0: `out` (output frame) + `ctx` let the preview show rendered effects.
struct DecReq { src: Arc<Source>, frame: u32, out: u32, ctx: Option<Arc<RenderCtx>>, tw: i32, th: i32 }
struct DecRes { frame: u32, tw: i32, th: i32, w: i32, h: i32, rgb: Vec<u8> }

type DecKey = (u32, u32, i32, i32, usize, usize);
struct Decoder { req: Arc<(Mutex<Option<DecReq>>, Condvar)>, res: Arc<Mutex<Option<DecRes>>>, last: Option<DecKey> }

impl Decoder {
    fn new() -> Self {
        let req: Arc<(Mutex<Option<DecReq>>, Condvar)> = Arc::new((Mutex::new(None), Condvar::new()));
        let res: Arc<Mutex<Option<DecRes>>> = Arc::new(Mutex::new(None));
        let (rq, rs) = (req.clone(), res.clone());
        thread::Builder::new().name("preview-dec".into()).spawn(move || loop {
            let r = {
                let (m, cv) = &*rq;
                // v1.3.1 FIX: poison-tolerant locks (a panic elsewhere must not kill the preview)
                let mut g = m.lock().unwrap_or_else(|e| e.into_inner());
                while g.is_none() { g = cv.wait(g).unwrap_or_else(|e| e.into_inner()); }
                g.take().unwrap()
            };
            let out = r.src.read(r.frame).and_then(|j| match &r.ctx {
                Some(c) if c.needs(r.out) => c.render(r.out, &j),
                _ => decode_jpeg_rgb(&j),
            });
            if let Ok((rgb, w, h)) = out {
                let (fw, fh) = fit(w as i32, h as i32, r.tw, r.th);
                let (ow, oh) = if fw as usize >= w { (w, h) } else { (fw as usize, fh as usize) };
                let px = scale_rgb(&rgb, w, h, ow, oh);
                *rs.lock().unwrap_or_else(|e| e.into_inner()) = Some(DecRes { frame: r.frame, tw: r.tw, th: r.th, w: ow as i32, h: oh as i32, rgb: px });
                app::awake();
            }
        }).expect("spawn decoder");
        Decoder { req, res, last: None }
    }
    fn request(&mut self, src: &Arc<Source>, frame: u32, out: u32, ctx: Option<&Arc<RenderCtx>>, tw: i32, th: i32) {
        // a static frame without effects at another output position decodes identically
        let o = if ctx.is_some() { out } else { 0 };
        let key = (frame, o, tw, th, Arc::as_ptr(src) as usize, ctx.map(|c| Arc::as_ptr(c) as usize).unwrap_or(0));
        if self.last == Some(key) || tw <= 0 || th <= 0 { return; }
        self.last = Some(key);
        let (m, cv) = &*self.req;
        *m.lock().unwrap_or_else(|e| e.into_inner()) = Some(DecReq { src: src.clone(), frame, out, ctx: ctx.cloned(), tw, th }); // latest wins
        cv.notify_one();
    }
}

// =====================================================================================
// Application state
// =====================================================================================

struct NarrRec { mic: AudioCapture, track: usize, start: u32, end: u32, clear: bool }

struct Ctl { win: Window, lbl: Frame, pause: Button }

struct App {
    proj: Option<Project>,
    play: Arc<PlayShared>, out: Option<cpal::Stream>, clock: Option<Instant>,
    playhead: u32, sel: Option<(u32, u32)>, sel_track: Option<usize>,
    zoom: f64, scroll: f64, drag_anchor: Option<u32>,
    peaks: HashMap<usize, Arc<Vec<(i16, i16)>>>,
    rec: Option<Recorder>, rec_pending: bool, ctl: Option<Ctl>, narr: Option<NarrRec>,
    dec: Decoder, preview: Option<(RgbImage, u32)>,
    job: Option<Job>, mics: Vec<String>, status: String, ffmpeg: Option<PathBuf>,
    /// Bumped on every edit; dead-air results are only shown/applied for the generation
    /// they were computed on (any edit invalidates them, so a stale range is never cut).
    edit_gen: u64, dead: Vec<(u32, u32)>, dead_gen: u64,
    /// Double-Space detection: time of the last Space press and the play state before it.
    space_at: Option<(Instant, bool)>,
    /// FX audition: track index + processed samples played instead of the stored ones.
    fx_preview: Option<(usize, Arc<Vec<i16>>)>,
    /// Output level meter state (linear peak with fall-off, peak-hold value + time).
    vu: [f32; 2], vu_hold: [(f32, Option<Instant>); 2],
    // ---- v1.4.0
    /// Effect renderer snapshot for the current edit (None = no effects: plain decode).
    rctx: Option<Arc<RenderCtx>>,
    /// Annotation tool (0 = select, then TOOL_NAMES order), selected overlay, drag state.
    tool: u8, ov_sel: Option<usize>, ov_drag: Option<OvDrag>, ov_color: usize,
    /// Where the preview image was last drawn (widget coords), for mouse mapping.
    pv_rect: (i32, i32, i32, i32),
    cams: Vec<(String, String)>,
    autosave: Option<JoinHandle<Res<()>>>, last_autosave: Instant,
}

/// Preview mouse interaction on overlays.
#[derive(Clone)]
struct OvDrag { mode: u8 /* 0 move, 1 resize, 2 create */, from: (f32, f32), orig: Option<Overlay>, undo: bool }

const TOOL_NAMES: [&str; 7] = ["Select", "Text", "Arrow", "Blur", "Pixelate", "Step", "Zoom"];
const OV_COLORS: [(&str, u32); 6] = [("Red", 0xe53935), ("Yellow", 0xfdd835), ("Blue", 0x1e88e5), ("Green", 0x43a047), ("White", 0xffffff), ("Black", 0x212121)];
/// v1.4.0: undo history keeps at most this many bytes of *distinct* audio buffers (a cut on
/// a 1 h mic track allocates a new 345 MB buffer; 64 of those would exhaust RAM).
const UNDO_BYTES: usize = 1536 << 20;

impl App {
    fn spf(&self) -> usize { self.proj.as_ref().map(|p| p.src.spf()).unwrap_or(1600) }
    fn total(&self) -> u32 { self.proj.as_ref().map(|p| p.edit.total()).unwrap_or(0) }
    fn fps(&self) -> u32 { self.proj.as_ref().map(|p| p.src.fps).unwrap_or(30) }

    fn push_undo(&mut self) {
        if let Some(p) = &mut self.proj {
            p.undo.push(p.edit.clone());
            if p.undo.len() > UNDO_MAX { p.undo.remove(0); }
            p.redo.clear();
            p.dirty = true;
            // v1.4.0 FIX: bound history by memory, not only by step count
            loop {
                let mut seen = std::collections::HashSet::new();
                let bytes: usize = p.undo.iter().chain(std::iter::once(&p.edit)).flat_map(|e| e.tracks.iter())
                    .filter(|t| seen.insert(Arc::as_ptr(&t.data) as usize)).map(|t| t.data.len() * 2).sum();
                if bytes <= UNDO_BYTES || p.undo.len() <= 1 { break; }
                p.undo.remove(0);
            }
        }
        self.edit_gen += 1;
    }

    /// Replace one track's samples as an undoable edit (FX apply). Track gain resets to 1.0
    /// when `reset_gain` (normalization bakes the level into the samples).
    fn set_track_samples(&mut self, ti: usize, data: Vec<i16>, reset_gain: bool) {
        self.push_undo();
        if let Some(t) = self.proj.as_mut().and_then(|p| p.edit.tracks.get_mut(ti)) {
            t.data = Arc::new(data);
            if reset_gain { t.gain = 1.0; }
        }
        self.after_edit();
    }

    /// Loudness-normalize tracks to `target` LUFS with a -1 dBFS peak ceiling (look-ahead
    /// limiter). Scope: the selected track, or every track. One undo step for all tracks.
    /// v1.4.0: every UI save goes through here so a running autosave finishes first (it
    /// writes into the same folder; finishing after the save would leave a stale autosave).
    fn save_project(&mut self) -> Res<()> {
        if let Some(h) = self.autosave.take() { let _ = h.join(); }
        self.proj.as_mut().map(|p| p.save()).unwrap_or(Ok(()))
    }

    /// v1.4.0: background autosave of unsaved edits (tracks unchanged since the last save
    /// are referenced, not rewritten). Never touches project.txt or track_N.wav.
    fn autosave_tick(&mut self) {
        if self.last_autosave.elapsed() < Duration::from_secs(60) { return; }
        if self.autosave.as_ref().map(|h| !h.is_finished()).unwrap_or(false) { return; }
        if let Some(h) = self.autosave.take() { if let Ok(Err(e)) = h.join() { eprintln!("[autosave] {e}"); } }
        self.last_autosave = Instant::now();
        let Some(p) = self.proj.as_ref().filter(|p| p.dirty) else { return };
        if self.rec.is_some() || self.narr.is_some() { return; }
        let job = p.autosave_job();
        self.autosave = thread::Builder::new().name("autosave".into()).spawn(job).ok();
    }

    fn dead_valid(&self) -> bool { self.dead_gen == self.edit_gen && !self.dead.is_empty() }

    /// Replace the open project (New/Open). Stops playback and clears per-project state.
    fn set_project(&mut self, p: Option<Project>, tl_w: i32) {
        self.set_playing(false);
        let n = p.as_ref().map(|p| p.edit.total()).unwrap_or(0);
        self.proj = p;
        self.fx_preview = None;
        self.sel = None; self.sel_track = None; self.drag_anchor = None;
        self.ov_sel = None; self.ov_drag = None;
        self.scroll = 0.0;
        self.zoom = (tl_w as f64 / n.max(1) as f64).max(1e-4);
        self.preview = None; self.peaks.clear();
        self.dead.clear(); self.edit_gen += 1;
        self.after_edit();
        self.seek(0);
    }

    /// Speed up the selection by sp/100 (ripple; audio pitch-preserved or muted).
    fn op_speed(&mut self, sp: u32, mute: bool) -> Res<()> {
        let (a, b) = self.sel.ok_or("Select a range to speed up (drag on the timeline)")?;
        if !(SPEED_MIN..=SPEED_MAX).contains(&sp) { return Err("invalid speed".into()); }
        self.push_undo();
        let spf = self.spf();
        let end = self.proj.as_mut().unwrap().edit.speed_range(a, b, sp, mute, spf);
        self.sel = Some((a, end));
        self.playhead = a;
        self.after_edit();
        let fps = self.fps() as f64;
        self.status = format!("Sped up {:.2}s → {:.2}s ({}×{})", (b - a) as f64 / fps, (end - a) as f64 / fps, fmt_speed(sp), if mute { ", audio muted" } else { "" });
        Ok(())
    }

    /// Cut every detected dead-air range as one undo step.
    fn op_remove_dead(&mut self) -> Res<()> {
        if !self.dead_valid() { return Err("Run \"Find dead air\" first (results are cleared by any edit)".into()); }
        let ranges = std::mem::take(&mut self.dead);
        let frames: u32 = ranges.iter().map(|(a, b)| b - a).sum();
        self.push_undo();
        let spf = self.spf();
        self.proj.as_mut().unwrap().edit.cut_many(&ranges, spf);
        self.sel = None;
        self.after_edit();
        self.status = format!("Removed {} dead-air region(s), {:.1}s. Use Compact project to reclaim disk space.", ranges.len(), frames as f64 / self.fps() as f64);
        Ok(())
    }

    /// Re-sync mixer + clamp cursor after any edit.
    fn after_edit(&mut self) {
        let total = self.total();
        self.playhead = self.playhead.min(total.saturating_sub(1));
        if let Some((a, b)) = self.sel { let b = b.min(total); self.sel = if a < b { Some((a, b)) } else { None }; }
        if let Some(t) = self.sel_track { if self.proj.as_ref().map(|p| t >= p.edit.tracks.len()).unwrap_or(true) { self.sel_track = None; } }
        self.sync_mix();
        // v1.4.0: effect snapshot for preview; keep the overlay selection valid
        self.rctx = self.proj.as_ref().and_then(|p| RenderCtx::new(&p.src, &p.edit));
        if let Some(i) = self.ov_sel { if self.proj.as_ref().map(|p| i >= p.edit.ov.len()).unwrap_or(true) { self.ov_sel = None; } }
        let live: Vec<usize> = self.proj.as_ref().map(|p| p.edit.tracks.iter().map(|t| Arc::as_ptr(&t.data) as usize).collect()).unwrap_or_default();
        self.peaks.retain(|k, _| live.contains(k));
    }

    fn sync_mix(&mut self) {
        let excl = self.narr.as_ref().map(|n| n.track);
        let over = self.fx_preview.as_ref().filter(|o| self.proj.as_ref().map(|p| o.0 < p.edit.tracks.len()).unwrap_or(false));
        let (ts, end) = match &self.proj { Some(p) => (mix_tracks_with(&p.edit, excl, over), p.edit.total() as u64 * p.src.spf() as u64), None => (vec![], 0) };
        *self.play.mix.lock().unwrap_or_else(|e| e.into_inner()) = ts; // v1.3.1 FIX: poison-tolerant
        self.play.end.store(end, Relaxed);
    }

    fn seek(&mut self, f: u32) {
        self.playhead = f.min(self.total().saturating_sub(1));
        self.play.seek_to(self.playhead as u64 * self.spf() as u64);
        if self.clock.is_some() { self.clock = Some(Instant::now()); }
    }

    fn set_playing(&mut self, on: bool) {
        if self.proj.is_none() { return; }
        if on {
            if self.playhead + 1 >= self.total() && self.narr.is_none() { self.seek(0); }
            self.play.seek_to(self.playhead as u64 * self.spf() as u64);
            self.clock = if self.out.is_none() { Some(Instant::now()) } else { None };
        }
        self.play.playing.store(on, Relaxed);
    }

    fn op_cut(&mut self) -> Res<()> {
        let (a, b) = self.sel.ok_or("Select a range on the timeline first (drag)")?;
        self.push_undo();
        let spf = self.spf();
        self.proj.as_mut().unwrap().edit.cut(a, b, spf);
        self.sel = None;
        self.playhead = a;
        self.after_edit();
        self.status = format!("Cut {:.2}s", (b - a) as f64 / self.fps() as f64);
        Ok(())
    }

    fn op_insert_freeze(&mut self, secs: f64) -> Res<()> {
        if self.proj.is_none() { return Err("no project".into()); }
        let n = ((secs * self.fps() as f64).round() as u32).clamp(1, self.fps() * 600);
        self.push_undo();
        let (at, spf) = (self.playhead, self.spf());
        self.proj.as_mut().unwrap().edit.insert_freeze(at, n, spf);
        self.after_edit();
        self.status = format!("Inserted {secs:.1}s freeze");
        Ok(())
    }

    fn op_freeze_sel(&mut self) -> Res<()> {
        let (a, b) = self.sel.ok_or("Select a range to freeze")?;
        self.push_undo();
        self.proj.as_mut().unwrap().edit.freeze_range(a, b);
        self.after_edit();
        self.status = "Froze selection (audio continues)".into();
        Ok(())
    }

    fn op_undo(&mut self, redo: bool) {
        if let Some(p) = &mut self.proj {
            let (from, to) = if redo { (&mut p.redo, &mut p.undo) } else { (&mut p.undo, &mut p.redo) };
            if let Some(e) = from.pop() { to.push(std::mem::replace(&mut p.edit, e)); p.dirty = true; }
        }
        self.edit_gen += 1;
        self.after_edit();
    }

    fn op_track<F: FnOnce(&mut Edit, usize, usize)>(&mut self, f: F) -> Res<()> {
        let ti = self.sel_track.ok_or("Select a track (click its lane or use the track menu)")?;
        self.push_undo();
        let spf = self.spf();
        f(&mut self.proj.as_mut().unwrap().edit, ti, spf);
        self.after_edit();
        Ok(())
    }

    fn add_track(&mut self, name: String, ch: u16, data: Vec<i16>, at: u32) {
        self.push_undo();
        let spf = self.spf();
        let mut v = vec![0i16; at as usize * spf * ch as usize];
        v.extend_from_slice(&data);
        let p = self.proj.as_mut().unwrap();
        p.edit.tracks.push(Track { name, ch, data: Arc::new(v), gain: 1.0, muted: false });
        self.sel_track = Some(p.edit.tracks.len() - 1);
        self.after_edit();
    }

    /// `live_fx`: v1.4.0 voice chain applied while narrating (RECORD › Live voice FX).
    fn start_narration(&mut self, mic: Option<String>, live_fx: Option<VoiceFx>) -> Res<()> {
        if self.proj.is_none() { return Err("record or open a project first".into()); }
        if self.narr.is_some() { return Ok(()); }
        let (a, b) = self.sel.unwrap_or((self.playhead, self.total()));
        if b <= a + 1 { return Err("Move the playhead before the end, or select a range to re-record".into()); }
        // v1.3.1 FIX: open the mic *before* touching the project. v1.3.0 added the new
        // "Narration N" track first, so a busy/missing mic left an empty track behind, and a
        // successful take cost two undo steps (add track + record). Now: one undo step,
        // and nothing changes if the mic fails.
        let m = AudioCapture::start(AudioSrc::Mic(mic.as_deref()), Arc::new(PauseClock::default()), SinkOpts { spill: None, fx: live_fx })?;
        let ti = match self.sel_track.filter(|&t| self.proj.as_ref().map(|p| t < p.edit.tracks.len()).unwrap_or(false)) {
            Some(t) => { self.push_undo(); t }
            None => {
                let n = self.proj.as_ref().map(|p| p.edit.tracks.len() + 1).unwrap_or(1);
                self.add_track(format!("Narration {n}"), 1, vec![], 0); // add_track pushes the undo step
                self.sel_track.unwrap_or(0)
            }
        };
        self.narr = Some(NarrRec { mic: m, track: ti, start: a, end: b, clear: self.sel.is_some() });
        self.sync_mix(); // excludes the track being re-recorded from playback
        self.seek(a);
        self.set_playing(true);
        self.status = "● Recording narration… (click again or reach end to stop)".into();
        Ok(())
    }

    fn stop_narration(&mut self) {
        let Some(n) = self.narr.take() else { return };
        self.set_playing(false);
        let spf = self.spf();
        let (_, mut s) = n.mic.finish(None); // mic capture is always mono
        let maxlen = (n.end - n.start) as usize * spf;
        s.truncate(maxlen);
        let secs = s.len() as f64 / AR as f64;
        if let Some(p) = &mut self.proj {
            p.edit.write_track(n.track, n.start, &s, if n.clear { Some(n.end) } else { None }, spf);
        }
        self.after_edit();
        self.status = format!("Narration recorded ({secs:.1}s) on track {}", n.track + 1);
    }

    fn peaks_for(&mut self, t: &Track) -> Arc<Vec<(i16, i16)>> {
        let key = Arc::as_ptr(&t.data) as usize;
        if let Some(p) = self.peaks.get(&key) { return p.clone(); }
        const B: usize = 480; // 10 ms buckets
        let ch = t.ch as usize;
        let v: Vec<(i16, i16)> = t.data.chunks(B * ch).map(|c| {
            c.iter().fold((i16::MAX, i16::MIN), |(lo, hi), &s| (lo.min(s), hi.max(s)))
        }).collect();
        let a = Arc::new(v);
        self.peaks.insert(key, a.clone());
        a
    }
}

// =====================================================================================
// UI
// =====================================================================================

#[derive(Clone)]
struct Ui {
    win: Window, rec_full: Button, rec_region: Button, fps: Choice, quality: Choice, half: CheckButton, cursor: CheckButton,
    mic: Choice, sys: CheckButton, open: Button, save: Button, exp_mkv: Button, exp_mov: Button, exp_mp4: Button, exp_wav: Button,
    preview: Frame, play: Button, time: Frame, cut: Button, ifreeze: Button, fsecs: Spinner, freeze_sel: Button,
    undo: Button, redo: Button, narr: Button, fitz: Button,
    track: Choice, mute: Button, gain: HorNiceSlider, shl: Button, shr: Button, newt: Button, import: Button, replace: Button, remove: Button,
    timeline: Frame, status: Frame,
    // v1.2.0
    new: Button, speed: Choice, speed_mute: CheckButton, speed_btn: Button, find_dead: Button, rm_dead: Button, compact: Button,
    // v1.3.0
    comp: Button, norm: Button, meter: Frame,
    // v1.4.0
    cam: Choice, live_fx: CheckButton, voicefx: Button, duck: Button, tools: Vec<fltk::button::RadioButton>, ov_color: Choice,
    ov_del: Button, ov_text: Button, ov_time: Button, autozoom: Button, effects: Button,
}

/// Speed choices (x100) shown in the "Speed" menu, in order.
const SPEEDS: [u32; 7] = [125, 150, 200, 300, 400, 800, 1600];

type Shared = std::rc::Rc<std::cell::RefCell<App>>;

const RULER_H: i32 = 18;
const VLANE_H: i32 = 28;
const FXLANE_H: i32 = 14; // v1.4.0: overlay/effects lane under the video lane

/// 150 -> "1.5", 200 -> "2", 125 -> "1.25"
fn fmt_speed(sp: u32) -> String {
    let t = format!("{}.{:02}", sp / 100, sp % 100);
    t.trim_end_matches('0').trim_end_matches('.').to_string()
}

fn fmt_time(frames: u32, fps: u32) -> String {
    let cs = (frames as u64 * 100 + fps.max(1) as u64 / 2) / fps.max(1) as u64; // rounded centiseconds
    // v1.3.1 FIX: past 99:59 the minutes field grew to 3 digits and the ruler's fixed [..5]
    // slice cut labels mid-number ("100:0"). Use H:MM:SS.cc from one hour on.
    if cs >= 360_000 { format!("{}:{:02}:{:02}.{:02}", cs / 360_000, cs / 6000 % 60, cs / 100 % 60, cs % 100) }
    else { format!("{:02}:{:02}.{:02}", cs / 6000, cs / 100 % 60, cs % 100) }
}

fn lane_h(app: &App, h: i32) -> i32 {
    let n = app.proj.as_ref().map(|p| p.edit.tracks.len()).unwrap_or(0) as i32;
    if n == 0 { 0 } else { ((h - RULER_H - VLANE_H - FXLANE_H) / n).clamp(14, 48) }
}

fn x_to_frame(app: &App, tl: &Frame, x: i32) -> u32 {
    let f = app.scroll + (x - tl.x()) as f64 / app.zoom.max(1e-6);
    (f.max(0.0) as u32).min(app.total())
}

fn draw_timeline(app: &mut App, tl: &Frame) {
    let (x0, y0, w, h) = (tl.x(), tl.y(), tl.w(), tl.h());
    draw::draw_rect_fill(x0, y0, w, h, Color::from_rgb(28, 30, 34));
    let Some(p) = &app.proj else {
        draw::set_draw_color(Color::from_rgb(140, 140, 140));
        draw::set_font(Font::Helvetica, 13);
        draw::draw_text2("Record the screen or open a project to start editing", x0, y0, w, h, Align::Center);
        return;
    };
    let (fps, total) = (p.src.fps, p.edit.total());
    if total == 0 { return; }
    let (scroll0, zoom0) = (app.scroll, app.zoom);
    let fx = move |f: f64| x0 + ((f - scroll0) * zoom0) as i32;
    draw::push_clip(x0, y0, w, h);
    // Ruler
    draw::draw_rect_fill(x0, y0, w, RULER_H, Color::from_rgb(44, 47, 54));
    let steps = [0.1, 0.2, 0.5, 1.0, 2.0, 5.0, 10.0, 15.0, 30.0, 60.0, 120.0, 300.0, 600.0, 1800.0];
    let step = steps.iter().copied().find(|s| s * fps as f64 * app.zoom >= 70.0).unwrap_or(3600.0);
    let mut k = (app.scroll / fps as f64 / step).floor() as i64;
    draw::set_font(Font::Helvetica, 10);
    loop {
        let t = k as f64 * step;
        k += 1;
        let x = fx(t * fps as f64);
        if x >= x0 + w { break; }
        draw::set_draw_color(Color::from_rgb(120, 125, 135));
        draw::draw_yxline(x, y0 + 8, y0 + RULER_H);
        draw::set_draw_color(Color::from_rgb(190, 190, 190));
        let lbl = fmt_time((t * fps as f64).round() as u32, fps);
        // v1.3.1 FIX: drop the ".cc" suffix by length, not a fixed byte offset (H:MM:SS labels)
        draw::draw_text(if step >= 1.0 { &lbl[..lbl.len() - 3] } else { &lbl }, x + 3, y0 + 11);
    }
    // Selection band
    if let Some((a, b)) = app.sel {
        let (xa, xb) = (fx(a as f64), fx(b as f64));
        draw::draw_rect_fill(xa, y0 + RULER_H, (xb - xa).max(1), h - RULER_H, Color::from_rgb(52, 70, 96));
        draw::draw_rect_fill(xa, y0 + RULER_H - 3, (xb - xa).max(1), 3, Color::from_rgb(120, 170, 255));
    }
    // Dead-air candidates (only while valid for the current edit)
    if app.dead_gen == app.edit_gen {
        for &(a, b) in &app.dead {
            let (xa, xb) = (fx(a as f64), fx(b as f64));
            if xb < x0 || xa > x0 + w { continue; }
            draw::draw_rect_fill(xa, y0 + RULER_H, (xb - xa).max(1), h - RULER_H, Color::from_rgb(78, 36, 40));
            draw::draw_rect_fill(xa, y0, (xb - xa).max(1), 4, Color::from_rgb(230, 80, 80));
        }
    }
    // Video lane
    let vy = y0 + RULER_H;
    let mut acc = 0u32;
    draw::set_font(Font::HelveticaBold, 10);
    for s in &p.edit.segs {
        let (xa, xb) = (fx(acc as f64), fx((acc + s.len()) as f64));
        if xb < x0 || xa > x0 + w { acc += s.len(); continue; } // off-screen: skip draw
        let fast;
        let (col, txt) = match s {
            Seg::Src { .. } => (Color::from_rgb(58, 104, 160), ""),
            Seg::Hold { .. } => (Color::from_rgb(196, 132, 40), "FREEZE"),
            Seg::Fast { sp, .. } => { fast = format!("{}×", fmt_speed(*sp)); (Color::from_rgb(52, 150, 96), fast.as_str()) }
        };
        draw::draw_rect_fill(xa, vy + 2, (xb - xa).max(1), VLANE_H - 4, col);
        draw::set_draw_color(Color::from_rgb(20, 20, 20));
        draw::draw_yxline(xa, vy + 2, vy + VLANE_H - 2);
        if !txt.is_empty() && xb - xa > 46 { draw::set_draw_color(Color::White); draw::draw_text2(txt, xa, vy, xb - xa, VLANE_H, Align::Center); }
        acc += s.len();
    }
    // v1.4.0: FX lane (overlays as bars; zooms blue, redaction grey, annotations in their colour)
    let fy = vy + VLANE_H;
    draw::draw_rect_fill(x0, fy, w, FXLANE_H, Color::from_rgb(34, 36, 42));
    for (i, o) in p.edit.ov.iter().enumerate() {
        let (xa, xb) = (fx(o.start as f64), fx(o.end as f64));
        if xb < x0 || xa > x0 + w { continue; }
        let col = match o.kind {
            OvKind::Zoom { .. } => Color::from_rgb(58, 120, 200),
            OvKind::Blur | OvKind::Pixelate => Color::from_rgb(120, 120, 130),
            _ => Color::from_rgb((o.color >> 16) as u8, (o.color >> 8) as u8, o.color as u8),
        };
        let (yy, hh) = if matches!(o.kind, OvKind::Zoom { .. }) { (fy + 8, 5) } else { (fy + 2, 5) };
        draw::draw_rect_fill(xa, yy, (xb - xa).max(2), hh, col);
        if app.ov_sel == Some(i) { draw::set_draw_color(Color::White); draw::draw_rect(xa - 1, fy, (xb - xa).max(2) + 2, FXLANE_H); }
    }
    if p.edit.ov.is_empty() && w > 300 {
        draw::set_font(Font::Helvetica, 9);
        draw::set_draw_color(Color::from_rgb(95, 100, 110));
        draw::draw_text("FX lane: annotations and zooms appear here", x0 + 6, fy + 10);
    }
    // Audio lanes
    let lh = lane_h(app, h);
    let spf = p.src.spf();
    let tracks: Vec<Track> = p.edit.tracks.clone();
    let (scroll, zoom, sel_track) = (app.scroll, app.zoom, app.sel_track);
    for (i, t) in tracks.iter().enumerate() {
        let ly = vy + VLANE_H + FXLANE_H + i as i32 * lh;
        if Some(i) == sel_track { draw::draw_rect_fill(x0, ly, w, lh, Color::from_rgb(40, 44, 52)); }
        draw::set_draw_color(Color::from_rgb(55, 58, 64));
        draw::draw_xyline(x0, ly, x0 + w);
        let pk = app.peaks_for(t);
        let mid = ly + lh / 2;
        let amp = (lh / 2 - 2).max(1) as f32;
        draw::set_draw_color(if t.muted { Color::from_rgb(90, 90, 90) } else { Color::from_rgb(90, 200, 120) });
        for px in 0..w {
            let f0 = scroll + px as f64 / zoom;
            let f1 = scroll + (px + 1) as f64 / zoom;
            if f0 >= total as f64 { break; }
            let (b0, b1) = ((f0 * spf as f64 / 480.0) as usize, ((f1 * spf as f64 / 480.0).ceil() as usize).max((f0 * spf as f64 / 480.0) as usize + 1));
            if b0 >= pk.len() { continue; }
            let (mut lo, mut hi) = (i16::MAX, i16::MIN);
            for &(l, h2) in &pk[b0..b1.min(pk.len())] { lo = lo.min(l); hi = hi.max(h2); }
            if lo > hi { continue; }
            let (ya, yb) = (mid - (hi as f32 / 32768.0 * amp * t.gain.min(2.0)) as i32, mid - (lo as f32 / 32768.0 * amp * t.gain.min(2.0)) as i32);
            draw::draw_yxline(x0 + px, ya, yb.max(ya));
        }
        draw::set_font(Font::Helvetica, 10);
        draw::set_draw_color(Color::from_rgb(230, 230, 230));
        draw::draw_text(&esc(&format!("{}{}{}", i + 1, if t.muted { " [M] " } else { " " }, t.name)), x0 + 4, ly + 11);
    }
    // Selection edges + playhead
    if let Some((a, b)) = app.sel {
        draw::set_draw_color(Color::from_rgb(120, 170, 255));
        draw::draw_yxline(fx(a as f64), y0, y0 + h);
        draw::draw_yxline(fx(b as f64), y0, y0 + h);
    }
    let px = fx(app.playhead as f64);
    draw::set_draw_color(Color::from_rgb(255, 70, 70));
    draw::draw_yxline(px, y0, y0 + h);
    draw::draw_yxline(px + 1, y0, y0 + RULER_H);
    draw::pop_clip();
}

/// v1.4.0: preview <-> source-normalized coordinate mapping (inverse of zoom/pan).
fn pv_to_src(a: &App, mx: i32, my: i32) -> Option<(f32, f32)> {
    let (dx, dy, dw, dh) = a.pv_rect;
    if dw <= 0 || dh <= 0 { return None; }
    let (u, v) = ((mx - dx) as f32 / dw as f32, (my - dy) as f32 / dh as f32);
    let z = a.rctx.as_ref().map(|c| c.zoom_at(a.playhead)).unwrap_or([1.0, 0.5, 0.5]);
    Some((z[1] - 0.5 / z[0] + u / z[0], z[2] - 0.5 / z[0] + v / z[0]))
}
fn src_to_pv(a: &App, x: f32, y: f32) -> (i32, i32) {
    let (dx, dy, dw, dh) = a.pv_rect;
    let z = a.rctx.as_ref().map(|c| c.zoom_at(a.playhead)).unwrap_or([1.0, 0.5, 0.5]);
    let (u, v) = ((x - (z[1] - 0.5 / z[0])) * z[0], (y - (z[2] - 0.5 / z[0])) * z[0]);
    (dx + (u * dw as f32).round() as i32, dy + (v * dh as f32).round() as i32)
}

/// Selection frames/handles for overlays visible at the playhead (drawn in the preview).
fn draw_overlay_marks(a: &App) {
    let Some(p) = &a.proj else { return };
    let mut items: Vec<(Option<usize>, Overlay)> = p.edit.ov.iter().enumerate()
        .filter(|(i, o)| o.active(a.playhead) || a.ov_sel == Some(*i)).map(|(i, o)| (Some(i), o.clone())).collect();
    if let Some(OvDrag { mode: 2, orig: Some(o), .. }) = &a.ov_drag { items.push((None, o.clone())); }
    for (i, o) in items {
        let sel = i.is_some() && i == a.ov_sel;
        if matches!(o.kind, OvKind::Zoom { .. }) && !sel && a.tool != 6 && i.is_some() { continue; }
        let col = if sel { Color::from_rgb(80, 190, 255) } else { Color::from_rgb(210, 210, 210) };
        draw::set_draw_color(col);
        draw::set_line_style(if sel { draw::LineStyle::Solid } else { draw::LineStyle::Dash }, 1);
        if matches!(o.kind, OvKind::Arrow) {
            let (ax, ay) = src_to_pv(a, o.x, o.y);
            let (bx, by) = src_to_pv(a, o.x + o.w, o.y + o.h);
            if i.is_none() { draw::draw_line(ax, ay, bx, by); }
            if sel { draw::draw_rect_fill(bx - 4, by - 4, 9, 9, col); draw::draw_rect_fill(ax - 3, ay - 3, 7, 7, col); }
        } else {
            let (x0, y0, x1, y1) = o.bounds();
            let (px0, py0) = src_to_pv(a, x0, y0);
            let (px1, py1) = src_to_pv(a, x1, y1);
            draw::draw_rect(px0, py0, (px1 - px0).max(1), (py1 - py0).max(1));
            if sel { draw::draw_rect_fill(px1 - 4, py1 - 4, 9, 9, col); }
            if matches!(o.kind, OvKind::Zoom { .. }) || i.is_none() || sel {
                draw::set_font(Font::HelveticaBold, 11);
                let lbl = match o.kind { OvKind::Zoom { scale, follow } => format!("Zoom {scale:.1}×{}", if follow { " · follow" } else { "" }), _ => o.name().to_string() };
                draw::draw_text(&lbl, px0 + 3, py0 - 3);
            }
        }
        draw::set_line_style(draw::LineStyle::Solid, 0);
    }
}

/// Topmost overlay under the mouse at the playhead: (index, on resize handle).
fn hit_overlay(a: &App, mx: i32, my: i32) -> Option<(usize, bool)> {
    let p = a.proj.as_ref()?;
    for (i, o) in p.edit.ov.iter().enumerate().rev() {
        if !o.active(a.playhead) && a.ov_sel != Some(i) { continue; }
        if matches!(o.kind, OvKind::Zoom { .. }) && a.tool != 6 && a.ov_sel != Some(i) { continue; }
        let near = |x: i32, y: i32| (mx - x).abs() <= 7 && (my - y).abs() <= 7;
        if matches!(o.kind, OvKind::Arrow) {
            let (ax, ay) = src_to_pv(a, o.x, o.y);
            let (bx, by) = src_to_pv(a, o.x + o.w, o.y + o.h);
            if near(bx, by) { return Some((i, true)); }
            // distance to the shaft
            let (dx, dy) = ((bx - ax) as f32, (by - ay) as f32);
            let l2 = (dx * dx + dy * dy).max(1.0);
            let t = (((mx - ax) as f32 * dx + (my - ay) as f32 * dy) / l2).clamp(0.0, 1.0);
            let (qx, qy) = (ax as f32 + t * dx - mx as f32, ay as f32 + t * dy - my as f32);
            if qx * qx + qy * qy <= 64.0 { return Some((i, false)); }
        } else {
            let (x0, y0, x1, y1) = o.bounds();
            let (px0, py0) = src_to_pv(a, x0, y0);
            let (px1, py1) = src_to_pv(a, x1, y1);
            if near(px1, py1) { return Some((i, true)); }
            if mx >= px0 - 3 && mx <= px1 + 3 && my >= py0 - 3 && my <= py1 + 3 { return Some((i, false)); }
        }
    }
    None
}

/// Default time span for a new overlay: the selection if the playhead is in it, else 4 s.
fn new_ov_span(a: &App) -> (u32, u32) {
    let total = a.total();
    if let Some((s, e)) = a.sel { if a.playhead >= s && a.playhead < e { return (s, e); } }
    let s = a.playhead.min(total.saturating_sub(1));
    (s, (s + a.fps() * 4).min(total).max(s + 1))
}

fn edit_text_dialog(cur: &str) -> Option<String> {
    let shown = cur.replace('\n', "\\n");
    let t = dialog::input_default("Callout text (type \\n for a new line):", &shown)?;
    let t: String = t.replace("\\n", "\n").chars().take(MAX_OV_TEXT).collect();
    if t.trim().is_empty() { None } else { Some(t) }
}

/// Mouse handling on the preview: select/move/resize overlays, or draw a new one.
fn preview_event(app: &Shared, ui: &Ui, ev: Event) -> bool {
    let Ok(mut a) = app.try_borrow_mut() else { return false };
    if a.proj.is_none() || a.job.is_some() || a.narr.is_some() || a.total() == 0 { return false; }
    let (mx, my) = (app::event_x(), app::event_y());
    match ev {
        Event::Push => {
            a.set_playing(false);
            let Some(pt) = pv_to_src(&a, mx, my) else { return false };
            if a.tool == 0 || hit_overlay(&a, mx, my).is_some_and(|(i, _)| a.ov_sel == Some(i)) {
                match hit_overlay(&a, mx, my) {
                    Some((i, resize)) => {
                        a.ov_sel = Some(i);
                        let o = a.proj.as_ref().unwrap().edit.ov[i].clone();
                        if app::event_clicks() && matches!(o.kind, OvKind::Text { .. }) && !resize {
                            drop(a);
                            if let OvKind::Text { text } = &o.kind {
                                if let Some(t) = edit_text_dialog(text) {
                                    let mut a = app.borrow_mut();
                                    a.push_undo();
                                    if let Some(x) = a.proj.as_mut().and_then(|p| p.edit.ov.get_mut(i)) { x.kind = OvKind::Text { text: t }; }
                                    a.after_edit();
                                }
                            }
                            refresh(app, ui);
                            return true;
                        }
                        a.ov_drag = Some(OvDrag { mode: resize as u8, from: pt, orig: Some(o), undo: false });
                    }
                    None => { a.ov_sel = None; a.ov_drag = None; }
                }
            } else {
                let kind = match a.tool {
                    1 => OvKind::Text { text: String::new() }, 2 => OvKind::Arrow, 3 => OvKind::Blur, 4 => OvKind::Pixelate,
                    5 => OvKind::Step { n: 0 }, _ => OvKind::Zoom { scale: 2.0, follow: false },
                };
                let (start, end) = new_ov_span(&a);
                let o = Overlay { start, end, x: pt.0, y: pt.1, w: 0.0, h: 0.0, color: OV_COLORS[a.ov_color.min(5)].1, kind };
                a.ov_drag = Some(OvDrag { mode: 2, from: pt, orig: Some(o), undo: false });
            }
            drop(a);
            refresh(app, ui);
            true
        }
        Event::Drag => {
            let Some(pt) = pv_to_src(&a, mx, my) else { return true };
            let Some(mut d) = a.ov_drag.clone() else { return true };
            let (ddx, ddy) = (pt.0 - d.from.0, pt.1 - d.from.1);
            match d.mode {
                2 => { if let Some(o) = &mut d.orig { o.w = ddx; o.h = ddy; } a.ov_drag = Some(d); }
                m => {
                    let Some(i) = a.ov_sel else { return true };
                    if !d.undo && ddx.abs() + ddy.abs() > 0.002 { a.push_undo(); d.undo = true; }
                    if d.undo {
                        let orig = d.orig.clone().unwrap();
                        if let Some(o) = a.proj.as_mut().and_then(|p| p.edit.ov.get_mut(i)) {
                            if m == 0 { o.x = (orig.x + ddx).clamp(-0.5, 1.5); o.y = (orig.y + ddy).clamp(-0.5, 1.5); }
                            else if matches!(o.kind, OvKind::Arrow) { o.w = orig.w + ddx; o.h = orig.h + ddy; }
                            else { o.w = (orig.w + ddx).max(0.01); o.h = (orig.h + ddy).max(0.01); }
                        }
                        a.after_edit();
                    }
                    a.ov_drag = Some(d);
                }
            }
            drop(a);
            let mut pv = ui.preview.clone();
            pv.redraw();
            refresh(app, ui);
            true
        }
        Event::Released => {
            let d = a.ov_drag.take();
            let Some(OvDrag { mode: 2, orig: Some(mut o), .. }) = d else { drop(a); refresh(app, ui); return true };
            let (sw, sh) = a.proj.as_ref().map(|p| (p.src.w as f32, p.src.h as f32)).unwrap_or((1.0, 1.0));
            // v1.4.0: clip to the video frame. A drag that starts in the letterbox would
            // otherwise create a box partly (or wholly) outside the picture.
            let tiny;
            if matches!(o.kind, OvKind::Arrow) {
                tiny = (o.w * sw).abs() < 6.0 && (o.h * sh).abs() < 6.0;
                let (ex, ey) = ((o.x + o.w).clamp(0.0, 1.0), (o.y + o.h).clamp(0.0, 1.0));
                o.x = o.x.clamp(0.0, 1.0); o.y = o.y.clamp(0.0, 1.0);
                o.w = ex - o.x; o.h = ey - o.y;
            } else {
                // normalise box direction, then clip
                let (x0, y0, x1, y1) = o.bounds();
                let (x0, y0, x1, y1) = (x0.max(0.0), y0.max(0.0), x1.min(1.0), y1.min(1.0));
                o.x = x0.min(1.0); o.y = y0.min(1.0); o.w = (x1 - x0).max(0.0); o.h = (y1 - y0).max(0.0);
                tiny = o.w * sw < 6.0 || o.h * sh < 6.0; // a box needs both sides
            }
            match &mut o.kind {
                OvKind::Text { text } => {
                    if tiny { o.w = 0.28; o.h = 0.08; o.x = o.x.min(1.0 - o.w); o.y = o.y.min(1.0 - o.h); }
                    drop(a);
                    let Some(t) = edit_text_dialog("") else { refresh(app, ui); return true };
                    *text = t;
                    a = app.borrow_mut();
                }
                OvKind::Step { n } => {
                    let d = 0.065 * sh;
                    *n = 1 + a.proj.as_ref().map(|p| p.edit.ov.iter().filter(|x| matches!(x.kind, OvKind::Step { .. })).count() as u32).unwrap_or(0);
                    o.w = d / sw; o.h = d / sh;
                    o.x = (o.x - o.w / 2.0).clamp(0.0, 1.0 - o.w); o.y = (o.y - o.h / 2.0).clamp(0.0, 1.0 - o.h);
                }
                OvKind::Zoom { scale, .. } => {
                    if tiny { return true; }
                    // keep the frame's aspect: the zoomed view is what fills the output
                    *scale = (1.0 / o.w.max(o.h)).clamp(1.25, 4.0);
                }
                _ => if tiny { return true; },
            }
            a.push_undo();
            if let Some(p) = a.proj.as_mut() {
                if p.edit.ov.len() < MAX_OVERLAYS { p.edit.ov.push(o); a.ov_sel = Some(a.proj.as_ref().unwrap().edit.ov.len() - 1); }
            }
            a.after_edit();
            a.status = "Overlay added: drag to move, corner handle to resize, Del to delete. Its time span = timeline selection or 4 s.".into();
            drop(a);
            refresh(app, ui);
            true
        }
        Event::Enter | Event::Move | Event::Leave => true,
        _ => false,
    }
}

/// v1.4.0: background DSP job that replaces track samples. Applied only if the edit is
/// unchanged since the job started (`gen`), as one undo step.
/// (track index, new samples, reset gain to 1.0) per changed track.
type TrackChanges = Vec<(usize, Vec<i16>, bool)>;

fn spawn_track_job(app: &Shared, ui: &Ui, label: &str, f: impl FnOnce(&AtomicU32, &AtomicBool) -> Res<(TrackChanges, String)> + Send + 'static) {
    let gen = app.borrow().edit_gen;
    spawn_job(app, ui, label, move |pg, cc| {
        let (ch, msg) = f(pg, cc)?;
        Ok(JobOut::Tracks { changes: ch.into_iter().map(|(i, v, g)| (i, Arc::new(v), g)).collect(), msg, gen })
    });
}

fn apply_track_job(a: &mut App, changes: Vec<(usize, Arc<Vec<i16>>, bool)>, msg: String, gen: u64) -> String {
    if gen != a.edit_gen { return "Audio processing discarded: the project changed meanwhile. Run it again.".into(); }
    if changes.is_empty() { return msg; }
    a.push_undo();
    if let Some(p) = a.proj.as_mut() {
        for (i, v, reset) in changes { if let Some(t) = p.edit.tracks.get_mut(i) { t.data = v; if reset { t.gain = 1.0; } } }
    }
    a.after_edit();
    msg
}

fn dlg_title(w: i32, text: &str) {
    let mut t = Frame::new(16, 10, w - 32, 26, None).with_align(Align::Left | Align::Inside);
    t.set_label(text);
    t.set_label_font(Font::HelveticaBold); t.set_label_size(15);
}
fn dlg_info(y: i32, w: i32, h: i32, text: &str) {
    let mut f = Frame::new(16, y, w - 32, h, None).with_align(Align::Left | Align::Inside | Align::Wrap | Align::Top);
    f.set_label(text);
    f.set_label_size(12); f.set_label_color(rgb(C_CAPTION));
}
/// Run a modal window until hidden; returns which button closed it (0 = cancel/close).
fn run_modal(w: &mut Window, buttons: &mut [(Button, u8)]) -> u8 {
    w.end();
    w.make_modal(true);
    let res = std::rc::Rc::new(std::cell::Cell::new(0u8));
    for (b, code) in buttons.iter_mut() {
        let (mut w2, r, c) = (w.clone(), res.clone(), *code);
        b.set_callback(move |_| { r.set(c); w2.hide(); });
    }
    w.show();
    while w.shown() { app::wait(); }
    res.get()
}

/// Voice FX: noise reduction (RNNoise), gate, EQ preset, de-esser, opto compressor.
/// Apply to the selected track (background job) and/or save as the live-recording chain.
fn voice_fx_dialog(app: &Shared, ui: &Ui) {
    let (ti, name) = {
        let a = app.borrow();
        if a.job.is_some() || a.narr.is_some() { return; }
        let t = a.sel_track.and_then(|i| a.proj.as_ref().and_then(|p| p.edit.tracks.get(i)).map(|t| (i, t.name.clone())));
        (t.as_ref().map(|t| t.0), t.map(|t| t.1).unwrap_or_default())
    };
    let fx = load_voice_fx();
    let mut w = Window::default().with_size(520, 380).with_label("Voice FX");
    dlg_title(520, "Voice FX chain");
    dlg_info(38, 520, 40, "Order: EQ (with rumble filter) → noise reduction → gate → de-esser → compressor. \
The same chain can run live while you record (RECORD › Live voice FX).");
    let nr = CheckButton::new(16, 84, 480, 26, "Noise reduction (RNNoise neural filter: fans, hum, keyboard)");
    nr.set_checked(fx.nr);
    let gate = CheckButton::new(16, 116, 220, 26, "Noise gate, threshold (dBFS)");
    gate.set_checked(fx.gate);
    let mut gdb = Spinner::new(250, 116, 80, 26, None);
    gdb.set_range(-80.0, -10.0); gdb.set_step(1.0); gdb.set_value(fx.gate_db as f64);
    Frame::new(16, 150, 120, 26, "EQ preset").with_align(Align::Left | Align::Inside);
    let mut eq = Choice::new(140, 150, 360, 26, None);
    for e in EQ_PRESETS { eq.add_choice(e); }
    eq.set_value(fx.eq as i32);
    Frame::new(16, 184, 120, 26, "De-esser").with_align(Align::Left | Align::Inside);
    let mut ds = HorValueSlider::new(140, 184, 360, 26, None);
    ds.set_range(0.0, 1.0); ds.set_step(0.05, 1); ds.set_value(fx.deess as f64);
    Frame::new(16, 218, 120, 26, "Compressor").with_align(Align::Left | Align::Inside);
    let mut comp = Choice::new(140, 218, 360, 26, None);
    comp.add_choice("Off");
    for (n, _) in OPTO_PRESETS { comp.add_choice(&format!("Opto: {}", esc(n))); }
    comp.set_value(fx.comp as i32);
    for c in [&mut eq, &mut comp] { c.set_color(rgb(C_PANEL)); }
    dlg_info(256, 520, 36, &esc(&match ti {
        Some(_) => format!("Apply processes track '{name}' in the background (one undo step)."),
        None => "Select a track to apply the chain to it; you can still save it for live recording.".into(),
    }));
    let mut cancel = btn("Close", "", None); cancel.resize(16, 334, 90, 32);
    let mut live = btn("Save for live recording", "", None); live.resize(150, 334, 190, 32);
    let mut apply = btn("Apply to track", "", Some(C_FX)); apply.resize(350, 334, 154, 32);
    if ti.is_none() { apply.deactivate(); }
    let code = run_modal(&mut w, &mut [(cancel, 0), (live, 1), (apply, 2)]);
    if code == 0 { return; }
    let v = VoiceFx { nr: nr.is_checked(), gate: gate.is_checked(), gate_db: gdb.value() as f32, eq: eq.value().max(0) as u8,
        deess: ds.value() as f32, comp: comp.value().max(0) as u8 }.sanitized();
    if code == 1 {
        let r = save_voice_fx(v);
        app.borrow_mut().status = match r { Ok(()) => format!("Live voice FX saved: {} (tick RECORD › Live voice FX to use it)", v.summary()), Err(e) => format!("Could not save: {e}") };
        let lf = ui.live_fx.clone();
        if v.active() { lf.set_checked(true); }
        refresh(app, ui);
        return;
    }
    let Some(ti) = ti else { return };
    let _ = save_voice_fx(v); // remember the last-used chain
    let (data, ch) = { let a = app.borrow(); let t = &a.proj.as_ref().unwrap().edit.tracks[ti]; (t.data.clone(), t.ch as usize) };
    app.borrow_mut().set_playing(false);
    spawn_track_job(app, ui, "Voice FX", move |pg, cc| {
        let out = voice_process(&data, ch, v, Some(pg), Some(cc))?;
        Ok((vec![(ti, out, false)], format!("Voice FX on '{name}': {} (Ctrl+Z to undo)", v.summary())))
    });
}

/// Auto-ducking: lower one track (music/system audio) wherever voice tracks are active.
fn duck_dialog(app: &Shared, ui: &Ui) {
    let names: Vec<String> = {
        let a = app.borrow();
        if a.job.is_some() || a.narr.is_some() { return; }
        a.proj.as_ref().map(|p| p.edit.tracks.iter().map(|t| t.name.clone()).collect()).unwrap_or_default()
    };
    if names.len() < 2 { alert("Auto-duck needs at least two tracks: a voice track (mic/narration) and one to lower (system audio/music)."); return; }
    let lname = |s: &str| s.to_lowercase();
    let duck_def = names.iter().position(|n| lname(n).contains("system") || lname(n).contains("music")).unwrap_or(names.len() - 1);
    let voice_def = names.iter().position(|n| lname(n).contains("mic") || lname(n).contains("narr")).filter(|&i| i != duck_def).unwrap_or(if duck_def == 0 { 1 } else { 0 });
    let mut w = Window::default().with_size(500, 300).with_label("Auto-duck");
    dlg_title(500, "Auto-duck");
    dlg_info(38, 500, 40, "Lowers the chosen track smoothly while the voice track is speaking (50 ms look-ahead, \
80 ms attack, 600 ms release), so music and system sound never fight your voice.");
    Frame::new(16, 88, 130, 26, "Lower this track").with_align(Align::Left | Align::Inside);
    let mut duck = Choice::new(150, 88, 334, 26, None);
    Frame::new(16, 122, 130, 26, "Under voice of").with_align(Align::Left | Align::Inside);
    let mut voice = Choice::new(150, 122, 334, 26, None);
    for (i, n) in names.iter().enumerate() {
        let l = format!("{}: {}", i + 1, esc(&n.replace(['/', '|', '&', '_', '\\'], " ")));
        duck.add_choice(&l); voice.add_choice(&l);
    }
    voice.add_choice("All other tracks");
    duck.set_value(duck_def as i32); voice.set_value(voice_def as i32);
    Frame::new(16, 156, 130, 26, "Depth (dB)").with_align(Align::Left | Align::Inside);
    let mut depth = HorValueSlider::new(150, 156, 334, 26, None);
    depth.set_range(3.0, 30.0); depth.set_step(1.0, 1); depth.set_value(12.0);
    Frame::new(16, 190, 130, 26, "Voice threshold").with_align(Align::Left | Align::Inside);
    let mut thr = HorValueSlider::new(150, 190, 334, 26, None);
    thr.set_range(-60.0, -20.0); thr.set_step(1.0, 1); thr.set_value(-40.0);
    for c in [&mut duck, &mut voice] { c.set_color(rgb(C_PANEL)); }
    let mut cancel = btn("Cancel", "", None); cancel.resize(262, 254, 100, 32);
    let mut ok = btn("Duck", "", Some(C_FX)); ok.resize(372, 254, 112, 32);
    if run_modal(&mut w, &mut [(cancel, 0), (ok, 1)]) != 1 { return; }
    let (di, vi) = (duck.value().max(0) as usize, voice.value().max(0) as usize);
    if di == vi { alert("Choose two different tracks."); return; }
    let p = DuckParams { depth_db: depth.value() as f32, thr_db: thr.value() as f32, ..Default::default() };
    let (voices, target, ch, frames, dname) = {
        let a = app.borrow();
        let e = &a.proj.as_ref().unwrap().edit;
        let vs: Vec<MixTrack> = e.tracks.iter().enumerate().filter(|(i, t)| *i != di && (vi >= names.len() || *i == vi) && !t.muted)
            .map(|(_, t)| MixTrack { data: t.data.clone(), ch: t.ch, gain: t.gain }).collect();
        let t = &e.tracks[di];
        (vs, t.data.clone(), t.ch as usize, t.data.len() / t.ch.max(1) as usize, t.name.clone())
    };
    app.borrow_mut().set_playing(false);
    spawn_track_job(app, ui, "Auto-duck", move |pg, _cc| {
        let env = duck_envelope(&voices, frames, p);
        pg.store(600, Relaxed);
        let ducked = env.iter().filter(|&&g| g < 0.9).count() as f64 * DUCK_BLOCK as f64 / AR as f64;
        let out = apply_duck(&target, ch, &env);
        Ok((vec![(di, out, false)], format!("Auto-duck: '{dname}' lowered {:.0} dB under voice ({ducked:.1}s ducked; Ctrl+Z to undo)", p.depth_db)))
    });
}

/// Cursor + click effects and webcam picture-in-picture settings (undoable, saved).
fn effects_dialog(app: &Shared, ui: &Ui) {
    let (fx, has_ev, has_cam) = {
        let a = app.borrow();
        let Some(p) = &a.proj else { return };
        (p.edit.fx, p.src.has_ev(), p.src.cam.is_some())
    };
    let mut w = Window::default().with_size(500, 390).with_label("Effects");
    dlg_title(500, "Cursor & webcam effects");
    dlg_info(38, 500, 34, if has_ev { "Rendered at export only on the frames they touch; everything else is still copied losslessly." }
        else { "This recording has no click/cursor data (recorded before v1.4.0), so cursor effects are unavailable." });
    let mut hl = CheckButton::new(16, 78, 460, 26, "Cursor highlight (soft yellow spotlight)");
    hl.set_checked(fx.cursor_hl);
    let mut rings = CheckButton::new(16, 106, 460, 26, "Click rings (blue = left click, orange = right click)");
    rings.set_checked(fx.click_rings);
    if !has_ev { hl.deactivate(); rings.deactivate(); }
    let cam = CheckButton::new(16, 146, 460, 26, "Show webcam (picture-in-picture)");
    cam.set_checked(fx.cam_show);
    Frame::new(32, 176, 90, 26, "Position").with_align(Align::Left | Align::Inside);
    let mut pos = Choice::new(130, 176, 150, 26, None);
    for p in CAM_POSITIONS { pos.add_choice(p); }
    pos.set_value(fx.cam_pos as i32);
    Frame::new(290, 176, 60, 26, "Shape").with_align(Align::Left | Align::Inside);
    let mut shape = Choice::new(350, 176, 130, 26, None);
    for s in CAM_SHAPES { shape.add_choice(s); }
    shape.set_value(fx.cam_shape as i32);
    Frame::new(32, 208, 90, 26, "Size").with_align(Align::Left | Align::Inside);
    let mut size = HorValueSlider::new(130, 208, 350, 26, None);
    size.set_range(0.10, 0.45); size.set_step(0.01, 1); size.set_value(fx.cam_size as f64);
    let blur = CheckButton::new(32, 240, 440, 26, "Background blur (keeps a soft head-and-shoulders area sharp)");
    blur.set_checked(fx.cam_blur);
    let mirror = CheckButton::new(32, 268, 440, 26, "Mirror (like looking in a mirror)");
    mirror.set_checked(fx.cam_mirror);
    for c in [&mut pos, &mut shape] { c.set_color(rgb(C_PANEL)); }
    if !has_cam {
        for mut wd in [cam.as_base_widget(), pos.as_base_widget(), shape.as_base_widget(), size.as_base_widget(), blur.as_base_widget(), mirror.as_base_widget()] { wd.deactivate(); }
        dlg_info(296, 500, 20, "No webcam was recorded (RECORD › Webcam).");
    }
    let mut cancel = btn("Cancel", "", None); cancel.resize(262, 344, 100, 32);
    let mut ok = btn("Apply", "", Some(C_ACCENT)); ok.resize(372, 344, 112, 32);
    if run_modal(&mut w, &mut [(cancel, 0), (ok, 1)]) != 1 { return; }
    let nf = FxSettings { cursor_hl: hl.is_checked(), click_rings: rings.is_checked(), cam_show: cam.is_checked(), cam_pos: pos.value().clamp(0, 3) as u8,
        cam_size: size.value() as f32, cam_shape: shape.value().clamp(0, 2) as u8, cam_blur: blur.is_checked(), cam_mirror: mirror.is_checked() };
    let mut a = app.borrow_mut();
    if a.proj.as_ref().map(|p| p.edit.fx != nf).unwrap_or(false) {
        a.push_undo();
        if let Some(p) = a.proj.as_mut() { p.edit.fx = nf; }
        a.after_edit();
        a.status = "Effects updated (preview shows them; Ctrl+Z to undo)".into();
    }
    drop(a);
    refresh(app, ui);
}

/// Annotation toolbar actions.
fn op_overlay(app: &Shared, ui: &Ui, what: &str) {
    let mut a = app.borrow_mut();
    if a.proj.is_none() || a.job.is_some() { return; }
    let sel = a.ov_sel;
    let r: Res<()> = match what {
        "del" => match sel {
            Some(i) => { a.push_undo(); a.proj.as_mut().unwrap().edit.ov.remove(i); a.ov_sel = None; a.after_edit(); Ok(()) }
            None => Err("Select an overlay first (click it in the preview or the FX lane)".into()),
        },
        "time" => match (sel, a.sel) {
            (Some(i), Some((s, e))) => {
                a.push_undo();
                if let Some(o) = a.proj.as_mut().unwrap().edit.ov.get_mut(i) { o.start = s; o.end = e.max(s + 1); }
                a.after_edit();
                Ok(())
            }
            (None, _) => Err("Select an overlay first".into()),
            (_, None) => Err("Drag a range on the timeline first: the overlay will show during it".into()),
        },
        "text" => match sel.and_then(|i| a.proj.as_ref().unwrap().edit.ov.get(i).cloned()) {
            Some(Overlay { kind: OvKind::Text { text }, .. }) => {
                drop(a);
                if let Some(t) = edit_text_dialog(&text) {
                    let mut a = app.borrow_mut();
                    a.push_undo();
                    if let Some(o) = a.proj.as_mut().unwrap().edit.ov.get_mut(sel.unwrap()) { o.kind = OvKind::Text { text: t }; }
                    a.after_edit();
                }
                refresh(app, ui);
                return;
            }
            _ => Err("Select a text callout first".into()),
        },
        "color" => {
            if let Some(i) = sel {
                let c = OV_COLORS[a.ov_color.min(5)].1;
                if a.proj.as_ref().unwrap().edit.ov.get(i).map(|o| o.color != c).unwrap_or(false) {
                    a.push_undo();
                    if let Some(o) = a.proj.as_mut().unwrap().edit.ov.get_mut(i) { o.color = c; }
                    a.after_edit();
                }
            }
            Ok(())
        }
        "autozoom" => {
            let p = a.proj.as_ref().unwrap();
            if !p.src.has_ev() { Err("No click/typing data in this recording (recorded before v1.4.0)".into()) } else {
                let z = auto_zoom(&p.src, &p.edit, 2.0);
                let n = z.len();
                if n == 0 { Err("No clicks or typing found to zoom on".into()) } else {
                    a.push_undo();
                    let e = &mut a.proj.as_mut().unwrap().edit;
                    e.ov.retain(|o| !matches!(o.kind, OvKind::Zoom { follow: true, .. }));
                    e.ov.extend(z);
                    a.after_edit();
                    a.status = format!("Auto zoom: {n} zoom-and-follow region(s) from clicks and typing (blue in the FX lane; Ctrl+Z to undo)");
                    Ok(())
                }
            }
        }
        _ => Ok(()),
    };
    if let Err(e) = r { a.status = e; }
    drop(a);
    refresh(app, ui);
}

/// Offer to recover interrupted recordings (crash / power loss / kill during a take).
fn offer_recovery(app: &Shared, ui: &Ui) {
    for dir in find_interrupted().into_iter().take(3) {
        let n = dir.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
        match dialog::choice2_default(&esc(&format!("A recording was interrupted before it was saved:\n{n}\n\nRecover it as a project?")), "Later", "Recover", "Discard") {
            Some(1) => match recover_recording(&dir) {
                Ok(p) => {
                    let secs = p.edit.total() as f64 / p.src.fps as f64;
                    let mut a = app.borrow_mut();
                    a.set_project(Some(p), ui.timeline.w());
                    a.status = format!("Recovered {secs:.1}s from {}", dir.display());
                    return;
                }
                Err(e) => alert(&format!("Recovery failed:\n{e}")),
            },
            // keep the media, just stop asking
            Some(2) => { let _ = std::fs::remove_file(dir.join(REC_MARK)); }
            _ => return,
        }
    }
}

/// Open a project folder, offering to restore a newer autosave (v1.4.0).
fn open_project(path: &Path) -> Res<Project> {
    let dir = if path.is_dir() { path.to_path_buf() } else { path.parent().ok_or("bad path")?.to_path_buf() };
    if has_newer_autosave(&dir) {
        match dialog::choice2_default("This project has unsaved changes from an autosave (the app closed without saving).\nRestore them?", "Ignore", "Restore", "Delete autosave") {
            Some(1) => match Project::load_from(&dir, AUTOSAVE_TXT) {
                Ok(p) => return Ok(p),
                Err(e) => alert(&format!("Autosave could not be read ({e}); opening the last save.")),
            },
            Some(2) => clear_autosave(&dir),
            _ => {}
        }
    }
    Project::load(&dir)
}

fn refresh(app: &Shared, ui: &Ui) {
    let Ok(mut a) = app.try_borrow_mut() else { return };
    let mut ui = ui.clone();
    let has = a.proj.is_some();
    let total = a.total();
    let fps = a.fps();
    let busy = a.job.is_some() || a.narr.is_some();
    for b in [&mut ui.cut, &mut ui.freeze_sel] { if has && a.sel.is_some() && !busy { b.activate() } else { b.deactivate() } }
    for b in [&mut ui.save, &mut ui.ifreeze, &mut ui.play, &mut ui.newt, &mut ui.import, &mut ui.fitz] { if has && !busy { b.activate() } else { b.deactivate() } }
    for b in [&mut ui.exp_mkv, &mut ui.exp_wav] { if has && total > 0 && !busy { b.activate() } else { b.deactivate() } }
    // MOV/MP4 stay clickable without ffmpeg: the click explains how to get/locate it.
    for b in [&mut ui.exp_mov, &mut ui.exp_mp4] { if has && total > 0 && !busy { b.activate() } else { b.deactivate() } }
    for b in [&mut ui.exp_mov, &mut ui.exp_mp4] { b.set_label_color(if a.ffmpeg.is_some() { Color::Foreground } else { Color::from_rgb(120, 120, 120) }); }
    // New/Open are refused while recording/narrating/exporting (checked again in the callback)
    for b in [&mut ui.new, &mut ui.open] { if a.rec.is_none() && !a.rec_pending && !busy { b.activate() } else { b.deactivate() } }
    if has && a.sel.is_some() && !busy { ui.speed_btn.activate() } else { ui.speed_btn.deactivate() }
    let has_tracks = a.proj.as_ref().map(|p| !p.edit.tracks.is_empty()).unwrap_or(false);
    for b in [&mut ui.comp, &mut ui.norm, &mut ui.voicefx, &mut ui.duck] { if has_tracks && !busy { b.activate() } else { b.deactivate() } }
    // v1.4.0 annotate row
    let ovs = a.ov_sel.is_some() && !busy;
    for b in [&mut ui.ov_del, &mut ui.ov_time] { if ovs { b.activate() } else { b.deactivate() } }
    let is_text = a.ov_sel.and_then(|i| a.proj.as_ref().and_then(|p| p.edit.ov.get(i))).map(|o| matches!(o.kind, OvKind::Text { .. })).unwrap_or(false);
    if is_text && !busy { ui.ov_text.activate() } else { ui.ov_text.deactivate() }
    for b in [&mut ui.autozoom, &mut ui.effects] { if has && !busy { b.activate() } else { b.deactivate() } }
    for (i, b) in ui.tools.iter_mut().enumerate() { b.set_value(i as u8 == a.tool); if has && !busy { b.activate() } else { b.deactivate() } }
    for b in [&mut ui.find_dead, &mut ui.compact] { if has && total > 0 && !busy { b.activate() } else { b.deactivate() } }
    let dv = a.dead_valid();
    ui.rm_dead.set_label(&if dv { format!("Remove dead air ({})", a.dead.len()) } else { "Remove dead air".to_string() });
    if dv && !busy { ui.rm_dead.activate() } else { ui.rm_dead.deactivate() }
    let (cu, cr) = a.proj.as_ref().map(|p| (!p.undo.is_empty(), !p.redo.is_empty())).unwrap_or((false, false));
    if cu && !busy { ui.undo.activate() } else { ui.undo.deactivate() }
    if cr && !busy { ui.redo.activate() } else { ui.redo.deactivate() }
    if has && a.job.is_none() { ui.narr.activate() } else { ui.narr.deactivate() }
    ui.narr.set_label(if a.narr.is_some() { "■ Stop" } else { "● Narrate" });
    let st = a.sel_track.is_some() && !busy;
    for b in [&mut ui.mute, &mut ui.shl, &mut ui.shr, &mut ui.replace, &mut ui.remove] { if st { b.activate() } else { b.deactivate() } }
    if st { ui.gain.activate() } else { ui.gain.deactivate() }
    // Track chooser
    let names: Vec<String> = a.proj.as_ref().map(|p| p.edit.tracks.iter().enumerate()
        .map(|(i, t)| format!("{}: {}{}", i + 1, esc(&t.name.replace(['/', '|', '&', '_', '\\'], " ")), if t.muted { " (muted)" } else { "" })).collect()).unwrap_or_default();
    ui.track.clear();
    for n in &names { ui.track.add_choice(n); }
    ui.track.set_value(a.sel_track.map(|i| i as i32).unwrap_or(-1));
    if let (Some(ti), Some(p)) = (a.sel_track, &a.proj) {
        if let Some(t) = p.edit.tracks.get(ti) {
            ui.gain.set_value(t.gain as f64);
            ui.mute.set_label(if t.muted { "Unmute" } else { "Mute" });
        }
    }
    ui.play.set_label(if a.play.playing.load(Relaxed) { "|| Pause" } else { "▶ Play" });
    ui.time.set_label(&format!("{} / {}", fmt_time(a.playhead, fps), fmt_time(total, fps)));
    let dirty = a.proj.as_ref().map(|p| p.dirty).unwrap_or(false);
    let title = match &a.proj {
        Some(p) => format!("{APP_NAME} v{VERSION} — {}{}", esc(&p.src.dir.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default()), if dirty { " *" } else { "" }),
        None => format!("{APP_NAME} v{VERSION}"),
    };
    if ui.win.label() != title { ui.win.set_label(&title); }
    ui.status.set_label(&esc(&a.status));
    request_preview(&mut a, &ui);
    drop(a);
    ui.timeline.redraw();
    ui.preview.redraw();
}

/// Ask the background decoder for the frame under the playhead at the preview's
/// physical size. Deduplicated, so it is cheap to call every tick (also tracks resizes).
fn request_preview(a: &mut App, ui: &Ui) {
    let sc = app::screen_scale(0).max(0.5);
    let (tw, th) = (((ui.preview.w() - 4) as f32 * sc) as i32, ((ui.preview.h() - 4) as f32 * sc) as i32);
    if tw < 8 || th < 8 { return; }
    let req = a.proj.as_ref().and_then(|p| p.edit.src_frame(a.playhead).map(|sf| (p.src.clone(), sf)));
    let ctx = a.rctx.clone();
    let ph = a.playhead;
    if let Some((src, sf)) = req { a.dec.request(&src, sf, ph, ctx.as_ref(), tw, th); } else if a.proj.is_some() && a.preview.is_some() && a.total() == 0 { a.preview = None; }
}

/// FLTK treats '@' in labels as a symbol prefix; escape dynamic text before display.
fn esc(s: &str) -> String { s.replace('@', "@@") }

fn alert(msg: &str) { dialog::alert_default(&esc(msg)); }

fn rec_dir() -> PathBuf {
    let base = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
        .map(PathBuf::from).map(|h| h.join("Videos").join(APP_NAME)).unwrap_or_else(|| PathBuf::from(APP_NAME));
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0) as i64;
    // civil-from-days (H. Hinnant), UTC
    let (days, rem) = (secs.div_euclid(86400), secs.rem_euclid(86400));
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    let mut p = base.join(format!("rec_{y:04}{m:02}{d:02}_{:02}{:02}{:02}", rem / 3600, rem % 3600 / 60, rem % 60));
    let mut k = 1;
    while p.exists() { p = base.join(format!("{}_{k}", p.file_name().unwrap().to_string_lossy())); k += 1; }
    p
}

/// Fullscreen overlay showing a frozen screenshot; drag to choose a region.
fn select_region() -> Res<Option<Rect>> {
    let vs = cap::virtual_screen()?;
    let mut buf = Vec::new();
    {
        let mut g = cap::Grabber::new(vs, false)?;
        g.grab(&mut buf)?;
    }
    let vs = Rect { w: vs.w, h: vs.h, ..vs };
    let bright = RgbImage::new(&bgra_to_rgb(&buf, false), vs.w, vs.h, fltk::enums::ColorDepth::Rgb8).map_err(es)?;
    let dim = RgbImage::new(&bgra_to_rgb(&buf, true), vs.w, vs.h, fltk::enums::ColorDepth::Rgb8).map_err(es)?;
    drop(buf);
    let sc = app::screen_scale(0).max(0.25) as f64;
    let (wx, wy, ww, wh) = ((vs.x as f64 / sc) as i32, (vs.y as f64 / sc) as i32, (vs.w as f64 / sc).ceil() as i32, (vs.h as f64 / sc).ceil() as i32);
    let mut win = Window::new(wx, wy, ww, wh, "Select region");
    win.set_border(false);
    let state = std::rc::Rc::new(std::cell::RefCell::new((None::<(i32, i32)>, (0, 0), 0u8))); // anchor, current, 0=active 1=done 2=cancel
    win.end();
    {
        let st = state.clone();
        let (mut bright, mut dim) = (bright, dim);
        win.draw(move |w| {
            dim.scale(w.w(), w.h(), false, true);
            bright.scale(w.w(), w.h(), false, true);
            dim.draw(0, 0, w.w(), w.h());
            let s = st.borrow();
            draw::set_font(Font::HelveticaBold, 16);
            if let Some((ax, ay)) = s.0 {
                let (cx, cy) = s.1;
                let (x, y, rw, rh) = (ax.min(cx), ay.min(cy), (ax - cx).abs(), (ay - cy).abs());
                draw::push_clip(x, y, rw, rh);
                bright.draw(0, 0, w.w(), w.h());
                draw::pop_clip();
                draw::set_draw_color(Color::from_rgb(255, 80, 60));
                draw::draw_rect(x, y, rw.max(1), rh.max(1));
                let (pw, ph) = ((rw as f64 * vs.w as f64 / w.w() as f64) as i32, (rh as f64 * vs.h as f64 / w.h() as f64) as i32);
                draw::set_draw_color(Color::White);
                draw::draw_text(&format!("{} × {}", pw & !1, ph & !1), x + 4, (y - 6).max(16));
            } else {
                draw::set_draw_color(Color::White);
                draw::draw_text2("Drag to select the recording area  •  Esc / right-click to cancel", 0, 20, w.w(), 30, Align::Center);
            }
        });
    }
    {
        let st = state.clone();
        win.handle(move |w, ev| {
            let (ex, ey) = (app::event_x(), app::event_y());
            match ev {
                Event::Push => {
                    if app::event_mouse_button() == app::MouseButton::Right { st.borrow_mut().2 = 2; w.hide(); return true; }
                    let mut s = st.borrow_mut(); s.0 = Some((ex, ey)); s.1 = (ex, ey); drop(s); w.redraw(); true
                }
                Event::Drag => { st.borrow_mut().1 = (ex, ey); w.redraw(); true }
                Event::Released => {
                    let mut s = st.borrow_mut();
                    s.1 = (ex, ey);
                    if let Some((ax, ay)) = s.0 {
                        if (ax - ex).abs() > 8 && (ay - ey).abs() > 8 { s.2 = 1; drop(s); w.hide(); } else { s.0 = None; drop(s); w.redraw(); }
                    }
                    true
                }
                Event::KeyDown if app::event_key() == Key::Escape => { st.borrow_mut().2 = 2; w.hide(); true }
                Event::Focus | Event::Enter => true,
                _ => false,
            }
        });
    }
    win.make_resizable(false);
    win.show();
    if app::screen_count() == 1 { win.fullscreen(true); }
    win.set_cursor(Cursor::Cross);
    win.set_on_top();
    win.take_focus().ok();
    while win.shown() { app::wait(); }
    let s = state.borrow();
    if s.2 != 1 { return Ok(None); }
    let ((ax, ay), (cx, cy)) = (s.0.unwrap(), s.1);
    let (fx, fy) = (vs.w as f64 / win.w().max(1) as f64, vs.h as f64 / win.h().max(1) as f64);
    let _ = (ww, wh);
    let x = vs.x + (ax.min(cx).max(0) as f64 * fx) as i32;
    let y = vs.y + (ay.min(cy).max(0) as f64 * fy) as i32;
    let w = (((ax - cx).abs() as f64 * fx) as i32).min(vs.x + vs.w - x) & !1; // even dims for yuv420 exports
    let h = (((ay - cy).abs() as f64 * fy) as i32).min(vs.y + vs.h - y) & !1;
    if w < 16 || h < 16 { return Ok(None); }
    Ok(Some(Rect { x, y, w, h }))
}

fn begin_recording(app: &Shared, ui: &Ui, region: Rect) {
    let mic = {
        let a = app.borrow();
        match ui.mic.value() { 0 => None, 1 => Some(None), i => Some(a.mics.get(i as usize - 2).cloned()) }
    };
    let qi = ui.quality.value().max(0) as usize;
    let (quality, s444) = [(60u8, false), (80, false), (92, true)][qi.min(2)];
    let cfg = RecConfig {
        region, fps: FPS_CHOICES[ui.fps.value().clamp(0, FPS_CHOICES.len() as i32 - 1) as usize], quality, s444,
        half: ui.half.is_checked(), cursor: ui.cursor.is_checked(), mic, sys: ui.sys.is_checked(), dir: rec_dir(),
        cam: match ui.cam.value() { i if i >= 1 => app.borrow().cams.get(i as usize - 1).map(|c| Some(c.0.clone())), _ => None },
        live_fx: ui.live_fx.is_checked().then(load_voice_fx).filter(|f| f.active()),
    };
    {
        let mut a = app.borrow_mut();
        if a.proj.as_ref().map(|p| p.dirty).unwrap_or(false) {
            if let Err(e) = a.save_project() { a.status = format!("autosave failed: {e}"); }
        }
        a.set_playing(false);
    }
    // Control window (outside region when possible)
    let sc = app::screen_scale(0).max(0.25) as f64;
    let (sx, sy, sw, sh) = app::screen_xywh(0);
    let (cw, chh) = (380, 44);
    let (rx, ry, rb) = ((region.x as f64 / sc) as i32, (region.y as f64 / sc) as i32, ((region.y + region.h) as f64 / sc) as i32);
    let (cx, cy) = if ry - chh - 8 >= sy { (rx.max(sx), ry - chh - 8) } else if rb + chh + 8 <= sy + sh { (rx.max(sx), rb + 8) } else { (sx + sw - cw - 16, sy + sh - chh - 56) };
    let mut win = Window::new(cx.min(sx + sw - cw), cy, cw, chh, "ScreenReel — recording");
    let mut row = Flex::default_fill().row();
    row.set_margin(6);
    let lbl = Frame::default().with_label("Starting in 3…");
    let mut pause = Button::default().with_label("|| Pause");
    let mut stop = Button::default().with_label("■ Stop");
    row.fixed(&pause, 80);
    row.fixed(&stop, 70);
    row.end();
    win.end();
    win.set_color(Color::from_rgb(40, 20, 20));
    let mut lbl2 = lbl.clone();
    lbl2.set_label_color(Color::White);
    win.show();
    win.set_on_top();
    cap::exclude_from_capture(&win);
    {
        let (a2, u2) = (app.clone(), ui.clone());
        stop.set_callback(move |_| stop_recording(&a2, &u2));
        let a3 = app.clone();
        pause.set_callback(move |b| {
            let a = a3.borrow();
            if let Some(r) = &a.rec { let p = r.sh.paused.toggle(); b.set_label(if p { "▶ Resume" } else { "|| Pause" }); }
        });
        let (a4, u4) = (app.clone(), ui.clone());
        win.set_callback(move |_| if app::event() == Event::Close { stop_recording(&a4, &u4) });
    }
    {
        let mut a = app.borrow_mut();
        a.ctl = Some(Ctl { win, lbl, pause });
        a.rec_pending = true;
    }
    let mut main = ui.win.clone();
    main.hide();
    // 3-2-1 countdown, then start (also lets the region overlay fully unmap).
    let (a, u) = (app.clone(), ui.clone());
    let mut cfg = Some(cfg);
    let mut n = 3;
    app::add_timeout3(1.0, move |h| {
        n -= 1;
        let still = a.borrow().rec_pending;
        if !still { return; }
        if n > 0 {
            if let Some(c) = &mut a.borrow_mut().ctl { c.lbl.set_label(&format!("Starting in {n}…")); }
            app::repeat_timeout3(1.0, h);
            return;
        }
        let r = Recorder::start(cfg.take().unwrap());
        let mut s = a.borrow_mut();
        s.rec_pending = false;
        match r {
            Ok(r) => {
                let mut w = vec![];
                if let Some(e) = &r.mic_error { w.push(format!("mic unavailable ({e})")); }
                if let Some(e) = &r.sys_error { w.push(format!("system audio unavailable ({e})")); }
                if let Some(e) = &r.cam_error { w.push(format!("webcam unavailable ({e})")); }
                if !w.is_empty() { s.status = format!("{} - recording without it", w.join("; ")); }
                else if let Some(c) = &r.camera { s.status = format!("Recording with webcam: {}", c.name); }
                s.rec = Some(r);
            }
            Err(e) => {
                drop(s);
                abort_ctl(&a, &u);
                alert(&format!("Could not start recording:\n{e}"));
            }
        }
    });
}

fn abort_ctl(app: &Shared, ui: &Ui) {
    if let Some(mut c) = app.borrow_mut().ctl.take() { c.win.hide(); app::delete_widget(c.win); }
    let mut w = ui.win.clone();
    w.show();
    refresh(app, ui);
}

fn stop_recording(app: &Shared, ui: &Ui) {
    let (rec, pending) = { let mut a = app.borrow_mut(); let p = a.rec_pending; a.rec_pending = false; (a.rec.take(), p) };
    if let Some(c) = &mut app.borrow_mut().ctl { c.lbl.set_label("Finalizing…"); c.win.redraw(); }
    app::flush();
    let Some(rec) = rec else { if pending { abort_ctl(app, ui); } return };
    let (fps, dims, dir) = (rec.fps, rec.dims, rec.dir.clone());
    let err = rec.sh.error.lock().unwrap_or_else(|e| e.into_inner()).clone() // v1.3.1 FIX: poison-tolerant
        .or_else(|| rec.mic_error.clone().map(|e| format!("mic unavailable: {e}")))
        .or_else(|| rec.sys_error.clone().map(|e| format!("system audio unavailable: {e}")))
        .or_else(|| rec.cam_error.clone().map(|e| format!("webcam unavailable: {e}")));
    let dropped = rec.sh.dups.load(Relaxed);
    let res = rec.finish();
    abort_ctl(app, ui);
    match res.and_then(|RecOut { frames, mic, sys, warn }| {
        let err = err.clone().or(warn);
        let src = Arc::new(Source::open(&dir, dims.0, dims.1, fps)?);
        let mut tracks = Vec::new();
        // Separate tracks: mic and system audio can be muted/replaced/re-recorded independently.
        if let Some((ch, m)) = mic { tracks.push(Track { name: "Mic".into(), ch, data: Arc::new(m), gain: 1.0, muted: false }); }
        if let Some((ch, v)) = sys { tracks.push(Track { name: "System".into(), ch, data: Arc::new(v), gain: 1.0, muted: false }); }
        let mut p = Project { edit: Edit { segs: vec![Seg::Src { start: 0, len: src.count() }], tracks, ..Default::default() }, src, undo: vec![], redo: vec![], dirty: true, saved: vec![] };
        p.save()?;
        clear_rec_spill(&dir); // v1.4.0: tracks are in WAVs now; drop raw spill + recovery marker
        Ok((p, frames, err))
    }) {
        Ok((p, frames, err)) => {
            let mut a = app.borrow_mut();
            let secs = frames as f64 / fps as f64;
            a.status = format!("Recorded {secs:.1}s ({}×{} @ {fps} fps, {dropped} dropped/duplicated) → {}{}", dims.0, dims.1, dir.display(),
                err.map(|e| format!("  [warning: {e}]")).unwrap_or_default());
            // v1.3.1 FIX: go through set_project like New/Open. v1.3.0 assigned `proj` directly,
            // so edit_gen was not bumped: dead-air ranges found on the *previous* project still
            // counted as valid (red bands + "Remove dead air" enabled) and could be cut from the
            // new recording; peaks/FX-audition state also leaked across projects.
            let status = std::mem::take(&mut a.status);
            a.set_project(Some(p), ui.timeline.w());
            a.status = status;
        }
        Err(e) => alert(&format!("Recording failed:\n{e}")),
    }
    refresh(app, ui);
}

fn choose_file(title: &str, filter: &str, save: Option<&str>) -> Option<PathBuf> {
    let mut d = dialog::NativeFileChooser::new(if save.is_some() { dialog::NativeFileChooserType::BrowseSaveFile } else { dialog::NativeFileChooserType::BrowseFile });
    d.set_title(title);
    d.set_filter(filter);
    if let Some(n) = save { d.set_preset_file(n); d.set_option(dialog::NativeFileChooserOptions::SaveAsConfirm); }
    d.show();
    let f = d.filename();
    if f.as_os_str().is_empty() { None } else { Some(std::path::absolute(&f).unwrap_or(f)) }
}

/// v1.3.1: same directory, tolerant of relative/symlinked spellings.
fn same_dir(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) { (Ok(x), Ok(y)) => x == y, _ => a == b }
}

fn with_ext(p: PathBuf, ext: &str) -> PathBuf {
    if p.extension().map(|e| e.eq_ignore_ascii_case(ext)).unwrap_or(false) { p } else { let mut s = p.into_os_string(); s.push("."); s.push(ext); PathBuf::from(s) }
}

// ---- Audio FX dialogs ------------------------------------------------------------------

struct CompDlg { params: OptoParams, done: Option<(OptoParams, Arc<Vec<i16>>, FxStats)>, gen: u64 }

/// Opto compressor dialog: presets, six controls, live transfer curve, A/B audition through
/// the real playback mixer (the processed track replaces the stored one until Cancel/Apply).
fn comp_dialog(app: &Shared, ui: &Ui) {
    let pick = {
        let mut a = app.borrow_mut();
        if a.job.is_some() || a.narr.is_some() { return; }
        let sel = a.sel_track;
        let Some(p) = a.proj.as_ref() else { return };
        if p.edit.tracks.is_empty() { None } else {
            // default target: the selected track, else the mic/narration track, else track 1
            let ti = sel.filter(|&i| i < p.edit.tracks.len())
                .or_else(|| p.edit.tracks.iter().position(|t| t.name.starts_with("Mic") || t.name.starts_with("Narration")))
                .unwrap_or(0);
            let t = &p.edit.tracks[ti];
            let r = (ti, t.name.clone(), t.ch as usize, t.data.clone());
            a.sel_track = Some(ti);
            Some(r)
        }
    };
    let Some((ti, name, ch, orig)) = pick else { alert("No audio tracks yet. Record with a mic or add an audio file first."); return; };
    refresh(app, ui);

    let st = std::rc::Rc::new(std::cell::RefCell::new(CompDlg { params: OPTO_PRESETS[0].1, done: None, gen: 0 }));
    let mut w = Window::default().with_size(640, 410).with_label("Opto Compressor");
    let mut title = Frame::new(16, 10, 608, 26, None).with_align(Align::Left | Align::Inside);
    title.set_label(&esc(&format!("Opto compressor (warm CL 1B style)  ·  track: {name}")));
    title.set_label_font(Font::HelveticaBold); title.set_label_size(15);
    let mut sub = Frame::new(16, 34, 608, 18, None).with_align(Align::Left | Align::Inside);
    sub.set_label("Optical gain cell with program-dependent release and a tube make-up stage");
    sub.set_label_size(11); sub.set_label_color(rgb(C_CAPTION));
    Frame::new(16, 62, 70, 26, "Preset").with_align(Align::Left | Align::Inside);
    let mut preset = Choice::new(96, 62, 262, 26, None);
    for (n, _) in OPTO_PRESETS.iter() { preset.add_choice(n); }
    preset.set_value(0);
    preset.set_color(rgb(C_PANEL));
    type Get = fn(&OptoParams) -> f64;
    type Set = fn(&mut OptoParams, f64);
    let specs: [(&str, f64, f64, f64, Get, Set, &str); 6] = [
        ("Threshold dB", -60.0, 0.0, 0.5, |p| p.threshold_db as f64, |p, v| p.threshold_db = v as f32, "Level where compression starts (soft 8 dB knee)"),
        ("Ratio", 1.0, 20.0, 0.1, |p| p.ratio as f64, |p, v| p.ratio = v as f32, "Compression ratio above threshold"),
        ("Attack ms", 0.5, 200.0, 0.5, |p| p.attack_ms as f64, |p, v| p.attack_ms = v as f32, "How fast the cell reacts"),
        ("Release s", 0.05, 5.0, 0.01, |p| p.release_s as f64, |p, v| p.release_s = v as f32, "Slow release; the opto cell releases slower after long/deep compression"),
        ("Warmth %", 0.0, 100.0, 1.0, |p| p.warmth as f64 * 100.0, |p, v| p.warmth = (v / 100.0) as f32, "Tube make-up stage: even-harmonic saturation"),
        ("Make-up dB", -12.0, 24.0, 0.5, |p| p.makeup_db as f64, |p, v| p.makeup_db = v as f32, "Output gain after compression"),
    ];
    let mut sliders: Vec<HorValueSlider> = Vec::new();
    for (k, (lbl, lo, hi, step, get, _, tip)) in specs.iter().enumerate() {
        let y = 100 + k as i32 * 38;
        Frame::new(16, y, 96, 28, None).with_label(lbl).with_align(Align::Left | Align::Inside);
        let mut sl = HorValueSlider::new(116, y, 242, 28, None);
        sl.set_range(*lo, *hi); sl.set_step(*step, 1); sl.set_value(get(&st.borrow().params));
        sl.set_tooltip(tip);
        sl.set_color(rgb(C_FIELD)); sl.set_selection_color(rgb(C_FX)); sl.set_text_color(rgb(C_TEXT)); sl.set_text_size(12);
        sliders.push(sl);
    }
    let mut curve = Frame::new(384, 62, 240, 268, None);
    let mut stats = Frame::new(16, 336, 608, 20, None).with_align(Align::Left | Align::Inside);
    stats.set_label_size(12); stats.set_label_color(rgb(C_CAPTION));
    stats.set_label("Press Audition to hear it on the video, toggle A/B to compare, then Apply.");
    let mut ab = CheckButton::new(16, 368, 170, 28, "A/B: processed");
    ab.set_checked(true);
    ab.set_tooltip("Ticked: hear the compressed track. Unticked: the original. Switch while playing.");
    let mut aud = btn("▶ Audition", "Play from the playhead with the compressor applied", Some(C_PLAY));
    aud.resize(196, 366, 130, 32);
    let mut cancel = btn("Cancel", "", None);
    cancel.resize(400, 366, 100, 32);
    let mut apply = btn("Apply", "Apply to the track (undo with Ctrl+Z)", Some(C_ACCENT));
    apply.resize(512, 366, 112, 32);
    w.end();
    w.make_modal(true);

    {
        let st = st.clone();
        curve.draw(move |f| {
            let p = st.borrow().params.sanitized();
            let (x, y, wd, ht) = (f.x(), f.y(), f.w(), f.h());
            draw::draw_rect_fill(x, y, wd, ht, rgb(C_FIELD));
            let px = |db: f32| x + (((db + 60.0) / 66.0).clamp(0.0, 1.0) * (wd - 1) as f32) as i32;
            let py = |db: f32| y + ht - 1 - (((db + 60.0) / 66.0).clamp(0.0, 1.0) * (ht - 1) as f32) as i32;
            draw::set_draw_color(Color::from_rgb(52, 56, 64));
            for g in [-48.0f32, -36.0, -24.0, -12.0, 0.0] { draw::draw_yxline(px(g), y, y + ht); draw::draw_xyline(x, py(g), x + wd); }
            draw::set_draw_color(Color::from_rgb(80, 84, 94));
            draw::draw_line(px(-60.0), py(-60.0), px(6.0), py(6.0));
            // threshold marker
            draw::set_draw_color(Color::from_rgb(110, 90, 60));
            draw::draw_yxline(px(p.threshold_db), y, y + ht);
            draw::set_draw_color(rgb(C_FX));
            draw::set_line_style(draw::LineStyle::Solid, 2);
            let mut last: Option<(i32, i32)> = None;
            let mut d = -60.0f32;
            while d <= 6.0 {
                let o = d - p.static_gr(d) + p.makeup_db;
                let pt = (px(d), py(o));
                if let Some(l) = last { draw::draw_line(l.0, l.1, pt.0, pt.1); }
                last = Some(pt);
                d += 0.5;
            }
            draw::set_line_style(draw::LineStyle::Solid, 0);
            draw::set_font(Font::Helvetica, 10);
            draw::set_draw_color(rgb(C_CAPTION));
            draw::draw_text("in dB →", x + wd - 46, y + ht - 4);
            draw::draw_text("out", x + 4, y + 12);
        });
    }
    // (Re)process if the params changed since the last run. O(n), ~1 s for 10 min of audio.
    let process = {
        let (st, orig) = (st.clone(), orig.clone());
        move || -> (Arc<Vec<i16>>, FxStats) {
            let p = st.borrow().params;
            if let Some((dp, d, s)) = &st.borrow().done { if *dp == p { return (d.clone(), *s); } }
            draw::set_cursor(Cursor::Wait); app::flush();
            let (v, s) = opto_compress(&orig, ch, p);
            draw::set_cursor(Cursor::Default);
            let d = Arc::new(v);
            st.borrow_mut().done = Some((p, d.clone(), s));
            (d, s)
        }
    };
    let show_stats = |stats: &mut Frame, s: FxStats| {
        let db = |x: f32| 20.0 * x.max(1e-6).log10();
        stats.set_label(&format!("Gain reduction: max {:.1} dB, average {:.1} dB   ·   peak in {:.1} dBFS → out {:.1} dBFS",
            s.max_gr_db, s.avg_gr_db, db(s.in_peak), db(s.out_peak)));
    };
    // Push the current A/B choice into the playback mixer.
    let route = {
        let (a, st, ab) = (app.clone(), st.clone(), ab.clone());
        move || {
            let mut s = a.borrow_mut();
            s.fx_preview = if ab.is_checked() { st.borrow().done.as_ref().map(|(_, d, _)| (ti, d.clone())) } else { None };
            s.sync_mix();
        }
    };
    // Debounced re-process while auditioning (slider drags fire many callbacks).
    let on_change = {
        let (a, st, curve, stats, process, route) = (app.clone(), st.clone(), curve.clone(), stats.clone(), process.clone(), route.clone());
        std::rc::Rc::new(move || {
            let mut c = curve.clone(); c.redraw();
            let g = { let mut d = st.borrow_mut(); d.gen += 1; d.gen };
            let (a, st, stats, process, route) = (a.clone(), st.clone(), stats.clone(), process.clone(), route.clone());
            app::add_timeout3(0.3, move |_| {
                if st.borrow().gen != g { return; }
                let auditioning = a.borrow().fx_preview.is_some() || a.borrow().play.playing.load(Relaxed);
                if auditioning {
                    let (_, s) = process();
                    let mut sf = stats.clone();
                    let db = |x: f32| 20.0 * x.max(1e-6).log10();
                    sf.set_label(&format!("Gain reduction: max {:.1} dB, average {:.1} dB   ·   peak in {:.1} dBFS → out {:.1} dBFS",
                        s.max_gr_db, s.avg_gr_db, db(s.in_peak), db(s.out_peak)));
                    route();
                }
            });
        })
    };
    for (k, sl) in sliders.iter_mut().enumerate() {
        let (st, oc, set) = (st.clone(), on_change.clone(), specs[k].5);
        sl.set_callback(move |s| { set(&mut st.borrow_mut().params, s.value()); oc(); });
    }
    {
        let (st, oc, sls) = (st.clone(), on_change.clone(), sliders.clone());
        preset.set_callback(move |c| {
            let Some((_, p)) = OPTO_PRESETS.get(c.value().max(0) as usize) else { return };
            st.borrow_mut().params = *p;
            for (k, mut sl) in sls.clone().into_iter().enumerate() { sl.set_value((specs[k].4)(p)); }
            oc();
        });
    }
    { let route = route.clone(); ab.set_callback(move |_| route()); }
    {
        let (a, u, process, route, mut stats) = (app.clone(), ui.clone(), process.clone(), route.clone(), stats.clone());
        aud.set_callback(move |b| {
            let playing = a.borrow().play.playing.load(Relaxed);
            if playing { a.borrow_mut().set_playing(false); b.set_label("▶ Audition"); return; }
            let (_, s) = process();
            show_stats(&mut stats, s);
            route();
            a.borrow_mut().set_playing(true);
            b.set_label("■ Stop");
            refresh(&a, &u);
        });
    }
    let close = {
        let (a, u, w) = (app.clone(), ui.clone(), w.clone());
        move || {
            { let mut s = a.borrow_mut(); s.set_playing(false); s.fx_preview = None; s.sync_mix(); }
            let mut w = w.clone(); w.hide();
            refresh(&a, &u);
        }
    };
    { let close = close.clone(); cancel.set_callback(move |_| close()); }
    { let close = close.clone(); w.set_callback(move |_| close()); }
    {
        let (a, close, name) = (app.clone(), close.clone(), name.clone());
        let (u, st, orig) = (ui.clone(), st.clone(), orig.clone());
        apply.set_callback(move |_| {
            // v1.4.0: an up-to-date auditioned result is applied at once; otherwise the
            // compressor runs on the job thread with a progress bar instead of freezing the UI.
            let p = st.borrow().params;
            let cached = st.borrow().done.as_ref().filter(|(dp, _, _)| *dp == p).map(|(_, d, s)| (d.clone(), *s));
            close();
            match cached {
                Some((d, s)) => {
                    let mut st2 = a.borrow_mut();
                    let v = Arc::try_unwrap(d).unwrap_or_else(|d| (*d).clone());
                    st2.set_track_samples(ti, v, false);
                    st2.status = format!("Opto compressor on '{name}': max gain reduction {:.1} dB, avg {:.1} dB (Ctrl+Z to undo)", s.max_gr_db, s.avg_gr_db);
                }
                None => {
                    let (orig, name) = (orig.clone(), name.clone());
                    spawn_track_job(&a, &u, "Compressing", move |_, cc| {
                        let (v, s) = opto_compress(&orig, ch, p);
                        if cc.load(Relaxed) { return Err("cancelled".into()); }
                        Ok((vec![(ti, v, false)], format!("Opto compressor on '{name}': max gain reduction {:.1} dB, avg {:.1} dB (Ctrl+Z to undo)", s.max_gr_db, s.avg_gr_db)))
                    });
                }
            }
        });
    }
    w.show();
    // keep the audition button label in sync when playback ends on its own
    while w.shown() {
        app::wait();
        let playing = app.try_borrow().map(|s| s.play.playing.load(Relaxed)).unwrap_or(false);
        if !playing && aud.label() != "▶ Audition" { aud.set_label("▶ Audition"); }
    }
    refresh(app, ui);
}

/// v1.4.0: loudness normalization as a pure function over a snapshot of the tracks, so it
/// runs on the job thread (v1.3.x blocked the UI for the gain + limiter pass on long tracks).
/// Progress advances per track; Cancel is honoured between tracks.
fn normalize_tracks(tracks: Vec<(usize, String, usize, Arc<Vec<i16>>)>, target: f64, pg: &AtomicU32, cc: &AtomicBool)
    -> Res<(TrackChanges, String)> {
    let ceiling = 10f32.powf(-1.0 / 20.0);
    let (mut out, mut report) = (Vec::new(), Vec::new());
    let n = tracks.len().max(1);
    for (k, (i, name, ch, data)) in tracks.into_iter().enumerate() {
        if cc.load(Relaxed) { return Err("cancelled".into()); }
        match loudness_lufs(&data, ch) {
            None => report.push(format!("{name}: silent, skipped")),
            Some(l) => {
                // cap at +30 dB so a near-silent track is not turned into amplified noise
                let g_db = (target - l).clamp(-40.0, 30.0);
                let (v, lim) = gain_with_limiter(&data, ch, 10f32.powf(g_db as f32 / 20.0), ceiling);
                report.push(format!("{name} {l:.1}→{target:.0} LUFS ({g_db:+.1} dB{})", if lim > 0.1 { format!(", limited {lim:.1} dB") } else { String::new() }));
                out.push((i, v, true)); // true: reset the track's gain fader to 1.0
            }
        }
        pg.store(((k + 1) * 1000 / n) as u32, Relaxed);
    }
    Ok((out, format!("Normalized: {}", report.join(" · "))))
}

/// Loudness normalization dialog: shows each track's measured LUFS, target + scope.
fn norm_dialog(app: &Shared, ui: &Ui) {
    let measured: Vec<String> = {
        let a = app.borrow();
        if a.job.is_some() || a.narr.is_some() { return; }
        let Some(p) = &a.proj else { return };
        if p.edit.tracks.is_empty() { drop(a); alert("No audio tracks to normalize."); return; }
        p.edit.tracks.iter().map(|t| match loudness_lufs(&t.data, t.ch as usize) {
            Some(l) => format!("{}: {:.1} LUFS", t.name, l), None => format!("{}: silent", t.name) }).collect()
    };
    let has_sel = app.borrow().sel_track.is_some();
    let mut w = Window::default().with_size(500, 300).with_label("Normalize levels");
    let mut title = Frame::new(16, 10, 468, 26, "Normalize levels (loudness)").with_align(Align::Left | Align::Inside);
    title.set_label_font(Font::HelveticaBold); title.set_label_size(15);
    let mut info = Frame::new(16, 38, 468, 54, None).with_align(Align::Left | Align::Inside | Align::Wrap | Align::Top);
    info.set_label("Measures each track's integrated loudness (ITU-R BS.1770, gated) and brings it to the target. A look-ahead limiter keeps peaks under -1 dBFS. One undo step.");
    info.set_label_size(12); info.set_label_color(rgb(C_CAPTION));
    Frame::new(16, 100, 80, 28, "Target").with_align(Align::Left | Align::Inside);
    let mut target = Choice::new(100, 100, 384, 28, None);
    let targets = [-14.0, -16.0, -19.0, -23.0];
    target.add_choice("-14 LUFS · YouTube and streaming|-16 LUFS · voice and tutorials (recommended)|-19 LUFS · quieter, background|-23 LUFS · EBU R128 broadcast");
    target.set_value(1);
    Frame::new(16, 136, 80, 28, "Apply to").with_align(Align::Left | Align::Inside);
    let mut scope = Choice::new(100, 136, 384, 28, None);
    scope.add_choice("All tracks (same level)");
    if has_sel { scope.add_choice("Selected track only"); }
    scope.set_value(0);
    for c in [&mut target, &mut scope] { c.set_color(rgb(C_PANEL)); }
    let mut cur = Frame::new(16, 172, 468, 72, None).with_align(Align::Left | Align::Inside | Align::Wrap | Align::Top);
    cur.set_label(&esc(&format!("Now:  {}", measured.join("   ·   "))));
    cur.set_label_size(12);
    let mut cancel = btn("Cancel", "", None); cancel.resize(262, 254, 100, 32);
    let mut ok = btn("Normalize", "", Some(C_ACCENT)); ok.resize(372, 254, 112, 32);
    w.end();
    w.make_modal(true);
    let go = std::rc::Rc::new(std::cell::Cell::new(false));
    { let (mut w2, g) = (w.clone(), go.clone()); ok.set_callback(move |_| { g.set(true); w2.hide(); }); }
    { let mut w2 = w.clone(); cancel.set_callback(move |_| w2.hide()); }
    w.show();
    while w.shown() { app::wait(); }
    if !go.get() { return; }
    let t = targets[target.value().clamp(0, 3) as usize];
    let only = scope.value() == 1;
    let snap = {
        let mut a = app.borrow_mut();
        a.set_playing(false);
        let sel = a.sel_track;
        let Some(p) = a.proj.as_ref() else { return };
        let idx: Vec<usize> = if only { sel.into_iter().filter(|&i| i < p.edit.tracks.len()).collect() } else { (0..p.edit.tracks.len()).collect() };
        idx.into_iter().map(|i| { let t = &p.edit.tracks[i]; (i, t.name.clone(), t.ch as usize, t.data.clone()) }).collect::<Vec<_>>()
    };
    if snap.is_empty() { app.borrow_mut().status = "Select a track first".into(); refresh(app, ui); return; }
    spawn_track_job(app, ui, "Normalizing", move |pg, cc| normalize_tracks(snap, t, pg, cc));
}

/// Unsaved-changes guard for New/Open. Returns true when it is OK to drop the project.
fn may_close_project(a: &Shared) -> bool {
    {
        let s = a.borrow();
        if s.rec.is_some() || s.rec_pending || s.narr.is_some() || s.job.is_some() {
            drop(s);
            alert("Finish the current recording, narration or background job first.");
            return false;
        }
    }
    let dirty = a.borrow().proj.as_ref().map(|p| p.dirty).unwrap_or(false);
    if !dirty { return true; }
    match dialog::choice2_default("Save changes to the current project first?", "Cancel", "Save", "Don't save") {
        Some(1) => {
            let r = a.borrow_mut().save_project();
            if let Err(e) = r { alert(&format!("Save failed: {e}")); return false; }
            true
        }
        Some(2) => {
            // v1.4.0: "Don't save" also drops the autosave (the user discarded those edits)
            let mut s = a.borrow_mut();
            if let Some(h) = s.autosave.take() { let _ = h.join(); }
            if let Some(p) = &s.proj { clear_autosave(&p.src.dir); }
            true
        }
        _ => false,
    }
}

/// Make sure ffmpeg is known before a MOV/MP4 export; offers search-again / locate.
fn ensure_ffmpeg(a: &Shared) -> Option<PathBuf> {
    if let Some(p) = a.borrow().ffmpeg.clone() { return Some(p); }
    loop {
        let hint = if cfg!(windows) {
            "Install it with:  winget install Gyan.FFmpeg\n(or scoop install ffmpeg / choco install ffmpeg),\nor put ffmpeg.exe next to ScreenReel.exe."
        } else { "Install it with your package manager, e.g.  sudo apt install ffmpeg" };
        let msg = format!("MOV and MP4 export use ffmpeg, which was not found.\n\n{hint}\n\nSearched: SCREENREEL_FFMPEG, saved location, ScreenReel's folder, PATH{}.\n\nMKV and WAV export never need ffmpeg.",
            if cfg!(windows) { ", WinGet Links, Scoop shims, Chocolatey, Program Files" } else { ", /usr/bin, /usr/local/bin, /snap/bin" });
        match dialog::choice2_default(&esc(&msg), "Cancel", "Locate ffmpeg…", "Search again") {
            Some(1) => {
                let filter = if cfg!(windows) { "ffmpeg\tffmpeg.exe" } else { "ffmpeg\tffmpeg*" };
                let Some(p) = choose_file("Locate ffmpeg", filter, None) else { continue };
                if ffmpeg_works(&p) {
                    if let Err(e) = save_ffmpeg_choice(&p) { a.borrow_mut().status = format!("Using {} (could not remember it: {e})", p.display()); }
                    a.borrow_mut().ffmpeg = Some(p.clone());
                    return Some(p);
                }
                alert(&format!("{} did not run as ffmpeg.", p.display()));
            }
            Some(2) => { if let Some(p) = find_ffmpeg() { a.borrow_mut().ffmpeg = Some(p.clone()); return Some(p); } }
            _ => return None,
        }
    }
}

/// Spawn a background job and register it (one job at a time).
fn spawn_job(app: &Shared, ui: &Ui, label: &str, f: impl FnOnce(&AtomicU32, &AtomicBool) -> Res<JobOut> + Send + 'static) {
    let progress = Arc::new(AtomicU32::new(0));
    let cancel = Arc::new(AtomicBool::new(false));
    let (pg, cc) = (progress.clone(), cancel.clone());
    let handle = thread::spawn(move || f(&pg, &cc));
    let mut a = app.borrow_mut();
    a.job = Some(Job { progress, cancel, handle, label: label.to_string() });
    a.status = format!("{label}…");
    drop(a);
    refresh(app, ui);
}

fn start_dead_scan(app: &Shared, ui: &Ui) {
    let (src, edit, gen, fps) = {
        let mut a = app.borrow_mut();
        if a.job.is_some() { return; }
        a.set_playing(false);
        let Some(p) = &a.proj else { return };
        (p.src.clone(), p.edit.clone(), a.edit_gen, p.src.fps)
    };
    let Some(v) = dialog::input_default("Find dead air: minimum length in seconds\n(silent on all audible tracks AND no picture change)", "2") else { return };
    let secs: f64 = match v.trim().parse::<f64>() { Ok(x) if x.is_finite() && (0.3..=3600.0).contains(&x) => x, _ => { alert("Enter a number of seconds between 0.3 and 3600."); return; } };
    let cfg = DeadCfg { min_frames: (secs * fps as f64).round() as u32, pad_frames: (fps / 5).max(1), ..DeadCfg::default() };
    spawn_job(app, ui, "Finding dead air", move |pg, cc| {
        find_dead_air(&src, &edit, cfg, pg, cc).map(|ranges| JobOut::DeadAir { ranges, gen })
    });
}

fn start_compact(app: &Shared, ui: &Ui) {
    {
        let a = app.borrow();
        if a.job.is_some() || a.proj.is_none() { return; }
    }
    let msg = "Compact project?\n\nThe recording is rewritten to hold only the frames your edit uses, so cut and\ndead-air footage stops taking disk space. It is lossless (JPEG bytes are copied),\nbut the edit is flattened, and undo history is cleared. The project is saved.";
    if dialog::choice2_default(msg, "Cancel", "Compact", "") != Some(1) { return; }
    // v1.3.1 FIX: no unwrap (project may have been closed while the dialog was open) + gen.
    let (src, edit, gen) = {
        let mut a = app.borrow_mut();
        a.set_playing(false);
        let Some(p) = a.proj.as_ref() else { return };
        (p.src.clone(), p.edit.clone(), a.edit_gen)
    };
    spawn_job(app, ui, "Compacting", move |pg, _| {
        src.compact(&edit, pg).map(|(src, before, after)| JobOut::Compact { src, before, after, gen })
    });
}

fn start_export(app: &Shared, ui: &Ui, kind: &'static str) {
    let (src, edit, base) = {
        let a = app.borrow();
        let Some(p) = &a.proj else { return };
        if a.job.is_some() { return; }
        (p.src.clone(), p.edit.clone(), p.src.dir.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or("export".into()))
    };
    app.borrow_mut().set_playing(false);
    let ff = if kind == "mov" || kind == "mp4" { match ensure_ffmpeg(app) { Some(p) => Some(p), None => { refresh(app, ui); return; } } } else { None };
    let (filter, ext) = match kind { "mkv" => ("Matroska\t*.mkv", "mkv"), "mov" => ("QuickTime MOV\t*.mov", "mov"), "mp4" => ("MP4\t*.mp4", "mp4"), _ => ("WAV audio\t*.wav", "wav") };
    let Some(path) = choose_file("Export", filter, Some(&format!("{base}.{ext}"))) else { return };
    let path = with_ext(path, ext);
    // v1.3.1 FIX: refuse to export over the project's own files (e.g. "track_0.wav" in the
    // project folder would silently replace an audio track that Save/Open rely on).
    if path.parent().map(|d| same_dir(d, &src.dir)).unwrap_or(false) {
        let n = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        if n.starts_with("track_") || n.starts_with("project.txt") || n.starts_with(&format!("{}.", src.base)) {
            alert(&format!("{n} is one of this project's own files. Choose another name or folder."));
            return;
        }
    }
    let dur_us = edit.total() as u64 * 1_000_000 / src.fps.max(1) as u64;
    spawn_job(app, ui, &format!("Exporting {}", kind.to_uppercase()), move |pg, cc| {
        let t0 = Instant::now();
        match (kind, ff) {
            ("mkv", _) => export_mkv(&path, &src, &edit, pg, cc, (0, 1000))?,
            ("wav", _) => export_wav(&path, &src, &edit, pg, cc)?,
            (_, Some(ff)) => {
                let tmp = path.with_extension("srtmp.mkv");
                // MOV is a quick remux; MP4 re-encodes, so ffmpeg gets most of the bar.
                let split = if kind == "mov" { 500 } else { 150 };
                let r = export_mkv(&tmp, &src, &edit, pg, cc, (0, split))
                    .and_then(|_| ffmpeg_convert(&ff, &tmp, &path, kind == "mov", dur_us, pg, cc, (split, 1000)));
                let _ = std::fs::remove_file(&tmp); // v1.3.1 FIX: also on MKV-phase errors
                let enc = r?;
                if !enc.is_empty() {
                    return Ok(JobOut::Msg(format!("Exported {} in {:.2}s (video: {enc})", path.display(), t0.elapsed().as_secs_f64())));
                }
            }
            _ => return Err("ffmpeg not available".into()),
        }
        Ok(JobOut::Msg(format!("Exported {} in {:.2}s", path.display(), t0.elapsed().as_secs_f64())))
    });
}

/// Swap in the compacted media: flatten the edit to one Src run, clear undo, save, and
/// delete the old media pair (best effort; the decoder may still hold it open briefly).
fn apply_compact(a: &mut App, src: Source, before: u64, after: u64, gen: u64) -> String {
    let total = src.count();
    // v1.3.1 FIX: the compacted media matches the edit *as it was when the job started*.
    // v1.3.0 flattened the current segs to it unconditionally, so any change made meanwhile
    // (track nudge, gain, narration...) was applied to the wrong frames and audio desynced.
    // Also guard against the project having been replaced. Discard the new media in that case.
    let same_proj = a.proj.as_ref().map(|p| p.src.dir == src.dir).unwrap_or(false);
    if gen != a.edit_gen || !same_proj {
        if src.base != a.proj.as_ref().map(|p| p.src.base.clone()).unwrap_or_default() || !same_proj {
            for ext in MEDIA_EXTS { let _ = std::fs::remove_file(src.dir.join(format!("{}.{ext}", src.base))); }
        }
        return "Compact discarded: the project changed while compacting. Run it again.".into();
    }
    let Some(p) = a.proj.as_mut() else { return "Compact finished but the project was closed".into() };
    let old = p.src.clone();
    p.src = Arc::new(src);
    p.edit.segs = vec![Seg::Src { start: 0, len: total }];
    p.undo.clear(); p.redo.clear();
    p.dirty = true;
    let saved = p.save();
    a.edit_gen += 1;
    a.preview = None;
    a.after_edit();
    if let Err(e) = saved { return format!("Compacted, but saving failed: {e} (old media kept)"); }
    if old.base != a.proj.as_ref().unwrap().src.base {
        for ext in MEDIA_EXTS { let _ = std::fs::remove_file(old.dir.join(format!("{}.{ext}", old.base))); }
    }
    let mb = |b: u64| b as f64 / 1_048_576.0;
    format!("Compacted: {:.1} MB → {:.1} MB ({:.1} MB freed)", mb(before), mb(after), mb(before.saturating_sub(after)))
}

/// UI heartbeat (~60 Hz): playback position, decoder results, export + recording status.
fn tick(app: &Shared, ui: &Ui) {
    let Ok(mut a) = app.try_borrow_mut() else { return };
    let mut need = false;
    // Recording status
    if a.rec.is_some() {
        let (written, dups, paused, err, fps) = { let r = a.rec.as_ref().unwrap(); (r.sh.written.load(Relaxed), r.sh.dups.load(Relaxed), r.sh.paused.is(), r.sh.stop.load(Relaxed), r.fps) };
        if let Some(c) = &mut a.ctl {
            let l = if dups > 0 { format!("{} REC {}  ({dups} dropped)", if paused { "||" } else { "●" }, fmt_time(written as u32, fps)) }
                else { format!("{} REC {}", if paused { "||" } else { "●" }, fmt_time(written as u32, fps)) };
            if c.lbl.label() != l { c.lbl.set_label(&l); }
            let _ = &c.pause;
        }
        if err { drop(a); stop_recording(app, ui); return; }
    }
    // Background job progress + completion
    if let Some((pct, label, done)) = a.job.as_ref().map(|j| (j.progress.load(Relaxed) / 10, j.label.clone(), j.handle.is_finished())) {
        let l = format!("{label}… {pct}%");
        if a.status != l && !done { a.status = l; need = true; }
        if done {
            let j = a.job.take().unwrap();
            let r = j.handle.join().unwrap_or_else(|_| Err("background thread panicked".into()));
            a.status = match r {
                Ok(JobOut::Msg(s)) => s,
                Ok(JobOut::DeadAir { ranges, gen }) => {
                    let fps = a.fps() as f64;
                    let secs: u32 = ranges.iter().map(|(x, y)| y - x).sum();
                    let n = ranges.len();
                    a.dead = ranges;
                    a.dead_gen = gen; // stale if the user edited meanwhile (gen != edit_gen)
                    if gen != a.edit_gen { "Dead-air scan discarded: the project changed while scanning".into() }
                    else if n == 0 { "No dead air found".into() }
                    else { format!("Found {n} dead-air region(s), {:.1}s total (red). Click \"Remove dead air\" to cut them.", secs as f64 / fps) }
                }
                Ok(JobOut::Compact { src, before, after, gen }) => apply_compact(&mut a, src, before, after, gen),
                Ok(JobOut::Tracks { changes, msg, gen }) => apply_track_job(&mut a, changes, msg, gen),
                Err(e) => format!("{label} failed: {e}"),
            };
            need = true;
        }
    }
    // Playback clock
    if a.proj.is_some() {
        let playing = a.play.playing.load(Relaxed);
        if playing {
            if let Some(t) = a.clock { // no output device: wall-clock fallback
                let adv = (t.elapsed().as_secs_f64() * AR as f64) as u64;
                a.clock = Some(Instant::now());
                let p = a.play.pos.load(Relaxed) + adv;
                a.play.pos.store(p, Relaxed);
                if p >= a.play.end.load(Relaxed) { a.play.playing.store(false, Relaxed); }
            }
            let f = (a.play.pos.load(Relaxed) / a.spf() as u64) as u32;
            let f = f.min(a.total().saturating_sub(1));
            if f != a.playhead {
                a.playhead = f;
                // Auto-scroll to keep the playhead visible.
                let vis = ui.timeline.w() as f64 / a.zoom;
                if (f as f64) < a.scroll || (f as f64) > a.scroll + vis * 0.95 { a.scroll = (f as f64 - vis * 0.1).max(0.0); }
                need = true;
            }
            if let Some(n) = &a.narr { if f + 1 >= n.end { a.stop_narration(); need = true; } }
        } else if a.narr.is_some() { a.stop_narration(); need = true; }
        if !playing && ui.play.label().starts_with("||") { need = true; }
    }
    // Output meter: instant attack, ~20 dB/s fall, 1.5 s peak hold (60 Hz tick)
    {
        let mut changed = false;
        for c in 0..2 {
            let pk = f32::from_bits(a.play.meter[c].swap(0, Relaxed)).min(4.0);
            let nv = pk.max(a.vu[c] * 0.9624);
            let nv = if nv < 1e-4 { 0.0 } else { nv };
            if nv != a.vu[c] { a.vu[c] = nv; changed = true; }
            let (h, t) = a.vu_hold[c];
            if pk >= h { a.vu_hold[c] = (pk, Some(Instant::now())); changed |= pk != h; }
            else if t.map(|t| t.elapsed() > Duration::from_millis(1500)).unwrap_or(false) { a.vu_hold[c] = (a.vu[c], Some(Instant::now())); changed = true; }
        }
        if changed { let mut m = ui.meter.clone(); m.redraw(); }
    }
    a.autosave_tick(); // v1.4.0
    request_preview(&mut a, ui);
    // Decoded preview frame
    let res = a.dec.res.lock().unwrap_or_else(|e| e.into_inner()).take(); // v1.3.1 FIX
    if let Some(r) = res {
        if let Ok(img) = RgbImage::new(&r.rgb, r.w, r.h, fltk::enums::ColorDepth::Rgb8) { a.preview = Some((img, r.frame)); }
        let _ = (r.tw, r.th);
        drop(a);
        let mut p = ui.preview.clone();
        p.redraw();
        if need { refresh(app, ui); }
        return;
    }
    drop(a);
    if need { refresh(app, ui); }
}

// ---- UI theme ------------------------------------------------------------------------
// One dark palette for the whole app (matches the timeline and video preview), with colour
// reserved for meaning: red = record, green = play, blue = export/apply, amber = FX.
const C_BG: (u8, u8, u8) = (34, 36, 41);
const C_PANEL: (u8, u8, u8) = (44, 47, 54);
const C_FIELD: (u8, u8, u8) = (24, 26, 30);
const C_TEXT: (u8, u8, u8) = (226, 228, 232);
const C_CAPTION: (u8, u8, u8) = (128, 136, 150);
const C_REC: (u8, u8, u8) = (170, 48, 48);
const C_PLAY: (u8, u8, u8) = (40, 128, 76);
const C_ACCENT: (u8, u8, u8) = (44, 96, 168);
const C_FX: (u8, u8, u8) = (150, 104, 30);

fn rgb(c: (u8, u8, u8)) -> Color { Color::from_rgb(c.0, c.1, c.2) }

fn apply_theme() {
    app::background(C_BG.0, C_BG.1, C_BG.2);
    app::background2(C_FIELD.0, C_FIELD.1, C_FIELD.2);
    app::foreground(C_TEXT.0, C_TEXT.1, C_TEXT.2);
    app::set_selection_color(70, 130, 220);
    app::set_inactive_color(110, 114, 122);
    fltk::misc::Tooltip::set_color(Color::from_rgb(250, 246, 220));
    fltk::misc::Tooltip::set_text_color(Color::from_rgb(20, 20, 20));
    fltk::misc::Tooltip::set_font_size(12);
    fltk::misc::Tooltip::set_delay(0.5);
}

/// Button with fixed role colour and readable label.
fn btn(label: &str, tip: &str, role: Option<(u8, u8, u8)>) -> Button {
    let mut b = Button::default().with_label(label);
    b.set_label_size(13);
    if !tip.is_empty() { b.set_tooltip(tip); }
    if let Some(c) = role {
        b.set_color(rgb(c));
        b.set_selection_color(rgb((c.0 / 2 + 20, c.1 / 2 + 20, c.2 / 2 + 20)));
        b.set_label_color(Color::White);
    } else {
        b.set_color(rgb(C_PANEL));
    }
    b
}

/// Small uppercase group caption at the start of a dock row.
fn caption(r: &mut Flex, text: &str, w: i32) {
    let mut f = Frame::default().with_label(text).with_align(Align::Left | Align::Inside);
    f.set_label_font(Font::HelveticaBold);
    f.set_label_size(10);
    f.set_label_color(rgb(C_CAPTION));
    r.fixed(&f, w);
}

/// Thin vertical divider between groups in a row.
fn divider(r: &mut Flex) {
    let mut f = Frame::default();
    f.draw(|f| {
        draw::set_draw_color(Color::from_rgb(70, 74, 84));
        draw::draw_yxline(f.x() + f.w() / 2, f.y() + 4, f.y() + f.h() - 4);
    });
    r.fixed(&f, 10);
}

/// One dock row: fixed-width widgets left-aligned; a trailing spacer absorbs extra width so
/// nothing stretches into giant buttons on wide screens.
fn row() -> Flex { let mut r = Flex::default().row(); r.set_pad(4); r }

fn end_row(col: &mut Flex, r: Flex) { Frame::default(); r.end(); col.fixed(&r, 30); }

/// v1.4.0: audio backends can fire their error callback hundreds of times a second once a
/// device disappears (seen with ALSA: snd_pcm_avail_delay I/O error). Log the first few and
/// then one in every 1000 instead of flooding stderr.
fn stream_err(what: &str, e: &dyn std::fmt::Display) {
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Relaxed);
    if n < 5 || n % 1000 == 0 { eprintln!("[{what}] stream error: {e} ({} so far)", n + 1); }
}

fn build_ui(app: &Shared) -> Ui {
    apply_theme();
    // v1.4.0: the extra rows made the default 900 px tall; fit it to smaller screens
    // (768 px laptops) instead of opening with the timeline off-screen.
    let (sw, sh) = app::screen_size();
    let (ww, wh) = if sw > 0.0 && sh > 0.0 { ((sw as i32 - 20).clamp(1000, 1180), (sh as i32 - 70).clamp(620, 900)) } else { (1180, 900) };
    let mut win = Window::default().with_size(ww, wh).with_label(&format!("{APP_NAME} v{VERSION}"));
    win.size_range(1000, 620, 0, 0);
    let mut col = Flex::default_fill().column();
    col.set_margin(6);
    col.set_pad(3);

    // Video preview on top: it gets all spare height.
    let mut preview = Frame::default();
    preview.set_frame(FrameType::FlatBox);
    preview.set_color(Color::Black);

    const CAP: i32 = 70;
    // Row 1: capture
    let mut r = row();
    caption(&mut r, "RECORD", CAP);
    let rec_full = btn("● Record Screen", "Record the whole screen (all monitors)", Some(C_REC));
    let rec_region = btn("● Record Region", "Drag a rectangle to record part of the screen", Some(C_REC));
    let mut fps = Choice::default();
    for f in FPS_CHOICES { fps.add_choice(&format!("{f} fps")); }
    fps.set_value(5);
    fps.set_tooltip("Capture frame rate");
    let mut quality = Choice::default();
    quality.add_choice("Draft (q60)|Standard (q80)|Sharp text (q92 4:4:4)");
    quality.set_value(1);
    quality.set_tooltip("JPEG quality of the recording (the only lossy step)");
    let mut half = CheckButton::default().with_label("Half-res");
    half.set_tooltip("Record at half width/height: 4x smaller files");
    let mut cursor = CheckButton::default().with_label("Cursor");
    cursor.set_checked(true);
    cursor.set_tooltip("Draw the mouse pointer into the video");
    divider(&mut r);
    // v1.4.0: webcam picture-in-picture source
    let mut cam = Choice::default();
    cam.add_choice("Webcam: off");
    for (_, n) in &app.borrow().cams { cam.add_choice(&format!("Webcam: {}", esc(&n.replace(['/', '|', '&', '_', '\\'], " ")))); }
    cam.set_value(0);
    cam.set_tooltip("Also record a webcam for a picture-in-picture overlay (shape, size, blur: ANNOTATE › Effects…)");
    r.fixed(&rec_full, 136); r.fixed(&rec_region, 136); r.fixed(&fps, 76); r.fixed(&quality, 178);
    r.fixed(&half, 78); r.fixed(&cursor, 70); r.fixed(&cam, 230);
    end_row(&mut col, r);

    // Row 1b: audio inputs
    let mut r = row();
    caption(&mut r, "AUDIO IN", CAP);
    let mut mic = Choice::default();
    mic.add_choice("Mic: off");
    mic.add_choice("Mic: default");
    for n in &app.borrow().mics { mic.add_choice(&format!("Mic: {}", esc(&n.replace(['/', '|', '&', '_', '\\'], " ")))); }
    mic.set_value(1);
    mic.set_tooltip("Microphone for recording and narration");
    let mut sys = CheckButton::default().with_label("System audio");
    sys.set_tooltip("Also record what the computer plays (videos, apps) to its own \"System\" track.\n\
Linux: needs PulseAudio or PipeWire. Use headphones to keep speaker sound out of the mic.");
    let mut live_fx = CheckButton::default().with_label("Live voice FX");
    live_fx.set_tooltip("Polish the mic while recording: noise reduction, gate, EQ, de-esser, compressor.\n\
Configure the chain in TRACK › Voice FX… (\"Save for live recording\").");
    r.fixed(&mic, 240); r.fixed(&sys, 112); r.fixed(&live_fx, 120);
    end_row(&mut col, r);

    // Row 2: project + export
    let mut r = row();
    caption(&mut r, "PROJECT", CAP);
    let new = btn("New", "Close this project and start fresh  (Ctrl+N)", None);
    let open = btn("Open…", "Open a project folder (project.txt)", None);
    let save = btn("Save", "Save the project  (Ctrl+S)", None);
    divider(&mut r);
    caption(&mut r, "EXPORT", 54);
    let exp_mkv = btn("MKV · instant", "Matroska, no re-encode: exports at disk speed, zero quality loss", Some(C_ACCENT));
    let exp_mov = btn("MOV · lossless", "QuickTime via ffmpeg -c copy (no re-encode). Needs ffmpeg.", Some(C_ACCENT));
    let exp_mp4 = btn("MP4 · H.264", "H.264 + AAC for upload sites (re-encodes). Needs ffmpeg.", Some(C_ACCENT));
    let exp_wav = btn("WAV · audio mix", "The stereo mix of all unmuted tracks", Some(C_ACCENT));
    r.fixed(&new, 70); r.fixed(&open, 80); r.fixed(&save, 70);
    r.fixed(&exp_mkv, 120); r.fixed(&exp_mov, 124); r.fixed(&exp_mp4, 116); r.fixed(&exp_wav, 130);
    end_row(&mut col, r);

    // Row 3: transport + edit
    let mut r = row();
    caption(&mut r, "PLAY · EDIT", CAP);
    let play = btn("▶ Play", "Play / pause  (Space). Double-tap Space: back to start", Some(C_PLAY));
    let mut time = Frame::default().with_label("00:00.00 / 00:00.00");
    time.set_frame(FrameType::FlatBox);
    time.set_color(rgb(C_FIELD));
    time.set_label_font(Font::Courier);
    time.set_label_size(14);
    let mut meter = Frame::default();
    meter.set_tooltip("Output level (peak, dBFS). Aim for speech peaks in the yellow zone.");
    divider(&mut r);
    let cut = btn("Cut", "Delete the selection (ripple)  (Del, Ctrl+X)", None);
    let ifreeze = btn("Insert freeze", "Insert a still of the current frame; tracks get silence  (F)", None);
    let mut fsecs = Spinner::default();
    fsecs.set_range(0.1, 600.0); fsecs.set_step(0.5); fsecs.set_value(2.0);
    fsecs.set_tooltip("Insert-freeze length in seconds");
    let freeze_sel = btn("Freeze sel.", "Hold the first frame across the selection; audio keeps playing", None);
    divider(&mut r);
    let undo = btn("Undo", "Undo  (Ctrl+Z)", None);
    let redo = btn("Redo", "Redo  (Ctrl+Y)", None);
    divider(&mut r);
    let narr = btn("● Narrate", "Re-record narration over the selection (or from the playhead)", Some(C_REC));
    let fitz = btn("⇔ Fit", "Zoom the timeline to show everything", None);
    r.fixed(&play, 84); r.fixed(&time, 156); r.fixed(&meter, 132); r.fixed(&cut, 50); r.fixed(&ifreeze, 104);
    r.fixed(&fsecs, 54); r.fixed(&freeze_sel, 92); r.fixed(&undo, 56); r.fixed(&redo, 56); r.fixed(&narr, 96); r.fixed(&fitz, 60);
    end_row(&mut col, r);

    // Row 4: track
    let mut r = row();
    caption(&mut r, "TRACK", CAP);
    let mut track = Choice::default();
    track.set_tooltip("Selected audio track (or click its lane on the timeline)");
    let mute = btn("Mute", "Mute / unmute the selected track", None);
    let mut gain = HorNiceSlider::default();
    gain.set_range(0.0, 2.0); gain.set_step(0.01, 1); gain.set_value(1.0); gain.set_tooltip("Track gain (0–200%)");
    gain.set_color(rgb(C_FIELD));
    let shl = btn("◀ 1f", "Nudge the track one frame earlier", None);
    let shr = btn("1f ▶", "Nudge the track one frame later", None);
    divider(&mut r);
    let newt = btn("+ Track", "Add an empty narration track", None);
    let import = btn("+ Audio file…", "Add an audio file (or a video's audio) at the playhead", None);
    let replace = btn("Swap…", "Replace the selected track's audio with a file", None);
    let remove = btn("× Remove", "Delete the selected track", None);
    divider(&mut r);
    let comp = btn("Opto compressor…", "Warm CL 1B-style optical compressor for the selected track (mic)", Some(C_FX));
    let voicefx = btn("Voice FX…", "Noise reduction, gate, EQ, de-esser and compressor for the selected track (or for live recording)", Some(C_FX));
    r.fixed(&track, 170); r.fixed(&mute, 56); r.fixed(&gain, 100); r.fixed(&shl, 44); r.fixed(&shr, 44);
    r.fixed(&newt, 76); r.fixed(&import, 110); r.fixed(&replace, 66); r.fixed(&remove, 84); r.fixed(&comp, 140); r.fixed(&voicefx, 90);
    end_row(&mut col, r);

    // Row 5: whole-project tools
    let mut r = row();
    caption(&mut r, "TOOLS", CAP);
    let norm = btn("Normalize levels…", "Bring every track to the same loudness (LUFS) with a -1 dBFS peak ceiling", Some(C_FX));
    let duck = btn("Auto-duck…", "Lower system audio / music automatically while you speak", Some(C_FX));
    divider(&mut r);
    let speed_lbl = Frame::default().with_label("Speed").with_align(Align::Right | Align::Inside);
    let mut speed = Choice::default();
    for sp in SPEEDS { speed.add_choice(&format!("{}×", fmt_speed(sp))); }
    speed.set_value(2);
    let mut speed_mute = CheckButton::default().with_label("Mute audio");
    speed_mute.set_tooltip("Off: audio is time-compressed with pitch preserved (speech stays natural).\nOn: the sped-up range is silent.");
    let speed_btn = btn("» Speed up sel.", "Play the selected range faster (ripple, lossless video). Undo with Ctrl+Z.", None);
    divider(&mut r);
    let find_dead = btn("Find dead air…", "Find stretches where all audio is silent AND the picture does not change", None);
    let rm_dead = btn("Remove dead air", "Cut all highlighted dead-air regions in one undoable step", None);
    let compact = btn("Compact…", "Rewrite the recording to hold only frames the edit uses (frees disk; clears undo)", None);
    r.fixed(&norm, 140); r.fixed(&duck, 96); r.fixed(&speed_lbl, 40); r.fixed(&speed, 62); r.fixed(&speed_mute, 96); r.fixed(&speed_btn, 118);
    r.fixed(&find_dead, 116); r.fixed(&rm_dead, 150); r.fixed(&compact, 90);
    end_row(&mut col, r);

    // Row 6 (v1.4.0): annotations
    let mut r = row();
    caption(&mut r, "ANNOTATE", CAP);
    let tips = ["Select / move / resize overlays in the preview (double-click text to edit)", "Text callout: drag a box (or click) on the preview",
        "Arrow: drag from tail to tip", "Blur box: hide secrets (drag a box)", "Pixelate box: the safest way to hide text (drag a box)",
        "Step counter: click to drop numbered steps 1, 2, 3…", "Zoom: drag the area to zoom into"];
    let mut tools = Vec::new();
    for (i, n) in TOOL_NAMES.iter().enumerate() {
        let mut b = fltk::button::RadioButton::default().with_label(n);
        b.set_tooltip(tips[i]);
        b.set_frame(FrameType::FlatBox);
        b.set_color(rgb(C_PANEL));
        b.set_selection_color(rgb(C_ACCENT));
        b.set_label_size(13);
        if i == 0 { b.set_value(true); }
        r.fixed(&b, if i == 4 { 66 } else { 52 });
        tools.push(b);
    }
    let mut ov_color = Choice::default();
    for (n, _) in OV_COLORS { ov_color.add_choice(n); }
    ov_color.set_value(0);
    ov_color.set_tooltip("Colour for new annotations (also recolours the selected one)");
    divider(&mut r);
    let ov_del = btn("Delete", "Delete the selected overlay  (Del)", None);
    let ov_text = btn("Edit text…", "Edit the selected text callout", None);
    let ov_time = btn("Time = selection", "Show the selected overlay during the timeline selection", None);
    divider(&mut r);
    let autozoom = btn("Auto zoom", "Zoom and follow the cursor wherever you clicked or typed (replaces earlier auto zooms)", Some(C_ACCENT));
    let effects = btn("Effects…", "Cursor highlight, click rings, webcam picture-in-picture", Some(C_ACCENT));
    r.fixed(&ov_color, 76); r.fixed(&ov_del, 62); r.fixed(&ov_text, 84); r.fixed(&ov_time, 120); r.fixed(&autozoom, 92); r.fixed(&effects, 80);
    end_row(&mut col, r);

    let mut timeline = Frame::default();
    timeline.set_frame(FrameType::FlatBox);
    col.fixed(&timeline, 200);
    let mut status = Frame::default().with_align(Align::Left | Align::Inside);
    status.set_frame(FrameType::FlatBox);
    status.set_color(rgb(C_FIELD));
    status.set_label_size(12);
    col.fixed(&status, 24);
    col.end();
    win.end();
    win.make_resizable(true);
    for mut c in [fps.clone(), quality.clone(), mic.clone(), track.clone(), speed.clone(), cam.clone(), ov_color.clone()] { c.set_color(rgb(C_PANEL)); c.set_label_size(13); c.set_text_size(13); }
    let ui = Ui { win, rec_full, rec_region, fps, quality, half, cursor, mic, sys, open, save, exp_mkv, exp_mov, exp_mp4, exp_wav,
        preview, play, time, cut, ifreeze, fsecs, freeze_sel, undo, redo, narr, fitz, track, mute, gain, shl, shr, newt, import,
        replace, remove, timeline, status, new, speed, speed_mute, speed_btn, find_dead, rm_dead, compact, comp, norm, meter,
        cam, live_fx, voicefx, duck, tools, ov_color, ov_del, ov_text, ov_time, autozoom, effects };
    // Buttons should not steal Space/Enter (used as shortcuts).
    for mut b in [ui.rec_full.clone(), ui.rec_region.clone(), ui.open.clone(), ui.save.clone(), ui.exp_mkv.clone(), ui.exp_mov.clone(),
        ui.exp_mp4.clone(), ui.exp_wav.clone(), ui.play.clone(), ui.cut.clone(), ui.ifreeze.clone(), ui.freeze_sel.clone(), ui.undo.clone(),
        ui.redo.clone(), ui.narr.clone(), ui.fitz.clone(), ui.mute.clone(), ui.shl.clone(), ui.shr.clone(), ui.newt.clone(),
        ui.import.clone(), ui.replace.clone(), ui.remove.clone(), ui.new.clone(), ui.speed_btn.clone(), ui.find_dead.clone(),
        ui.rm_dead.clone(), ui.compact.clone(), ui.comp.clone(), ui.norm.clone(), ui.voicefx.clone(), ui.duck.clone(),
        ui.ov_del.clone(), ui.ov_text.clone(), ui.ov_time.clone(), ui.autozoom.clone(), ui.effects.clone()] { b.clear_visible_focus(); }
    for mut b in ui.tools.clone() { b.clear_visible_focus(); }
    ui
}

/// Draw the stereo output meter: -48..+0 dBFS, green < -18, yellow < -6, red above,
/// with a 1.5 s peak-hold tick per channel.
fn draw_meter(f: &Frame, vu: [f32; 2], hold: [f32; 2]) {
    let (x, y, w, h) = (f.x(), f.y(), f.w(), f.h());
    draw::draw_rect_fill(x, y, w, h, rgb(C_FIELD));
    let pos = |lin: f32| -> i32 {
        let db = 20.0 * lin.max(1e-6).log10();
        (((db + 48.0) / 48.0).clamp(0.0, 1.0) * (w - 4) as f32) as i32
    };
    let (z1, z2) = (pos(10f32.powf(-18.0 / 20.0)), pos(10f32.powf(-6.0 / 20.0)));
    let bh = (h - 8) / 2;
    for c in 0..2 {
        let by = y + 3 + c as i32 * (bh + 2);
        draw::draw_rect_fill(x + 2, by, w - 4, bh, Color::from_rgb(40, 43, 50));
        let p = pos(vu[c]);
        let seg = |a: i32, b: i32, col: Color| { if b > a { draw::draw_rect_fill(x + 2 + a, by, b - a, bh, col); } };
        seg(0, p.min(z1), Color::from_rgb(60, 190, 100));
        seg(z1, p.min(z2), Color::from_rgb(220, 200, 60));
        seg(z2, p, Color::from_rgb(230, 70, 60));
        if hold[c] > 1e-4 {
            let hx = x + 2 + pos(hold[c]).min(w - 5);
            draw::set_draw_color(if hold[c] >= 0.891 { Color::from_rgb(255, 90, 80) } else { Color::White });
            draw::draw_yxline(hx, by, by + bh - 1);
        }
    }
    // scale ticks at -36, -24, -18, -12, -6, 0
    draw::set_draw_color(Color::from_rgb(90, 95, 105));
    for db in [-36.0f32, -24.0, -18.0, -12.0, -6.0, 0.0] {
        let tx = x + 2 + pos(10f32.powf(db / 20.0)).min(w - 5);
        draw::draw_yxline(tx, y + h - 3, y + h - 1);
    }
}

/// Global editing shortcuts (called from the focused timeline and the main window).
fn handle_key(a: &Shared, u: &Ui) -> bool {
    let k = app::event_key();
    let ctrl = app::event_state().contains(Shortcut::Ctrl) || app::event_state().contains(Shortcut::Command);
    if ctrl && k == Key::from_char('n') {
        // Routed through the button so the unsaved-changes / busy checks live in one place.
        if u.new.active() { u.new.clone().do_callback(); }
        return true;
    }
    let Ok(mut s) = a.try_borrow_mut() else { return false };
    if s.proj.is_none() || s.job.is_some() { return false; }
    if s.narr.is_some() {
        // While re-recording narration only Space/Esc (stop) are honoured.
        if k == Key::from_char(' ') || k == Key::Escape { s.set_playing(false); drop(s); refresh(a, u); return true; }
        return false;
    }
    if !ctrl && (k == Key::Delete || k == Key::BackSpace) && s.ov_sel.is_some() {
        // v1.4.0: Del deletes the selected overlay (clicking the timeline deselects it)
        drop(s);
        op_overlay(a, u, "del");
        return true;
    }
    let r: Res<()> = match (ctrl, k) {
        (false, Key::Delete) | (false, Key::BackSpace) => s.op_cut(),
        (true, k) if k == Key::from_char('x') => s.op_cut(),
        (false, k) if k == Key::from_char(' ') => {
            // Double-tap Space (< 400 ms): jump to 0 and restore the play state from before
            // the first tap (so playing stays playing from the start, paused stays paused).
            let was = s.play.playing.load(Relaxed);
            match s.space_at.take() {
                Some((t, before)) if t.elapsed() < Duration::from_millis(400) => {
                    s.set_playing(false);
                    s.seek(0);
                    s.scroll = 0.0;
                    if before { s.set_playing(true); }
                    s.status = "Back to start".into();
                }
                _ => { s.space_at = Some((Instant::now(), was)); s.set_playing(!was); }
            }
            Ok(())
        }
        (false, k) if k == Key::from_char('f') => { let v = u.fsecs.value(); s.op_insert_freeze(v) }
        (true, k) if k == Key::from_char('z') => { s.op_undo(false); Ok(()) }
        (true, k) if k == Key::from_char('y') => { s.op_undo(true); Ok(()) }
        (true, k) if k == Key::from_char('s') => s.save_project().map(|_| { s.status = "Project saved".into(); }),
        (false, Key::Home) => { s.seek(0); Ok(()) }
        (false, Key::End) => { let t = s.total(); s.seek(t); Ok(()) }
        (false, Key::Left) => { let f = s.playhead.saturating_sub(1); s.seek(f); Ok(()) }
        (false, Key::Right) => { let f = s.playhead + 1; s.seek(f); Ok(()) }
        (false, Key::Escape) => { s.sel = None; s.ov_sel = None; s.ov_drag = None; Ok(()) }
        _ => return false,
    };
    if let Err(e) = r { s.status = e; }
    drop(s);
    refresh(a, u);
    true
}

fn wire(app: &Shared, ui: &Ui) {
    macro_rules! cb {
        ($w:expr, |$a:ident, $u:ident| $body:block) => {{
            let ($a, $u) = (app.clone(), ui.clone());
            let mut w = $w.clone();
            w.set_callback(move |_| { #[allow(unused_variables)] let ($a, $u) = (&$a, &$u); $body; refresh($a, $u); });
        }};
    }
    fn report(app: &Shared, r: Res<()>) { if let Err(e) = r { app.borrow_mut().status = e; } }

    // Preview drawing
    {
        let a = app.clone();
        let mut pv = ui.preview.clone();
        pv.draw(move |f| {
            let Ok(s) = a.try_borrow() else { return };
            if let Some((img, _)) = &s.preview {
                let mut img = img.clone();
                let (dw, dh) = fit(img.data_w(), img.data_h(), f.w() - 4, f.h() - 4);
                let (dw, dh) = if app::screen_scale(0) <= 1.0 && img.data_w() <= f.w() - 4 && img.data_h() <= f.h() - 4 { (img.data_w(), img.data_h()) } else { (dw, dh) };
                img.scale(dw, dh, false, true);
                let (ix, iy) = (f.x() + (f.w() - dw) / 2, f.y() + (f.h() - dh) / 2);
                img.draw(ix, iy, dw, dh);
                drop(s);
                // v1.4.0: remember where the frame is (mouse mapping) + overlay frames/handles
                if let Ok(mut s) = a.try_borrow_mut() {
                    s.pv_rect = (ix, iy, dw, dh);
                    draw::push_clip(f.x(), f.y(), f.w(), f.h());
                    draw_overlay_marks(&s);
                    draw::pop_clip();
                }
            } else if s.proj.is_none() {
                draw::set_draw_color(Color::from_rgb(150, 150, 150));
                draw::set_font(Font::Helvetica, 16);
                draw::draw_text2(&format!("{APP_NAME} v{VERSION}\n\n● Record Screen / ● Record Region to begin\n\nSpace play/pause · Space Space back to 0 · Del/Ctrl+X cut · F insert freeze\nCtrl+Z/Y undo/redo · Ctrl+S save · Ctrl+N new\n\nAfter recording: TRACK › Opto compressor for the mic, TOOLS › Normalize levels"),
                    f.x(), f.y(), f.w(), f.h(), Align::Center);
            }
        });
    }
    // v1.4.0: preview mouse = annotation editing
    {
        let (a, u) = (app.clone(), ui.clone());
        let mut pv = ui.preview.clone();
        pv.set_visible_focus();
        pv.handle(move |f, ev| {
            match ev {
                Event::Focus | Event::Unfocus => return true,
                Event::KeyDown => return handle_key(&a, &u),
                Event::Push => { let _ = f.take_focus(); }
                _ => {}
            }
            preview_event(&a, &u, ev)
        });
    }
    // Timeline drawing + interaction
    {
        let a = app.clone();
        let mut tl = ui.timeline.clone();
        tl.set_visible_focus(); // needed for take_focus(); FlatBox draws no focus box
        tl.draw(move |f| { if let Ok(mut s) = a.try_borrow_mut() { draw_timeline(&mut s, f); } });
        let (a, u) = (app.clone(), ui.clone());
        tl.handle(move |f, ev| {
            // Focus/keys first: take_focus() re-enters this handler while `s` is borrowed below.
            match ev {
                Event::Focus | Event::Unfocus => return true,
                Event::KeyDown => return handle_key(&a, &u),
                _ => {}
            }
            let Ok(mut s) = a.try_borrow_mut() else { return false };
            if s.proj.is_none() || s.job.is_some() { return false; }
            let (ex, ey) = (app::event_x(), app::event_y());
            match ev {
                Event::Push => {
                    drop(s);
                    let _ = f.take_focus();
                    let Ok(mut s) = a.try_borrow_mut() else { return false };
                    let fr = x_to_frame(&s, f, ex);
                    let lh = lane_h(&s, f.h());
                    let lane_y = f.y() + RULER_H + VLANE_H + FXLANE_H;
                    // v1.4.0: click in the FX lane selects the overlay there (last drawn = on top)
                    let fxy = f.y() + RULER_H + VLANE_H;
                    if ey >= fxy && ey < lane_y {
                        let hit = s.proj.as_ref().unwrap().edit.ov.iter().enumerate().rev().find(|(_, o)| fr >= o.start && fr < o.end.max(o.start + 1)).map(|(i, _)| i);
                        s.ov_sel = hit;
                        if let Some(i) = hit {
                            let st = s.proj.as_ref().unwrap().edit.ov[i].start;
                            if s.playhead < st || s.playhead >= s.proj.as_ref().unwrap().edit.ov[i].end { s.seek(st); }
                            drop(s);
                            refresh(&a, &u);
                            return true;
                        }
                    } else { s.ov_sel = None; }
                    if ey >= lane_y && lh > 0 {
                        let li = ((ey - lane_y) / lh) as usize;
                        let n = s.proj.as_ref().unwrap().edit.tracks.len();
                        s.sel_track = if li < n { Some(li) } else { None };
                    }
                    if s.narr.is_some() { drop(s); return true; }
                    s.drag_anchor = Some(fr);
                    s.sel = None;
                    s.seek(fr);
                    drop(s);
                    refresh(&a, &u);
                    true
                }
                Event::Drag => {
                    if let Some(an) = s.drag_anchor {
                        // Edge auto-scroll while dragging.
                        if ex > f.x() + f.w() - 10 { s.scroll += 10.0 / s.zoom; } else if ex < f.x() + 10 { s.scroll = (s.scroll - 10.0 / s.zoom).max(0.0); }
                        let fr = x_to_frame(&s, f, ex);
                        s.sel = if fr != an { Some((an.min(fr), an.max(fr))) } else { None };
                        drop(s);
                        refresh(&a, &u);
                    }
                    true
                }
                Event::Released => { s.drag_anchor = None; true }
                Event::MouseWheel => {
                    let dy = app::event_dy_value();
                    let dx = app::event_dx_value();
                    if app::is_event_ctrl() || (dx == 0 && !app::is_event_shift()) {
                        // zoom about mouse
                        let at = s.scroll + (ex - f.x()) as f64 / s.zoom;
                        let k = if dy > 0 { 1.0 / 1.25 } else { 1.25 };
                        // v1.3.1 FIX: on clips shorter than ~w/128 frames minz exceeded 64 and
                        // f64::clamp(min > max) panicked, killing the app on Ctrl+wheel.
                        let maxz = 64.0;
                        let minz = (f.w() as f64 / s.total().max(1) as f64 * 0.5).min(maxz);
                        s.zoom = (s.zoom * k).clamp(minz, maxz);
                        s.scroll = (at - (ex - f.x()) as f64 / s.zoom).max(0.0);
                    } else {
                        let d = if dx != 0 { dx } else { dy };
                        s.scroll = (s.scroll + d as f64 * 40.0 / s.zoom).max(0.0);
                    }
                    drop(s);
                    f.redraw();
                    true
                }
                Event::Enter | Event::Leave | Event::Move => true,
                _ => false,
            }
        });
    }
    // Recording
    cb!(ui.rec_full, |a, u| {
        match cap::virtual_screen() { Ok(r) => begin_recording(a, u, Rect { w: r.w & !1, h: r.h & !1, ..r }), Err(e) => alert(&e) }
    });
    cb!(ui.rec_region, |a, u| {
        let mut w = u.win.clone();
        w.hide();
        app::flush();
        thread::sleep(Duration::from_millis(250)); // let the main window unmap before the screenshot
        match select_region() {
            Ok(Some(r)) => begin_recording(a, u, r),
            Ok(None) => { w.show(); }
            Err(e) => { w.show(); alert(&format!("Region selection failed:\n{e}")); }
        }
    });
    // File ops
    cb!(ui.new, |a, u| {
        if !may_close_project(a) { return; }
        a.borrow_mut().set_project(None, u.timeline.w());
        a.borrow_mut().status = "New project: press ● Record Screen or ● Record Region".into();
    });
    cb!(ui.open, |a, u| {
        if !may_close_project(a) { return; }
        if let Some(p) = choose_file("Open ScreenReel project (project.txt)", "ScreenReel project\tproject.txt", None) {
            a.borrow_mut().set_playing(false);
            match open_project(&p) {
                Ok(pr) => {
                    let msg = format!("Opened {}{}", pr.src.dir.display(), if pr.dirty { " (restored from autosave: Save to keep)" } else { "" });
                    let mut s = a.borrow_mut();
                    s.set_project(Some(pr), u.timeline.w());
                    s.status = msg;
                }
                Err(e) => alert(&format!("Open failed:\n{e}")),
            }
        }
    });
    cb!(ui.save, |a, u| {
        let mut s = a.borrow_mut();
        let r = s.save_project();
        s.status = match r { Ok(()) => "Project saved".into(), Err(e) => format!("Save failed: {e}") };
    });
    cb!(ui.exp_mkv, |a, u| { start_export(a, u, "mkv"); });
    // v1.4.0
    cb!(ui.voicefx, |a, u| { voice_fx_dialog(a, u); });
    cb!(ui.duck, |a, u| { duck_dialog(a, u); });
    cb!(ui.effects, |a, u| { effects_dialog(a, u); });
    cb!(ui.autozoom, |a, u| { op_overlay(a, u, "autozoom"); });
    cb!(ui.ov_del, |a, u| { op_overlay(a, u, "del"); });
    cb!(ui.ov_text, |a, u| { op_overlay(a, u, "text"); });
    cb!(ui.ov_time, |a, u| { op_overlay(a, u, "time"); });
    {
        let (a, u) = (app.clone(), ui.clone());
        let mut c = ui.ov_color.clone();
        c.set_callback(move |c| { a.borrow_mut().ov_color = c.value().clamp(0, 5) as usize; op_overlay(&a, &u, "color"); });
    }
    for (i, b) in ui.tools.iter().enumerate() {
        let (a, u) = (app.clone(), ui.clone());
        let mut b = b.clone();
        b.set_callback(move |_| {
            let mut s = a.borrow_mut();
            s.tool = i as u8;
            s.status = match i {
                0 => "Select: click an overlay in the preview to move/resize it".into(),
                6 => "Zoom: drag the area to zoom into (the zoom eases in and out over its time span)".into(),
                _ => format!("{}: draw on the preview. It shows during the timeline selection (or 4 s from the playhead).", TOOL_NAMES[i]),
            };
            drop(s);
            refresh(&a, &u);
        });
    }
    cb!(ui.exp_mov, |a, u| { start_export(a, u, "mov"); });
    cb!(ui.exp_mp4, |a, u| { start_export(a, u, "mp4"); });
    cb!(ui.exp_wav, |a, u| { start_export(a, u, "wav"); });
    // Transport + edit
    cb!(ui.play, |a, u| { let mut s = a.borrow_mut(); let p = !s.play.playing.load(Relaxed); s.set_playing(p); });
    cb!(ui.cut, |a, u| { let r = a.borrow_mut().op_cut(); report(a, r); });
    cb!(ui.ifreeze, |a, u| { let secs = u.fsecs.value(); let r = a.borrow_mut().op_insert_freeze(secs); report(a, r); });
    cb!(ui.freeze_sel, |a, u| { let r = a.borrow_mut().op_freeze_sel(); report(a, r); });
    cb!(ui.undo, |a, u| { a.borrow_mut().op_undo(false); });
    cb!(ui.redo, |a, u| { a.borrow_mut().op_undo(true); });
    cb!(ui.speed_btn, |a, u| {
        let sp = SPEEDS.get(u.speed.value().max(0) as usize).copied().unwrap_or(200);
        let mute = u.speed_mute.is_checked();
        let r = a.borrow_mut().op_speed(sp, mute);
        report(a, r);
    });
    cb!(ui.find_dead, |a, u| { start_dead_scan(a, u); });
    cb!(ui.rm_dead, |a, u| { let r = a.borrow_mut().op_remove_dead(); report(a, r); });
    cb!(ui.compact, |a, u| { start_compact(a, u); });
    cb!(ui.comp, |a, u| { comp_dialog(a, u); });
    cb!(ui.norm, |a, u| { norm_dialog(a, u); });
    {
        let a = app.clone();
        let mut m = ui.meter.clone();
        m.draw(move |f| { if let Ok(s) = a.try_borrow() { draw_meter(f, s.vu, [s.vu_hold[0].0, s.vu_hold[1].0]); } });
    }
    cb!(ui.fitz, |a, u| { let mut s = a.borrow_mut(); s.scroll = 0.0; s.zoom = (u.timeline.w() as f64 / s.total().max(1) as f64).max(1e-4); });
    cb!(ui.narr, |a, u| {
        let mic = { let s = a.borrow(); match u.mic.value() { i if i >= 2 => s.mics.get(i as usize - 2).cloned(), _ => None } };
        let mut s = a.borrow_mut();
        if s.narr.is_some() { s.stop_narration(); } else if let Err(e) = s.start_narration(mic, u.live_fx.is_checked().then(load_voice_fx).filter(|f| f.active())) { drop(s); alert(&format!("Narration: {e}")); }
    });
    // Tracks
    cb!(ui.track, |a, u| { let v = u.track.value(); a.borrow_mut().sel_track = if v >= 0 { Some(v as usize) } else { None }; });
    cb!(ui.mute, |a, u| { let r = a.borrow_mut().op_track(|e, i, _| { if let Some(t) = e.tracks.get_mut(i) { t.muted = !t.muted; } }); report(a, r); });
    {
        let (a, u) = (app.clone(), ui.clone());
        let mut g = ui.gain.clone();
        g.set_callback(move |g| {
            let v = g.value() as f32;
            let mut s = a.borrow_mut();
            if let (Some(ti), Some(p)) = (s.sel_track, s.proj.as_mut()) { if let Some(t) = p.edit.tracks.get_mut(ti) { t.gain = v; p.dirty = true; } }
            s.edit_gen += 1; // the audible mix changed: dead-air results no longer apply
            s.sync_mix();
            drop(s);
            let mut tl = u.timeline.clone(); tl.redraw();
        });
    }
    cb!(ui.shl, |a, u| { let r = a.borrow_mut().op_track(|e, i, spf| e.shift_track(i, -1, spf)); report(a, r); });
    cb!(ui.shr, |a, u| { let r = a.borrow_mut().op_track(|e, i, spf| e.shift_track(i, 1, spf)); report(a, r); });
    cb!(ui.newt, |a, u| { let mut s = a.borrow_mut(); let n = s.proj.as_ref().map(|p| p.edit.tracks.len() + 1).unwrap_or(1); s.add_track(format!("Narration {n}"), 1, vec![], 0); });
    cb!(ui.remove, |a, u| {
        let r = a.borrow_mut().op_track(|e, i, _| { if i < e.tracks.len() { e.tracks.remove(i); } });
        report(a, r);
        a.borrow_mut().sel_track = None;
    });
    let audio_filter = "Audio/Video\t*.{wav,mp3,flac,ogg,m4a,aac,mp4,mkv,mov}";
    cb!(ui.import, |a, u| {
        if let Some(p) = choose_file("Add audio track (placed at playhead)", audio_filter, None) {
            match decode_audio(&p) {
                Ok((ch, d)) => {
                    let mut s = a.borrow_mut();
                    let at = s.playhead;
                    let name = p.file_stem().map(|x| x.to_string_lossy().to_string()).unwrap_or("Audio".into());
                    s.add_track(sanitize_name(&name), ch, d, at);
                    s.status = format!("Added track '{name}' at {}", fmt_time(at, s.fps()));
                }
                Err(e) => alert(&format!("Import failed:\n{e}")),
            }
        }
    });
    cb!(ui.replace, |a, u| {
        if a.borrow().sel_track.is_none() { return; }
        if let Some(p) = choose_file("Swap selected track's audio with file (starts at 0:00)", audio_filter, None) {
            match decode_audio(&p) {
                Ok((ch, d)) => {
                    let name = sanitize_name(&p.file_stem().map(|x| x.to_string_lossy().to_string()).unwrap_or("Audio".into()));
                    let r = a.borrow_mut().op_track(move |e, i, _| { if let Some(t) = e.tracks.get_mut(i) { t.ch = ch; t.data = Arc::new(d); t.name = name; } });
                    report(a, r);
                }
                Err(e) => alert(&format!("Swap failed:\n{e}")),
            }
        }
    });
    // Keyboard shortcuts + close guard
    {
        let (a, u) = (app.clone(), ui.clone());
        let mut w = ui.win.clone();
        w.handle(move |_, ev| matches!(ev, Event::KeyDown | Event::Shortcut) && handle_key(&a, &u));
        let a2 = app.clone();
        w.set_callback(move |w| {
            if app::event() != Event::Close { return; }
            // v1.3.1 FIX: finish an in-progress narration take first. v1.3.0 dropped the mic
            // capture on quit, so the take was lost even when the user chose "Save".
            if a2.borrow().narr.is_some() { a2.borrow_mut().stop_narration(); }
            let dirty = a2.borrow().proj.as_ref().map(|p| p.dirty).unwrap_or(false);
            if a2.borrow().job.is_some() {
                if dialog::choice2_default("A background job (export/scan/compact) is running. Cancel it and quit?", "Keep working", "Quit", "") != Some(1) { return; }
                // v1.3.1 FIX: cancel *and wait* (bounded) so the worker can delete its partial
                // output and kill a running ffmpeg, instead of being torn down mid-write.
                let job = a2.borrow_mut().job.take();
                if let Some(j) = job {
                    j.cancel.store(true, Relaxed);
                    let t = Instant::now();
                    while !j.handle.is_finished() && t.elapsed() < Duration::from_secs(5) { thread::sleep(Duration::from_millis(20)); }
                    if j.handle.is_finished() { let _ = j.handle.join(); }
                }
            }
            if dirty {
                match dialog::choice2_default("Save changes to the project before quitting?", "Cancel", "Save", "Don't save") {
                    Some(1) => { if let Some(p) = a2.borrow_mut().proj.as_mut() { if let Err(e) = p.save() { alert(&format!("Save failed: {e}")); return; } } }
                    Some(2) => {}
                    _ => return,
                }
            }
            a2.borrow_mut().set_playing(false);
            w.hide();
            app::quit();
        });
    }
}

// =====================================================================================
// Self-test (headless-capable): capture N seconds, edit, export, verify
// =====================================================================================

fn selftest(out: &Path, secs: f64, full: bool, sys: bool) -> Res<()> {
    let vs = cap::virtual_screen()?;
    let region = if full { Rect { w: vs.w & !1, h: vs.h & !1, ..vs } } else { Rect { x: 0, y: 0, w: vs.w.min(640) & !1, h: vs.h.min(360) & !1 } };
    let dir = out.join("st_rec");
    let _ = std::fs::remove_dir_all(&dir);
    // v1.4.0: synthetic webcam + real input events (xdotool, when installed) during the take
    std::env::set_var("SCREENREEL_FAKE_CAM", "1");
    let rec = Recorder::start(RecConfig { region, fps: 30, quality: 80, s444: false, half: false, cursor: true, mic: None, sys, dir: dir.clone(),
        cam: Some(None), live_fx: None })?;
    println!("capture: {}x{} shm={} cam={}", rec.dims.0, rec.dims.1, rec.shm, rec.has_cam);
    if let Some(e) = &rec.sys_error { return Err(format!("system audio: {e}")); }
    if let Some(e) = &rec.cam_error { return Err(format!("webcam: {e}")); }
    assert!(dir.join(REC_MARK).is_file(), "recovery marker missing while recording");
    let t0 = Instant::now();
    let xdo = cfg!(target_os = "linux") && std::process::Command::new("xdotool").arg("version").output().map(|o| o.status.success()).unwrap_or(false);
    if xdo {
        thread::sleep(Duration::from_secs_f64((secs * 0.3).min(1.0)));
        let _ = std::process::Command::new("xdotool").args(["mousemove", "50", "40", "mousedown", "1", "sleep", "0.1", "mouseup", "1"]).status();
        let _ = std::process::Command::new("xdotool").args(["keydown", "a", "sleep", "0.1", "keyup", "a"]).status();
        let _ = std::process::Command::new("xdotool").args(["mousemove", "200", "150"]).status();
    }
    thread::sleep(Duration::from_secs_f64((secs - t0.elapsed().as_secs_f64()).max(0.2)));
    let dups = rec.sh.dups.load(Relaxed);
    let (dims, fps) = (rec.dims, rec.fps);
    let RecOut { frames, sys: sys_trk, .. } = rec.finish()?;
    println!("recorded {frames} frames in {:.2}s (dups {dups})", t0.elapsed().as_secs_f64());
    let src = Arc::new(Source::open(&dir, dims.0, dims.1, fps)?);
    let spf = src.spf();
    let n = src.count();
    // ================= v1.4.0 checks =================
    // Side streams are written in lockstep with the frame index.
    {
        let evn = std::fs::metadata(dir.join("frames.ev")).map_err(es)?.len() / 5;
        let camn = std::fs::metadata(dir.join("frames.cam.idx")).map_err(es)?.len() / 12;
        assert_eq!(evn, n as u64, ".ev entries != frames");
        assert_eq!(camn, n as u64, ".cam.idx entries != frames");
        assert!(!dir.join(REC_MARK).exists() || dir.join(REC_MARK).is_file());
        let cj = src.read_cam(n - 1).ok_or("no webcam frame recorded (fake camera)")?;
        let (_, cw, chh) = decode_jpeg_rgb(&cj)?;
        let distinct = src.cam.as_ref().map(|c| { let mut v: Vec<u64> = c.idx.iter().map(|e| e.0).collect(); v.dedup(); v.len() }).unwrap_or(0);
        let vis = src.ev.iter().filter(|e| e.f & EV_VIS != 0).count();
        let clicks = src.ev.iter().filter(|e| e.f & EV_CLICK != 0).count();
        let keys = src.ev.iter().filter(|e| e.f & EV_KEY != 0).count();
        println!("v1.4.0 streams: ev {evn}, cam {camn} entries ({distinct} distinct {cw}x{chh} JPEGs), pointer visible {vis}, clicks {clicks}, keys {keys}");
        if xdo { assert!(clicks >= 1, "xdotool click not captured"); assert!(keys >= 1, "xdotool key not captured"); }
    }
    // LinRes: streaming resample keeps length (+-1 per chunk) and frequency
    {
        let x: Vec<i16> = (0..44_100).map(|i| ((i as f32 * 1000.0 * std::f32::consts::TAU / 44_100.0).sin() * 9000.0) as i16).collect();
        let mut r = LinRes::new(44_100, AR, 1);
        let mut y = Vec::new();
        for c in x.chunks(441) { r.push(c, &mut y); }
        let zc = y.windows(2).filter(|w| (w[0] < 0) != (w[1] < 0)).count() as f64 / 2.0 * AR as f64 / y.len() as f64;
        println!("LinRes 44.1k->48k: {} -> {} samples, ~{zc:.0} Hz (want 1000)", x.len(), y.len());
        assert!((y.len() as i64 - 48_000).abs() <= 2, "resampled length {}", y.len());
        assert!((990.0..1010.0).contains(&zc));
    }
    // Overlay + fx serialization round trip (incl. escaped text), strict parser
    let ov_all = vec![
        Overlay { start: 3, end: 40, x: 0.1, y: 0.1, w: 0.4, h: 0.12, color: 0xfdd835, kind: OvKind::Text { text: "Click \"Save\"\nthen \\ wait".into() } },
        Overlay { start: 0, end: 30, x: 0.7, y: 0.7, w: -0.2, h: -0.25, color: 0xe53935, kind: OvKind::Arrow },
        Overlay { start: 0, end: 30, x: 0.05, y: 0.6, w: 0.3, h: 0.2, color: 0, kind: OvKind::Blur },
        Overlay { start: 0, end: 30, x: 0.55, y: 0.05, w: 0.3, h: 0.25, color: 0, kind: OvKind::Pixelate },
        Overlay { start: 5, end: 25, x: 0.45, y: 0.45, w: 0.06, h: 0.1, color: 0x1e88e5, kind: OvKind::Step { n: 3 } },
        Overlay { start: 10, end: 50, x: 0.25, y: 0.25, w: 0.5, h: 0.5, color: 0x3a8dde, kind: OvKind::Zoom { scale: 2.0, follow: false } },
    ];
    for o in &ov_all {
        let l = o.to_line();
        let back = Overlay::from_line(l.trim_end().strip_prefix("ov=").unwrap())?;
        assert_eq!(&back, o, "overlay round trip: {l}");
    }
    for bad in ["Z 0 10 0 0 1 1 ffffff NaN 1", "T 5 2 0 0 1 1 ffffff|x", "Q 0 1 0 0 1 1 0", "A 0 10 0 0 9 1 ffffff", "S 0 10 0 0 1 1 zz 1"] {
        assert!(Overlay::from_line(bad).is_err(), "accepted bad overlay {bad}");
    }
    let fxs = FxSettings { cursor_hl: true, click_rings: false, cam_show: true, cam_pos: 2, cam_size: 0.3, cam_shape: 1, cam_blur: true, cam_mirror: false };
    assert_eq!(FxSettings::from_line(fxs.to_line().trim_end().strip_prefix("fx=").unwrap()), fxs);
    // Ripple edits move overlays with the picture
    {
        let mut d = Edit { segs: vec![Seg::Src { start: 0, len: n }], ov: ov_all.clone(), ..Default::default() };
        d.cut(0, 5, spf);
        assert_eq!((d.ov[0].start, d.ov[0].end), (0, 35));
        d.insert_freeze(0, 10, spf);
        assert_eq!((d.ov[0].start, d.ov[0].end), (10, 45));
        let end = d.speed_range(0, 20, 200, true, spf);
        assert_eq!(d.ov[0].start, 5);
        assert!(end == 10 && d.ov[0].end == 35, "speed ripple {end} {}", d.ov[0].end);
        d.cut(0, d.total(), spf);
        assert!(d.ov.is_empty(), "overlays inside a cut range must go");
    }
    // normalize_tracks (job-thread version): per-track LUFS to target, silent skipped, cancel
    {
        let tone: Vec<i16> = (0..AR as usize * 3).map(|i| ((i as f32 * 0.05).sin() * 1500.0) as i16).collect();
        let tr = vec![(0, "A".to_string(), 1, Arc::new(tone)), (2, "B".to_string(), 1, Arc::new(vec![0i16; 4800]))];
        let pg = AtomicU32::new(0);
        let (ch, msg) = normalize_tracks(tr.clone(), -16.0, &pg, &AtomicBool::new(false))?;
        let l = loudness_lufs(&ch[0].1, 1).unwrap_or(-99.0);
        assert!(ch.len() == 1 && ch[0].0 == 0 && ch[0].2 && (l + 16.0).abs() < 0.3 && pg.load(Relaxed) == 1000, "normalize_tracks: {l} {msg}");
        assert!(msg.contains("silent"));
        assert!(normalize_tracks(tr, -16.0, &pg, &AtomicBool::new(true)).is_err());
        println!("normalize job: {msg}");
    }
    // Renderer: which frames change, and what they look like
    {
        let mut d = Edit { segs: vec![Seg::Src { start: 0, len: n }], ov: ov_all.clone(), ..Default::default() };
        d.fx.cam_show = false;
        d.fx.click_rings = false;
        let rc = RenderCtx::new(&src, &d).ok_or("no render ctx")?;
        assert!(rc.needs(0) && rc.needs(45) && !rc.needs(n - 1), "needs() wrong");
        let z = rc.zoom_at(30);
        assert!((z[0] - 2.0).abs() < 1e-3 && (z[1] - 0.5).abs() < 1e-3, "zoom at 30: {z:?}");
        assert!(rc.zoom_at(10)[0] < 1.2 && rc.zoom_at(49)[0] < 1.2, "zoom must ease in/out");
        let j = src.read(rc.map[20])?;
        let t = Instant::now();
        let (px, w, h) = rc.render(20, &j)?;
        println!("render frame 20 ({w}x{h}, text+arrow+blur+pixelate+step+zoom) in {:.1} ms", t.elapsed().as_secs_f64() * 1000.0);
        assert_eq!((w, h), (src.w as usize, src.h as usize));
        assert_eq!(px.len(), w * h * 3);
        // pixelate: a block is flat (no zoom at frame 5 apart from easing; use frame 2)
        let (px2, _, _) = rc.render(2, &src.read(rc.map[2])?)?;
        let (bx, by) = ((0.56 * w as f32) as usize + 1, (0.06 * h as f32) as usize + 1);
        let blk = ((0.3 * w as f32).min(0.25 * h as f32) as usize / 8).clamp(10, 64);
        let p0 = &px2[(by * w + bx) * 3..(by * w + bx) * 3 + 3];
        let p1 = &px2[(by * w + bx + 2) * 3..(by * w + bx + 2) * 3 + 3];
        assert!(blk >= 10 && p0 == p1, "pixelate block not flat");
        // RenderCtx::None when nothing can change: export stays a byte copy
        let plain = Edit { segs: vec![Seg::Src { start: 0, len: n }], ..Default::default() };
        let mut plain_nocam = plain.clone();
        plain_nocam.fx.cam_show = false;
        assert!(RenderCtx::new(&src, &plain_nocam).is_none() || src.has_ev());
        // export with effects: only affected frames re-encoded
        let mk = out.join("selftest_fx.mkv");
        let t = Instant::now();
        export_mkv(&mk, &src, &d, &AtomicU32::new(0), &AtomicBool::new(false), (0, 1000))?;
        let need = (0..n).filter(|&f| rc.needs(f)).count();
        println!("export with effects: {need}/{n} frames rendered, {:.3}s, {} bytes", t.elapsed().as_secs_f64(), std::fs::metadata(&mk).map_err(es)?.len());
        if let Some(ff) = find_ffmpeg() {
            let probe = ffmpeg_cmd(&ff).args(["-v", "error", "-i"]).arg(&mk).args(["-f", "null", "-"]).output().map_err(es)?;
            assert!(probe.status.success() && probe.stderr.is_empty(), "ffmpeg decode errors: {}", String::from_utf8_lossy(&probe.stderr));
            println!("ffmpeg decodes the effects export cleanly");
        }
        // Webcam PiP (all shapes + background blur) on a real recorded camera frame
        let mut dc = plain.clone();
        for shape in 0..3u8 {
            dc.fx = FxSettings { cam_shape: shape, cam_blur: shape == 0, cam_pos: shape, ..FxSettings::default() };
            let rc = RenderCtx::new(&src, &dc).ok_or("cam ctx")?;
            assert!(rc.needs(n / 2));
            let (o, w, h) = rc.render(n / 2, &src.read(n / 2)?)?;
            let base = decode_jpeg_rgb(&src.read(n / 2)?)?.0;
            let diff = o.iter().zip(base.iter()).filter(|(a, b)| a != b).count();
            let frac = diff as f64 / (w * h * 3) as f64;
            println!("cam PiP shape {} ({}): {:.1}% of pixels changed", shape, CAM_SHAPES[shape as usize], frac * 100.0);
            assert!(frac > 0.005 && frac < 0.25, "PiP area implausible");
        }
    }
    // Click rings + auto zoom from (synthetic) input events
    {
        let mut s2 = Source::open(&dir, dims.0, dims.1, fps)?;
        let k = n / 3;
        for f in 0..n as usize { s2.ev[f] = Ev { x: (dims.0 / 3) as i16, y: (dims.1 / 3) as i16, f: EV_VIS }; }
        s2.ev[k as usize].f |= EV_CLICK | EV_LDOWN;
        let s2 = Arc::new(s2);
        let d = Edit { segs: vec![Seg::Src { start: 0, len: n }], fx: FxSettings { cam_show: false, ..Default::default() }, ..Default::default() };
        let rc = RenderCtx::new(&s2, &d).ok_or("ring ctx")?;
        assert!(rc.needs(k) && rc.needs(k + rc.ring_n - 1) && !rc.needs(k + rc.ring_n) && (k == 0 || !rc.needs(k - 1)), "ring frames wrong");
        let z = auto_zoom(&s2, &d, 2.0);
        assert_eq!(z.len(), 1);
        assert!(z[0].start <= k && z[0].end > k && z[0].end - z[0].start >= 2 * fps.min(n));
        println!("click ring spans {} frames; auto zoom {}..{} around click at {k}", rc.ring_n, z[0].start, z[0].end);
    }
    // Project save/load keeps overlays + fx; autosave round trip + newer-autosave detection
    {
        let pd = out.join("st_proj14");
        let _ = std::fs::remove_dir_all(&pd);
        std::fs::create_dir_all(&pd).map_err(es)?;
        for ext in MEDIA_EXTS { if dir.join(format!("frames.{ext}")).exists() { std::fs::copy(dir.join(format!("frames.{ext}")), pd.join(format!("frames.{ext}"))).map_err(es)?; } }
        let ps = Arc::new(Source::open(&pd, dims.0, dims.1, fps)?);
        let tr = Track { name: "Voice".into(), ch: 1, data: Arc::new(vec![100i16; n as usize * spf]), gain: 1.0, muted: false };
        let mut pr = Project { edit: Edit { segs: vec![Seg::Src { start: 0, len: n }], tracks: vec![tr], ov: ov_all.clone(), fx: fxs }, src: ps, undo: vec![], redo: vec![], dirty: true, saved: vec![] };
        pr.save()?;
        let l = Project::load(&pd)?;
        assert_eq!(l.edit.ov, ov_all.iter().filter(|o| o.start < n).cloned().map(|mut o| { o.end = o.end.min(n); o }).collect::<Vec<_>>());
        assert_eq!(l.edit.fx, fxs);
        assert!(!has_newer_autosave(&pd));
        thread::sleep(Duration::from_millis(20));
        pr.edit.ov.truncate(2);
        pr.edit.tracks[0].data = Arc::new(vec![7i16; n as usize * spf]); // changed track -> autosave_track_0.wav
        pr.dirty = true;
        (pr.autosave_job())()?;
        assert!(has_newer_autosave(&pd) && pd.join("autosave_track_0.wav").is_file());
        let r = Project::load_from(&pd, AUTOSAVE_TXT)?;
        assert!(r.dirty && r.edit.ov.len() == 2 && r.edit.tracks[0].data[5] == 7);
        pr.save()?;
        assert!(!pd.join(AUTOSAVE_TXT).exists() && !pd.join("autosave_track_0.wav").exists(), "save must clear autosave");
        println!("project: overlays/fx persist; autosave restore + cleanup ok");
    }
    // Crash recovery: torn tail (half-written JPEG + partial index entry), raw audio spill
    {
        let rd = out.join("st_crash");
        let _ = std::fs::remove_dir_all(&rd);
        std::fs::create_dir_all(&rd).map_err(es)?;
        for ext in MEDIA_EXTS { if dir.join(format!("frames.{ext}")).exists() { std::fs::copy(dir.join(format!("frames.{ext}")), rd.join(format!("frames.{ext}"))).map_err(es)?; } }
        let mlen = std::fs::metadata(rd.join("frames.mjpg")).map_err(es)?.len();
        let last = src.entry(n - 1).ok_or("idx")?;
        // the last frame's JPEG is only half on disk (unique last frame) + 5 stray idx bytes
        let cut_to = if last.0 + last.1 as u64 == mlen { last.0 + last.1 as u64 / 2 } else { mlen };
        std::fs::OpenOptions::new().write(true).open(rd.join("frames.mjpg")).map_err(es)?.set_len(cut_to).map_err(es)?;
        std::fs::OpenOptions::new().append(true).open(rd.join("frames.idx")).map_err(es)?.write_all(&[1, 2, 3, 4, 5]).map_err(es)?;
        let raw: Vec<u8> = (0..(n as usize - 3) * spf).flat_map(|i| (((i % 200) as i16 - 100) * 50).to_le_bytes()).collect();
        std::fs::write(rd.join("mic.raw"), raw).map_err(es)?;
        std::fs::write(rd.join(REC_MARK), format!("screenreel_rec=1\nwidth={}\nheight={}\nfps={fps}\nmic_ch=1\nsys_ch=0\ncam=1\n", dims.0, dims.1)).map_err(es)?;
        let p = recover_recording(&rd)?;
        let got = p.src.count();
        let expect_max = if cut_to < mlen { (0..n).take_while(|&f| src.entry(f).map(|e| e.0 + e.1 as u64 <= cut_to).unwrap_or(false)).count() as u32 } else { n };
        assert!(got == expect_max && got >= n.saturating_sub(2), "recovered {got} frames, expected {expect_max}");
        assert_eq!(p.edit.tracks.len(), 1);
        assert_eq!(p.edit.tracks[0].frames(), got as usize * spf, "recovered mic not fitted");
        assert!(rd.join("project.txt").is_file() && !rd.join(REC_MARK).exists() && !rd.join("mic.raw").exists());
        assert_eq!(p.src.ev.len(), got as usize);
        println!("crash recovery: {got}/{n} frames (torn tail dropped), mic fitted, spill cleaned");
    }

    // synthetic 440 Hz tone track + edits
    let tone: Vec<i16> = (0..n as usize * spf).map(|i| ((i as f32 * 440.0 * std::f32::consts::TAU / AR as f32).sin() * 8000.0) as i16).collect();
    let mut e = Edit { segs: vec![Seg::Src { start: 0, len: n }], tracks: vec![Track { name: "Tone".into(), ch: 1, data: Arc::new(tone), gain: 1.0, muted: false }], ..Default::default() };
    if let Some((ch, v)) = sys_trk {
        // loopback track must be fitted exactly to the video and carry real signal
        assert_eq!(v.len(), n as usize * spf * ch as usize, "system track length != video length");
        let peak = v.iter().map(|x| x.unsigned_abs()).max().unwrap_or(0);
        let rms = (v.iter().map(|&x| (x as f64).powi(2)).sum::<f64>() / v.len().max(1) as f64).sqrt();
        write_wav(&out.join("selftest_system.wav"), ch, AR, &v)?;
        println!("system audio: {ch} ch, {:.2}s, peak {peak}, rms {rms:.0}", v.len() as f64 / ch as f64 / AR as f64);
        if peak < 300 { return Err("system audio track is silent (was anything playing?)".into()); }
        e.tracks.push(Track { name: "System".into(), ch, data: Arc::new(v), gain: 1.0, muted: false });
    }
    e.cut(10, 20, spf);
    assert_eq!(e.total(), n - 10);
    assert_eq!(e.tracks[0].frames(), (n as usize - 10) * spf);
    e.insert_freeze(5, 30, spf);
    assert_eq!(e.total(), n + 20);
    assert_eq!(e.tracks[0].frames(), (n as usize + 20) * spf);
    e.freeze_range(40, 50);
    assert_eq!(e.total(), n + 20);
    assert_eq!(e.src_frame(5), Some(5));
    assert_eq!(e.src_frame(34), Some(5));
    e.shift_track(0, 2, spf);
    e.shift_track(0, -2, spf);
    assert_eq!(e.tracks[0].frames(), (n as usize + 20) * spf);
    // --- v1.2.0: WSOLA keeps exact length and pitch (zero-crossing rate of the 440 Hz tone)
    {
        let x: Vec<i16> = (0..AR as usize * 2).map(|i| ((i as f32 * 440.0 * std::f32::consts::TAU / AR as f32).sin() * 8000.0) as i16).collect();
        let y = time_stretch(&x, 1, AR as usize); // 2x
        assert_eq!(y.len(), AR as usize);
        let zc = y.windows(2).filter(|w| (w[0] < 0) != (w[1] < 0)).count() as f64 / 2.0; // Hz over 1 s
        println!("time_stretch 2x: {} samples, ~{zc:.0} Hz (want ~440)", y.len());
        assert!((400.0..480.0).contains(&zc), "pitch not preserved: {zc} Hz");
        let big: Vec<i16> = (0..AR as usize * 60 * 2).map(|i| ((i as f32 * 0.013).sin() * 9000.0) as i16).collect();
        let t = Instant::now();
        let z = time_stretch(&big, 2, AR as usize * 30);
        assert_eq!(z.len(), AR as usize * 60);
        println!("time_stretch 60 s stereo -> 30 s in {:.3}s", t.elapsed().as_secs_f64());
    }
    // --- v1.3.0: loudness meter accuracy, normalization ceiling, opto compressor gain reduction
    {
        let sine = |amp: f32, secs: usize, ch: usize| -> Vec<i16> {
            (0..AR as usize * secs).flat_map(|i| { let v = ((i as f32 * 997.0 * std::f32::consts::TAU / AR as f32).sin() * amp * 32767.0) as i16; std::iter::repeat(v).take(ch) /* v1.3.1 FIX: repeat_n needs Rust 1.82 > MSRV 1.79 */ }).collect()
        };
        // BS.1770: a full-scale-relative stereo 997 Hz sine at A dBFS peak reads ~A LUFS
        let st = sine(0.1, 4, 2);
        let l = loudness_lufs(&st, 2).ok_or("stereo tone measured silent")?;
        let lm = loudness_lufs(&sine(0.1, 4, 1), 1).ok_or("mono tone measured silent")?;
        println!("loudness: stereo -20 dBFS tone {l:.2} LUFS, mono (dual-mono) {lm:.2} LUFS");
        assert!((l + 20.0).abs() < 0.8 && (lm - l).abs() < 0.05, "BS.1770 loudness off");
        assert!(loudness_lufs(&vec![0i16; AR as usize * 2], 1).is_none(), "silence must be gated out");
        // normalize a quiet mono track up to -16 LUFS: level hits target, peaks stay <= -1 dBFS
        let quiet = sine(0.02, 4, 1);
        let lq = loudness_lufs(&quiet, 1).unwrap();
        let (v, lim) = gain_with_limiter(&quiet, 1, 10f32.powf((-16.0 - lq) as f32 / 20.0), 10f32.powf(-1.0 / 20.0));
        let ln = loudness_lufs(&v, 1).unwrap();
        let pk = v.iter().map(|x| x.unsigned_abs()).max().unwrap_or(0) as f32 / 32768.0;
        println!("normalize: {lq:.1} -> {ln:.2} LUFS, peak {:.2} dBFS, limiter {lim:.2} dB", 20.0 * pk.log10());
        assert!((ln + 16.0).abs() < 0.6, "normalized loudness off target");
        assert!(pk <= 10f32.powf(-1.0 / 20.0) + 1e-4, "peak ceiling exceeded");
        // a hot track must be limited, never clipped
        let (h, lim2) = gain_with_limiter(&sine(0.9, 2, 1), 1, 4.0, 10f32.powf(-1.0 / 20.0));
        let pk2 = h.iter().map(|x| x.unsigned_abs()).max().unwrap_or(0) as f32 / 32768.0;
        assert!(pk2 <= 0.8913 + 1e-3 && lim2 > 10.0, "limiter failed: peak {pk2} lim {lim2}");
        // opto compressor: -6 dBFS tone, thr -20, 4:1, no makeup/warmth -> ~10.5 dB reduction
        let pr = OptoParams { threshold_db: -20.0, ratio: 4.0, attack_ms: 5.0, release_s: 0.3, warmth: 0.0, makeup_db: 0.0 };
        let hot = sine(0.5, 3, 1);
        let t3 = Instant::now();
        let (c, cs) = opto_compress(&hot, 1, pr);
        assert_eq!(c.len(), hot.len());
        let tail = &c[c.len() - AR as usize / 2..];
        let pko = tail.iter().map(|x| x.unsigned_abs()).max().unwrap_or(0) as f32 / 32768.0;
        println!("opto: max GR {:.1} dB, steady out peak {:.2} dBFS (want ~-16.5) in {:.3}s", cs.max_gr_db, 20.0 * pko.log10(), t3.elapsed().as_secs_f64());
        assert!((20.0 * pko.log10() + 16.5).abs() < 1.0, "compressor steady-state gain wrong");
        // warmth: unity small-signal gain, adds harmonics, no DC
        let (wv, _) = opto_compress(&sine(0.01, 2, 1), 1, OptoParams { warmth: 1.0, threshold_db: 0.0, ..pr });
        let rms = |v: &[i16]| (v.iter().map(|&x| (x as f64).powi(2)).sum::<f64>() / v.len() as f64).sqrt();
        let ratio = rms(&wv[AR as usize..]) / rms(&sine(0.01, 2, 1)[AR as usize..]);
        let dc = wv[AR as usize..].iter().map(|&x| x as f64).sum::<f64>() / AR as f64;
        assert!((ratio - 1.0).abs() < 0.05 && dc.abs() < 5.0, "tube stage not unity/DC-free: {ratio} {dc}");
    }
    // --- v1.4.0: voice chain (RNNoise, gate, EQ, de-esser, compressor) + auto-duck
    {
        let rms_db = |v: &[i16]| { let r = (v.iter().map(|&x| (x as f64).powi(2)).sum::<f64>() / v.len().max(1) as f64).sqrt(); 20.0 * (r / 32768.0).max(1e-9).log10() };
        let tone = |hz: f32, amp: f32, secs: f32| -> Vec<i16> { (0..(AR as f32 * secs) as usize).map(|i| ((i as f32 * hz * std::f32::consts::TAU / AR as f32).sin() * amp * 32767.0) as i16).collect() };
        // deterministic white noise (xorshift)
        let noise = |amp: f32, n: usize| -> Vec<i16> { let mut x = 0x9E3779B9u32; (0..n).map(|_| { x ^= x << 13; x ^= x >> 17; x ^= x << 5; ((x as f32 / u32::MAX as f32 * 2.0 - 1.0) * amp * 32767.0) as i16 }).collect() };
        // voiced, speech-like test signal: 140 Hz harmonic stack with a 4 Hz syllable envelope
        let voice: Vec<i16> = (0..AR as usize * 3).map(|i| {
            let t = i as f32 / AR as f32;
            let env = (0.5 - 0.5 * (std::f32::consts::TAU * 4.0 * t).cos()).powi(2);
            let v: f32 = (1..12).map(|k| (std::f32::consts::TAU * 140.0 * k as f32 * t).sin() / k as f32).sum();
            (v * env * 0.25 * 32767.0) as i16 }).collect();
        // exact length for any chunking
        for chunk in [1usize, 37, 480, 1000, 48000] {
            let mut vc = VoiceChain::new(VoiceFx::default());
            let mut out = Vec::new();
            for c in voice.chunks(chunk) { vc.push(c, &mut out); }
            vc.finish(&mut out);
            assert_eq!(out.len(), voice.len(), "voice chain length (chunk {chunk})");
        }
        // RNNoise delay compensation: output must line up with input (cross-correlation peak at 0)
        let nr_only = VoiceFx { nr: true, gate: false, eq: 0, deess: 0.0, comp: 0, ..VoiceFx::default() };
        let y = voice_process(&voice, 1, nr_only, None, None)?;
        let corr = |lag: i64| -> f64 { (AR as i64..2 * AR as i64).map(|i| voice[i as usize] as f64 * y[(i + lag) as usize] as f64).sum() };
        let best = (-700i64..=700).step_by(5).max_by(|a, b| corr(*a).partial_cmp(&corr(*b)).unwrap()).unwrap_or(0);
        println!("voice chain delay: best lag {best} samples (want ~0), speech kept {:.1} dB -> {:.1} dB", rms_db(&voice), rms_db(&y));
        assert!(best.abs() <= 10, "RNNoise delay not compensated: {best}");
        assert!(rms_db(&y) > rms_db(&voice) - 6.0, "NR removed the voice");
        // NR removes steady noise
        let nz = noise(0.03, AR as usize * 2);
        let yn = voice_process(&nz, 1, nr_only, None, None)?;
        println!("NR: white noise {:.1} dB -> {:.1} dB", rms_db(&nz[AR as usize..]), rms_db(&yn[AR as usize..]));
        assert!(rms_db(&yn[AR as usize..]) < rms_db(&nz[AR as usize..]) - 10.0, "noise reduction too weak");
        // gate: room tone below threshold drops ~30 dB, speech passes
        let gate_only = VoiceFx { nr: false, gate: true, gate_db: -45.0, eq: 0, deess: 0.0, comp: 0 };
        let q = noise(0.002, AR as usize); // -54 dBFS
        let yq = voice_process(&q, 1, gate_only, None, None)?;
        assert!(rms_db(&yq[AR as usize / 2..]) < rms_db(&q[AR as usize / 2..]) - 25.0, "gate did not close");
        let yv = voice_process(&voice, 1, gate_only, None, None)?;
        assert!((rms_db(&yv) - rms_db(&voice)).abs() < 1.5, "gate hurt speech");
        // de-esser: 7 kHz sibilance cut, 500 Hz untouched
        let ds = VoiceFx { nr: false, gate: false, eq: 0, deess: 1.0, comp: 0, ..VoiceFx::default() };
        let (s7, s5) = (tone(7000.0, 0.3, 1.0), tone(500.0, 0.3, 1.0));
        let (o7, o5) = (voice_process(&s7, 1, ds, None, None)?, voice_process(&s5, 1, ds, None, None)?);
        let (d7, d5) = (rms_db(&o7[24000..]) - rms_db(&s7[24000..]), rms_db(&o5[24000..]) - rms_db(&s5[24000..]));
        println!("de-esser: 7 kHz {d7:+.1} dB, 500 Hz {d5:+.2} dB");
        assert!(d7 < -4.0 && d5.abs() < 0.3, "de-esser band split wrong");
        // EQ: high-pass removes rumble, 1 kHz ~unity
        let eq = VoiceFx { nr: false, gate: false, eq: 1, deess: 0.0, comp: 0, ..VoiceFx::default() };
        let (r30, k1) = (tone(30.0, 0.3, 1.0), tone(1000.0, 0.3, 1.0));
        let (e30, e1) = (rms_db(&voice_process(&r30, 1, eq, None, None)?[24000..]) - rms_db(&r30[24000..]), rms_db(&voice_process(&k1, 1, eq, None, None)?[24000..]) - rms_db(&k1[24000..]));
        println!("EQ voice clarity: 30 Hz {e30:+.1} dB, 1 kHz {e1:+.2} dB");
        assert!(e30 < -12.0 && e1.abs() < 1.0, "voice EQ response wrong");
        // stereo track: per-channel processing keeps interleaving + length
        let st2: Vec<i16> = voice.iter().flat_map(|&v| [v, v / 2]).collect();
        assert_eq!(voice_process(&st2, 2, VoiceFx::default(), None, None)?.len(), st2.len());
        // auto-duck: music dips ~12 dB under voice, recovers in the gap
        let n = AR as usize * 4;
        let talk: Vec<i16> = (0..n).map(|i| if i < AR as usize * 2 { voice[i % voice.len()] } else { 0 }).collect();
        let music = tone(220.0, 0.2, 4.0);
        let vt = [MixTrack { data: Arc::new(talk), ch: 1, gain: 1.0 }];
        let env = duck_envelope(&vt, n, DuckParams::default());
        let ducked = apply_duck(&music, 1, &env);
        let (dv, dg) = (rms_db(&ducked[AR as usize / 2..AR as usize * 3 / 2]) - rms_db(&music[..AR as usize]), rms_db(&ducked[AR as usize * 3 + AR as usize / 2..]) - rms_db(&music[..AR as usize]));
        println!("auto-duck: under voice {dv:+.1} dB, after release {dg:+.1} dB");
        assert!(dv < -9.0 && dg > -1.0 && ducked.len() == music.len(), "auto-duck wrong");
        // throughput
        let long: Vec<i16> = voice.iter().cycle().take(AR as usize * 60).copied().collect();
        let tv = Instant::now();
        voice_process(&long, 1, VoiceFx::default(), None, None)?;
        println!("voice chain: 60 s mono in {:.3}s (live headroom {:.0}x real time)", tv.elapsed().as_secs_f64(), 60.0 / tv.elapsed().as_secs_f64());
    }
    // --- v1.2.0: speed-up of a range (ripple), Fast seg mapping + save/load round trip
    let before_speed = e.total();
    let a0 = 3u32;
    let b0 = (a0 + 40).min(before_speed);
    let end = e.speed_range(a0, b0, 200, false, spf);
    // each segment piece rounds up independently, so allow one extra frame per piece
    assert!(end - a0 >= (b0 - a0) / 2 && end - a0 <= (b0 - a0) / 2 + 4, "2x speed must ~halve the range: {}", end - a0);
    assert_eq!(e.total(), before_speed - (b0 - a0) + (end - a0));
    for t in &e.tracks { assert_eq!(t.frames(), e.total() as usize * spf, "track length after speed-up"); }
    assert!(e.segs.iter().any(|s| matches!(s, Seg::Fast { .. })), "expected a Fast segment");
    for f in 0..e.total() { assert!(e.src_frame(f).map(|x| x < n).unwrap_or(false), "speed-up frame {f} out of range"); }
    // splitting a Fast segment must not change the frame mapping
    let map0: Vec<Option<u32>> = (0..e.total()).map(|f| e.src_frame(f)).collect();
    let k = e.split(a0 + 5); let _ = k; e.normalize();
    assert_eq!(map0, (0..e.total()).map(|f| e.src_frame(f)).collect::<Vec<_>>(), "split/normalize changed Fast mapping");
    // --- v1.2.0: dead air. Silence a tone stretch; the Xvfb screen is static, so it must be found.
    {
        let mut d = Edit { segs: vec![Seg::Src { start: 0, len: n }], tracks: vec![], ..Default::default() };
        let (q0, q1) = (n / 4, n / 4 + (n / 2).max(1));
        let tone: Vec<i16> = (0..n as usize * spf).map(|i| {
            let f = (i / spf) as u32;
            if f >= q0 && f < q1 { 0 } else { ((i as f32 * 440.0 * std::f32::consts::TAU / AR as f32).sin() * 8000.0) as i16 }
        }).collect();
        d.tracks.push(Track { name: "T".into(), ch: 1, data: Arc::new(tone), gain: 1.0, muted: false });
        let cfg = DeadCfg { min_frames: fps / 2, pad_frames: 2, ..DeadCfg::default() };
        let t2 = Instant::now();
        let r = find_dead_air(&src, &d, cfg, &AtomicU32::new(0), &AtomicBool::new(false))?;
        println!("dead air: {r:?} (silence {q0}..{q1}) in {:.3}s", t2.elapsed().as_secs_f64());
        if !full {
            // region capture of a static Xvfb root: exactly one range inside the silence
            assert_eq!(r.len(), 1, "expected one dead-air range");
            assert!(r[0].0 >= q0 && r[0].1 <= q1 && r[0].1 - r[0].0 >= (q1 - q0) / 2, "dead-air range misplaced");
            let tot = d.total();
            d.cut_many(&r, spf);
            assert_eq!(d.total(), tot - (r[0].1 - r[0].0));
            assert_eq!(d.tracks[0].frames(), d.total() as usize * spf);
        }
    }
    let mut p = Project { src: src.clone(), edit: e.clone(), undo: vec![], redo: vec![], dirty: true, saved: vec![] };
    p.save()?;
    let mut p2 = Project::load(&dir)?;
    assert_eq!(p2.edit.segs, p.edit.segs);
    assert_eq!(p2.edit.tracks[0].data.len(), p.edit.tracks[0].data.len());
    // --- v1.3.1: save skips unchanged tracks, removes stale/staging files only after the swap
    {
        let t0p = dir.join("track_0.wav");
        let m0 = std::fs::metadata(&t0p).and_then(|m| m.modified()).map_err(es)?;
        std::fs::write(dir.join("track_9.wav"), b"stale").map_err(es)?;
        std::fs::write(dir.join("track_0.wav.new"), b"interrupted").map_err(es)?;
        p2.save()?;
        let m1 = std::fs::metadata(&t0p).and_then(|m| m.modified()).map_err(es)?;
        assert_eq!(m0, m1, "unchanged track was rewritten on save");
        assert!(!dir.join("track_9.wav").exists() && !dir.join("track_0.wav.new").exists(), "stale files not cleaned");
        // a changed track IS rewritten and reloads identically
        let mut v = (*p2.edit.tracks[0].data).clone();
        v[0] = 1234;
        p2.edit.tracks[0].data = Arc::new(v);
        p2.save()?;
        assert_eq!(Project::load(&dir)?.edit.tracks[0].data[0], 1234, "changed track not saved");
        // non-finite gain in project.txt is rejected (v1.3.0 accepted NaN and muted the mix)
        let pt = dir.join("project.txt");
        let good = std::fs::read_to_string(&pt).map_err(es)?;
        let bad: String = good.lines().map(|l| if l.starts_with("track=") {
            let mut f: Vec<&str> = l.split('|').collect(); f[1] = "NaN"; f.join("|") } else { l.to_string() } + "\n").collect();
        std::fs::write(&pt, bad).map_err(es)?;
        assert!(Project::load(&dir).is_err(), "NaN gain accepted");
        std::fs::write(&pt, good).map_err(es)?;
        assert_eq!(fmt_time(30 * 3600, 30), "1:00:00.00");
        assert_eq!(fmt_time(30 * 61, 30), "01:01.00");
        println!("save: unchanged tracks skipped, stale files cleaned, NaN gain rejected");
    }
    // --- v1.2.0: compaction keeps every output frame byte-identical
    {
        let (cs, before, after) = p2.src.compact(&p2.edit, &AtomicU32::new(0))?;
        assert_eq!(cs.count(), p2.edit.total());
        for f in (0..p2.edit.total()).step_by(7) {
            assert_eq!(cs.read(f)?, p2.src.read(p2.edit.src_frame(f).unwrap())?, "compacted frame {f} differs");
        }
        println!("compact: {before} -> {after} bytes, {} index entries", cs.count());
        for ext in MEDIA_EXTS { let _ = std::fs::remove_file(dir.join(format!("{}.{ext}", cs.base))); }
    }
    let mkv = out.join("selftest.mkv");
    let t1 = Instant::now();
    export_mkv(&mkv, &src, &p2.edit, &AtomicU32::new(0), &AtomicBool::new(false), (0, 1000))?;
    println!("export {} ({} bytes) in {:.3}s", mkv.display(), std::fs::metadata(&mkv).map_err(es)?.len(), t1.elapsed().as_secs_f64());
    export_wav(&out.join("selftest.wav"), &src, &p2.edit, &AtomicU32::new(0), &AtomicBool::new(false))?;
    // v1.3.1: a cancelled WAV export leaves neither the target nor its temp file behind
    {
        let cw = out.join("selftest_cancel.wav");
        assert!(export_wav(&cw, &src, &p2.edit, &AtomicU32::new(0), &AtomicBool::new(true)).is_err());
        assert!(!cw.exists() && !cw.with_extension("wav.tmp").exists(), "cancelled export left files");
        let cm = out.join("selftest_cancel.mkv");
        assert!(export_mkv(&cm, &src, &p2.edit, &AtomicU32::new(0), &AtomicBool::new(true), (0, 1000)).is_err());
        assert!(!cm.exists() && !cm.with_extension("mkv.part").exists(), "cancelled MKV left files");
    }
    // v1.3.1: ffmpeg runs as a polled, cancellable child with -progress (when installed)
    if let Some(ff) = find_ffmpeg() {
        let dur_us = p2.edit.total() as u64 * 1_000_000 / fps as u64;
        let (mov, pg) = (out.join("selftest.mov"), AtomicU32::new(0));
        ffmpeg_convert(&ff, &mkv, &mov, true, dur_us, &pg, &AtomicBool::new(false), (500, 1000))?;
        assert!(std::fs::metadata(&mov).map(|m| m.len() > 1000).unwrap_or(false) && pg.load(Relaxed) == 1000, "MOV remux failed");
        let mp4 = out.join("selftest_cancel.mp4");
        assert!(ffmpeg_convert(&ff, &mkv, &mp4, false, dur_us, &pg, &AtomicBool::new(true), (0, 1000)).is_err());
        assert!(!mp4.exists(), "cancelled ffmpeg left output");
        // v1.4.0: full MP4 encode with whichever H.264 encoder was picked (GPU or libx264)
        let mp4 = out.join("selftest.mp4");
        let t = Instant::now();
        let enc = ffmpeg_convert(&ff, &mkv, &mp4, false, dur_us, &pg, &AtomicBool::new(false), (0, 1000))?;
        let fp_name = if cfg!(windows) { "ffprobe.exe" } else { "ffprobe" };
        let fp = Some(ff.with_file_name(fp_name)).filter(|p| p.is_file()).unwrap_or_else(|| PathBuf::from(fp_name));
        let probe = std::process::Command::new(fp)
            .args(["-v", "error", "-count_frames", "-select_streams", "v:0", "-show_entries", "stream=nb_read_frames,codec_name", "-of", "csv=p=0"])
            .arg(&mp4).output().ok().map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).unwrap_or_default();
        println!("ffmpeg: MP4 via {enc} in {:.2}s, ffprobe: {probe}", t.elapsed().as_secs_f64());
        if let Some(nf) = probe.split(',').nth(1).and_then(|v| v.trim().parse::<u32>().ok()) {
            assert!(probe.starts_with("h264") && nf == p2.edit.total(), "MP4 frames {nf} != {}", p2.edit.total());
        }
        println!("ffmpeg: MOV remux ok, cancel kills the child and removes output");
    }
    let (ch, d) = decode_audio(&out.join("selftest.wav"))?;
    assert_eq!(ch, 2);
    assert_eq!(d.len() / 2, p2.edit.total() as usize * spf);
    let j = src.read(0)?;
    let (_, w, h) = decode_jpeg_rgb(&j)?;
    assert_eq!((w as u32, h as u32), dims);
    println!("selftest OK: {} output frames, {:.2}s", p2.edit.total(), p2.edit.total() as f64 / fps as f64);
    Ok(())
}

// =====================================================================================
// main
// =====================================================================================

fn main() {
    cap::set_dpi_aware();
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--version" || a == "-V") { println!("{FILE_TAG} ({APP_NAME} {VERSION})"); return; }
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("{APP_NAME} {VERSION}\nUsage: screenreel [project_dir] | --selftest <outdir> [seconds] [full] [sys] | --probe-audio <file> | --version");
        return;
    }
    if let Some(f) = args.iter().position(|a| a == "--probe-audio").and_then(|i| args.get(i + 1)) {
        match decode_audio(Path::new(f)) {
            Ok((ch, d)) => println!("{f}: {ch} ch, {:.3}s @ {AR} Hz (decoded + resampled)", d.len() as f64 / ch as f64 / AR as f64),
            Err(e) => { eprintln!("{f}: {e}"); std::process::exit(1); }
        }
        return;
    }
    if let Some(i) = args.iter().position(|a| a == "--selftest") {
        let out = PathBuf::from(args.get(i + 1).cloned().unwrap_or_else(|| ".".into()));
        let secs = args.get(i + 2).and_then(|s| s.parse().ok()).unwrap_or(3.0);
        let _ = std::fs::create_dir_all(&out);
        let rest = &args[(i + 3).min(args.len())..];
        let full = rest.iter().any(|s| s == "full");
        let sys = rest.iter().any(|s| s == "sys");
        match selftest(&out, secs, full, sys) { Ok(()) => return, Err(e) => { eprintln!("selftest FAILED: {e}"); std::process::exit(1); } }
    }

    let a = app::App::default().with_scheme(app::Scheme::Gtk);
    let play = Arc::new(PlayShared::new());
    let (out, out_err) = match start_output(play.clone()) { Ok(s) => (Some(s), None), Err(e) => (None, Some(e)) };
    let app_state: Shared = std::rc::Rc::new(std::cell::RefCell::new(App {
        proj: None, play, out, clock: None, playhead: 0, sel: None, sel_track: None, zoom: 1.0, scroll: 0.0, drag_anchor: None,
        peaks: HashMap::new(), rec: None, rec_pending: false, ctl: None, narr: None, dec: Decoder::new(), preview: None, job: None,
        edit_gen: 0, dead: Vec::new(), dead_gen: u64::MAX, space_at: None,
        fx_preview: None, vu: [0.0; 2], vu_hold: [(0.0, None); 2],
        mics: input_device_names(),
        rctx: None, tool: 0, ov_sel: None, ov_drag: None, ov_color: 0, pv_rect: (0, 0, 0, 0), cams: Vec::new(),
        autosave: None, last_autosave: Instant::now(),
        status: match out_err { Some(e) => format!("Ready (no audio output: {e}; preview is silent)"), None => "Ready".into() },
        ffmpeg: find_ffmpeg(),
    }));
    app_state.borrow_mut().cams = cam::list(); // v1.4.0 (before build_ui fills the chooser)
    let ui = build_ui(&app_state);
    wire(&app_state, &ui);
    let mut w = ui.win.clone();
    w.show();
    refresh(&app_state, &ui);
    if args.get(1).map(|p| p.starts_with('-')).unwrap_or(true) { offer_recovery(&app_state, &ui); refresh(&app_state, &ui); }
    {
        let (s, u) = (app_state.clone(), ui.clone());
        app::add_timeout3(1.0 / 60.0, move |h| { tick(&s, &u); app::repeat_timeout3(1.0 / 60.0, h); });
    }
    if let Some(p) = args.get(1).filter(|p| !p.starts_with('-')) {
        match open_project(Path::new(p)) {
            Ok(pr) => app_state.borrow_mut().set_project(Some(pr), ui.timeline.w()), // v1.3.1: one code path
            Err(e) => app_state.borrow_mut().status = format!("Open failed: {e}"),
        }
        refresh(&app_state, &ui);
    }
    a.run().ok();
    // Explicit teardown order: stop recording threads before streams drop.
    let teardown = app_state.try_borrow_mut();
    if let Ok(mut s) = teardown {
        if let Some(r) = s.rec.take() { let _ = r.finish(); }
        s.narr.take();
        s.out.take();
    };
}
