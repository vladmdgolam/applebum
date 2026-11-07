# Applebum agent guide

Applebum is a Spotifast fork that plays Apple Music through MusicKit. Upstream's
architecture still applies: `src/ui/` draws views and emits `Action`s, applied
after drawing in `src/app.rs`; network and playback work runs in `src/backend.rs`.

- `src/apple/mod.rs` answers `ApiRequest`s and `PlayerCommand`s from Apple Music,
  keeping `spotify:<kind>:<id>` URIs internally because the views route on them.
  A request it doesn't answer falls through to the Spotify path and fails as not
  signed in.
- `src/apple/native/Bridge.swift` holds MusicKit and speaker discovery;
  `airplay.cpp` wraps the vendored AirPlay sender in `third_party/`.
- Never bypass DRM. Sound is taken from Apple's player with a Core Audio process
  tap and only passes through in memory; nothing is stored.
- Don't add skins without a license to this repository.
- `cargo test --lib` runs the unit tests. Use `--demo-shot` to render the UI.
