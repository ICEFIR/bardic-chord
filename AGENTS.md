# Bardic Chord Agents Guide

## Intent

Bardic Chord is an open-source Rust-first desktop experiment. The product bar is higher than the feature count: keep the implementation direct, keep the UX intentional, and prefer flows that feel easy to operate locally.

## Working Rules

- Preserve the mythical, playful tone in copy, but keep the engineering pragmatic.
- Prefer latest stable dependencies unless there is a concrete compatibility reason not to.
- Treat cross-platform behavior as a default requirement, not a follow-up.
- Avoid bolting on web-app conventions that make the desktop flow feel heavier than it needs to.

## Architecture Direction

- `desktop/src/backend.rs` owns real integrations, local persistence, local audio runtime control, and Discord relay orchestration.
- `desktop/src/lib.rs` owns the native UI controller wiring.
- `desktop/ui/` owns the Slint desktop control surface and should reflect the real backend flow.
- The active media path is local desktop-audio capture, not a Spotify Connect receiver.
- Linux currently uses a dedicated Bardic Chord sink plus monitor capture through the system audio stack.
- Windows now targets app-process loopback capture behind the same guided flow rather than reviving the old librespot path.
- Do not reintroduce Tauri/WebView scaffolding unless the user explicitly asks for it.

## UX Direction

- The happy path should be:
  1. save the Discord token and route locally
  2. validate Discord and open the invite page when needed
  3. prepare the Bardic Chord desktop output
  4. route the chosen capture app to that output in system sound settings when needed
  5. start the party
  6. optionally follow a target Discord user between voice channels
- Keep actions obvious and stateful. If something is cached locally, surface that clearly.
- Favor a guided multi-page flow over sprawling settings forms.
- Non-technical users should have one clear next action per page.

## Delivery Standard

- If backend behavior changes, update the UI in the same pass.
- If the capture or relay flow changes, update `README.md`.
- Treat “static build” claims carefully: Windows static CRT is fine, but Linux and macOS desktop binaries still have platform-native linkage constraints.
- Before closing work, run the relevant build or test commands when feasible.
