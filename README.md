# Bardic Chord

<p align="center">
  <img src="./banner.png" alt="Bardic Chord banner" width="900">
</p>

<p align="center">
  <strong>Route desktop audio into Discord voice with a guided local-first ritual.</strong>
</p>

<p align="center">
  Bardic Chord is a Rust desktop app with a native Slint UI that helps a user connect a Discord bot,
  choose an app to capture, and relay that audio into a Discord voice channel.
</p>

<p align="center">
  <a href="https://github.com/ICEFIR/bardic-chord/actions/workflows/ci.yml"><img alt="CI" src="https://github.com/ICEFIR/bardic-chord/actions/workflows/ci.yml/badge.svg"></a>
  <a href="https://github.com/ICEFIR/bardic-chord/releases"><img alt="Latest Release" src="https://img.shields.io/github/v/release/ICEFIR/bardic-chord?display_name=tag"></a>
  <a href="./LICENSE"><img alt="License" src="https://img.shields.io/github/license/ICEFIR/bardic-chord"></a>
  <img alt="Rust" src="https://img.shields.io/badge/Rust-stable-orange">
  <img alt="UI" src="https://img.shields.io/badge/UI-Slint-6c5ce7">
  <img alt="Discord Voice" src="https://img.shields.io/badge/Discord-voice%20relay-5865F2">
</p>

## Why This Exists

Most Discord music bots feel remote, brittle, or overbuilt for a private hangout. Bardic Chord takes a different approach:

- the app runs on the user's machine
- audio stays local until it is relayed into Discord voice
- setup is guided through a desktop UI instead of scattered terminal steps
- Linux and Windows use native capture paths instead of relying on remote playback-device tricks

The current product direction is intentionally simple: capture local app audio well, route it clearly, and make the setup feel approachable.

## Feature Snapshot

| Area | What Bardic Chord does today |
| --- | --- |
| Guided setup | Walks the user through `Welcome`, `Discord`, `Desktop Audio`, and `Launch` |
| Discord | Validates a bot token, discovers guilds and voice channels, and joins voice through `serenity` + `songbird` |
| Linux capture | Creates a dedicated local output with `pactl`, captures the monitor stream with `parec`, and relays PCM into Discord |
| Windows capture | Uses WASAPI per-process loopback capture for the selected app |
| Local state | Stores settings and logs under `./.bardic-chord/` in the current working directory |
| Release flow | Uses `release-plz` for release PRs, tags, changelogs, and GitHub releases, then uploads Linux and Windows archives built by `cargo xtask release` |

## How It Works

```mermaid
flowchart LR
    A[Desktop app audio] --> B{Capture backend}
    B -->|Linux| C[Null sink + monitor capture]
    B -->|Windows| D[WASAPI process loopback]
    C --> E[PCM bridge]
    D --> E
    E --> F[Songbird voice runtime]
    F --> G[Discord voice channel]
```

## The User Flow

1. Paste the Discord bot token.
2. Open the generated bot authorize page if the bot is not in the server yet.
3. Choose the Discord server and voice channel.
4. Choose the desktop app you want Bardic Chord to capture.
5. Prepare desktop audio.
6. Route the app to the Bardic Chord output on Linux, or keep the target app open on Windows so loopback capture can attach.
7. Start the party so Bardic Chord joins voice and forwards the local audio stream.

## Platform Status

| Platform | Status | Notes |
| --- | --- | --- |
| Linux x86_64 | Working | Uses PulseAudio or PipeWire-compatible null sink + monitor capture |
| Windows x86_64 GNU | Working | Uses WASAPI process loopback; cross-built from Linux with `cargo-zigbuild` |
| Linux ARM64 | Planned | Best added through a native ARM64 runner |
| macOS | Planned | Needs a native capture backend that fits the same flow |

## Current Tech Stack

- `slint`
  - native desktop UI
- `serenity`
  - Discord API and gateway client
- `songbird`
  - Discord voice transport and playback runtime
- `tokio`
  - async runtime
- `reqwest` + `rustls`
  - network stack and validation requests
- `symphonia`
  - PCM media/input support for the relay path
- `wasapi`
  - Windows application loopback capture

## Quick Start For Users

### Discord permissions

Current bot permissions integer:

```text
3146752
```

Current required permissions:

- `View Channels`
- `Connect`
- `Speak`

### Launch locally

```bash
cargo run -p bardic-chord
```

For the current POC, the Discord bot token is stored in Bardic Chord's local config file on the user's machine. It is not hard-coded into the binary, and it is not using OS keychain storage yet.

## Development

Check the workspace:

```bash
cargo check
```

Run unit tests:

```bash
cargo test -p bardic-chord --lib
```

Format the workspace:

```bash
cargo fmt --all
```

## Releases

Build both packaged release targets:

```bash
cargo xtask release
```

Build one target:

```bash
cargo xtask release --target linux
cargo xtask release --target windows
```

Prerequisites:

```bash
rustup target add x86_64-pc-windows-gnu
cargo install --locked cargo-zigbuild
```

Artifacts are written to `dist/`:

- `bardic-chord-x86_64-unknown-linux-gnu.tar.xz`
- `bardic-chord-x86_64-unknown-linux-gnu.tar.xz.sha256`
- `bardic-chord-x86_64-pc-windows-gnu.zip`
- `bardic-chord-x86_64-pc-windows-gnu.zip.sha256`

Release flow:

- merge normal work into `main`
- GitHub Actions runs `release-plz` and updates or opens a release PR automatically
- merge the release PR when you want to publish
- `release-plz` creates the version tag and GitHub release
- the release workflow rebuilds the Linux and Windows archives and uploads them to that release

Repository setup notes:

- enable GitHub Actions workflow permissions to allow creating and approving pull requests
- add a `RELEASE_PLZ_TOKEN` secret backed by a fine-grained PAT with `Contents` and `Pull requests` write access
- using `RELEASE_PLZ_TOKEN` is the recommended setup because GitHub does not trigger normal PR/tag-based workflows from actions that use only the default `GITHUB_TOKEN`
- `main` pushes run the `Release` workflow, while `CI` runs on pull requests only

## CI/CD

GitHub Actions currently covers the repo lifecycle:

- `CI`
  - runs on pushes to `main` and on pull requests
  - checks formatting, builds the workspace, runs desktop unit tests, and packages Linux and Windows release archives
- `Release`
  - runs on pushes to `main`
  - opens or updates a release PR with version and changelog changes
  - when the release PR is merged, creates the tag and GitHub release, rebuilds both release archives, and uploads the release assets

## Repo Layout

- `Cargo.toml`
  - workspace root
- `desktop/Cargo.toml`
  - native app crate
- `desktop/src/backend.rs`
  - Discord, capture, config, and relay orchestration
- `desktop/src/lib.rs`
  - Slint controller wiring
- `desktop/ui/app.slint`
  - guided desktop UI
- `release-plz.toml`
  - release-plz configuration for changelog, tags, and release PR behavior
- `CHANGELOG.md`
  - project changelog maintained by release-plz
- `xtask/`
  - release packaging automation

## Project Direction

- keep the setup local-first and simple
- keep the UX guided and explicit
- keep the backend Rust-first
- prefer local audio capture over fragile remote playback-device workarounds
- make cross-platform release packaging boring and repeatable

## Roadmap

- [ ] add a macOS capture backend
- [ ] polish screenshots and release-page media
- [ ] make capture target selection more flexible across desktop apps
- [ ] improve release coverage for more architectures

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
- `Spytify`
  - useful reference for the Windows direction around isolating per-app desktop audio: https://github.com/spytify/spytify

## License

This repository is released under the MIT License. See `LICENSE`.
