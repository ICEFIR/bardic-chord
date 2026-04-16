# Bardic Chord

Bardic Chord is an open-source, cross-platform Rust desktop experiment for routing Spotify playback into Discord voice. The current app uses a native Slint shell and a guided setup flow built around local desktop-audio capture instead of a Spotify Connect receiver.

## Current State

- native Slint desktop UI with a four-step wizard: `Welcome`, `Discord`, `Desktop Audio`, `Launch`
- local settings and logs stored under `./.bardic-chord/` in the current working directory
- Discord validation and voice relay runtime backed by `serenity` and `songbird`
- Linux desktop-audio backend that creates a local null sink with `pactl` and captures it with `parec`
- live PCM bridge from that local Bardic Chord output into Discord voice

For the current POC, the Discord bot token is stored in Bardic Chord's local config file on the user's machine. It is not hard-coded into the binary, and it is not using OS keychain storage yet.

## Why The Flow Changed

The previous experiment relied on `librespot` as a Spotify Connect receiver. That path is no longer the active product direction for this repo.

The current repo now prefers:

1. creating a local Bardic Chord audio output
2. routing Spotify desktop playback into that output at the OS level
3. capturing that output locally
4. relaying the PCM stream into Discord voice

This keeps the user flow aligned with the longer-term Windows loopback plan while avoiding the unstable Spotify Connect receiver path.

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
4. Prepare the Bardic Chord desktop audio output.
5. In the system sound settings, route Spotify to that Bardic Chord output.
6. Start the party so Bardic Chord joins the voice channel and forwards the local audio stream.

## Linux Backend

The current implemented capture backend is Linux-first:

- create a local null sink with `pactl load-module module-null-sink`
- capture the sink's monitor stream with `parec`
- convert `s16le` stereo samples to float PCM
- feed that PCM into Songbird's raw input adapter

This gives Linux a dedicated Bardic Chord output that can be selected from PipeWire or PulseAudio-compatible sound settings. The current Linux flow also attempts to move Spotify into that output automatically when the stream is visible.

## Windows And macOS

The app UX now targets a shared local-output / loopback model, but only the Linux backend is implemented in this repo today.

Planned direction:

- Windows: WASAPI loopback or app-session loopback behind the same guided flow
- macOS: a native capture backend that fits the same UI, if the experiment needs it later

The backend shape is now centered on local audio capture, so adding those platform runtimes later should not require another UX reset.

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

## Build Targets

The repo is still set up to target these desktop binaries:

- `x86_64-unknown-linux-gnu`
- `aarch64-unknown-linux-gnu`
- `x86_64-pc-windows-msvc`
- `aarch64-pc-windows-msvc`
- `x86_64-apple-darwin`
- `aarch64-apple-darwin`

Windows targets are configured to use a static CRT via `.cargo/config.toml`.

That does not mean every desktop target is fully self-contained:

- Windows can be pushed closer to a single self-contained `.exe`
- Linux still depends on native user-space audio and windowing stacks
- macOS still has normal platform-native desktop linkage constraints

## License

This repository is released under the MIT License. See `LICENSE`.

## CI/CD

GitHub Actions workflows are intentionally disabled for now. Build and release automation can be reintroduced later once the public packaging story is stable.

## Product Direction

- keep the setup local-first and simple
- keep the UX guided and explicit
- keep the backend Rust-first
- prefer local audio capture over fragile Spotify Connect receiver workarounds
