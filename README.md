# screen-reel — ScreenReel v1.3.1

A lightweight, ScreenPal-style screen recorder and editor written in Rust and FLTK. It builds as a single binary for Linux and Windows.

- Records the full screen or a region, with the mouse cursor, the microphone and, optionally, system audio (what the computer plays) on its own track.
- A visual timeline lets you cut, insert freezes, freeze a selection, speed up a range (pitch-preserving audio), and re-record narration over any part.
- Mic polish: a warm CL 1B-style opto compressor and one-click loudness normalization (LUFS) so every track sits at the same level.
- Finds and removes dead air (silent, unchanging stretches), and compacts a project to reclaim disk space.
- Video is stored as MJPEG, so edits, audio-track swaps and MKV export never re-encode the video. Nothing is lost in quality, and export runs at disk speed.
- Exports to MKV (instant), MOV (lossless remux) or MP4 (H.264). MOV and MP4 need ffmpeg, which is found automatically in common install locations or can be located once from the app.

📖 Full user and developer guide: [README_screenreel_v1.3.1.md](README_screenreel_v1.3.1.md)

```sh
./build_screenreel_v1.3.1.sh native         # portable glibc build
./build_screenreel_v1.3.1.sh linux-static   # fully static musl build (podman/docker)
./build_screenreel_v1.3.1.sh windows        # static .exe (mingw-w64 cross)
```

CI (`.github/workflows/screenreel_v1.3.1.yml`) builds the static Linux binary and the Windows `.exe`. Pushing a `v*` tag publishes them as a GitHub release.
