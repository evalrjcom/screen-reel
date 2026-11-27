# screen-reel — ScreenReel v1.0.0

A lightweight, ScreenPal-style screen recorder and editor written in Rust and FLTK. It builds as a single binary for Linux and Windows.

- Records the full screen or a region, with the mouse cursor and the microphone.
- A visual timeline lets you cut, insert freezes, freeze a selection, and re-record narration over any part.
- Video is stored as MJPEG, so edits, audio-track swaps and MKV export never re-encode the video. Nothing is lost in quality, and export runs at disk speed.
- Exports to MKV (instant), MOV (lossless remux) or MP4 (H.264). MOV and MP4 need ffmpeg.

📖 Full user and developer guide: [README_screenreel_v1.0.0.md](README_screenreel_v1.0.0.md)

```sh
./build_screenreel_v1.0.0.sh native         # portable glibc build
./build_screenreel_v1.0.0.sh linux-static   # fully static musl build (podman/docker)
./build_screenreel_v1.0.0.sh windows        # static .exe (mingw-w64 cross)
```

CI (`.github/workflows/screenreel_v1.0.0.yml`) builds the static Linux binary and the Windows `.exe`. Pushing a `v*` tag publishes them as a GitHub release.
