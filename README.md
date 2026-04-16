# Bardic Chord

Bardic Chord is an open-source, cross-platform Rust desktop experiment for routing desktop app audio into Discord voice. The current app uses a native Slint shell and a guided setup flow built around local desktop-audio capture instead of a Spotify Connect receiver.

## Current State

- native Slint desktop UI with a four-step wizard: `Welcome`, `Discord`, `Desktop Audio`, `Launch`
- local settings and logs stored under `./.bardic-chord/` in the current working directory
- Discord validation and voice relay runtime backed by `serenity` and `songbird`
- Linux desktop-audio backend that creates a local null sink with `pactl` and captures it with `parec`
- Windows desktop-audio backend that captures the selected app directly through WASAPI process loopback
- live PCM bridge from that local Bardic Chord output into Discord voice

For the current POC, the Discord bot token is stored in Bardic Chord's local config file on the user's machine. It is not hard-coded into the binary, and it is not using OS keychain storage yet.

## Why The Flow Changed

The previous experiment relied on `librespot` as a Spotify Connect receiver. That path is no longer the active product direction for this repo.

The current repo now prefers a local capture path:

1. preparing the desktop audio path
2. capturing the selected app locally
3. relaying the PCM stream into Discord voice

On Linux that means a dedicated local output plus monitor capture. On Windows that means direct process loopback for the selected app process. This keeps the UX aligned while avoiding the unstable Spotify Connect receiver path.

## Repo Layout

- `Cargo.toml`
  - workspace root
- `desktop/Cargo.toml`
  - native app crate
- `desktop/src/backend.rs`
  - Discord, local audio-output runtime, config, and relay orchestration
- `desktop/src/lib.rs`
  - Slint controller wiring
- `desktop/ui/app.slint`
  - guided desktop UI

## Current Flow

1. Paste the Discord bot token.
2. Open the generated Discord authorize page if the bot is not in the server yet.
3. Choose the Discord server and voice channel.
4. Choose the app you want to capture.
5. Prepare desktop audio.
6. On Linux, route that app to the Bardic Chord output if needed. On Windows, keep the app open so loopback capture can attach to it.
7. Start the party so Bardic Chord joins the voice channel and forwards the local audio stream.

## Linux Backend

The current implemented capture backend is Linux-first:

- create a local null sink with `pactl load-module module-null-sink`
- capture the sink's monitor stream with `parec`
- convert `s16le` stereo samples to float PCM
- feed that PCM into Songbird's raw input adapter

This gives Linux a dedicated Bardic Chord output that can be selected from PipeWire or PulseAudio-compatible sound settings. The current Linux flow also attempts to move the selected app into that output automatically when the stream is visible.
The selected app target is used to decide which stream Bardic Chord should try to move automatically.

## Windows Backend

The Windows path now uses WASAPI application loopback capture against the selected target process.

- Bardic Chord looks for the configured app process on prepare
- it opens a per-process loopback client instead of creating a virtual output device
- the captured float PCM is forwarded into the same Songbird relay path as Linux

Current caveat:

- the Windows backend is implemented and cross-builds from Linux now work through `cargo-zigbuild`, but the runtime still needs real Windows-side smoke testing before every public release

## macOS

macOS still needs a native capture backend that fits the same UI if the experiment needs it later.

## Discord Bot Notes

- bot permissions integer: `3146752`
- current required permissions: `View Channels`, `Connect`, `Speak`

## Development

Check the workspace:

```bash
cargo check
```

Run the app:

```bash
cargo run -p bardic-chord
```

Run unit tests:

```bash
cargo test -p bardic-chord --lib
```

Format the crate:

```bash
cargo fmt --all
```

Build the supported Linux and Windows release targets with one command:

```bash
cargo xtask release
```

Build only one release target:

```bash
cargo xtask release --target linux
cargo xtask release --target windows
```

Prerequisites:

```bash
rustup target add x86_64-pc-windows-gnu
cargo install --locked cargo-zigbuild
```

Current packaged release targets in this repo:

- `x86_64-unknown-linux-gnu`
- `x86_64-pc-windows-gnu`

Why this matrix is smaller:

- Linux x86_64 is verified locally on this machine.
- Windows x86_64 uses the GNU target because it cross-builds cleanly from Linux with `cargo-zigbuild`.
- Linux ARM64 can still be added later through a native ARM64 Linux runner.

Built artifacts are written to `dist/`.

Release tagging:

- push code to `main` to run CI
- run the GitHub Actions workflow `Tag Release` with a version like `v0.1.0`, or push a `v*` tag manually
- the `Release` workflow will build both archives and publish them to the GitHub release for that tag

## Build Targets

The repo is still set up to target these desktop binaries:

- `x86_64-unknown-linux-gnu`
- `x86_64-pc-windows-gnu`
- `x86_64-apple-darwin`
- `aarch64-apple-darwin`

Windows targets are configured to use a static CRT via `.cargo/config.toml`.

That does not mean every desktop target is fully self-contained:

- Windows can be pushed closer to a single self-contained `.exe`
- Linux still depends on native user-space audio and windowing stacks
- macOS still has normal platform-native desktop linkage constraints

## License

This repository is released under the MIT License. See `LICENSE`.

## Acknowledgements

Bardic Chord builds on and learns from several open-source projects. Thanks to their maintainers and contributors.

- `slint`
  - native desktop UI runtime used for the app shell and guided setup flow
- `serenity`
  - Discord API and gateway client
- `songbird`
  - Discord voice transport and audio playback runtime
- `tokio`
  - async runtime used throughout the app
- `reqwest`
  - HTTP client for Discord API validation
- `rustls`
  - TLS backend used by the network stack
- `symphonia`
  - PCM media/input support used in the relay path
- `wasapi`
  - Windows process loopback capture backend
- `librespot`
  - earlier experiments and product direction research around Spotify playback handling
- `aoede`
  - earlier reference point while exploring Spotify-to-Discord relay patterns
- `Spytify`
  - useful reference for the Windows direction around isolating Spotify audio specifically: https://github.com/spytify/spytify

## CI/CD

GitHub Actions now covers the basic repo lifecycle:

- `CI`
  - runs on pushes to `main` and on pull requests
  - checks formatting, builds the workspace, runs desktop unit tests, and packages Linux and Windows release archives
- `Tag Release`
  - manual workflow used to create an annotated `v*` tag from GitHub
- `Release`
  - runs on `v*` tag pushes, rebuilds both release archives, uploads workflow artifacts, and publishes the GitHub release assets

## Product Direction

- keep the setup local-first and simple
- keep the UX guided and explicit
- keep the backend Rust-first
- prefer local audio capture over fragile Spotify Connect receiver workarounds
