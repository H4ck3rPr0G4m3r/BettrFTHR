# BettrFTHR

A small, reliable, local-first game clipper for Windows. Press a hotkey, get the last
N seconds as an in-sync MP4. Trim it, mute your mic, drop it in Discord.

Inspired by [FTHR Clips](https://github.com/FTHR-Community/FTHR-Clips), rebuilt from scratch
with a Rust capture engine and a focus on stability over features. See
[`docs/design.md`](docs/design.md).

## Status

- **Engine**: replay clips, manual recording, any number of audio sources (one track each
  plus a mix), crash recovery. Soak-tested.
- **App**: library grouped by game and day with multi-select delete, clip view with a
  precise timeline and a filmstrip of the other clips, editor (trim, mute or turn down each
  audio source, mute sections), copy for Discord that compresses to your upload limit,
  settings (length, fps, resolution, quality, hotkeys incl. F13-F24, audio sources with live
  level meters), a storage limit that compresses the oldest clips and then moves the oldest to
  the Recycle Bin (opt-in, Settings > Storage),
  game detection, tray, installer, start with Windows.

## Running the app

```sh
cd app && npm install
npx tauri dev      # development (builds the engine first)
npx tauri build    # installer: target/release/bundle/nsis/BettrFTHR_<version>_x64-setup.exe
```

The installer is per-user (no admin prompt) and bundles the engine next to the app. An
installed copy starts with Windows, hidden in the tray (Settings > Behavior); development
builds never register themselves.

Settings live in `%APPDATA%\dev.bettrfthr.app\settings.json`, the engine log in
`%LOCALAPPDATA%\dev.bettrfthr.app\engine.log`. Clips default to `Videos\BettrFTHR`, one
folder per detected game.

For UI checks without touching the mouse, start the dev app with
`WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS=--remote-debugging-port=9222` and drive it with
`node tools/cdp.mjs eval|shot|call ...`.

## Using the engine directly

`target/release/bettr-engine.exe` runs headless. While it runs:

| Key | Action |
|-----|--------|
| **F9** | Save the last 30 s to `./clips` |
| **F10** | Start / stop a recording |

Useful flags:

```sh
bettr-engine --seconds 60 --out D:\Clips          # longer replay, other folder
bettr-engine --height 1080 --bitrate 25000         # downscale 1440p → 1080p, 25 Mbps
bettr-engine --list-audio                          # show audio devices
bettr-engine --meter "front panel"                 # 6 s of level + timestamp health for one mic
bettr-engine --output default --input "Shure" --input "Line 1@0.7"   # several mics, per-source gain
bettr-engine --config settings.json                # everything from a JSON file (see main.rs)
```

While it runs you can also type commands into the console: `save` (or `save 15` for the last
15 s), `record`, `status`, `list-audio`, `game VALORANT` (clips then go to `clips/VALORANT/`),
`help`, `quit`. The app sends the same commands as JSON lines (`{"cmd":"save","seconds":15}`)
and reads events back as JSON lines on stdout. Logs go to stderr (`RUST_LOG=debug` for more).

Each clip contains the video, a **Mix** audio track (what players and Discord play) and
one extra track per audio source, so the editor can mute or rebalance each one.

## Testing

```sh
cargo test
python tools/soak.py --minutes 30          # saves clips on a loop and validates every one
```

`soak.py` expects ffmpeg/ffprobe in `.tools/ffmpeg-master-latest-win64-gpl/bin` (or pass
`--ffmpeg-dir`).

## License

GPL-3.0-only, see [`LICENSE`](LICENSE). Portions are informed by FTHR Clips (GPL-3.0).
