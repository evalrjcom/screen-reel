# ScreenReel v1.4.0 — User & Developer Guide

ScreenReel is a lightweight, ScreenPal-style screen recorder and editor written in Rust and FLTK. It builds as a single binary for Linux and Windows.

| | |
|---|---|
| Source | `screenreel_v1.4.0.rs` (single file, about 7,700 lines) plus `assets/` (embedded font) |
| Build helper | `build_screenreel_v1.4.0.sh` |
| CI | `.github/workflows/screenreel_v1.4.0.yml` |
| Binary size | about 6.5 MB (Linux, stripped, LTO; the noise-reduction model and the annotation font account for most of the growth since v1.3) |

---

## 1. What it does

| Feature | How |
|---|---|
| Record the full screen or a region | X11 MIT-SHM capture on Linux (falls back to XGetImage). GDI BitBlt on Windows. The cursor is composited into the video. |
| Microphone | Records directly into a **Mic** track that is sample-aligned to the video. |
| Live voice FX (v1.4.0) | Optional. Noise reduction (RNNoise-style), a noise gate, a voice EQ preset, a de-esser and the opto compressor run on the mic **while you record**, so the take is already polished. The same chain is available afterwards as **Voice FX…** on any track. |
| System audio | Optional. Records what the computer is playing (a video, a browser tab, an app) into its own stereo **System** track, alongside the mic. Mute, swap, cut or re-level each track on its own. |
| Visual timeline | Ruler, video lane (blue = recorded, orange = FREEZE, green = sped up, labelled "2×" etc.) and one waveform lane per audio track. Includes selection, playhead, zoom and scroll. Dead-air candidates show in red. |
| Cut | Ripple-deletes the selection from the video and from every audio track. |
| Insert freeze | Holds the frame under the playhead for N seconds. Tracks get matching silence, which you can narrate over. |
| Freeze selection | Holds the first frame of the selection across the whole range. Audio keeps playing. |
| Opto compressor | Warm, CL 1B-style optical compressor for the mic (or any track). It has presets, a live transfer curve and A/B audition on the video before you apply. |
| Normalize levels | Brings every track to the same loudness (ITU-R BS.1770 LUFS, e.g. −16 LUFS for voice). A look-ahead limiter keeps peaks under −1 dBFS. |
| Level meter | Stereo output peak meter with peak hold, next to the time display. |
| Auto-duck (v1.4.0) | Lowers a music or system-audio track automatically while you speak, with smooth attack and release. |
| Webcam overlay (v1.4.0) | Records a webcam alongside the screen and shows it picture-in-picture: circle, rounded or square, any corner, adjustable size, optional background blur and mirroring. |
| Cursor effects and auto zoom (v1.4.0) | Records where the pointer is, when you click and when you type (never which key). Adds click rings, a cursor highlight, and **Auto zoom**, which zooms and pans smoothly to where you click and type. |
| Annotations (v1.4.0) | Text callouts, arrows, blur and pixelate boxes (hide passwords and keys), numbered step markers and manual zoom regions. Drawn directly on the preview, timed on the timeline, undoable. |
| Autosave and crash recovery (v1.4.0) | Unsaved edits are autosaved every 60 s. A recording interrupted by a crash or power cut is recovered on next start. |
| Speed up selection | Plays the selected range 1.25× to 16× faster. Video skips frames losslessly. Audio is time-compressed with its pitch kept (WSOLA), or muted if you prefer. |
| Find / remove dead air | Finds stretches where every audible track is silent **and** the picture doesn't change, highlights them, and cuts them all in one undoable step. |
| Compact project | Rewrites the recording so it keeps only the frames your edit uses. Cut footage and dead air stop taking disk space. |
| Narration re-record | Plays the video and records the mic over the selection (or from the playhead to the end), replacing that part of the track. |
| Audio tracks | Add, swap, remove, mute, set gain (0–200 %) and nudge by ±1 frame. Imports WAV, MP3, FLAC, OGG, M4A/AAC and the audio from MP4, MKV or MOV files. |
| Export | **MKV**: instant, no video re-encode. **MOV**: lossless remux through ffmpeg. **MP4**: H.264 re-encode through ffmpeg, on the GPU when available. **WAV**: the audio mix. |
| Projects | **New**, **Open…** and **Save**. Saved as a folder you can reopen. Undo/redo keeps up to 64 steps (fewer on very long tracks; see §8). |

### Why edits and exports never re-encode the video

Video is captured as **MJPEG**: every frame is an independent JPEG key frame, encoded once at capture time by a worker pool. A cut, a freeze or an audio swap only changes an *edit list* and the PCM audio buffers. Export copies the original JPEG bytes into Matroska next to PCM audio.

That gives you:
- no generation loss
- frame-accurate cuts, with no GOP or keyframe snapping
- exports that run at disk speed (5.7 s of 1080p30 exported in about 25 ms in testing)

**Effects (v1.4.0)** keep this property wherever they aren't used. Annotations, click rings, zoom and the webcam overlay are drawn only on the frames they touch; those frames are re-encoded once at export (quality 90, 4:4:4), and every other frame is still copied byte for byte. The webcam overlay and the cursor highlight touch every frame while they're on, so with either enabled the whole video is re-encoded at export (in parallel on all cores).

Audio is 48 kHz 16-bit PCM throughout. Mixing tracks for export is plain sample arithmetic, never a lossy codec. The only lossy step is the JPEG quality you choose at capture time.

---

## 2. Quick start

1. Run the binary (`screenreel_v1.4.0-linux-x86_64-static`, `...-glibc`, or `screenreel_v1.4.0-windows-x86_64.exe`).
2. Pick an fps, a quality and (optionally) a **Webcam** in the **RECORD** row, and a microphone in the **AUDIO IN** row. Tick **System audio** to also record what the computer plays, and **Live voice FX** to clean up the mic as you record.
3. Click **● Record Screen**, or **● Record Region** and drag a rectangle (Esc or right-click cancels).
4. After a 3-2-1 countdown, a small **REC** bar shows elapsed time with **|| Pause** and **■ Stop**.
5. Click **■ Stop**. The recording opens in the editor and is already saved as a project in `~/Videos/ScreenReel/rec_YYYYMMDD_HHMMSS/` (`%USERPROFILE%\Videos\ScreenReel\…` on Windows; the timestamp is in UTC).
6. Edit, then click **MKV · instant** (EXPORT, in the PROJECT row). For speech, run **Opto compressor…** on the mic track and **Normalize levels…** before exporting.

### Capture settings

| Setting | Meaning |
|---|---|
| fps | 10, 15, 20, 24, 25, 30, 50 or 60. All of these divide 48 kHz exactly, so audio and video boundaries are sample-exact. |
| Quality | **Draft** = q60, 4:2:0. **Standard** = q80, 4:2:0. **Sharp text** = q92, 4:4:4, which keeps small coloured text crisp but makes bigger files. |
| Half-res | Applies a 2×2 box downscale before encoding. Use it for 4K screens: about 4× smaller files and 4× less CPU. |
| Cursor | Composites the mouse pointer into the video. |
| Mic | Off, the system default, or a named input device. |
| System audio | Also record the computer's sound output (loopback) to a separate stereo **System** track. |
| Webcam | Off or a camera. Recorded at up to 640×480 next to the screen (`frames.cam.*`); shown as picture-in-picture, adjustable afterwards under **Effects…**. |
| Live voice FX | Runs the saved voice chain on the mic while recording (set it up under **Voice FX… → Save for live recording**). |

### Recording system audio with the mic

Tick **System audio** next to the mic picker. A recording then gets up to two tracks:
- **Mic**: your voice, mono.
- **System**: everything the computer plays through its default output, stereo.

Things to know:
- **Use headphones** when recording both. With speakers, the mic also picks up the system sound, so you hear it twice with a slight echo.
- Silence is kept in its place. If nothing plays for a while, or you pause the recording, later sound still lines up with the video.
- System audio follows the **default output device**. If you switch outputs (for example, plug in a headset) during a recording, capture stays on the original device. Pick your output before you press Record.
- If system audio can't be opened, the recording continues without it and the status line says why.
- Only record audio you have the right to use.

| Platform | How system audio is captured |
|---|---|
| Windows | WASAPI loopback on the default output device. Nothing to set up. |
| Linux | The default output's *monitor* source on PulseAudio or PipeWire (`pipewire-pulse`). Needs a running sound server. Plain ALSA has no loopback. |

If the encoders can't keep up, a frame is duplicated rather than letting audio drift. The REC bar counts these, and the final status line reports the total.

---

## 3. Editing

### The control dock

All controls sit in one dock under the video preview. The preview takes the rest of the window. There are seven labelled rows, ordered by when you use them:

| Row | Contains |
|---|---|
| **RECORD** | ● Record Screen, ● Record Region, fps, quality, Half-res, Cursor, Webcam |
| **AUDIO IN** | mic, System audio, Live voice FX |
| **PROJECT · EXPORT** | New, Open…, Save, then MKV · instant, MOV · lossless, MP4 · H.264, WAV · audio mix |
| **PLAY · EDIT** | ▶ Play, time, level meter, Cut, Insert freeze (+ seconds), Freeze sel., Undo, Redo, ● Narrate, ⇔ Fit |
| **TRACK** | track picker, Mute, gain, ±1 frame nudge, + Track, + Audio file…, Swap…, × Remove, **Opto compressor…**, **Voice FX…** |
| **TOOLS** | **Normalize levels…**, **Auto-duck…**, Speed (picker, Mute audio, » Speed up sel.), Find dead air…, Remove dead air, Compact… |
| **ANNOTATE** | tools (Select, Text, Arrow, Blur, Pixelate, Step, Zoom), colour, Delete, Edit text…, Time = selection, **Auto zoom**, **Effects…** |

Colour carries meaning: red records, green plays, blue exports or applies, amber is audio FX. Every control has a fixed width with a spacer at the end of its row, so nothing stretches into giant buttons on wide monitors. Every button has a tooltip showing its shortcut. Unavailable actions are dimmed rather than hidden, so the layout never jumps. The whole app uses one dark theme that matches the timeline and video.

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
| Space Space (double tap within 0.4 s) | Back to 0:00. If it was playing, it keeps playing from the start; if paused, it stays paused |
| Del / Backspace / **Ctrl+X** | Cut the selection |
| **Ctrl+N** | New project (asks to save unsaved changes first) |
| F | Insert a freeze at the playhead (length = spinner value) |
| ← / → | Step one frame |
| Home / End | Jump to start / end |
| Ctrl+Z / Ctrl+Y | Undo / redo |
| Ctrl+S | Save the project |
| Esc | Clear the selection (and the selected annotation), or stop narration |
| Del (with an annotation selected) | Delete that annotation |

### Live voice chain and Voice FX (v1.4.0)

**Voice FX…** (TRACK row) opens the voice chain. It runs in this order:

| Stage | What it does |
|---|---|
| EQ | A high-pass (70–90 Hz) plus shelving and bell filters for each preset: *Voice clarity*, *Warm broadcast* or *Bright (dull mic fix)*. Runs first so noise reduction gets rumble-free input. |
| Noise reduction | RNNoise (the `nnnoiseless` Rust port): a small recurrent network that removes fans, hum and keyboard hiss while keeping speech. About 11 dB on white noise in testing. |
| Gate | A downward expander (−30 dB floor, 6 dB hysteresis, 120 ms hold), not a hard mute, so breaths and room tone don't pump. |
| De-esser | Detects sibilance in the high band and turns the voice down for the few milliseconds an "s" lasts, only when that band dominates (so bright vowels are left alone). It uses wideband gain to avoid the phase smearing of a filter-split de-esser. |
| Compressor | Any opto compressor preset (see below), or off. |

**Apply** processes the selected track (one undo step, runs in the background). **Save for live recording** stores the chain (`~/.config/screenreel/voice_fx`, `%APPDATA%\ScreenReel\voice_fx`) and ticks **Live voice FX**, so the next recording's mic track comes out already processed. The live chain runs on the audio sink thread at well over 100× real time on one core, so it doesn't affect capture. Noise reduction adds 10 ms of delay, which is compensated: the processed track stays sample-aligned with the video.

Live processing is permanent for that take. If you want to keep the raw mic, leave **Live voice FX** off and apply **Voice FX…** after recording instead.

### Auto-duck (v1.4.0)

**Auto-duck…** (TOOLS row) lowers one track (usually **System** or music) whenever another (usually **Mic**) has speech. You set the depth (3–30 dB, default 12) and the voice threshold (default −40 dBFS). The gain moves on a 10 ms envelope with 80 ms attack, 300 ms hold and 600 ms release, so the music comes back smoothly between sentences. One undo step.

### Annotations (v1.4.0)

Pick a tool in the **ANNOTATE** row and drag on the preview:

| Tool | Result |
|---|---|
| Text | A callout box (rounded, contrasting text colour). Click without dragging for a default-size box; double-click an existing one to edit its text. |
| Arrow | An arrow from where you press to where you release. |
| Blur / Pixelate | Hides what's underneath, e.g. passwords, API keys or e-mail addresses. |
| Step | A numbered circle (1, 2, 3…) for step-by-step tutorials. |
| Zoom | Zooms the video into the box you drew, with a smooth 0.5 s ease in and out. |

New annotations last for the timeline selection, or 4 s from the playhead if nothing is selected. **Time = selection** retimes the selected annotation; the coloured bars under the video lane show when each one is on screen (click one to select it). With **Select**, drag an annotation to move it or its corner handle to resize it. Annotations move with the picture when you cut, insert freezes or speed up ranges, and are removed when their whole span is cut. They are clipped to the video frame.

### Cursor, clicks and auto zoom (v1.4.0)

While recording, ScreenReel stores per frame where the pointer is, whether a mouse button went down and whether any key was pressed (`frames.ev`, 5 bytes per frame). **It never records which key.** Modifier keys alone (Shift, Ctrl, Alt) don't count as typing.

- **Click rings:** blue for left clicks, orange for right clicks, expanding for half a second.
- **Cursor highlight:** a soft yellow spotlight around the pointer.
- **Auto zoom:** creates zoom annotations around clusters of clicks and typing, following the pointer smoothly. Each one is an ordinary annotation, so you can delete, retime or resize it.

Turn rings and the highlight on or off under **Effects…**. Clicks are sampled at 125 Hz, which catches every normal click (people hold a button for 60–150 ms); clicks generated by software in under 8 ms can be missed.

### Webcam picture-in-picture (v1.4.0)

Choose a camera in the RECORD row before recording. Under **Effects…** you can show or hide it and set the corner, size (10–45 % of the width), shape (circle, rounded, square), **background blur** and **mirror**. Changes are undoable and affect only export and preview; the camera recording itself isn't changed.

Background blur keeps a soft oval for the head and shoulders sharp and blurs the rest. It is a fixed shape, not person segmentation, so it works best when you sit centred in the frame.

| Platform | Camera API |
|---|---|
| Linux | V4L2 (`/dev/video*`), MJPEG, falling back to YUYV. 64-bit builds only. |
| Windows | Media Foundation. |

If the camera can't be opened, recording continues without it and the status line says why.

### Autosave and crash recovery (v1.4.0)

- **Autosave:** every 60 s, unsaved edits are written in the background to `project.autosave.txt` (plus `autosave_track_N.wav` for changed tracks). Your real save files are never touched. When you open a project with a newer autosave, ScreenReel offers to restore it. Saving, or choosing **Don't save** when closing, removes the autosave.
- **Recording recovery:** during a recording, video and its index are flushed to disk about once a second, and mic and system audio are written to `mic.raw` / `sys.raw` as they arrive. If ScreenReel or the computer crashes, the next start offers to recover the recording. A partly written last frame is dropped, and audio is fitted to the frames that survived.

### Narration (ScreenPal-style re-record)

1. Select the track to record into. If no track is selected, a new mono **Narration N** track is created. **+ Track** also makes one.
2. Optionally drag-select a range.
3. Click **● Narrate**. The video plays from the start of the range while the mic records. The track being replaced is muted during the take.
4. The take stops at the end of the range, or when you click again or press Space. With a selection, any part of the range you didn't record over becomes silence, so no old take is left behind.

To sync narration, **◀ 1f / 1f ▶** shifts the selected track by one video frame.

### Opto compressor (warm CL 1B style)

Select the mic track (it's picked automatically if nothing is selected), then click **Opto compressor…**.

| Control | Range | What it does |
|---|---|---|
| Preset | 5 | Voice: gentle 2:1 (default) · Voice: leveling 4:1 · Broadcast 6:1 · Warm glue 1.5:1 · Peak tamer 10:1 |
| Threshold | −60…0 dB | Where compression starts. The 8 dB soft knee is typical of optical cells. |
| Ratio | 1…20 | Compression above the threshold |
| Attack | 0.5…200 ms | How fast the cell reacts |
| Release | 0.05…5 s | Base release time. The release is **program-dependent**: after short peaks it recovers quickly, and after long or deep compression it recovers slowly, like the T4 photocell's "memory". That's why opto compression sounds smooth and doesn't pump. |
| Warmth | 0…100 % | Tube make-up stage: asymmetric saturation that adds mostly 2nd-harmonic colour. Small signals pass at unity gain, and a DC blocker follows. |
| Make-up | −12…+24 dB | Output gain |

The curve on the right is the static transfer curve (input dB → output dB), and it updates as you move the controls.

- **▶ Audition** plays the video from the playhead with the processed track in the real mixer.
- **A/B: processed** switches between the processed and original sound while playing.
- Changes made while auditioning are re-rendered automatically after about 0.3 s.
- **Apply** writes the result to the track as one undo step. **Cancel** leaves the track untouched.
- After a run, the dialog shows the maximum and average gain reduction and the input and output peaks.

### Normalize levels

**Normalize levels…** shows every track's current loudness. Pick a target:

| Target | Use |
|---|---|
| −14 LUFS | YouTube and streaming |
| **−16 LUFS** | voice and tutorials (recommended) |
| −19 LUFS | quieter, background |
| −23 LUFS | EBU R128 broadcast |

Apply it to **all tracks** (they end up at the same level) or the selected track only.

- **Measurement:** integrated loudness per ITU-R BS.1770-4: K-weighting, 400 ms blocks with 75 % overlap, −70 LUFS absolute and −10 LU relative gates.
- **Mono tracks** are measured as the dual-mono signal they become in the stereo mix, so a mono mic and a stereo System track really do match.
- **Peaks:** a 2 ms look-ahead / 80 ms release limiter keeps every sample at or under −1 dBFS. The status line reports any limiting.
- **Boost cap:** gain is capped at +30 dB, so a nearly silent track isn't blown up into noise. Silent tracks are skipped.
- **Undo:** all tracks change in one undo step, and the track gain sliders reset to 100 %.

**Recommended order:** compress the mic first, then normalize everything.

### Level meter and mix ceiling

The meter next to the time shows the stereo output peak. It covers −48 to 0 dBFS: green below −18, yellow up to −6, then red. Its tick marks peak hold for 1.5 s.

Playback and every export pass the mix through a soft ceiling. It's transparent below −1 dBFS, then a smooth tanh knee, so overlapping tracks never hard-clip.

### Speeding up a range

1. Drag-select the range on the timeline.
2. Pick a speed next to **Speed** (1.25×, 1.5×, 2×, 3×, 4×, 8×, 16×).
3. Optionally tick **Mute audio**. Otherwise the audio is compressed to the new length with its pitch kept, so speech still sounds like speech, just faster.
4. Click **» Speed up sel.** The range turns green with its speed label, and the selection now covers the shorter range.

Speeding up an already sped-up range multiplies the speeds, up to 64×. Freeze segments inside the range simply get shorter. Undo with Ctrl+Z.

How it works: a sped-up range is a `Fast` segment that plays every *n*th recorded frame. No video is re-encoded, and MKV export stays instant.

### Removing dead air

1. Click **Find dead air…** and enter the minimum length to remove (default 2 s, minimum 0.3 s).
2. The scan runs in the background, with progress on the status line. Matching regions turn red.
3. Click **Remove dead air (N)** to cut them all as one undo step.

A frame counts as dead when:
- **every unmuted track is silent**: its peak is below −45 dBFS, after gain;
- **and the picture doesn't change**: fewer than 0.05 % of pixels (sampled on a 4-pixel grid) change by more than 40/255 compared with the first frame of the still stretch. Moving the mouse alone doesn't count as a change, but typing or scrolling does.

About 0.2 s is left at each edge of every region, so cuts never feel abrupt. Any edit, mute or gain change clears the results, so a stale range can never be cut.

### Freeing disk space: Compact project

Cutting, removing dead air and speeding up only change the edit list. The original recording stays on disk, which is what makes undo instant. When you're done, **Compact…** reclaims the space:

- It writes `frames_cN.mjpg` / `.idx` holding each frame the edit uses exactly once. Holds and speed-ups become repeated or skipped 12-byte index entries.
- It points `project.txt` at the new files and deletes the old ones.

It is lossless: the JPEG bytes are copied. The edit is flattened to one blue segment and undo history is cleared, so the app asks you to confirm first. New files are fully written and synced before the project switches to them.

### Swapping and adding audio

| Button | What it does |
|---|---|
| **+ Audio file…** | Adds a new track starting at the playhead. |
| **Swap…** | Replaces the selected track's audio, starting at 0:00. Use it to drop in a cleaned-up voice-over or music bed. |
| **Mute** and the gain slider | Change how the track is mixed for playback and export. Neither touches the stored samples. |
| **× Remove** | Deletes the track. Undo restores it. |

---

## 4. Export

| Button | Output | Re-encode? | Needs |
|---|---|---|---|
| MKV · instant | Matroska with `V_MJPEG` + `A_PCM/INT/LIT` 48 kHz stereo, 1-second clusters and cues (seekable) | **None** | nothing |
| MOV · lossless | QuickTime container, same streams, via `ffmpeg -c copy` | **None** | ffmpeg |
| MP4 · H.264 | H.264 at about CRF 18 quality + AAC 192k, faststart, for upload sites. Uses the GPU encoder when one works (see below) | Yes (labelled) | ffmpeg |
| WAV · audio mix | 48 kHz stereo 16-bit mix of all unmuted tracks | n/a | nothing |

### GPU encoding for MP4 (v1.4.0)

On the first MP4 export, ScreenReel checks which H.264 encoders your ffmpeg offers and test-encodes 0.3 s with each in turn (10 s limit per test), then uses the first that works for the rest of the session:

| Order | Encoder | Hardware |
|---|---|---|
| 1 | `h264_nvenc` | NVIDIA |
| 2 | `h264_qsv` | Intel Quick Sync |
| 3 | `h264_amf` | AMD (Windows) |
| 4 | `h264_vaapi` | Intel/AMD on Linux, via `/dev/dri/renderD128` |
| 5 | `libx264` | CPU (always the fallback) |

If a GPU encode fails part-way (driver reset, out of GPU memory), the export is retried once with libx264, so you still get a file. The status line names the encoder used. Set `SCREENREEL_ENCODER=libx264` (or one of the names above) to force a choice.

### Getting ffmpeg for MOV/MP4

In v1.1.0, MOV and MP4 were greyed out whenever `ffmpeg` wasn't on the PATH of the running app. That was common on Windows: a winget or Scoop install, or an unzipped ffmpeg, isn't on PATH for apps started before it.

Since v1.2.0, the buttons are always clickable. ScreenReel looks for ffmpeg in this order:

1. the `SCREENREEL_FFMPEG` environment variable (full path to the executable)
2. a location you picked earlier with **Locate ffmpeg…** (stored in `%APPDATA%\ScreenReel\ffmpeg_path` or `~/.config/screenreel/ffmpeg_path`)
3. the folder ScreenReel itself is in: just drop `ffmpeg.exe` next to `ScreenReel.exe`
4. `PATH`
5. Windows: `%LOCALAPPDATA%\Microsoft\WinGet\Links`, `%USERPROFILE%\scoop\shims`, `%ProgramData%\chocolatey\bin`, `Program Files\ffmpeg\bin`, `C:\ffmpeg\bin`. Linux: `/usr/bin`, `/usr/local/bin`, `/snap/bin`

If none of these is found, clicking MOV or MP4 explains how to install ffmpeg (`winget install Gyan.FFmpeg` on Windows) and offers **Locate ffmpeg…** or **Search again**. Each candidate is checked by running `ffmpeg -version`. On Windows it runs without a console window.

MKV plays in VLC, mpv, ffplay, Kdenlive, Shotcut, DaVinci Resolve and OBS. Use MP4 when a website needs H.264. Exports write to `*.part` / `*.tmp` and rename on success, so an interrupted export never leaves a truncated file under the final name. Since v1.3.1, partial files are also removed after any error, not just after Cancel.

Since v1.3.1, every export (including the ffmpeg phase of MOV/MP4) shows live progress and can be cancelled: ffmpeg runs as a child process that ScreenReel stops on Cancel. WAV export streams straight to disk instead of building the whole mix in memory. ScreenReel refuses to export over the project's own files (`track_N.wav`, `project.txt`, media).

---

## 5. Project format

Every project is a folder:

```
rec_20261125_193739/
  frames.mjpg     concatenated JPEG frames (append-only, never modified by edits)
  frames.idx      12 bytes per frame: u64 offset + u32 length (little-endian)
  project.txt     key=value: version, width, height, fps, media, seg=..., track=name|gain|muted|file
                    seg=S start len           recorded frames start..start+len
                    seg=H frame len           hold one frame (freeze)
                    seg=F start len sp ph     sped up: output k shows start + (ph+k)*sp/100 (sp = speed×100)
                    media=frames              media pair name (frames_cN after Compact project)
  track_N.wav     48 kHz PCM per track (mono or stereo)
  frames.ev       v1.4.0, 5 bytes per frame: i16 x, i16 y (pointer), u8 flags
                    (1 left down, 2 right down, 4 click started, 8 key typed, 16 pointer visible)
  frames.cam.mjpg v1.4.0, webcam JPEGs (each distinct frame stored once)
  frames.cam.idx  v1.4.0, 12 bytes per screen frame; length 0 = no camera frame yet
  project.txt     v1.4.0 keys:
                    fx=cursor_hl click_rings cam_show cam_pos cam_size cam_shape cam_blur cam_mirror
                    ov=K start end x y w h rrggbb[|text]   one per annotation; K = T text, A arrow,
                      B blur, P pixelate, S step (+ n), Z zoom (+ scale follow). x/y/w/h are fractions
                      of the frame; text escapes \n and \\.
  project.autosave.txt, autosave_track_N.wav   v1.4.0 autosave (removed on Save)
  rec.txt, mic.raw, sys.raw                    v1.4.0, present only while recording (crash recovery)
```

The `.ev` and `.cam.*` files are optional: projects from v1.3 and earlier open unchanged, just without cursor effects or webcam.

**Saving (v1.3.1):** changed tracks are first written as `track_N.wav.new`, then `project.txt.tmp` is written. The staged tracks are promoted, `project.txt` is swapped atomically, and stale files are removed only after that. A failed or interrupted save leaves the previous save loadable. Tracks whose audio hasn't changed since the last save or load are not rewritten, so Ctrl+S is near-instant on long projects. Project files with a non-finite gain (`NaN`, `inf`) or more than 64 tracks are rejected.

Open a project with **Open…** (choose `project.txt`), or pass the folder on the command line: `screenreel ~/Videos/ScreenReel/rec_…`.

---

## 6. Platform notes

### Linux

- Capture needs an **X11** session, or a Wayland session where `DISPLAY` points at XWayland. Under pure Wayland, XWayland can only see X11 windows.
- Audio uses the pure-Rust PulseAudio protocol client first, so it works with PipeWire (through `pipewire-pulse`) and PulseAudio and needs no libpulse. If no Pulse server is running, it falls back to ALSA (mic only: system audio needs the sound server).
- Mic and system-audio capture open native Pulse record streams. They ask for 20 ms fragments with the server's full queue, and the server converts to 48 kHz. Preview playback uses a 20 ms period (about 40 ms of latency), so narration lines up with the picture.
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
./build_screenreel_v1.4.0.sh native            # -> dist/screenreel_v1.4.0-linux-x86_64-glibc
./build_screenreel_v1.4.0.sh selftest dist/screenreel_v1.4.0-linux-x86_64-glibc
```

### Fully static Linux (musl)

```sh
./build_screenreel_v1.4.0.sh linux-static      # needs podman or docker; builds inside alpine:3.20
```

The script:
1. Installs Alpine's `*-static` X, fontconfig and freetype archives, building any missing X.Org library from its release tarball with `--enable-static`.
2. Builds alsa-lib statically.
3. Hides the `.so` dev symlinks inside the container. This forces fltk-sys's `dylib=X11…` requests to resolve to `.a` archives.
4. Links a non-PIE `+crt-static` binary, resolving the X/font/C++ stack as one linker group, and checks with `readelf` that there is no interpreter and no `NEEDED` entry.

### Windows

| Method | Command |
|---|---|
| Native (MSVC) | `set RUSTFLAGS=-C target-feature=+crt-static` then `cargo build --release`. Static CRT, so no VC++ redistributable is needed. |
| Cross-build from Linux with mingw-w64 | `./build_screenreel_v1.4.0.sh windows` |

CI (`.github/workflows/screenreel_v1.4.0.yml`) builds all three, runs the self-test under Xvfb, and attaches binaries to a GitHub release when you push a `v*` tag.

### Command line

```
screenreel [project_dir]                      open the editor (optionally with a project)
screenreel --selftest <outdir> [secs] [full] [sys]
                                              headless capture → edit → save/load → export → verify;
                                              `sys` also records system audio and requires it to be non-silent
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
audio capture (mic mono, system stereo):
  Linux: native Pulse record stream (pulse-rec thread, s16/48 kHz from the server)
  Windows/ALSA: cpal input stream (WASAPI loopback for system) ─► resample to 48 kHz
  └► Placer: chunks placed on the PauseClock's active time; gaps > 200 ms become silence
     └► on stop: wait for the last fragment, trim to the stop instant, fit to frames × (48000/fps)

UI thread (FLTK) ─ 60 Hz tick: playback clock, decoder results, export progress, REC status
preview decoder thread ─ latest-request-wins JPEG decode (zune-jpeg) + 2×2 averaged downscale
cpal output stream ─ real-time mixer (try_lock, never blocks); playhead = the sample being heard (queued position minus device delay)
export thread ─ Matroska muxer, cancellable, progress in permille
               v1.4.0: per 1 s cluster, frames with effects are rendered in parallel
               (std::thread::scope), all others byte-copied

v1.4.0 additions
audio sink thread (one per capture) ─ streaming LinRes resampler → live VoiceChain
   → raw spill (mic.raw / sys.raw) → in-memory track
input poller (125 Hz) ─ X11 QueryPointer + QueryKeymap / GetCursorPos + GetAsyncKeyState;
   clicks and keys are latched between frames → WMsg::Meta before each frame
camera thread ─ V4L2 mmap (Linux) / IMFSourceReader (Windows) → latest JPEG (Arc, id)
writer ─ frames.mjpg/.idx + .ev + .cam.* in lockstep, flushed about once a second
autosave thread ─ every 60 s when dirty; joined before Save
RenderCtx ─ output→source map, zoom table, overlay list; shared by preview decoder and export
```

**Edit model.** The edit is a `Vec<Seg>`, where `Seg` is `Src{start,len}`, `Hold{frame,len}` or `Fast{start,len,sp,ph}`, plus `Vec<Track{Arc<Vec<i16>>}>`.
- `Fast` maps output frame *k* to `start + (ph+k)·sp/100`. Splitting it at *a* gives `ph+a` to the right half, so cuts inside a sped-up range keep the exact frame mapping, and `normalize` merges the halves back.
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

### Audit notes (v1.1.0)

**Bugs found and fixed while adding system audio**
- **Linux mic capture through cpal's Pulse backend** had three problems:
  - With the default buffer, the server picked fragments of about 2 s. Audio arrived in bursts and the buffered tail (up to about 2 s) was lost at Stop.
  - With a fixed buffer, cpal caps the server queue at one fragment, so any late read silently dropped audio. A 2.5 s tone came back as about 1.75 s.
  - Fix: a native record stream (20 ms fragments, full server queue). The same test tone is now captured complete in every run.
- **Preview A/V sync:** Pulse playback buffered about 2 s, so preview audio lagged the picture by about 2 s. That broke narration timing. Fix: a 20 ms period, and the playhead now reports the sample being heard.
- **Gap placement:**
  - A loopback source sends nothing while the output is idle. Audio after a silent stretch would have slid earlier than the video.
  - Fix: chunks are placed on an active-time clock that excludes pauses, and real gaps become silence.
  - On Windows the capture timestamp comes from the WASAPI hardware clock (QPC). On Linux the native stream's 20 ms fragments arrive as they're captured.
  - Drift can't build up into false gaps, because placement re-anchors on every chunk.
- **Stop:** each source gets 80 ms to deliver its last fragment. Anything captured after the Stop instant is trimmed off.

**Security and robustness**
- The Pulse reader bounds packet size (16 MiB) and keeps frames aligned across packets.
- It exits on a socket error or a server "stream killed" message.
- Stop closes the socket, which unblocks the reader, and then joins it.
- The handshake has a 3 s timeout.
- Errors from either audio source never abort a recording. The source is dropped and the reason shown.

### Audit notes (v1.2.0)

**Root cause: MOV/MP4 unavailable on Windows**
- `ffmpeg_available()` only spawned `ffmpeg` from PATH, once, at startup. The buttons were then disabled for the whole session.
- Fixed with multi-location discovery, a remembered user choice and a dialog that explains what to do.
- ffmpeg runs from an absolute path with an argv array (no shell) and with `CREATE_NO_WINDOW` on Windows.

**Correctness**
- `Fast` frame math uses `u64` (`(ph+k)·sp` can't overflow). At load, every segment's highest source frame (`max_src`) is checked against the recording, so a hostile `project.txt` can't index outside `frames.idx`.
- Speeds are limited to 1.01×–64×, the phase to `u32::MAX/2`, the total length to `u32::MAX/2`, and the file to 2²⁰ segments.
- The WSOLA output length is exact (`frames × 48000/fps`), so audio and video stay in lock-step after a speed-up. This is asserted in `--selftest`.
- Dead-air results are tagged with an edit generation. Any edit, undo, mute or gain change makes them stale, and **Remove dead air** refuses stale ranges.
- Ranges are cut last-first, so earlier ranges keep their indices.
- **Open** had no unsaved-changes check, so opening a project silently discarded edits. **New** and **Open** now share a save / don't save / cancel guard, and both refuse while recording, narrating or running a background job.

**Performance**
- WSOLA uses a coarse-to-fine search: a step-4 decimated correlation, then a ±3 full-rate refine. 60 s of stereo compresses to 30 s in about 0.17 s.
- The dead-air scan decodes a JPEG only when the frame index entry actually changes. Unchanged screens are stored as repeated index entries, so idle stretches cost nothing. Pixels are compared on a 4-pixel grid against the still run's anchor frame.
- The timeline skips drawing off-screen segments.

**Robustness**
- Compaction writes new files under a new name, `fsync`s them, then saves `project.txt`. A crash at any point leaves a valid project, either old or new. Old media is deleted only after the save succeeds.
- Export, the dead-air scan and compaction all run as one cancellable background job with progress. Quitting asks before cancelling it.

### Audit notes (v1.3.0)

**Correctness**
- **Loudness meter:** checked against BS.1770 reference behaviour in `--selftest`. A stereo −20 dBFS 997 Hz tone reads −20.00 LUFS, and mono reads the same as its stereo equivalent. Silence is gated out (returns `None`, no divide-by-zero).
- **Limiter:** a sliding-window minimum gives O(n) look-ahead. A hard `min(env, need)` on every sample guarantees the ceiling: a +12 dB overdriven tone stays under −1 dBFS.
- **Compressor:** the steady-state gain matches the static curve (thr −20 / 4:1 on a −6 dBFS tone gives −16.6 dBFS out, theory −16.5). The tube stage is unity at low level and DC-free. Parameters are clamped and NaN-safe (`sanitized`).
- **Hard clipping removed:** `clamp(-1,1)` in playback, MKV and WAV export was replaced by a smooth −1 dBFS ceiling.
- **UI:** `/` in menu item text is FLTK's submenu separator, so the dialogs avoid it.

**Concurrency and UX**
- **Audition** goes through the existing lock-free mixer handoff (`try_lock` on the audio thread). The override is dropped on Cancel, Apply, New and Open, so a preview can't leak into an export.
- **Re-processing** while auditioning is debounced by 0.3 s, with a generation counter, so slider drags don't queue work.
- **Level meter:** the audio thread publishes peaks with `fetch_max` on the f32 bit pattern (valid for non-negative floats). The UI applies ballistics at 60 Hz. No locks on the real-time path.

**Performance**
- The compressor is O(n), with one `log10` and one `powf` per frame: 3 s of audio takes about 6 ms, and 10 minutes about 1 s.
- Loudness measurement is O(n) with two biquads per channel.

### Audit notes (v1.3.1)

This release is a full line-by-line audit of v1.3.0. Every change is marked `// v1.3.1 FIX` or `// v1.3.1 PERF` in the source.

**Crashes**
- **Ctrl+wheel zoom on very short clips:** under about 9 frames, the minimum zoom exceeded the 64× maximum, and `f64::clamp(min > max)` panicked. The minimum is now capped at the maximum.
- **Mutex poisoning:** capture, encoder, writer, decoder and mixer locks use `unwrap_or_else(into_inner)`. A panic in one worker no longer cascades into the UI thread.

**Data safety**
- **Save ordering:** v1.3.0 deleted stale tracks and overwrote WAVs before replacing `project.txt`, so a failure in between left a project that wouldn't open. The new order is: stage, then swap, then clean up (see §5).
- **Quitting during narration** now finalizes the take before the save prompt. Before, it was silently dropped.
- **Quitting during a job** cancels it and waits (up to 5 s) so the job can remove partial output and stop ffmpeg.
- **Compact** records the edit generation it started from. If the project changed meanwhile, the new media is discarded instead of being paired with a different edit, which would desync the audio.
- Exports can't overwrite the project's own files. `.part` and `.tmp` files are removed on every error path. A failed ffmpeg run removes its truncated output.
- Load rejects `NaN` and `inf` gains (`f32::clamp` passes NaN through, which turned the mix into NaN) and caps the number of tracks.

**Correctness**
- **`decode_audio` track pick:** `a && b || c` meant a NULL-codec stream (subtitle or attachment) with a sample rate could be chosen over real audio. It is now parenthesized.
- **`decode_audio` sample buffer:** it is rebuilt when the stream spec changes mid-file (chained OGG, broadcast MP3), which avoids a panic or mis-interleaved frames.
- **After a new recording,** the app resets its state like New/Open (`set_project`). Before, dead-air ranges from the previous project stayed "valid" and could be cut from the new recording.
- **Narration:** the mic is opened before the project is modified. A mic failure no longer leaves an empty "Narration N" track, and a take is one undo step instead of two.
- **Time labels past one hour** use `H:MM:SS.cc`. The ruler no longer slices labels at a fixed byte offset.

**Efficiency**
- Save skips unchanged tracks. It tracks them with `Weak` pointers, which hold no audio in memory and can't hit ABA pointer reuse.
- WAV export uses O(1) memory: it streams 1 s blocks with progress and cancel. Before, the whole mix was built in RAM (about 1.4 GB for 2 hours).
- The dead-air scan tests audio first and decodes JPEGs only during silent stretches.
- ffmpeg progress is parsed from `-progress pipe:1`. Its pipes are drained on helper threads, so there's no pipe-buffer deadlock.

**Code quality:** `cargo clippy` reports zero warnings. The MSRV-breaking `iter::repeat_n` (needs Rust 1.82, `rust-version` is 1.79) was replaced. `--selftest` adds checks for: save skipping unchanged tracks and stale cleanup, NaN-gain rejection, cleanup after a cancelled WAV or MKV export, cancelling ffmpeg, MOV remux progress, and hour time formatting.

### Audit notes (v1.4.0)

**Robustness and data safety**
- **Crash recovery:** v1.3 kept recorded audio only in memory until Stop, so a crash lost all of it and the video index tail could be torn. Audio is now written to raw spill files as it arrives, all media files are flushed about once a second (data before index), and `rec.txt` marks an unfinished recording. Recovery trims every index to whole entries that point inside the data file, drops a half-written last JPEG, fits audio to the surviving frames and saves a normal project. Tested in `--selftest` with a torn JPEG and a partial index entry.
- **Autosave** never writes `project.txt` or `track_N.wav`, so a crash during autosave can't damage the real save. Tracks unchanged since the last save are referenced, not rewritten. Save joins a running autosave first, then removes the autosave files.
- **Undo memory:** v1.3 kept 64 snapshots regardless of size. An edit on a 1-hour track copies about 345 MB, so 64 steps could exhaust RAM. History is now also capped at 1.5 GiB of distinct audio (shared buffers counted once). Storing undo as diffs was considered and rejected for this release: every ripple edit touches every track from the edit point onwards, so a diff is about as large as the track tail it shifts.

**Correctness**
- **Resampling:** v1.3 buffered a whole take at the device rate and resampled at Stop (a CPU and memory spike at the worst moment). A streaming linear resampler now runs in the sink thread; the result is tested for length (±2 samples per second) and frequency.
- **Voice chain alignment:** RNNoise has one frame (10 ms) of latency; the chain trims it so the processed track stays aligned (selftest: best cross-correlation lag 0).
- **Annotation parsing** is strict: unknown kinds, `NaN`, reversed spans, out-of-range geometry (beyond −0.5…1.5 of the frame) and bad colours are rejected. Text is escaped. At most 10,000 annotations are loaded; spans are clamped to the edit length on load.
- **Ripple:** cut, insert freeze and speed-up remap annotation times through the same mapping as the video, so an annotation stays on the frames it was drawn on.
- **Clicks on repeated frames:** the screen writer stores an unchanged screen as a repeated index entry. Click detection only counts the first output frame of a source frame, so held or frozen frames don't replay the same click ring, and Compact clears click/key flags on repeated frames.
- **Drawing in the letterbox:** an annotation dragged partly outside the video is clipped to the frame; one drawn entirely outside is discarded.

**Security and privacy**
- **Typing detection** records one bit per frame, "a non-modifier key went down". Key codes are never stored, so `frames.ev` can't be used to reconstruct what was typed. The keymap query masks modifier keycodes.
- Camera buffers are bounds-checked: a V4L2 MJPEG payload is clamped to its mapped kernel buffer, raw frames must match the negotiated size, and dimensions are capped at 8192 (cameras wider than 960 px are halved). The camera runs on its own thread and the recorder only reads the latest frame, so a camera that stalls can never stall the screen capture. A camera that doesn't deliver a first frame within 8 s is skipped and the recording continues without it.
- GPU encoder probes run ffmpeg with an argv array (no shell), stdin closed, and a hard 10 s kill deadline. `SCREENREEL_ENCODER` only accepts known encoder names.

**Performance**
- Effects render only on affected frames: in testing, 50 of 91 frames were rendered for a mix of text, arrow, blur, pixelate, step and zoom (about 11 ms per 640×360 frame, parallel across cores); the rest stayed byte copies.
- The live voice chain processes 60 s of mono audio in 0.53 s (about 113× real time).
- Normalize and Compressor **Apply** now run as background jobs with progress (Compressor reuses an up-to-date audition result instantly).
- Waveform peaks take about 14 ms per 10 minutes of stereo audio (about 85 ms per hour). That's as fast as reading a cache file, so peaks are not cached on disk.
- Audio-device error callbacks (which can fire hundreds of times a second after a device disappears) are rate-limited in the log.

### Known limitations and roadmap

- MJPEG files are larger than H.264, roughly 2–8 MB/s at 1080p30 depending on content and preset. Use Half-res or Draft for long recordings, and MP4 export for sharing.
- Audio tracks are held in RAM: about 5.7 MB per minute for a mono track.
- Live voice FX is applied to the mic as it is recorded; there is no raw copy of that take. Leave it off and use **Voice FX…** afterwards if you want to keep the original.
- Webcam background blur is a fixed soft oval, not AI person segmentation.
- The webcam overlay and cursor highlight make every frame re-encode at export (see §1). Annotations, click rings and zoom re-encode only the frames they touch.
- Undo snapshots share unchanged audio, but an edit to a long track copies that track. History is therefore capped by memory (1.5 GiB of distinct audio) as well as by 64 steps.
- Speeding up and dead-air removal operate on whole video frames (1/fps). The audio is cut on the same boundaries.
- Compact project flattens the edit (it becomes one segment), and undo history is cleared.
- System audio records the default output only. Choosing a specific output, or a single app's audio, is not supported yet.
- The Windows webcam and GPU encoder paths are compile-checked in CI but were not runtime-tested on Windows hardware for this release.
- Planned: Wayland capture through the xdg-desktop-portal/PipeWire screencast, global hotkeys, per-app audio capture, and person segmentation for the webcam blur.

---

## 9. Changelog

**1.4.0** (2026-11-28):
- **Live voice FX while recording:** noise reduction (RNNoise), noise gate, voice EQ presets, de-esser and opto compressor on the mic as you record; the same chain as **Voice FX…** for any track
- **Auto-duck:** system audio or music dips automatically under your voice
- **Cursor effects and Auto zoom:** click rings, cursor highlight, and smooth zoom and pan that follows clicks and typing (key identity never recorded)
- **Webcam picture-in-picture:** circle, rounded or square, any corner, adjustable size, background blur, mirror (V4L2 on Linux, Media Foundation on Windows)
- **Annotations:** text callouts, arrows, blur and pixelate boxes, numbered steps and zoom regions, timed on the timeline and undoable
- **GPU MP4 export:** NVENC, Quick Sync, AMF or VAAPI when available, with libx264 fallback and automatic retry
- **Autosave** every 60 s with restore on open; **crash recovery** for interrupted recordings
- Normalize and Compressor run in the background with progress; undo capped by memory; streaming resampler; window fits small screens
- `--selftest` adds webcam and input streams, overlays (round trip, ripple, rendering, pixelate), cursor and webcam effects, auto zoom, effects export checked with ffmpeg, MP4 encode frame count, autosave and crash recovery

**1.3.1** (2026-11-28), an audit release (details under *Audit notes (v1.3.1)*):
- Fix: crash on Ctrl+wheel zoom over very short clips
- Fix: an interrupted Save could leave a project that wouldn't open. Save is now crash-safe and skips unchanged tracks
- Fix: narration lost when quitting mid-take. Background jobs are cancelled and cleaned up on quit
- Fix: Compact applied to an edit that had changed meanwhile could desync audio. It is now discarded safely
- Fix: stale dead-air results from the previous project stayed usable after a new recording
- Fix: audio import could pick a non-audio stream, or panic on files whose format changes mid-stream
- Fix: a failed mic start left an empty narration track. A narration take is now one undo step
- Fix: exports leave no partial files on error, can't overwrite project files, and reject NaN gains on load
- MOV/MP4: ffmpeg phase shows progress and is cancellable. WAV export streams to disk with progress and cancel
- Faster dead-air scan. Time labels past one hour. Zero clippy warnings, and MSRV 1.79 is honoured again

**1.3.0** (2026-11-27):
- **Opto compressor** (warm CL 1B-style) for the mic or any track: soft-knee optical cell with program-dependent release and a tube warmth stage; 5 presets, live transfer curve, A/B audition on the video, one-step apply
- **Normalize levels**: BS.1770 gated LUFS normalization of all or the selected track to −14/−16/−19/−23 LUFS, with a −1 dBFS look-ahead limiter
- **Output level meter** with peak hold; **soft −1 dBFS ceiling** instead of hard clipping in playback and exports
- **New UI**: every control moved into one dark, grouped dock under the preview (RECORD · PROJECT/EXPORT · PLAY/EDIT · TRACK · TOOLS); fixed widths (no stretched buttons on wide screens); colour-coded roles; tooltips with shortcuts
- `--selftest` adds loudness, limiter-ceiling, compressor gain and tube-stage checks

**1.2.0** (2026-11-27):
- Fix: **MOV/MP4 export greyed out** when ffmpeg wasn't on PATH (typical on Windows). ScreenReel now searches common install locations, remembers **Locate ffmpeg…**, honours `SCREENREEL_FFMPEG` and explains how to install ffmpeg
- **New** project button and **Ctrl+N**, with an unsaved-changes prompt (also added to **Open…**, which previously discarded edits)
- **Double-tap Space** jumps back to 0:00
- **Ctrl+X** cuts the selection
- **Find dead air… / Remove dead air (N)**: detects silent, static stretches; one-step removal
- **Compact project…**: lossless rewrite of the recording to reclaim disk space
- **Speed up selection** (1.25×–16×, stackable to 64×): lossless frame-skipping video, pitch-preserving (WSOLA) or muted audio, green "N×" segments; new `seg=F` and `media=` project keys
- `--selftest` adds WSOLA pitch/length checks, speed-up, split invariance, dead-air detection and compaction byte-equality

**1.1.0** (2026-11-27):
- **System audio** checkbox: records the computer's sound output to its own stereo **System** track, next to the mic (WASAPI loopback on Windows, default-sink monitor on PulseAudio/PipeWire on Linux)
- Linux mic and system capture use a native Pulse record stream: no more ~2 s bursts, dropped audio or lost tail
- Audio placement survives idle output and pause/resume; the end is trimmed to the exact stop time
- Preview playback latency on Linux cut from ~2 s to ~40 ms; the playhead follows the audible sample
- `--selftest … sys`, plus a CI system-audio test on a private PulseAudio null sink
- Static musl build fixed (compiler aliases, linker group, non-PIE static link)

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
