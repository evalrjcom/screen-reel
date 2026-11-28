# screen-reel — ScreenReel v1.4.0

A lightweight, ScreenPal-style screen recorder and editor written in Rust and FLTK. It builds as a single binary for Linux and Windows.

- Records the full screen or a region, with the mouse cursor, the microphone and, optionally, system audio (what the computer plays) on its own track.
- A visual timeline lets you cut, insert freezes, freeze a selection, speed up a range (pitch-preserving audio), and re-record narration over any part.
- Mic polish: a warm CL 1B-style opto compressor and one-click loudness normalization (LUFS) so every track sits at the same level.
- **New in v1.4.0:** live voice FX while recording (noise reduction, gate, EQ, de-esser, compressor), auto-ducking, webcam picture-in-picture with background blur, click rings and cursor highlight, auto zoom that follows clicks and typing, annotations (text, arrows, blur/pixelate, steps, zoom), GPU MP4 export, autosave and crash recovery.
- Finds and removes dead air (silent, unchanging stretches), and compacts a project to reclaim disk space.
- Video is stored as MJPEG, so edits, audio-track swaps and MKV export never re-encode the video. Nothing is lost in quality, and export runs at disk speed.
- Exports to MKV (instant), MOV (lossless remux) or MP4 (H.264, GPU-accelerated when available). MOV and MP4 need ffmpeg, which is found automatically in common install locations or can be located once from the app.

📖 Full user and developer guide: [README_screenreel_v1.4.0.md](README_screenreel_v1.4.0.md)

```sh
./build_screenreel_v1.4.0.sh native         # portable glibc build
./build_screenreel_v1.4.0.sh linux-static   # fully static musl build (podman/docker)
./build_screenreel_v1.4.0.sh windows        # static .exe (mingw-w64 cross)
```

CI (`.github/workflows/screenreel_v1.4.0.yml`) builds the static Linux binary and the Windows `.exe`. Pushing a `v*` tag publishes them as a GitHub release.
