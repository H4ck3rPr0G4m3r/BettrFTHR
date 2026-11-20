# BettrFTHR

A small, reliable, local-first game clipper for Windows. Press a hotkey, get the last
N seconds as an in-sync MP4. Trim it, mute your mic, drop it in Discord.

Inspired by [FTHR Clips](https://github.com/FTHR-Community/FTHR-Clips), rebuilt from scratch
with a Rust capture engine and a focus on stability over features. See
[`docs/design.md`](docs/design.md).

## Status

Early development. The engine is being built and tested first.

## Building

Requirements: Windows 10 1903+ / 11, Rust (stable, MSVC toolchain).

```sh
cargo build --release
# replay-buffer test harness: F9 saves the last 30 s to ./clips
target/release/bettr-engine.exe --cli
```

## License

GPL-3.0-only, see [`LICENSE`](LICENSE). Portions are informed by FTHR Clips (GPL-3.0).
