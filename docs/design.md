# Bardic — Design (approved): RPG Soundtrack Controller over Spotify

## Context
You want a Pocket-Bard-style app for game masters that:
- organizes music **per RPG campaign → per chapter/session → per scene**,
- tags music by **situation** (combat, tavern, exploration, boss…) and **emotion** (tension, sorrow, triumph, mystery…),
- plays through **Spotify, controlled from inside the app** (no switching to the Spotify UI),
- runs **locally** (no own backend server) on **Linux desktop first** and an **Android tablet**,
- uses Kotlin, Rust (+TypeScript) and C (Dart dropped — decided).

**Decided**: name **Bardic** · Dart dropped · Linux UI = Tauri v2 + TypeScript · hierarchy = Campaign → Chapter → Scene · ambient music is its own library with fallback behavior.

The working directory is empty, so this is a greenfield design. This document is the design to adapt/approve; implementation starts after approval.

---

## 1. Product concept (what the GM sees)

### Domain model
```
Campaign (an RPG, e.g. "Curse of Strahd")
 └── Chapter (e.g. "Ch.3 – The Village of Barovia")
      └── Scene (e.g. "Ambush at the gates")      ← optional level
           └── Cue slots ─┬─ Situation: Combat  → [track A, track B]  (shuffle / ordered)
                          ├─ Situation: Calm    → [track C]
                          └─ Emotion:   Dread   → [track D, playlist E]
```
- **Track ref** = Spotify URI (track, album, or playlist) + local metadata (name, art URL, duration, start offset in ms, loop flag, volume %).
- **Situations** and **Emotions** are two independent tag vocabularies, with defaults shipped and user-editable (name, color, icon).
- **Resolution rule** (the "mood board" logic): when the GM taps *Combat* + *Dread* inside a chapter, the engine picks tracks in this priority:
  1. Scene-level cue matching both tags → 2. Chapter-level → 3. Campaign-level → 4. Global library defaults → 5. **Ambience fallback** (scene/chapter default environment → global default ambience).
  This lets you set campaign-wide defaults ("this campaign's combat music") and override per chapter.
- **Ambience library (first-class, not a tag)**: a separate, campaign-independent collection of ambient music for moments with nothing prepared.
  - Organized by **environment**: Forest, Cave/Dungeon, City/Market, Tavern, Sea/Ship, Storm/Rain, Night, Temple, Desert, Snow, Battlefield aftermath… (user-editable, each with several Spotify tracks/playlists).
  - Each ambience entry can also carry optional emotion tags, so "Forest + Mystery" picks a fitting forest ambience.
  - **Always one tap away**: a dedicated "Ambience" drawer/strip on the main screen to pick an environment on the fly; it plays in shuffle + repeat-context so it never runs out.
  - **Automatic fallback**: if a Situation/Emotion pad has no prepared music at any hierarchy level, the resolver falls back to the ambience of the current scene/chapter's **default environment** (set per chapter or scene, e.g. Ch.3 → "Night"), then to a global default ambience — never silence by accident.
  - Campaigns can "pin" favorite ambiences; ambiences are reusable across all campaigns.
  - **Two ambience modes** (both supported):
    - **Spotify ambience** — replaces the music (Spotify plays one stream at a time). Used for "nothing prepared" moments and the automatic fallback.
    - **Layered local ambience** — local audio loops (rain, wind, fire crackle, crowd, waves…) played by Bardic itself *under* the Spotify music, e.g. rain + battle theme. Several layers at once, each with its own volume slider and seamless loop; imported from your own files (ogg/mp3/flac/wav) into `~/.local/share/bardic/ambience/` / Android app storage.
    - An environment (e.g. "Storm") can define both: a Spotify ambience track/playlist and a default set of local layers, so one tap gives the full mood.
    - Ducking: optional auto-lower of layers when a Spotify cue starts, restore after.
- **Cue options**: loop track (repeat=track), loop list (repeat=context), shuffle, start offset (skip intros), target volume, fade in/out duration.

### Main screens (same layout on tablet landscape and desktop)
```
┌───────────────┬──────────────────────────────────────────┬──────────────┐
│ CAMPAIGNS     │  Ch.3 – Village of Barovia     [Scene ▾] │ NOW PLAYING  │
│ ▸ Strahd      │                                          │  [cover art] │
│   ▸ Ch.1      │  SITUATIONS                              │  Track name  │
│   ▸ Ch.2      │  [Combat] [Tavern] [Explore] [Boss]      │  ◀◀  ▶  ▶▶   │
│   ● Ch.3      │  [Travel] [Rest]   [+]                   │  🔁 loop     │
│ ▸ Homebrew    │                                          │  ━━━●━━━━━   │
│               │  EMOTIONS                                │  vol ━━●━━   │
│ [+ campaign]  │  (Tension)(Sorrow)(Triumph)(Mystery)     │  device: ▾   │
│               │                                          │              │
│ LIBRARY       │  CUES IN THIS CHAPTER                    │ QUEUE / NEXT │
│ 🔍 Spotify    │  ▸ Ambush (Combat+Tension)  ▶            │              │
│ 🌲 Ambiences  │                                          │              │
├───────────────┴──────────────────────────────────────────┴──────────────┤
│ AMBIENCE  🌲Forest  🕳Cave  🏙City  🍺Tavern  🌊Sea  ⛈Storm  🌙Night  [+] │
└──────────────────────────────────────────────────────────────────────────┘
```
- Big touch-friendly "pads" (Situation buttons) — one tap switches music with a fade.
- **Panic / Silence** button (fade to 0 and pause).
- Spotify search inside the app (Web API `/v1/search`) → drag result onto a situation/emotion pad.
- Device picker (which Spotify Connect device plays: the PC, the tablet, a speaker).

---

## 2. Architecture (language roles)

```
                ┌───────────────────────── bardic-core (Rust) ─────────────────────────┐
                │ auth (PKCE, token refresh, secure store)   spotify client (reqwest)  │
                │ state machine + poll loop (tokio)          library DB (SQLite/rusqlite)│
                │ cue resolver (situation × emotion × hierarchy)  fade engine (volume) │
                │ import/export (JSON campaign packs)        event bus → UI observers  │
                │ ambience mixer (local loops, per-layer volume, ducking)              │
                └───────┬───────────────────────────┬──────────────────────┬──────────┘
                 UniFFI (Kotlin)            direct Rust crate         C-ABI (cbindgen)
                        │                           │                      │
         ┌──────────────▼─────────┐   ┌─────────────▼─────────────┐  ┌─────▼──────────┐
         │ Android app (Kotlin)   │   │ Linux app: Tauri v2       │  │ C: bardic.h +  │
         │ Jetpack Compose UI     │   │ TS (Svelte or React) UI   │  │ MPRIS/D-Bus    │
         │ Custom Tabs OAuth      │   │ tray, global hotkeys      │  │ helper, tests, │
         │ Foreground Service +   │   │ loopback OAuth listener   │  │ other bindings │
         │ MediaSession           │   │                           │  │                │
         └────────────────────────┘   └───────────────────────────┘  └────────────────┘
```

| Language | Role | Why |
|---|---|---|
| **Rust** | Shared core: auth, Spotify client, SQLite library, cue resolver, fades, state/poll loop | One source of truth for both platforms; UniFFI kills JNI boilerplate |
| **TypeScript** | Linux UI inside **Tauri v2** (core linked directly, no FFI on desktop) | Fastest UI iteration; Tauri gives tray + global shortcuts plugins |
| **Kotlin** | Android tablet UI (Compose), OAuth Custom Tab + deep link, Foreground Service, MediaSession | Best Android OS integration |
| **C** | Stable **C-ABI** surface `bardic.h` (generated with cbindgen) + small native **MPRIS** shim on Linux (control the local Spotify desktop client over D-Bus with zero latency, fallback to Web API) | Gives a language-neutral boundary; MPRIS via libdbus/sd-bus is natural in C |

### Core details
- **Auth**: PKCE (verifier/challenge via `sha2` + `base64url`), refresh ~5 min before expiry; tokens stored via `keyring` crate on Linux (Secret Service) and Android Keystore-backed `EncryptedSharedPreferences` passed in through a UniFFI callback interface (`SecureStore` trait implemented in Kotlin).
- **Spotify client**: `reqwest` + `serde`; endpoints: `/me/player` (state), `/me/player/play` (uris/context_uri + position_ms), `/me/player/repeat`, `/me/player/shuffle`, `/me/player/volume`, `/me/player/devices`, `/me/player/transfer`, `/search`. Handles 429 `Retry-After`, 404 "no active device" (→ prompts device picker / transfer).
- **Fade engine**: Spotify has no API crossfade, so fades = stepped `PUT /volume` (e.g. 6–8 steps over 1.5 s), rate-limit aware. On Linux with local client, MPRIS `Volume` is used instead (smooth, no network).
- **Poll loop**: adaptive — 1 s while app foreground & playing, 5 s idle, paused when backgrounded on Android unless Foreground Service active.
- **Storage**: SQLite file in app data dir (`~/.local/share/bardic/` / Android `filesDir`), migrations via `rusqlite_migration`. Campaigns exportable as JSON "packs" to share with other GMs.
- **API to UIs** (single facade object): `BardicCore::new(config, secure_store, observer)` with methods like `login_url()`, `complete_login(code)`, `play_cue(cue_id)`, `trigger(situation_id, emotion_id?)`, `set_loop(mode)`, `fade_out_and_pause()`, CRUD for campaigns/chapters/scenes/cues/tags, `search(query)`. State pushed through an `Observer` callback (`on_playback_changed`, `on_auth_changed`, `on_error`).

- **Ambience mixer**: on Linux the core plays layers itself with `rodio` (gapless looping, per-layer gain, fades). On Android the core keeps the mixer *state* (layers, volumes, fades) and drives a Kotlin `AmbiencePlayer` (Media3/ExoPlayer, one player per layer, `REPEAT_MODE_ONE`) through a UniFFI callback interface — so Android audio focus/Foreground Service rules stay native. Audio focus is requested with `mayDuck` style so it doesn't pause Spotify.

### Platform specifics
- **Linux (Tauri)**: OAuth via loopback `http://127.0.0.1:<port>/callback` (Spotify requires explicit loopback IP, not `localhost`). Tray menu: pads for current chapter, silence, loop toggle. Global hotkeys: media keys + configurable `Ctrl+Alt+1..9` → situation pads.
- **Android**: Custom Tab login → `bardic://callback` deep link; Foreground Service only while a session is "live"; MediaSession so lock-screen/notification shows the current cue. Built with `cargo-ndk` (arm64-v8a, x86_64), 16 KB page alignment.
- **Playback reality check**: the app *controls* Spotify; audio comes from a Spotify Connect device (the Spotify app on the tablet, the desktop client, or a speaker). On Android we can additionally use the **Spotify Android App Remote SDK** later for lower-latency local control (phase 2).

### Spotify constraints (accepted)
- Premium required for playback control; Development Mode = up to 5 users (fine for you + your table).
- Spotify app/client must be installed on the playing device.

---

## 3. Repository layout
```
bardic/
├── core/                 # Rust workspace
│   ├── bardic-core/      # domain, db, spotify, auth, resolver, fades
│   ├── bardic-ffi/       # UniFFI scaffolding (.udl or proc-macros) → Kotlin
│   └── bardic-capi/      # cbindgen C-ABI → include/bardic.h
├── native-c/mpris/       # C MPRIS shim (sd-bus), linked by Linux app
├── desktop/              # Tauri v2 app (src-tauri/ Rust + src/ TypeScript)
├── android/              # Gradle project, Compose app, consumes generated Kotlin + .so
└── docs/                 # this design, ADRs
```

## 4. Delivery phases
1. **Core MVP (Rust)** — PKCE login, token refresh, play/pause/loop/volume, device list, SQLite schema (campaign/chapter/scene/cue/tags/ambience/environment), resolver incl. ambience fallback; CLI test harness.
2. **Linux app (Tauri + TS)** — campaign tree, pads, now-playing, search, tray + hotkeys, MPRIS shim (C).
3. **Android tablet (Kotlin)** — UniFFI bindings, Compose UI mirroring desktop layout, Custom Tab OAuth, Foreground Service/MediaSession.
   Layered local ambience ships in phase 2 (Linux, `rodio`) and phase 3 (Android, Media3).
4. **Polish** — fades, JSON pack import/export, LAN "remote mode" (tablet controls the desktop's session over local network via a small Rust WebSocket server in core), optional Spotify App Remote SDK.

## 5. Agentic programming plan (which model does what)

Caps: **Opus ≤ medium**, **Sonnet ≤ high**, **Haiku any**. The main session acts as orchestrator: it splits each phase into tasks, spawns sub-agents with the model/effort below, reviews their diffs, and runs the verification step before moving on. Independent tasks (e.g. Tauri UI and Android UI against a frozen core API) run in parallel in separate git worktrees.

| Task type | Model / effort | Why this model |
|---|---|---|
| Core API design: UniFFI/C-ABI facade, observer/callback contracts, DB schema, resolver + ambience fallback rules | **Opus / medium** | Decisions are expensive to change later; needs the strongest reasoning, but scope is small |
| Security-sensitive code & review: PKCE, token storage/refresh, OAuth redirects (loopback + deep link) | **Opus / medium** (write) + **Opus / medium** (review) | Subtle bugs = leaked tokens or broken login |
| Concurrency: tokio poll loop, state machine, fade engine with rate limiting, ambience mixer state + ducking | **Opus / medium** | Race conditions and cancellation are hard to get right |
| Hard cross-language debugging (FFI crashes, `.so` loading, JNA/ProGuard, 16 KB alignment) | **Sonnet / high** first, escalate to **Opus / medium** if unresolved | Most are config issues Sonnet handles; Opus for deep ones |
| Main feature implementation: Spotify client, repositories, Tauri commands, Svelte/React screens, Compose screens, Foreground Service/MediaSession, C MPRIS shim, `rodio` layer playback, Kotlin Media3 `AmbiencePlayer` | **Sonnet / high** | Bulk of the code; best quality/cost for well-specified work |
| Build tooling: Cargo workspace, cargo-ndk + Gradle wiring, cbindgen, Tauri config, CI scripts | **Sonnet / medium** | Well-documented patterns, some iteration needed |
| General code review of each PR/phase | **Sonnet / high** | Good bug-catching across all languages |
| Boilerplate: CRUD queries, SQL migrations, DTOs/serde structs, TS types, default tag/ambience seed data, test fixtures (wiremock JSON) | **Haiku / medium** | Repetitive, fully specified |
| Unit tests from existing specs, docs/README, i18n strings, lint/format fixes | **Haiku / low–medium** | Mechanical |
| Codebase search/exploration while working | **Haiku / low** (Explore agent) | Fast and cheap; only needs to locate code |

Workflow per phase: Opus designs interfaces → Sonnet implements in parallel worktrees → Haiku fills boilerplate/tests → Sonnet reviews (Opus reviews auth code) → orchestrator runs verification.

## 6. Spotify credentials (what I can and can't do)
- With **PKCE there is no client secret / API key to manage** — only a public **Client ID**, which is safe to ship inside the app config.
- **I can't create the Client ID for you**: Spotify only issues it through the Developer Dashboard, which requires logging into *your* Spotify account and accepting Spotify's Developer Terms yourself; there is no API for creating apps. It takes ~2 minutes and only once:
  1. Go to developer.spotify.com/dashboard → log in → **Create app**.
  2. Name "Bardic", any description, Redirect URIs: `http://127.0.0.1:43821/callback` and `bardic://callback`; check **Web API** (and **Android** later for App Remote SDK).
  3. Copy the **Client ID** and give it to me (or put it in `bardic.toml` / `local.properties`; the app also has a first-run screen to paste it).
- Everything else (PKCE flow, token storage, refresh, redirect handling, config files) I build. Note: Spotify requires the app owner account to have **Premium** in Development Mode.

## 7. Verification (per phase)
- Rust: `cargo test` (resolver priority rules, token refresh timing with mocked clock, Spotify client against `wiremock`).
- Manual end-to-end with a Premium account: login → create campaign/chapter → assign tracks to Combat/Tension → tap pad → music switches on chosen device with loop on.
- Linux: hotkeys + tray work without focusing window; MPRIS path verified with `playerctl -p spotify status`.
- Android: emulator/tablet — login via Custom Tab, background playback control survives screen-off.

---

## 8. Open decisions for you
None — all decided:
- Name: **Bardic**.
- Ambience: **both** modes — Spotify ambiences (replace music) **and** local layered ambience under the music.
