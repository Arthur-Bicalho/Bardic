# Bardic

A soundtrack controller for tabletop RPG game masters. Organize Spotify music by
**campaign → chapter → scene**, tag it by **situation** (Combat, Tavern, Travel…) and
**emotion** (Tension, Dread, Triumph…), and switch the mood with one tap. If nothing is
prepared for a moment, Bardic falls back to an **ambience** (Forest, Cave, Storm, Night…).
Spotify is controlled from inside the app.

Targets: Linux desktop (Tauri + TypeScript) and Android tablets (Kotlin + Jetpack Compose),
sharing one Rust core. See [`docs/design.md`](docs/design.md) for the full design.

## Status

| Phase | Scope | State |
|---|---|---|
| 1 | Rust core: library DB, cue resolver, Spotify login (PKCE), Spotify client, playback engine, CLI | done (core + CLI; untested against live Spotify) |
| 2 | Linux app (Tauri + TS), tray, hotkeys, MPRIS (C), layered local ambience | planned |
| 3 | Android tablet app (Kotlin, UniFFI), layered local ambience | planned |
| 4 | Polish: JSON campaign packs, LAN remote mode, App Remote SDK | planned |

## Layout

```
core/bardic-core   Rust core library (all logic)
core/bardic-cli    `bardic` developer CLI for exercising the core
docs/              design document
```

## Requirements

- Rust (stable), and on Linux `libdbus-1-dev` + `pkg-config` (OS keychain for tokens).
- A **Spotify Premium** account and the Spotify app/client running on the device that plays.
- A Spotify **Client ID**: create an app at https://developer.spotify.com/dashboard with
  redirect URIs `http://127.0.0.1:43821/callback` and `bardic://callback`, and tick *Web API*.
  No client secret is needed (PKCE).

## Build & test

```bash
cargo test --workspace
```

```bash
cargo build -p bardic-cli
```

## Trying it with the CLI

```bash
export BARDIC_CLIENT_ID=<your client id>
./target/debug/bardic login
./target/debug/bardic new-campaign "Curse of Strahd"
./target/debug/bardic new-chapter 1 "Village of Barovia"
./target/debug/bardic tags
./target/debug/bardic search "dark fantasy battle"
./target/debug/bardic new-cue chapter 1 "Barovia fights" --situation 1 --repeat track
./target/debug/bardic add-item 1 spotify:track:<id> "Battle theme"
./target/debug/bardic trigger --chapter 1 --situation 1
```

Run `./target/debug/bardic help` for all commands. Data is stored in
`~/.local/share/bardic/bardic.db`; tokens in the OS keychain.

## How music is chosen

When you tap a situation and/or emotion, Bardic looks for a cue in this order:

1. exact match (same situation **and** emotion) at scene → chapter → campaign → global level;
2. if you tapped both tags: situation-only cues, then emotion-only cues, same level order;
   if you tapped one tag: cues that have that tag plus another one (e.g. Combat pad → "Combat + Tension" cue);
3. ambience of the scene/chapter/campaign default environment, then the global default;
4. otherwise nothing plays.
