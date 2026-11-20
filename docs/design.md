# BettrFTHR — design

## Why

FTHR Clips has the right idea — an open-source, local-first Medal replacement — but the
Windows build is unreliable (crashes, missed clips in Valorant, A/V drift) and has grown to
~140k lines of source chasing features most users never touch. BettrFTHR keeps the idea
and rebuilds it small and solid.

**Success:** you press the hotkey in any game and a correct, in-sync clip is on disk. Every
time. Then you trim it, mute your mic where needed, and drop it into Discord.

## Goals

- Instant replay clip on a hotkey (last N seconds, configurable).
- Manual recording (start/stop).
- Automatic game detection (clips are tagged and grouped by game).
- Light editing: trim, mute/adjust the mic track per section.
- One-click Discord export (fits the size limit, copied to clipboard for Ctrl+V).
- A polished, fast UI.

## Non-goals

Uploader/cloud, plugins, themes marketplace, albums, trash, webcam/keyboard overlays,
full NLE editor, Linux. (Can be revisited only once the core is boringly reliable.)

## Anti-goals

- A feature that ships while capture is flaky.
- Silent failure: if a clip can't be saved, the user is told why.
- Unbounded growth: target ≈15–20k lines total.

## Approaches considered

| | Approach | Trade-off |
|---|---|---|
| A | Keep FTHR's C++ engine, new UI | Fast start, inherits an unproven 36k-line engine |
| B | Fix FTHR in place | Inherits 25k-line `main.py` and unexplained native Qt crashes |
| C | New UI on OBS's libobs | Proven capture, heavy build, Vanguard may block a custom game-capture hook |
| **✔** | **A, adjusted: new Rust engine + Tauri UI** | Most work up front, but one small codebase we fully understand; FTHR's C++ remains a reference (GPL-3) |

## Architecture

```
┌──────────────── BettrFTHR app (Tauri, Rust + web UI) ───────────────┐
│ tray · library · editor · settings · game detection · supervision   │
└───────────────┬─────────────────────────────────────────────────────┘
                │ JSON lines over stdin/stdout (commands ↓, events ↑)
┌───────────────▼──────────── bettr-engine.exe (Rust) ────────────────┐
│ WGC capture ─► GPU BGRA→NV12/scale (D3D11 VideoProcessor)           │
│   ─► Media Foundation HW H.264 encoder (NVENC/AMF/QSV via MFT)      │
│   ─► encoded ring buffer ─┐                                         │
│ WASAPI loopback + mic ─► clock-locked timeline ─► AAC ─► ring ─┴► MP4 │
│ global hotkeys (RegisterHotKey)                                     │
└─────────────────────────────────────────────────────────────────────┘
```

Key decisions (and why):

- **Separate engine process.** If the engine dies, the app restarts it and the UI survives;
  hotkeys live in the engine so a hung UI never costs a clip.
- **Frames never leave the GPU.** Capture texture → video processor → hardware encoder.
- **Constant frame rate tick** (OBS-style): the latest captured frame is encoded every tick,
  duplicated when the screen is static. Fixed GOP → precise, predictable clip trimming.
- **No B-frames** → PTS == DTS, trivial muxing.
- **One clock (QPC) for everything.** Video ticks and audio packets are stamped on QPC.
  Audio is written to a continuous timeline: gaps (WASAPI loopback goes quiet when nothing
  plays) become silence, drift is corrected against QPC. This is where most clippers drift.
- **Own MP4 muxer, no edit lists.** Both tracks start at 0, audio aligned to the first video
  keyframe. Edit lists were the source of FTHR's player-dependent offsets.
- **Atomic saves:** write `*.tmp`, then rename.
- **Device on the monitor's adapter**, encoder chosen by adapter LUID (hybrid GPU safe).

## Milestones

1. **M1 — Video replay:** monitor capture, HW encode, ring buffer, MP4 save on hotkey (CLI).
2. **M2 — Audio:** system loopback + mic as separate AAC tracks, clock-locked.
3. **M3 — Robustness:** resolution/monitor/device-lost recovery, logs, soak-test tool that
   saves clips for hours and validates each one (decode + A/V offset).
4. **M4 — Manual recording:** streamed fragmented MP4 (crash-safe).
5. **M5 — App:** Tauri shell, tray, engine supervision, settings, game detection, library.
6. **M6 — Editor:** trim, mic sections, Discord export (bundled ffmpeg CLI).
7. **M7 — Ship:** installer, autostart, polish.

Gate: M1–M3 must pass a multi-hour soak and a Valorant session before UI work starts in
earnest.

## Test hardware

Ryzen 7 9800X3D (Radeon iGPU) + RTX 3060 Ti driving the display. NVIDIA path first; the iGPU
keeps us honest about adapter selection.
