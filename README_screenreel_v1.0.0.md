# ScreenReel v1.0.0 — User & Developer Guide

ScreenReel is a lightweight, ScreenPal-style screen recorder and editor written in Rust and FLTK. It builds as a single binary for Linux and Windows.

| | |
|---|---|
| Source | `screenreel_v1.0.0.rs` (single file, about 2,600 lines) |
| Build helper | `build_screenreel_v1.0.0.sh` |
| CI | `.github/workflows/screenreel_v1.0.0.yml` |
| Binary size | about 4.7 MB (Linux, stripped, LTO) |

---

## 1. What it does

| Feature | How |
|---|---|
| Record the full screen or a region | X11 MIT-SHM capture on Linux (falls back to XGetImage). GDI BitBlt on Windows. The cursor is composited into the video. |
| Microphone | Records directly into a **Mic** track that is sample-aligned to the video. |
| Visual timeline | Ruler, video lane (blue = recorded, orange = FREEZE) and one waveform lane per audio track. Includes selection, playhead, zoom and scroll. |
| Cut | Ripple-deletes the selection from the video and from every audio track. |
| Insert freeze | Holds the frame under the playhead for N seconds. Tracks get matching silence, which you can narrate over. |
| Freeze selection | Holds the first frame of the selection across the whole range. Audio keeps playing. |
| Narration re-record | Plays the video and records the mic over the selection (or from the playhead to the end), replacing that part of the track. |
| Audio tracks | Add, swap, remove, mute, set gain (0–200 %) and nudge by ±1 frame. Imports WAV, MP3, FLAC, OGG, M4A/AAC and the audio from MP4, MKV or MOV files. |
| Export | **MKV**: instant, no video re-encode. **MOV**: lossless remux through ffmpeg. **MP4**: H.264 re-encode through ffmpeg. **WAV**: the audio mix. |
| Projects | Saved as a folder you can reopen. Undo/redo keeps 64 steps. |

### Why edits and exports never re-encode the video

Video is captured as **MJPEG**: every frame is an independent JPEG key frame, encoded once at capture time by a worker pool. A cut, a freeze or an audio swap only changes an *edit list* and the PCM audio buffers. Export copies the original JPEG bytes into Matroska next to PCM audio.

That gives you:
- no generation loss
- frame-accurate cuts, with no GOP or keyframe snapping
- exports that run at disk speed (5.7 s of 1080p30 exported in about 25 ms in testing)

Audio is 48 kHz 16-bit PCM throughout. Mixing tracks for export is plain sample arithmetic, never a lossy codec. The only lossy step is the JPEG quality you choose at capture time.

---

## 2. Quick start

1. Run the binary (`screenreel_v1.0.0-linux-x86_64-static`, `...-glibc`, or `screenreel_v1.0.0-windows-x86_64.exe`).
2. Pick an fps, a quality and a microphone in the top bar.
3. Click **● Record Screen**, or **● Record Region** and drag a rectangle (Esc or right-click cancels).
4. After a 3-2-1 countdown, a small **REC** bar shows elapsed time with **|| Pause** and **■ Stop**.
5. Click **■ Stop**. The recording opens in the editor and is already saved as a project in `~/Videos/ScreenReel/rec_YYYYMMDD_HHMMSS/` (`%USERPROFILE%\Videos\ScreenReel\…` on Windows; the timestamp is in UTC).
6. Edit, then click **Export MKV (instant)**.

### Capture settings

| Setting | Meaning |
|---|---|
| fps | 10, 15, 20, 24, 25, 30, 50 or 60. All of these divide 48 kHz exactly, so audio and video boundaries are sample-exact. |
| Quality | **Draft** = q60, 4:2:0. **Standard** = q80, 4:2:0. **Sharp text** = q92, 4:4:4, which keeps small coloured text crisp but makes bigger files. |
| Half-res | Applies a 2×2 box downscale before encoding. Use it for 4K screens: about 4× smaller files and 4× less CPU. |
| Cursor | Composites the mouse pointer into the video. |
| Mic | Off, the system default, or a named input device. |

If the encoders can't keep up, a frame is duplicated rather than letting audio drift. The REC bar counts these, and the final status line reports the total.

---

## 3. Editing

### Timeline mouse controls

| Action | Effect |
|---|---|
| Click | Moves the playhead. |
| Drag | Selects a range. Dragging near either edge auto-scrolls. |
| Click a waveform lane | Selects that track. |
| Mouse wheel | Zooms around the pointer. |
| Shift+wheel or horizontal wheel | Scrolls. |
| **⇔ Fit** | Shows the whole timeline. |

### Shortcuts

These work when the timeline or window has focus.

| Key | Action |
|---|---|
| Space | Play / pause (also stops a narration take) |
| Del / Backspace | Cut the selection |
| F | Insert a freeze at the playhead (length = spinner value) |
| ← / → | Step one frame |
| Home / End | Jump to start / end |
| Ctrl+Z / Ctrl+Y | Undo / redo |
| Ctrl+S | Save the project |
| Esc | Clear the selection, or stop narration |

### Narration (ScreenPal-style re-record)

1. Select the track to record into. If no track is selected, a new mono **Narration N** track is created. **+ Empty track** also makes one.
2. Optionally drag-select a range.
3. Click **● Re-record narration**. The video plays from the start of the range while the mic records. The track being replaced is muted during the take.
4. The take stops at the end of the range, or when you click again or press Space. With a selection, any part of the range you didn't record over becomes silence, so no old take is left behind.

To sync narration, **◀ 1f / 1f ▶** shifts the selected track by one video frame.

### Swapping and adding audio

| Button | What it does |
|---|---|
| **+ Add audio file…** | Adds a new track starting at the playhead. |
| **Swap audio…** | Replaces the selected track's audio, starting at 0:00. Use it to drop in a cleaned-up voice-over or music bed. |
| **Mute** and the gain slider | Change how the track is mixed for playback and export. Neither touches the stored samples. |
| **× Remove track** | Deletes the track. Undo restores it. |

---

## 4. Export

| Button | Output | Re-encode? | Needs |
|---|---|---|---|
| Export MKV (instant) | Matroska with `V_MJPEG` + `A_PCM/INT/LIT` 48 kHz stereo, 1-second clusters and cues (seekable) | **None** | nothing |
| Export MOV (lossless) | QuickTime container, same streams, via `ffmpeg -c copy` | **None** | `ffmpeg` on PATH |
| Export MP4 (H.264) | H.264 CRF 18 + AAC 192k, faststart, for upload sites | Yes (labelled) | `ffmpeg` on PATH |
| Export WAV mix | 48 kHz stereo 16-bit mix of all unmuted tracks | n/a | nothing |

MKV plays in VLC, mpv, ffplay, Kdenlive, Shotcut, DaVinci Resolve and OBS. Use MP4 when a website needs H.264. Exports write to `*.part` and rename on success, so an interrupted export never leaves a truncated file under the final name.

---

## 5. Project format

Every project is a folder:

```
rec_20261125_193739/
  frames.mjpg     concatenated JPEG frames (append-only, never modified by edits)
  frames.idx      12 bytes per frame: u64 offset + u32 length (little-endian)
  project.txt     key=value: version, width, height, fps, seg=S|H ..., track=name|gain|muted|file
  track_N.wav     48 kHz PCM per track (mono or stereo)
```

Open a project with **Open…** (choose `project.txt`), or pass the folder on the command line: `screenreel ~/Videos/ScreenReel/rec_…`.

---

## 6. Platform notes

### Linux

- Capture needs an **X11** session, or a Wayland session where `DISPLAY` points at XWayland. Under pure Wayland, XWayland can only see X11 windows.
- Audio uses the pure-Rust PulseAudio protocol client first, so it works with PipeWire (through `pipewire-pulse`) and PulseAudio and needs no libpulse. If no Pulse server is running, it falls back to ALSA.
- In the fully static build, ALSA can't load `.so` plugins, so plain-ALSA systems should use `hw:`/`plughw:` devices.
- On Linux the REC bar is captured if it's inside the recorded area. It is placed above or below the region when there's room. For full-screen recordings, cut it out afterwards or minimise it; recording continues.

### Windows

- Windows 10 2004 or later: the REC bar is excluded from capture (`WDA_EXCLUDEFROMCAPTURE`).
- The process is per-monitor DPI-aware, so capture works in physical pixels on every monitor.
- Capture pauses on the secure desktop (UAC prompts). Those frames become duplicates and the recording carries on.

macOS is not supported.

---

## 7. Building

### Native Linux, portable glibc (tested)

Rust, FLTK, the JPEG code and ALSA are all linked in. Only the system X11, fontconfig/freetype and libc libraries are dynamic.

```sh
sudo apt install cmake ninja-build g++ pkg-config autoconf automake libtool \
  libx11-dev libxext-dev libxinerama-dev libxcursor-dev libxrender-dev libxfixes-dev libxft-dev libfontconfig1-dev
./build_screenreel_v1.0.0.sh native            # -> dist/screenreel_v1.0.0-linux-x86_64-glibc
./build_screenreel_v1.0.0.sh selftest dist/screenreel_v1.0.0-linux-x86_64-glibc
```

### Fully static Linux (musl)

```sh
./build_screenreel_v1.0.0.sh linux-static      # needs podman or docker; builds inside alpine:3.20
```

The script:
1. Installs Alpine's `*-static` X, fontconfig and freetype archives, building any missing X.Org library from its release tarball with `--enable-static`.
2. Builds alsa-lib statically.
3. Hides the `.so` dev symlinks inside the container. This forces fltk-sys's `dylib=X11…` requests to resolve to `.a` archives.
4. Links with `+crt-static` and checks with `ldd` that nothing dynamic is left.

### Windows

| Method | Command |
|---|---|
| Native (MSVC) | `set RUSTFLAGS=-C target-feature=+crt-static` then `cargo build --release`. Static CRT, so no VC++ redistributable is needed. |
| Cross-build from Linux with mingw-w64 | `./build_screenreel_v1.0.0.sh windows` |

CI (`.github/workflows/screenreel_v1.0.0.yml`) builds all three, runs the self-test under Xvfb, and attaches binaries to a GitHub release when you push a `v*` tag.

### Command line

```
screenreel [project_dir]                      open the editor (optionally with a project)
screenreel --selftest <outdir> [secs] [full]  headless capture → edit → save/load → export → verify
screenreel --probe-audio <file>               check that a file imports (channels/duration)
screenreel --version
```

---

## 8. Architecture (for developers)

```
capture thread ──(bounded, try_send)──► N JPEG encoder workers ──(bounded)──► writer thread
   │  wall-clock scheduler, pause-aware            │ jpeg-encoder (AVX2)           │ re-orders by seq,
   │  emits Dup(seq) when behind/full ─────────────┼──────────────────────────────►│ appends frames.mjpg,
   └ buffer pool ◄──── raw frame buffers recycled ─┘                               │ duplicates = repeated index entries (0 bytes)
cpal input stream ─► mono i16 chunks (pause-aware) ─► resample to 48 kHz ─► fit to frames × (48000/fps)

UI thread (FLTK) ─ 60 Hz tick: playback clock, decoder results, export progress, REC status
preview decoder thread ─ latest-request-wins JPEG decode (zune-jpeg) + 2×2 averaged downscale
cpal output stream ─ real-time mixer (try_lock, never blocks), audio clock drives the playhead
export thread ─ Matroska muxer, cancellable, progress in permille
```

**Edit model.** The edit is a `Vec<Seg>`, where `Seg` is `Src{start,len}` or `Hold{frame,len}`, plus `Vec<Track{Arc<Vec<i16>>}>`.
- Snapshots for undo are cheap: they clone `Arc`s.
- Every operation is ripple-consistent between video and audio, because `samples_per_frame = 48000/fps` is an exact integer.

### Audit notes (v1.0.0)

**Correctness**
- Frame scheduling uses an integer frame index against accumulated active time, so pauses are excluded and there is no cumulative drift.
- Encoder overflow becomes a duplicate frame instead of a timestamp gap.
- Mic drift of 3 % or less (device clock vs wall clock) is absorbed by a linear stretch; anything larger is padded or truncated.

**Concurrency**
- The audio callback never blocks: it uses `try_lock` and an atomic seek handoff.
- UI callbacks never hold the app state borrowed across nested dialog loops, and draw/tick paths use `try_borrow`.

**Security**
- Project files are validated on load: index bounds checked against the file size, 64 MiB per-frame cap, segment ranges, file-name allowlist for track files (no path traversal), header limits.
- JPEG decode is capped at 16384² and audio import at 4 hours.
- ffmpeg is spawned with an argv array (no shell) and absolute paths.
- `@` is escaped in all FLTK labels, so file and device names can't inject symbols.

**Robustness**
- Saves are atomic (temp file + rename).
- X11 SHM segments are marked `IPC_RMID` right after attach, so they can't leak if the process crashes.
- GDI handles and the `GetIconInfo` bitmaps are freed; the cursor hotspot is cached per handle.

### Known limitations and roadmap

- MJPEG files are larger than H.264, roughly 2–8 MB/s at 1080p30 depending on content and preset. Use Half-res or Draft for long recordings, and MP4 export for sharing.
- Audio tracks are held in RAM: about 5.7 MB per minute for a mono track.
- Planned: Wayland capture through the xdg-desktop-portal/PipeWire screencast, webcam overlay, text and arrow annotations, global hotkeys, and system-audio (loopback) capture.

---

## 9. Changelog

**1.0.0** (2026-11-25) — First release:
- full-screen and region capture with cursor
- microphone recording
- timeline with cut, insert freeze, freeze selection, undo and redo
- narration re-record
- audio track add, swap, remove, mute, gain and shift
- MKV, MOV, MP4 and WAV export
- projects
- `--selftest` and `--probe-audio`
- CI for static Linux and Windows builds
