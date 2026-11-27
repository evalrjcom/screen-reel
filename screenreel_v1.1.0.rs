//! ScreenReel v1.1.0 — lightweight screen recorder + editor (Rust + FLTK)
//! File: screenreel_v1.1.0.rs
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
//!   paths: lossless MOV remux (-c copy) or H.264/AAC MP4 (re-encode, labelled).
//!
//! Changelog
//! ---------
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
    valuator::HorNiceSlider, window::Window,
};

// Every in-file version reference derives from these two constants (bump together).
const VERSION: &str = "1.1.0";
const FILE_TAG: &str = "screenreel_v1.1.0";
const APP_NAME: &str = "ScreenReel";
const AR: u32 = 48_000; // canonical project audio rate
const FPS_CHOICES: [u32; 8] = [10, 15, 20, 24, 25, 30, 50, 60]; // all divide 48000 exactly
const UNDO_MAX: usize = 64;
const MAX_JPEG: u32 = 64 << 20; // sanity bound per frame when loading projects
const MAX_AUDIO_FRAMES: usize = AR as usize * 60 * 60 * 4; // 4 h import cap (memory guard)

type Res<T> = Result<T, String>;
fn es<E: std::fmt::Display>(e: E) -> String { e.to_string() }

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Rect { x: i32, y: i32, w: i32, h: i32 }

// =====================================================================================
// Screen capture backends
// =====================================================================================

/// Alpha-blend a premultiplied ARGB cursor image onto a BGRA frame.
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
    let bytes = data.len() as u64 * 2;
    if bytes > (u32::MAX - 44) as u64 { return Err("WAV would exceed 4 GiB".into()); }
    let tmp = path.with_extension("wav.tmp");
    {
        let mut w = BufWriter::with_capacity(1 << 20, File::create(&tmp).map_err(es)?);
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
        let mut buf = Vec::with_capacity(64 * 1024);
        for chunk in data.chunks(32 * 1024) {
            buf.clear();
            for s in chunk { buf.extend_from_slice(&s.to_le_bytes()); }
            w.write_all(&buf).map_err(es)?;
        }
        w.flush().map_err(es)?;
    }
    std::fs::rename(&tmp, path).map_err(es)
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
    let track = fmt.tracks().iter().find(|t| t.codec_params.codec != CODEC_TYPE_NULL && t.codec_params.channels.is_some()
        || t.codec_params.sample_rate.is_some()).ok_or("no audio track found")?;
    let tid = track.id;
    let mut rate = track.codec_params.sample_rate.unwrap_or(0);
    // Some demuxers (e.g. MKV with PCM) omit max_frames_per_packet, which the PCM decoder requires.
    let mut params = track.codec_params.clone();
    if params.max_frames_per_packet.is_none() { params.with_max_frames_per_packet(1 << 18); }
    let mut dec = symphonia::default::get_codecs().make(&params, &DecoderOptions::default()).map_err(es)?;
    let mut out_ch = 0usize;
    let mut out: Vec<i16> = Vec::new();
    let mut sb: Option<SampleBuffer<i16>> = None;
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
                if sb.as_ref().map(|s| s.capacity() < buf.capacity() * c).unwrap_or(true) {
                    sb = Some(SampleBuffer::<i16>::new(buf.capacity() as u64, spec));
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
            loop {
                let Ok(d) = pa::read_descriptor(&mut sock) else { break };
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

enum CapInner { Cpal(cpal::Stream), #[cfg(target_os = "linux")] Pulse(pulse_rec::Reader) }

/// Audio capture (mic or system loopback): interleaved i16, chunks shipped over a channel.
/// Mic is mono; system audio is stereo. Linux uses a native Pulse/PipeWire stream when a
/// sound server is running (falls back to cpal/ALSA for the mic); Windows uses WASAPI.
struct AudioCapture { inner: CapInner, rx: Receiver<Vec<i16>>, rate: u32, ch: u16, end: Arc<Mutex<f64>> }

impl AudioCapture {
    fn start(src: AudioSrc, clock: Arc<PauseClock>) -> Res<Self> {
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
    fn start_pulse(src: AudioSrc, clock: Arc<PauseClock>) -> Res<Self> {
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
        Ok(AudioCapture { inner: CapInner::Pulse(reader), rx, rate: AR, ch: ch as u16, end })
    }

    fn start_cpal(src: AudioSrc, clock: Arc<PauseClock>) -> Res<Self> {
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
        Ok(AudioCapture { inner: CapInner::Cpal(stream), rx, rate: cfg.sample_rate, ch: out_ch, end })
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
        }, move |e| eprintln!("[{what}] stream error: {e}"), None).map_err(|e| format!("{what} open: {e}"))
    }

    /// Stop and return (channels, interleaved samples at 48 kHz). With `stop_at` (active-clock
    /// time the user pressed stop) the source gets a moment to deliver its last fragment and
    /// anything captured after that instant is trimmed off.
    fn finish(self, stop_at: Option<f64>) -> (u16, Vec<i16>) {
        let AudioCapture { inner, rx, rate, ch, end } = self;
        if stop_at.is_some() { thread::sleep(Duration::from_millis(80)); }
        match inner {
            CapInner::Cpal(s) => drop(s), // stops callbacks, drops the Sender
            #[cfg(target_os = "linux")]
            CapInner::Pulse(r) => r.stop(),
        }
        let mut all = Vec::new();
        for c in rx.try_iter() { all.extend_from_slice(&c); }
        let mut out = resample(&all, ch as usize, rate, AR);
        if let Some(s) = stop_at {
            let over = ((*end.lock().unwrap_or_else(|e| e.into_inner()) - s).max(0.0) * AR as f64) as usize;
            let keep = (out.len() / ch as usize).saturating_sub(over);
            out.truncate(keep * ch as usize);
        }
        (ch, out)
    }
}

// =====================================================================================
// Recorder: capture thread -> encoder pool -> ordered writer
// =====================================================================================

struct RecConfig {
    region: Rect, fps: u32, quality: u8, s444: bool, half: bool, cursor: bool,
    mic: Option<Option<String>>, // None = off; Some(None) = default device
    sys: bool,                   // also record system audio (loopback) to its own track
    dir: PathBuf,
}

#[derive(Default)]
struct RecShared { stop: AtomicBool, paused: Arc<PauseClock>, written: AtomicU64, dups: AtomicU64, error: Mutex<Option<String>> }

enum WMsg { Frame(u64, Vec<u8>), Dup(u64), Done(u64) }

struct Recorder {
    pub sh: Arc<RecShared>, cap: Option<JoinHandle<()>>, workers: Vec<JoinHandle<()>>,
    writer: Option<JoinHandle<Res<u64>>>, mic: Option<AudioCapture>, sys: Option<AudioCapture>,
    pub fps: u32, pub dims: (u32, u32), pub dir: PathBuf, pub mic_error: Option<String>, pub sys_error: Option<String>, pub shm: bool,
}

impl Recorder {
    fn start(cfg: RecConfig) -> Res<Recorder> {
        std::fs::create_dir_all(&cfg.dir).map_err(|e| format!("create {}: {e}", cfg.dir.display()))?;
        let sh = Arc::new(RecShared::default());
        let nworkers = thread::available_parallelism().map(|n| n.get()).unwrap_or(2).saturating_sub(1).clamp(1, 8);
        let (enc_tx, enc_rx) = mpsc::sync_channel::<(u64, Vec<u8>)>(nworkers * 2);
        let (w_tx, w_rx) = mpsc::sync_channel::<WMsg>(64);
        let (pool_tx, pool_rx) = mpsc::channel::<Vec<u8>>();
        let (ready_tx, ready_rx) = mpsc::channel::<Res<(Rect, bool)>>();
        let (go_tx, go_rx) = mpsc::channel::<bool>();

        // ---- capture thread (owns the Grabber; platform handles never cross threads)
        let (fps, region, cursor) = (cfg.fps, cfg.region, cfg.cursor);
        let sh_c = sh.clone();
        let w_tx_c = w_tx.clone();
        let cap = thread::Builder::new().name("capture".into()).spawn(move || {
            let mut g = match cap::Grabber::new(region, cursor) {
                Ok(g) => { let _ = ready_tx.send(Ok((g.r, g.uses_shm()))); g }
                Err(e) => { let _ = ready_tx.send(Err(e)); return; }
            };
            if !go_rx.recv().unwrap_or(false) { return; }
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
                    if w_tx_c.send(WMsg::Dup(seq)).is_err() { return; }
                    sh_c.dups.fetch_add(1, Relaxed);
                    seq += 1;
                }
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
                            *sh_c.error.lock().unwrap() = Some(format!("capture failing: {e}"));
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
                    let msg = { let g = rx.lock().unwrap(); g.recv() };
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
                        Err(e) => { *sh_w.error.lock().unwrap() = Some(format!("jpeg encode: {e}")); WMsg::Dup(seq) }
                    };
                    if wtx.send(m).is_err() { break; }
                }
            }).map_err(es)?);
        }
        drop(pool_tx);

        // ---- writer (re-orders, appends JPEG bytes, writes 12-byte index entries)
        let dir = cfg.dir.clone();
        let sh_wr = sh.clone();
        drop(w_tx);
        let writer = thread::Builder::new().name("writer".into()).spawn(move || -> Res<u64> {
            let r = (|| -> Res<u64> {
                let mut data = BufWriter::with_capacity(4 << 20, File::create(dir.join("frames.mjpg")).map_err(es)?);
                let mut idx = BufWriter::with_capacity(64 << 10, File::create(dir.join("frames.idx")).map_err(es)?);
                let mut pending: BTreeMap<u64, Option<Vec<u8>>> = BTreeMap::new();
                let (mut next, mut off, mut leading) = (0u64, 0u64, 0u64);
                let mut last: Option<[u8; 12]> = None;
                let mut total: Option<u64> = None;
                let put = |e: &[u8; 12], idx: &mut BufWriter<File>| idx.write_all(e).map_err(es);
                loop {
                    let m = w_rx.recv();
                    match m {
                        Ok(WMsg::Frame(s, v)) => { pending.insert(s, Some(v)); }
                        Ok(WMsg::Dup(s)) => { pending.insert(s, None); }
                        Ok(WMsg::Done(n)) => total = Some(n),
                        Err(_) => break,
                    }
                    while let Some(ent) = pending.remove(&next) {
                        match ent {
                            Some(v) => {
                                data.write_all(&v).map_err(es)?;
                                let mut e = [0u8; 12];
                                e[..8].copy_from_slice(&off.to_le_bytes());
                                e[8..].copy_from_slice(&(v.len() as u32).to_le_bytes());
                                off += v.len() as u64;
                                for _ in 0..=leading { put(&e, &mut idx)?; }
                                leading = 0;
                                last = Some(e);
                            }
                            None => match &last { Some(e) => put(e, &mut idx)?, None => leading += 1 },
                        }
                        next += 1;
                        sh_wr.written.store(next, Relaxed);
                    }
                    if let Some(n) = total { if next >= n && pending.is_empty() { break; } }
                }
                // Channel closed with gaps (should not happen): fill as duplicates.
                let end = total.unwrap_or(next).max(pending.keys().next_back().map(|k| k + 1).unwrap_or(0));
                while next < end {
                    match pending.remove(&next) {
                        Some(Some(v)) => {
                            data.write_all(&v).map_err(es)?;
                            let mut e = [0u8; 12];
                            e[..8].copy_from_slice(&off.to_le_bytes());
                            e[8..].copy_from_slice(&(v.len() as u32).to_le_bytes());
                            off += v.len() as u64;
                            for _ in 0..=leading { put(&e, &mut idx)?; }
                            leading = 0; last = Some(e);
                        }
                        _ => match &last { Some(e) => put(e, &mut idx)?, None => leading += 1 },
                    }
                    next += 1;
                }
                if last.is_none() { return Err("no frames were captured".into()); }
                let written = next - leading; // leading dups without a successor are dropped
                data.flush().map_err(es)?;
                idx.flush().map_err(es)?;
                Ok(written)
            })();
            if let Err(e) = &r { *sh_wr.error.lock().unwrap() = Some(e.clone()); sh_wr.stop.store(true, Relaxed); }
            r
        }).map_err(es)?;

        // ---- audio (main thread: cpal streams may be !Send). Failures degrade to video-only
        // for that source and are reported, never abort the recording.
        let open = |want: bool, src: AudioSrc| -> (Option<AudioCapture>, Option<String>) {
            if !want { return (None, None); }
            match AudioCapture::start(src, sh.paused.clone()) { Ok(c) => (Some(c), None), Err(e) => (None, Some(e)) }
        };
        let (mic, mic_error) = open(cfg.mic.is_some(), AudioSrc::Mic(cfg.mic.as_ref().and_then(|m| m.as_deref())));
        let (sys, sys_error) = open(cfg.sys, AudioSrc::System);
        let _ = go_tx.send(true);
        Ok(Recorder { sh, cap: Some(cap), workers, writer: Some(writer), mic, sys, fps: cfg.fps, dims, dir: cfg.dir,
                      mic_error, sys_error, shm })
    }

    /// Stop everything, join threads, return (frame count, mic, system audio); audio is
    /// (channels, 48 kHz interleaved samples) fitted exactly to the video length.
    fn finish(mut self) -> Res<(u32, Option<(u16, Vec<i16>)>, Option<(u16, Vec<i16>)>)> {
        let stop_at = self.sh.paused.active(Instant::now());
        self.sh.stop.store(true, Relaxed);
        if let Some(c) = self.cap.take() { let _ = c.join(); }
        for w in self.workers.drain(..) { let _ = w.join(); }
        let frames = self.writer.take().map(|w| w.join().unwrap_or_else(|_| Err("writer panicked".into())))
            .unwrap_or(Err("no writer".into()))?;
        let frames = u32::try_from(frames).map_err(|_| "recording too long")?;
        let spf = (AR / self.fps) as usize;
        let target = frames as usize * spf;
        let fit = |c: AudioCapture| { let (ch, v) = c.finish(Some(stop_at)); (ch, fit_len(v, ch as usize, target)) };
        let mic = self.mic.take().map(fit);
        let sys = self.sys.take().map(fit);
        Ok((frames, mic, sys))
    }
}

// =====================================================================================
// Project model: source frames + edit decision list + audio tracks
// =====================================================================================

struct Source { dir: PathBuf, file: File, idx: Vec<(u64, u32)>, w: u32, h: u32, fps: u32 }

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
    fn open(dir: &Path, w: u32, h: u32, fps: u32) -> Res<Source> {
        let file = File::open(dir.join("frames.mjpg")).map_err(|e| format!("frames.mjpg: {e}"))?;
        let flen = file.metadata().map_err(es)?.len();
        let mut raw = Vec::new();
        File::open(dir.join("frames.idx")).map_err(|e| format!("frames.idx: {e}"))?.read_to_end(&mut raw).map_err(es)?;
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
        Ok(Source { dir: dir.to_path_buf(), file, idx, w, h, fps })
    }
    fn count(&self) -> u32 { self.idx.len() as u32 }
    fn read(&self, i: u32) -> Res<Vec<u8>> {
        let (o, l) = *self.idx.get(i as usize).ok_or("frame index out of range")?;
        let mut b = vec![0u8; l as usize];
        read_at(&self.file, &mut b, o).map_err(es)?;
        Ok(b)
    }
    fn spf(&self) -> usize { (AR / self.fps) as usize }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Seg { Src { start: u32, len: u32 }, Hold { frame: u32, len: u32 } }

impl Seg {
    fn len(&self) -> u32 { match *self { Seg::Src { len, .. } | Seg::Hold { len, .. } => len } }
    fn src_at(&self, k: u32) -> u32 { match *self { Seg::Src { start, .. } => start + k, Seg::Hold { frame, .. } => frame } }
    fn sub(&self, a: u32, b: u32) -> Seg {
        match *self { Seg::Src { start, .. } => Seg::Src { start: start + a, len: b - a }, Seg::Hold { frame, .. } => Seg::Hold { frame, len: b - a } }
    }
}

#[derive(Clone)]
struct Track { name: String, ch: u16, data: Arc<Vec<i16>>, gain: f32, muted: bool }

impl Track {
    fn frames(&self) -> usize { self.data.len() / self.ch as usize }
}

#[derive(Clone)]
struct Edit { segs: Vec<Seg>, tracks: Vec<Track> }

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
                    _ => {}
                }
            }
            out.push(s);
        }
        self.segs = out;
    }

    /// Ripple-delete output frames [a,b) from video and every audio track.
    fn cut(&mut self, a: u32, b: u32, spf: usize) {
        if b <= a { return; }
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

struct Project { src: Arc<Source>, edit: Edit, undo: Vec<Edit>, redo: Vec<Edit>, dirty: bool }

fn safe_file_name(s: &str) -> bool {
    !s.is_empty() && s.len() < 128 && !s.starts_with('.')
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.')
}

fn sanitize_name(s: &str) -> String {
    let t: String = s.chars().filter(|c| !c.is_control() && *c != '|').take(48).collect();
    if t.trim().is_empty() { "Track".into() } else { t }
}

impl Project {
    fn save(&mut self) -> Res<()> {
        let dir = self.src.dir.clone();
        let mut txt = format!("screenreel_project=1\nversion={VERSION}\nwidth={}\nheight={}\nfps={}\n", self.src.w, self.src.h, self.src.fps);
        for s in &self.edit.segs {
            match s { Seg::Src { start, len } => txt += &format!("seg=S {start} {len}\n"), Seg::Hold { frame, len } => txt += &format!("seg=H {frame} {len}\n") }
        }
        let mut keep = Vec::new();
        for (i, t) in self.edit.tracks.iter().enumerate() {
            let f = format!("track_{i}.wav");
            write_wav(&dir.join(&f), t.ch, AR, &t.data)?;
            txt += &format!("track={}|{}|{}|{}\n", sanitize_name(&t.name), t.gain, t.muted as u8, f);
            keep.push(f);
        }
        // Remove stale track files from earlier saves.
        if let Ok(rd) = std::fs::read_dir(&dir) {
            for e in rd.flatten() {
                let n = e.file_name().to_string_lossy().to_string();
                if n.starts_with("track_") && n.ends_with(".wav") && !keep.contains(&n) { let _ = std::fs::remove_file(e.path()); }
            }
        }
        let tmp = dir.join("project.txt.tmp");
        std::fs::write(&tmp, txt).map_err(es)?;
        std::fs::rename(&tmp, dir.join("project.txt")).map_err(es)?;
        self.dirty = false;
        Ok(())
    }

    fn load(path: &Path) -> Res<Project> {
        let dir = if path.is_dir() { path.to_path_buf() } else { path.parent().ok_or("bad path")?.to_path_buf() };
        let txt = std::fs::read_to_string(dir.join("project.txt")).map_err(|e| format!("project.txt: {e}"))?;
        if txt.len() > 16 << 20 { return Err("project.txt too large".into()); }
        let (mut w, mut h, mut fps, mut magic) = (0u32, 0u32, 0u32, false);
        let mut segs = Vec::new();
        let mut tdefs = Vec::new();
        for line in txt.lines() {
            let Some((k, v)) = line.split_once('=') else { continue };
            match k {
                "screenreel_project" => magic = v == "1",
                "width" => w = v.parse().map_err(es)?,
                "height" => h = v.parse().map_err(es)?,
                "fps" => fps = v.parse().map_err(es)?,
                "seg" => {
                    let p: Vec<&str> = v.split_whitespace().collect();
                    if p.len() != 3 { return Err(format!("bad seg line: {line}")); }
                    let (a, b): (u32, u32) = (p[1].parse().map_err(es)?, p[2].parse().map_err(es)?);
                    segs.push(match p[0] { "S" => Seg::Src { start: a, len: b }, "H" => Seg::Hold { frame: a, len: b }, _ => return Err("bad seg kind".into()) });
                }
                "track" => {
                    let p: Vec<&str> = v.split('|').collect();
                    if p.len() != 4 || !safe_file_name(p[3]) { return Err(format!("bad track line: {line}")); }
                    let gain: f32 = p[1].parse().map_err(es)?;
                    tdefs.push((p[0].to_string(), gain.clamp(0.0, 4.0), p[2] == "1", p[3].to_string()));
                }
                _ => {}
            }
        }
        if !magic { return Err("not a ScreenReel project".into()); }
        if !FPS_CHOICES.contains(&fps) || w == 0 || h == 0 || w > 16384 || h > 16384 { return Err("invalid project header".into()); }
        let src = Arc::new(Source::open(&dir, w, h, fps)?);
        let n = src.count();
        for s in &segs {
            let ok = match *s { Seg::Src { start, len } => start.checked_add(len).map(|e| e <= n).unwrap_or(false), Seg::Hold { frame, len } => frame < n && len > 0 };
            if !ok { return Err("segment references frames outside the recording".into()); }
        }
        let mut tracks = Vec::new();
        for (name, gain, muted, f) in tdefs {
            let (ch, data) = decode_audio(&dir.join(&f))?;
            tracks.push(Track { name: sanitize_name(&name), ch, data: Arc::new(data), gain, muted });
        }
        let mut edit = Edit { segs, tracks };
        edit.normalize();
        Ok(Project { src, edit, undo: vec![], redo: vec![], dirty: false })
    }
}

// =====================================================================================
// Mixing + playback engine
// =====================================================================================

#[derive(Clone)]
struct MixTrack { data: Arc<Vec<i16>>, ch: u16, gain: f32 }

fn mix_tracks(edit: &Edit, exclude: Option<usize>) -> Vec<MixTrack> {
    edit.tracks.iter().enumerate()
        .filter(|(i, t)| !t.muted && Some(*i) != exclude && t.gain > 0.0)
        .map(|(_, t)| MixTrack { data: t.data.clone(), ch: t.ch, gain: t.gain }).collect()
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
        out.extend_from_slice(&f2i(l).to_le_bytes());
        out.extend_from_slice(&f2i(r).to_le_bytes());
    }
}

const NO_SEEK: u64 = u64::MAX;

struct PlayShared { playing: AtomicBool, pos: AtomicU64, seek: AtomicU64, end: AtomicU64, mix: Mutex<Vec<MixTrack>> }

impl PlayShared {
    fn new() -> Self { PlayShared { playing: AtomicBool::new(false), pos: AtomicU64::new(0), seek: AtomicU64::new(NO_SEEK), end: AtomicU64::new(0), mix: Mutex::new(vec![]) } }
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
        for fr in data.chunks_mut(ch) {
            if pos >= end { fr.iter_mut().for_each(|x| *x = T::EQUILIBRIUM); ps.playing.store(false, Relaxed); continue; }
            let (l0, r0) = mix_frame(&ts, pos as usize);
            let (l, r) = if frac > 0.0 && step != 1.0 {
                let (l1, r1) = mix_frame(&ts, pos as usize + 1);
                let f = frac as f32;
                (l0 + (l1 - l0) * f, r0 + (r1 - r0) * f)
            } else { (l0, r0) };
            let (l, r) = (l.clamp(-1.0, 1.0), r.clamp(-1.0, 1.0));
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
        ps.pos.store(heard.max(start.min(pos)), Relaxed);
    }, |e| eprintln!("[out] stream error: {e}"), None).map_err(es)
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

struct ExportJob { progress: Arc<AtomicU32>, cancel: Arc<AtomicBool>, handle: JoinHandle<Res<String>>, label: String }

fn export_mkv(path: &Path, src: &Source, edit: &Edit, progress: &AtomicU32, cancel: &AtomicBool) -> Res<()> {
    use mkv::*;
    let total = edit.total();
    if total == 0 { return Err("timeline is empty".into()); }
    let fps = src.fps;
    let spf = src.spf();
    let ts = mix_tracks(edit, None);
    let has_audio = !edit.tracks.is_empty();
    let tmp = path.with_extension("mkv.part");
    let mut w = BufWriter::with_capacity(8 << 20, File::create(&tmp).map_err(|e| format!("{}: {e}", tmp.display()))?);
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
    let mut abuf = Vec::with_capacity(spf * 4);
    let mut f = 0u32;
    while f < total {
        if cancel.load(Relaxed) { drop(w); let _ = std::fs::remove_file(&tmp); return Err("export cancelled".into()); }
        let c_ms = f as u64 * 1000 / fps as u64;
        let c_end = (f + fps).min(total);
        let mut body = uint(0xE7, c_ms);
        for fr in f..c_end {
            let sf = edit.src_frame(fr).ok_or("timeline corrupt")?;
            if cache.as_ref().map(|c| c.0 != sf).unwrap_or(true) { cache = Some((sf, src.read(sf)?)); }
            let jpg = &cache.as_ref().unwrap().1;
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
        progress.store((f as u64 * 1000 / total as u64) as u32, Relaxed);
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
    std::fs::rename(&tmp, path).map_err(es)?;
    Ok(())
}

fn export_wav(path: &Path, src: &Source, edit: &Edit) -> Res<()> {
    let n = edit.total() as usize * src.spf();
    let ts = mix_tracks(edit, None);
    let mut out = Vec::with_capacity(n * 2);
    for i in 0..n { let (l, r) = mix_frame(&ts, i); out.push(f2i(l)); out.push(f2i(r)); }
    write_wav(path, 2, AR, &out)
}

fn ffmpeg_available() -> bool {
    std::process::Command::new("ffmpeg").arg("-version").stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null()).status().map(|s| s.success()).unwrap_or(false)
}

/// Run ffmpeg on an exported MKV. Args are passed as argv (no shell → no injection).
fn ffmpeg_convert(input: &Path, output: &Path, lossless_remux: bool) -> Res<()> {
    let mut c = std::process::Command::new("ffmpeg");
    c.args(["-hide_banner", "-loglevel", "error", "-y", "-i"]).arg(input);
    if lossless_remux {
        c.args(["-c", "copy", "-f", "mov"]);
    } else {
        c.args(["-c:v", "libx264", "-preset", "veryfast", "-crf", "18", "-pix_fmt", "yuv420p",
            "-vf", "scale=trunc(iw/2)*2:trunc(ih/2)*2", "-c:a", "aac", "-b:a", "192k", "-movflags", "+faststart", "-f", "mp4"]);
    }
    c.arg(output).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null());
    let out = c.output().map_err(|e| format!("ffmpeg: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(format!("ffmpeg failed: {}", err.lines().last().unwrap_or("unknown error")));
    }
    Ok(())
}

// =====================================================================================
// Background JPEG decoder for the preview
// =====================================================================================

struct DecReq { src: Arc<Source>, frame: u32, tw: i32, th: i32 }
struct DecRes { frame: u32, tw: i32, th: i32, w: i32, h: i32, rgb: Vec<u8> }

struct Decoder { req: Arc<(Mutex<Option<DecReq>>, Condvar)>, res: Arc<Mutex<Option<DecRes>>>, last: Option<(u32, i32, i32, usize)> }

impl Decoder {
    fn new() -> Self {
        let req: Arc<(Mutex<Option<DecReq>>, Condvar)> = Arc::new((Mutex::new(None), Condvar::new()));
        let res: Arc<Mutex<Option<DecRes>>> = Arc::new(Mutex::new(None));
        let (rq, rs) = (req.clone(), res.clone());
        thread::Builder::new().name("preview-dec".into()).spawn(move || loop {
            let r = {
                let (m, cv) = &*rq;
                let mut g = m.lock().unwrap();
                while g.is_none() { g = cv.wait(g).unwrap(); }
                g.take().unwrap()
            };
            let out = r.src.read(r.frame).and_then(|j| decode_jpeg_rgb(&j));
            if let Ok((rgb, w, h)) = out {
                let (fw, fh) = fit(w as i32, h as i32, r.tw, r.th);
                let (ow, oh) = if fw as usize >= w { (w, h) } else { (fw as usize, fh as usize) };
                let px = scale_rgb(&rgb, w, h, ow, oh);
                *rs.lock().unwrap() = Some(DecRes { frame: r.frame, tw: r.tw, th: r.th, w: ow as i32, h: oh as i32, rgb: px });
                app::awake();
            }
        }).expect("spawn decoder");
        Decoder { req, res, last: None }
    }
    fn request(&mut self, src: &Arc<Source>, frame: u32, tw: i32, th: i32) {
        let key = (frame, tw, th, Arc::as_ptr(src) as usize);
        if self.last == Some(key) || tw <= 0 || th <= 0 { return; }
        self.last = Some(key);
        let (m, cv) = &*self.req;
        *m.lock().unwrap() = Some(DecReq { src: src.clone(), frame, tw, th }); // latest wins
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
    export: Option<ExportJob>, mics: Vec<String>, status: String, ffmpeg: bool,
}

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
        }
    }

    /// Re-sync mixer + clamp cursor after any edit.
    fn after_edit(&mut self) {
        let total = self.total();
        self.playhead = self.playhead.min(total.saturating_sub(1));
        if let Some((a, b)) = self.sel { let b = b.min(total); self.sel = if a < b { Some((a, b)) } else { None }; }
        if let Some(t) = self.sel_track { if self.proj.as_ref().map(|p| t >= p.edit.tracks.len()).unwrap_or(true) { self.sel_track = None; } }
        self.sync_mix();
        let live: Vec<usize> = self.proj.as_ref().map(|p| p.edit.tracks.iter().map(|t| Arc::as_ptr(&t.data) as usize).collect()).unwrap_or_default();
        self.peaks.retain(|k, _| live.contains(k));
    }

    fn sync_mix(&mut self) {
        let excl = self.narr.as_ref().map(|n| n.track);
        let (ts, end) = match &self.proj { Some(p) => (mix_tracks(&p.edit, excl), p.edit.total() as u64 * p.src.spf() as u64), None => (vec![], 0) };
        *self.play.mix.lock().unwrap() = ts;
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

    fn start_narration(&mut self, mic: Option<String>) -> Res<()> {
        if self.proj.is_none() { return Err("record or open a project first".into()); }
        if self.narr.is_some() { return Ok(()); }
        { let (a, b) = self.sel.unwrap_or((self.playhead, self.total())); if b <= a + 1 { return Err("Move the playhead before the end, or select a range to re-record".into()); } }
        let ti = match self.sel_track {
            Some(t) => t,
            None => {
                let n = self.proj.as_ref().unwrap().edit.tracks.len() + 1;
                self.add_track(format!("Narration {n}"), 1, vec![], 0);
                self.sel_track.unwrap()
            }
        };
        let (a, b) = self.sel.unwrap_or((self.playhead, self.total()));
        if b <= a + 1 { return Err("Move the playhead before the end, or select a range to re-record".into()); }
        let m = AudioCapture::start(AudioSrc::Mic(mic.as_deref()), Arc::new(PauseClock::default()))?;
        self.push_undo();
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
}

type Shared = std::rc::Rc<std::cell::RefCell<App>>;

const RULER_H: i32 = 18;
const VLANE_H: i32 = 28;

fn fmt_time(frames: u32, fps: u32) -> String {
    let cs = (frames as u64 * 100 + fps.max(1) as u64 / 2) / fps.max(1) as u64; // rounded centiseconds
    format!("{:02}:{:02}.{:02}", cs / 6000, cs / 100 % 60, cs % 100)
}

fn lane_h(app: &App, h: i32) -> i32 {
    let n = app.proj.as_ref().map(|p| p.edit.tracks.len()).unwrap_or(0) as i32;
    if n == 0 { 0 } else { ((h - RULER_H - VLANE_H) / n).clamp(14, 48) }
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
        draw::draw_text(&fmt_time((t * fps as f64).round() as u32, fps)[..if step >= 1.0 { 5 } else { 8 }], x + 3, y0 + 11);
    }
    // Selection band
    if let Some((a, b)) = app.sel {
        let (xa, xb) = (fx(a as f64), fx(b as f64));
        draw::draw_rect_fill(xa, y0 + RULER_H, (xb - xa).max(1), h - RULER_H, Color::from_rgb(52, 70, 96));
        draw::draw_rect_fill(xa, y0 + RULER_H - 3, (xb - xa).max(1), 3, Color::from_rgb(120, 170, 255));
    }
    // Video lane
    let vy = y0 + RULER_H;
    let mut acc = 0u32;
    draw::set_font(Font::HelveticaBold, 10);
    for s in &p.edit.segs {
        let (xa, xb) = (fx(acc as f64), fx((acc + s.len()) as f64));
        let (col, txt) = match s { Seg::Src { .. } => (Color::from_rgb(58, 104, 160), ""), Seg::Hold { .. } => (Color::from_rgb(196, 132, 40), "FREEZE") };
        draw::draw_rect_fill(xa, vy + 2, (xb - xa).max(1), VLANE_H - 4, col);
        draw::set_draw_color(Color::from_rgb(20, 20, 20));
        draw::draw_yxline(xa, vy + 2, vy + VLANE_H - 2);
        if !txt.is_empty() && xb - xa > 46 { draw::set_draw_color(Color::White); draw::draw_text2(txt, xa, vy, xb - xa, VLANE_H, Align::Center); }
        acc += s.len();
    }
    // Audio lanes
    let lh = lane_h(app, h);
    let spf = p.src.spf();
    let tracks: Vec<Track> = p.edit.tracks.clone();
    let (scroll, zoom, sel_track) = (app.scroll, app.zoom, app.sel_track);
    for (i, t) in tracks.iter().enumerate() {
        let ly = vy + VLANE_H + i as i32 * lh;
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

fn refresh(app: &Shared, ui: &Ui) {
    let Ok(mut a) = app.try_borrow_mut() else { return };
    let mut ui = ui.clone();
    let has = a.proj.is_some();
    let total = a.total();
    let fps = a.fps();
    let busy = a.export.is_some() || a.narr.is_some();
    for b in [&mut ui.cut, &mut ui.freeze_sel] { if has && a.sel.is_some() && !busy { b.activate() } else { b.deactivate() } }
    for b in [&mut ui.save, &mut ui.ifreeze, &mut ui.play, &mut ui.newt, &mut ui.import, &mut ui.fitz] { if has && !busy { b.activate() } else { b.deactivate() } }
    for b in [&mut ui.exp_mkv, &mut ui.exp_wav] { if has && total > 0 && !busy { b.activate() } else { b.deactivate() } }
    for b in [&mut ui.exp_mov, &mut ui.exp_mp4] { if has && total > 0 && !busy && a.ffmpeg { b.activate() } else { b.deactivate() } }
    let (cu, cr) = a.proj.as_ref().map(|p| (!p.undo.is_empty(), !p.redo.is_empty())).unwrap_or((false, false));
    if cu && !busy { ui.undo.activate() } else { ui.undo.deactivate() }
    if cr && !busy { ui.redo.activate() } else { ui.redo.deactivate() }
    if has && a.export.is_none() { ui.narr.activate() } else { ui.narr.deactivate() }
    ui.narr.set_label(if a.narr.is_some() { "■ Stop narration" } else { "● Re-record narration" });
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
    if let Some((src, sf)) = req { a.dec.request(&src, sf, tw, th); } else if a.proj.is_some() && a.preview.is_some() && a.total() == 0 { a.preview = None; }
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
    };
    {
        let mut a = app.borrow_mut();
        if a.proj.as_ref().map(|p| p.dirty).unwrap_or(false) {
            if let Some(p) = &mut a.proj { if let Err(e) = p.save() { a.status = format!("autosave failed: {e}"); } }
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
                if !w.is_empty() { s.status = format!("{} - recording without it", w.join("; ")); }
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
    let err = rec.sh.error.lock().unwrap().clone()
        .or_else(|| rec.mic_error.clone().map(|e| format!("mic unavailable: {e}")))
        .or_else(|| rec.sys_error.clone().map(|e| format!("system audio unavailable: {e}")));
    let dropped = rec.sh.dups.load(Relaxed);
    let res = rec.finish();
    abort_ctl(app, ui);
    match res.and_then(|(frames, mic, sys)| {
        let src = Arc::new(Source::open(&dir, dims.0, dims.1, fps)?);
        let mut tracks = Vec::new();
        // Separate tracks: mic and system audio can be muted/replaced/re-recorded independently.
        if let Some((ch, m)) = mic { tracks.push(Track { name: "Mic".into(), ch, data: Arc::new(m), gain: 1.0, muted: false }); }
        if let Some((ch, v)) = sys { tracks.push(Track { name: "System".into(), ch, data: Arc::new(v), gain: 1.0, muted: false }); }
        let mut p = Project { edit: Edit { segs: vec![Seg::Src { start: 0, len: src.count() }], tracks }, src, undo: vec![], redo: vec![], dirty: true };
        p.save()?;
        Ok((p, frames))
    }) {
        Ok((p, frames)) => {
            let mut a = app.borrow_mut();
            let secs = frames as f64 / fps as f64;
            a.status = format!("Recorded {secs:.1}s ({}×{} @ {fps} fps, {dropped} dropped/duplicated) → {}{}", dims.0, dims.1, dir.display(),
                err.map(|e| format!("  [warning: {e}]")).unwrap_or_default());
            a.proj = Some(p);
            a.sel = None;
            a.sel_track = None;
            a.playhead = 0;
            a.scroll = 0.0;
            a.zoom = (ui.timeline.w() as f64 / frames.max(1) as f64).max(1e-4);
            a.after_edit();
            a.seek(0);
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

fn with_ext(p: PathBuf, ext: &str) -> PathBuf {
    if p.extension().map(|e| e.eq_ignore_ascii_case(ext)).unwrap_or(false) { p } else { let mut s = p.into_os_string(); s.push("."); s.push(ext); PathBuf::from(s) }
}

fn start_export(app: &Shared, ui: &Ui, kind: &'static str) {
    let (src, edit, base) = {
        let a = app.borrow();
        let Some(p) = &a.proj else { return };
        if a.export.is_some() { return; }
        (p.src.clone(), p.edit.clone(), p.src.dir.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or("export".into()))
    };
    app.borrow_mut().set_playing(false);
    let (filter, ext) = match kind { "mkv" => ("Matroska\t*.mkv", "mkv"), "mov" => ("QuickTime MOV\t*.mov", "mov"), "mp4" => ("MP4\t*.mp4", "mp4"), _ => ("WAV audio\t*.wav", "wav") };
    let Some(path) = choose_file("Export", filter, Some(&format!("{base}.{ext}"))) else { return };
    let path = with_ext(path, ext);
    let progress = Arc::new(AtomicU32::new(0));
    let cancel = Arc::new(AtomicBool::new(false));
    let (pg, cc) = (progress.clone(), cancel.clone());
    let handle = thread::spawn(move || -> Res<String> {
        let t0 = Instant::now();
        match kind {
            "mkv" => export_mkv(&path, &src, &edit, &pg, &cc)?,
            "wav" => export_wav(&path, &src, &edit)?,
            _ => {
                let tmp = path.with_extension("srtmp.mkv");
                export_mkv(&tmp, &src, &edit, &pg, &cc)?;
                let r = ffmpeg_convert(&tmp, &path, kind == "mov");
                let _ = std::fs::remove_file(&tmp);
                r?;
            }
        }
        Ok(format!("Exported {} in {:.2}s", path.display(), t0.elapsed().as_secs_f64()))
    });
    let mut a = app.borrow_mut();
    a.export = Some(ExportJob { progress, cancel, handle, label: kind.to_uppercase() });
    a.status = format!("Exporting {}…", kind.to_uppercase());
    drop(a);
    refresh(app, ui);
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
    // Export completion
    if let Some((pct, label, done)) = a.export.as_ref().map(|j| (j.progress.load(Relaxed) / 10, j.label.clone(), j.handle.is_finished())) {
        let l = format!("Exporting {label}… {pct}%");
        if a.status != l && !done { a.status = l; need = true; }
        if done {
            let j = a.export.take().unwrap();
            let r = j.handle.join().unwrap_or_else(|_| Err("export thread panicked".into()));
            a.status = match r { Ok(s) => s, Err(e) => format!("Export failed: {e}") };
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
    request_preview(&mut a, ui);
    // Decoded preview frame
    let res = a.dec.res.lock().unwrap().take();
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

fn build_ui(app: &Shared) -> Ui {
    let mut win = Window::default().with_size(1180, 780).with_label(&format!("{APP_NAME} v{VERSION}"));
    win.size_range(820, 560, 0, 0);
    let mut col = Flex::default_fill().column();
    col.set_margin(6);
    col.set_pad(4);

    let mut r1 = Flex::default().row();
    r1.set_pad(4);
    let rec_full = Button::default().with_label("● Record Screen");
    let rec_region = Button::default().with_label("● Record Region");
    let mut fps = Choice::default();
    for f in FPS_CHOICES { fps.add_choice(&format!("{f} fps")); }
    fps.set_value(5);
    let mut quality = Choice::default();
    quality.add_choice("Draft (q60)|Standard (q80)|Sharp text (q92 4:4:4)");
    quality.set_value(1);
    let half = CheckButton::default().with_label("Half-res");
    let cursor = CheckButton::default().with_label("Cursor");
    cursor.set_checked(true);
    let mut mic = Choice::default();
    mic.add_choice("Mic: off");
    mic.add_choice("Mic: default");
    for n in &app.borrow().mics { mic.add_choice(&format!("Mic: {}", esc(&n.replace(['/', '|', '&', '_', '\\'], " ")))); }
    mic.set_value(1);
    let mut sys = CheckButton::default().with_label("System audio");
    sys.set_tooltip("Also record what the computer plays (videos, apps) to its own \"System\" track.\n\
Linux: needs PulseAudio or PipeWire. Use headphones to keep speaker sound out of the mic.");
    r1.fixed(&rec_full, 130); r1.fixed(&rec_region, 130); r1.fixed(&fps, 80); r1.fixed(&quality, 180); r1.fixed(&half, 80); r1.fixed(&cursor, 70); r1.fixed(&sys, 115);
    r1.end();
    col.fixed(&r1, 30);

    let mut r2 = Flex::default().row();
    r2.set_pad(4);
    let open = Button::default().with_label("Open…");
    let save = Button::default().with_label("Save");
    let exp_mkv = Button::default().with_label("Export MKV (instant)");
    let exp_mov = Button::default().with_label("Export MOV (lossless, ffmpeg)");
    let exp_mp4 = Button::default().with_label("Export MP4 (H.264, ffmpeg)");
    let exp_wav = Button::default().with_label("Export WAV mix");
    Frame::default();
    r2.fixed(&open, 90); r2.fixed(&save, 80); r2.fixed(&exp_mkv, 160); r2.fixed(&exp_mov, 210); r2.fixed(&exp_mp4, 200); r2.fixed(&exp_wav, 120);
    r2.end();
    col.fixed(&r2, 30);

    let mut preview = Frame::default();
    preview.set_frame(FrameType::FlatBox);
    preview.set_color(Color::Black);

    let mut r3 = Flex::default().row();
    r3.set_pad(4);
    let play = Button::default().with_label("▶ Play");
    let mut time = Frame::default().with_label("00:00.00 / 00:00.00");
    time.set_label_font(Font::Courier);
    let cut = Button::default().with_label("Cut");
    let ifreeze = Button::default().with_label("Insert freeze");
    let mut fsecs = Spinner::default();
    fsecs.set_range(0.1, 600.0); fsecs.set_step(0.5); fsecs.set_value(2.0);
    let freeze_sel = Button::default().with_label("Freeze selection");
    let undo = Button::default().with_label("Undo");
    let redo = Button::default().with_label("Redo");
    let narr = Button::default().with_label("● Re-record narration");
    let fitz = Button::default().with_label("⇔ Fit");
    r3.fixed(&play, 80); r3.fixed(&time, 170); r3.fixed(&cut, 70); r3.fixed(&ifreeze, 120); r3.fixed(&fsecs, 64);
    r3.fixed(&freeze_sel, 140); r3.fixed(&undo, 70); r3.fixed(&redo, 70); r3.fixed(&narr, 180);
    r3.end();
    col.fixed(&r3, 30);

    let mut r4 = Flex::default().row();
    r4.set_pad(4);
    let track = Choice::default().with_label("Track");
    let mute = Button::default().with_label("Mute");
    let mut gain = HorNiceSlider::default();
    gain.set_range(0.0, 2.0); gain.set_step(0.01, 1); gain.set_value(1.0); gain.set_tooltip("Track gain (0–200%)");
    let shl = Button::default().with_label("◀ 1f");
    let shr = Button::default().with_label("1f ▶");
    let newt = Button::default().with_label("+ Empty track");
    let import = Button::default().with_label("+ Add audio file…");
    let replace = Button::default().with_label("Swap audio…");
    let remove = Button::default().with_label("× Remove track");
    let mut sp = Frame::default(); sp.set_label("");
    r4.fixed(&track, 200); r4.fixed(&mute, 70); r4.fixed(&gain, 140); r4.fixed(&shl, 50); r4.fixed(&shr, 50);
    r4.fixed(&newt, 120); r4.fixed(&import, 150); r4.fixed(&replace, 120); r4.fixed(&remove, 130);
    r4.end();
    col.fixed(&r4, 30);

    let mut timeline = Frame::default();
    timeline.set_frame(FrameType::FlatBox);
    col.fixed(&timeline, 210);
    let mut status = Frame::default().with_align(Align::Left | Align::Inside);
    status.set_label_size(12);
    col.fixed(&status, 22);
    col.end();
    win.end();
    win.make_resizable(true);
    let ui = Ui { win, rec_full, rec_region, fps, quality, half, cursor, mic, sys, open, save, exp_mkv, exp_mov, exp_mp4, exp_wav,
        preview, play, time, cut, ifreeze, fsecs, freeze_sel, undo, redo, narr, fitz, track, mute, gain, shl, shr, newt, import,
        replace, remove, timeline, status };
    // Buttons should not steal Space/Enter (used as shortcuts).
    for mut b in [ui.rec_full.clone(), ui.rec_region.clone(), ui.open.clone(), ui.save.clone(), ui.exp_mkv.clone(), ui.exp_mov.clone(),
        ui.exp_mp4.clone(), ui.exp_wav.clone(), ui.play.clone(), ui.cut.clone(), ui.ifreeze.clone(), ui.freeze_sel.clone(), ui.undo.clone(),
        ui.redo.clone(), ui.narr.clone(), ui.fitz.clone(), ui.mute.clone(), ui.shl.clone(), ui.shr.clone(), ui.newt.clone(),
        ui.import.clone(), ui.replace.clone(), ui.remove.clone()] { b.clear_visible_focus(); }
    ui
}

/// Global editing shortcuts (called from the focused timeline and the main window).
fn handle_key(a: &Shared, u: &Ui) -> bool {
    let k = app::event_key();
    let ctrl = app::event_state().contains(Shortcut::Ctrl) || app::event_state().contains(Shortcut::Command);
    let Ok(mut s) = a.try_borrow_mut() else { return false };
    if s.proj.is_none() || s.export.is_some() { return false; }
    if s.narr.is_some() {
        // While re-recording narration only Space/Esc (stop) are honoured.
        if k == Key::from_char(' ') || k == Key::Escape { s.set_playing(false); drop(s); refresh(a, u); return true; }
        return false;
    }
    let r: Res<()> = match (ctrl, k) {
        (false, Key::Delete) | (false, Key::BackSpace) => s.op_cut(),
        (false, k) if k == Key::from_char(' ') => { let p = !s.play.playing.load(Relaxed); s.set_playing(p); Ok(()) }
        (false, k) if k == Key::from_char('f') => { let v = u.fsecs.value(); s.op_insert_freeze(v) }
        (true, k) if k == Key::from_char('z') => { s.op_undo(false); Ok(()) }
        (true, k) if k == Key::from_char('y') => { s.op_undo(true); Ok(()) }
        (true, k) if k == Key::from_char('s') => s.proj.as_mut().unwrap().save().map(|_| { s.status = "Project saved".into(); }),
        (false, Key::Home) => { s.seek(0); Ok(()) }
        (false, Key::End) => { let t = s.total(); s.seek(t); Ok(()) }
        (false, Key::Left) => { let f = s.playhead.saturating_sub(1); s.seek(f); Ok(()) }
        (false, Key::Right) => { let f = s.playhead + 1; s.seek(f); Ok(()) }
        (false, Key::Escape) => { s.sel = None; Ok(()) }
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
                img.draw(f.x() + (f.w() - dw) / 2, f.y() + (f.h() - dh) / 2, dw, dh);
            } else if s.proj.is_none() {
                draw::set_draw_color(Color::from_rgb(150, 150, 150));
                draw::set_font(Font::Helvetica, 16);
                draw::draw_text2(&format!("{APP_NAME} v{VERSION}\n\n● Record Screen / ● Record Region to begin\n\nSpace play/pause · Del cut · F insert freeze · Ctrl+Z/Y undo/redo · Ctrl+S save"),
                    f.x(), f.y(), f.w(), f.h(), Align::Center);
            }
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
            if s.proj.is_none() || s.export.is_some() { return false; }
            let (ex, ey) = (app::event_x(), app::event_y());
            match ev {
                Event::Push => {
                    drop(s);
                    let _ = f.take_focus();
                    let Ok(mut s) = a.try_borrow_mut() else { return false };
                    let fr = x_to_frame(&s, f, ex);
                    let lh = lane_h(&s, f.h());
                    let lane_y = f.y() + RULER_H + VLANE_H;
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
                        let minz = f.w() as f64 / s.total().max(1) as f64 * 0.5;
                        s.zoom = (s.zoom * k).clamp(minz, 64.0);
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
    cb!(ui.open, |a, u| {
        if let Some(p) = choose_file("Open ScreenReel project (project.txt)", "ScreenReel project\tproject.txt", None) {
            a.borrow_mut().set_playing(false);
            match Project::load(&p) {
                Ok(pr) => {
                    let mut s = a.borrow_mut();
                    let n = pr.edit.total();
                    s.status = format!("Opened {}", pr.src.dir.display());
                    s.proj = Some(pr);
                    s.sel = None; s.sel_track = None; s.scroll = 0.0;
                    s.zoom = (u.timeline.w() as f64 / n.max(1) as f64).max(1e-4);
                    s.after_edit();
                    s.seek(0);
                }
                Err(e) => alert(&format!("Open failed:\n{e}")),
            }
        }
    });
    cb!(ui.save, |a, u| {
        let mut s = a.borrow_mut();
        let r = s.proj.as_mut().map(|p| p.save()).unwrap_or(Ok(()));
        s.status = match r { Ok(()) => "Project saved".into(), Err(e) => format!("Save failed: {e}") };
    });
    cb!(ui.exp_mkv, |a, u| { start_export(a, u, "mkv"); });
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
    cb!(ui.fitz, |a, u| { let mut s = a.borrow_mut(); s.scroll = 0.0; s.zoom = (u.timeline.w() as f64 / s.total().max(1) as f64).max(1e-4); });
    cb!(ui.narr, |a, u| {
        let mic = { let s = a.borrow(); match u.mic.value() { i if i >= 2 => s.mics.get(i as usize - 2).cloned(), _ => None } };
        let mut s = a.borrow_mut();
        if s.narr.is_some() { s.stop_narration(); } else if let Err(e) = s.start_narration(mic) { drop(s); alert(&format!("Narration: {e}")); }
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
            let dirty = a2.borrow().proj.as_ref().map(|p| p.dirty).unwrap_or(false);
            if a2.borrow().export.is_some() {
                if dialog::choice2_default("An export is running. Cancel it and quit?", "Keep working", "Quit", "") != Some(1) { return; }
                if let Some(j) = &a2.borrow().export { j.cancel.store(true, Relaxed); }
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
    let rec = Recorder::start(RecConfig { region, fps: 30, quality: 80, s444: false, half: false, cursor: true, mic: None, sys, dir: dir.clone() })?;
    println!("capture: {}x{} shm={}", rec.dims.0, rec.dims.1, rec.shm);
    if let Some(e) = &rec.sys_error { return Err(format!("system audio: {e}")); }
    let t0 = Instant::now();
    thread::sleep(Duration::from_secs_f64(secs));
    let dups = rec.sh.dups.load(Relaxed);
    let (dims, fps) = (rec.dims, rec.fps);
    let (frames, _, sys_trk) = rec.finish()?;
    println!("recorded {frames} frames in {:.2}s (dups {dups})", t0.elapsed().as_secs_f64());
    let src = Arc::new(Source::open(&dir, dims.0, dims.1, fps)?);
    let spf = src.spf();
    let n = src.count();
    // synthetic 440 Hz tone track + edits
    let tone: Vec<i16> = (0..n as usize * spf).map(|i| ((i as f32 * 440.0 * std::f32::consts::TAU / AR as f32).sin() * 8000.0) as i16).collect();
    let mut e = Edit { segs: vec![Seg::Src { start: 0, len: n }], tracks: vec![Track { name: "Tone".into(), ch: 1, data: Arc::new(tone), gain: 1.0, muted: false }] };
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
    let mut p = Project { src: src.clone(), edit: e.clone(), undo: vec![], redo: vec![], dirty: true };
    p.save()?;
    let p2 = Project::load(&dir)?;
    assert_eq!(p2.edit.segs, p.edit.segs);
    assert_eq!(p2.edit.tracks[0].data.len(), p.edit.tracks[0].data.len());
    let mkv = out.join("selftest.mkv");
    let t1 = Instant::now();
    export_mkv(&mkv, &src, &p2.edit, &AtomicU32::new(0), &AtomicBool::new(false))?;
    println!("export {} ({} bytes) in {:.3}s", mkv.display(), std::fs::metadata(&mkv).map_err(es)?.len(), t1.elapsed().as_secs_f64());
    export_wav(&out.join("selftest.wav"), &src, &p2.edit)?;
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
        peaks: HashMap::new(), rec: None, rec_pending: false, ctl: None, narr: None, dec: Decoder::new(), preview: None, export: None,
        mics: input_device_names(),
        status: match out_err { Some(e) => format!("Ready (no audio output: {e}; preview is silent)"), None => "Ready".into() },
        ffmpeg: ffmpeg_available(),
    }));
    let ui = build_ui(&app_state);
    wire(&app_state, &ui);
    let mut w = ui.win.clone();
    w.show();
    refresh(&app_state, &ui);
    {
        let (s, u) = (app_state.clone(), ui.clone());
        app::add_timeout3(1.0 / 60.0, move |h| { tick(&s, &u); app::repeat_timeout3(1.0 / 60.0, h); });
    }
    if let Some(p) = args.get(1).filter(|p| !p.starts_with('-')) {
        match Project::load(Path::new(p)) {
            Ok(pr) => {
                let mut s = app_state.borrow_mut();
                let n = pr.edit.total();
                s.proj = Some(pr);
                s.zoom = (ui.timeline.w() as f64 / n.max(1) as f64).max(1e-4);
                s.after_edit();
                s.seek(0);
            }
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
