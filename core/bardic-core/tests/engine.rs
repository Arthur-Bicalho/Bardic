//! Engine tests against a scripted in-memory `SpotifyApi`. Time is paused (tokio test-util),
//! so fades and poll sleeps complete instantly but in deterministic order.
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use bardic_core::auth::AccessTokenProvider;
use bardic_core::clock::MockClock;
use bardic_core::engine::{Engine, EngineConfig, EngineObserver, PollMode};
use bardic_core::model::{
    AmbienceTrack, Cue, CueItem, CueOptions, CueOwner, Device, PlaybackState, RepeatMode,
    Resolution, SearchResults, TrackRef,
};
use bardic_core::spotify::{PlayRequest, SpotifyApi};
use bardic_core::{BardicError, Result};
use tokio::time::{sleep, Instant};

// ------------------------------------------------------------------ fakes

#[derive(Default)]
struct FakeSpotify {
    calls: Mutex<Vec<String>>,
    state: Mutex<Option<PlaybackState>>,
    errors: Mutex<HashMap<&'static str, VecDeque<BardicError>>>,
}

impl FakeSpotify {
    fn record(&self, method: &'static str, call: String) -> Result<()> {
        self.calls.lock().unwrap().push(call);
        match self
            .errors
            .lock()
            .unwrap()
            .get_mut(method)
            .and_then(|q| q.pop_front())
        {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }
    fn fail(&self, method: &'static str, e: BardicError) {
        self.errors
            .lock()
            .unwrap()
            .entry(method)
            .or_default()
            .push_back(e);
    }
    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
    fn clear(&self) {
        self.calls.lock().unwrap().clear();
    }
    fn set_state(&self, s: Option<PlaybackState>) {
        *self.state.lock().unwrap() = s;
    }
    fn edit_state(&self, f: impl FnOnce(&mut PlaybackState)) {
        f(self.state.lock().unwrap().as_mut().unwrap())
    }
    fn count(&self, prefix: &str) -> usize {
        self.calls()
            .iter()
            .filter(|c| c.starts_with(prefix))
            .count()
    }
    fn volumes(&self) -> Vec<u8> {
        self.calls()
            .iter()
            .filter_map(|c| c.strip_prefix("volume ").map(|v| v.parse().unwrap()))
            .collect()
    }
}

fn dev(d: Option<&str>) -> &str {
    d.unwrap_or("-")
}

#[async_trait]
impl SpotifyApi for FakeSpotify {
    async fn playback_state(&self, _t: &str) -> Result<Option<PlaybackState>> {
        self.record("playback_state", "playback_state".into())?;
        Ok(self.state.lock().unwrap().clone())
    }
    async fn devices(&self, _t: &str) -> Result<Vec<Device>> {
        self.record("devices", "devices".into())?;
        Ok(vec![device("d1", 50), device("d2", 30)])
    }
    async fn play(&self, _t: &str, d: Option<&str>, req: &PlayRequest) -> Result<()> {
        let s = match req {
            PlayRequest::Uris {
                uris,
                offset_index,
                position_ms,
            } => format!(
                "play uris={} offset={offset_index} pos={position_ms} dev={}",
                uris.join(","),
                dev(d)
            ),
            PlayRequest::Context {
                context_uri,
                position_ms,
            } => {
                format!(
                    "play context={context_uri} pos={position_ms} dev={}",
                    dev(d)
                )
            }
        };
        self.record("play", s)
    }
    async fn resume(&self, _t: &str, d: Option<&str>) -> Result<()> {
        self.record("resume", format!("resume dev={}", dev(d)))
    }
    async fn pause(&self, _t: &str, d: Option<&str>) -> Result<()> {
        self.record("pause", format!("pause dev={}", dev(d)))
    }
    async fn next(&self, _t: &str, _d: Option<&str>) -> Result<()> {
        self.record("next", "next".into())
    }
    async fn previous(&self, _t: &str, _d: Option<&str>) -> Result<()> {
        self.record("previous", "previous".into())
    }
    async fn seek(&self, _t: &str, _d: Option<&str>, pos: u32) -> Result<()> {
        self.record("seek", format!("seek {pos}"))
    }
    async fn set_repeat(&self, _t: &str, _d: Option<&str>, m: RepeatMode) -> Result<()> {
        self.record("set_repeat", format!("repeat {}", m.as_spotify()))
    }
    async fn set_shuffle(&self, _t: &str, _d: Option<&str>, on: bool) -> Result<()> {
        self.record("set_shuffle", format!("shuffle {on}"))
    }
    async fn set_volume(&self, _t: &str, _d: Option<&str>, p: u8) -> Result<()> {
        self.record("set_volume", format!("volume {p}"))?;
        if let Some(d) = self
            .state
            .lock()
            .unwrap()
            .as_mut()
            .and_then(|s| s.device.as_mut())
        {
            d.volume_percent = Some(p);
        }
        Ok(())
    }
    async fn transfer(&self, _t: &str, d: &str, play: bool) -> Result<()> {
        self.record("transfer", format!("transfer {d} play={play}"))
    }
    async fn search(&self, _t: &str, q: &str, _l: u8) -> Result<SearchResults> {
        self.record("search", format!("search {q}"))?;
        Ok(SearchResults {
            tracks: vec![],
            playlists: vec![],
            albums: vec![],
        })
    }
}

struct StaticTokens;

#[async_trait]
impl AccessTokenProvider for StaticTokens {
    async fn access_token(&self) -> Result<String> {
        Ok("tok".into())
    }
}

#[derive(Default)]
struct RecObserver {
    changes: Mutex<Vec<PlaybackState>>,
    errors: Mutex<Vec<String>>,
}

impl RecObserver {
    fn changes(&self) -> usize {
        self.changes.lock().unwrap().len()
    }
    fn errors(&self) -> Vec<String> {
        self.errors.lock().unwrap().clone()
    }
}

impl EngineObserver for RecObserver {
    fn on_playback_changed(&self, state: &PlaybackState) {
        self.changes.lock().unwrap().push(state.clone());
    }
    fn on_error(&self, error: &BardicError) {
        self.errors.lock().unwrap().push(error.to_string());
    }
}

// ------------------------------------------------------------------ builders

fn device(id: &str, vol: u8) -> Device {
    Device {
        id: id.into(),
        name: id.into(),
        device_type: "Computer".into(),
        is_active: true,
        is_restricted: false,
        volume_percent: Some(vol),
        supports_volume: true,
    }
}

fn playing(track: &str, context: Option<&str>, vol: u8) -> PlaybackState {
    PlaybackState {
        is_playing: true,
        device: Some(device("d1", vol)),
        track_uri: Some(track.into()),
        track_name: Some(track.into()),
        context_uri: context.map(Into::into),
        duration_ms: 200_000,
        ..Default::default()
    }
}

fn tref(uri: &str, offset: u32) -> TrackRef {
    TrackRef {
        spotify_uri: uri.into(),
        title: uri.into(),
        subtitle: String::new(),
        art_url: None,
        duration_ms: None,
        start_offset_ms: offset,
    }
}

fn cue(id: i64, uris: &[(&str, u32)], options: CueOptions) -> Cue {
    Cue {
        id,
        owner: CueOwner::Global,
        name: format!("cue{id}"),
        situation_id: Some(1),
        emotion_id: None,
        options,
        position: 0,
        items: uris
            .iter()
            .enumerate()
            .map(|(i, (u, off))| CueItem {
                id: i as i64 + 1,
                cue_id: id,
                track: tref(u, *off),
                position: i as i64,
            })
            .collect(),
    }
}

fn opts(
    repeat: RepeatMode,
    shuffle: bool,
    volume: Option<u8>,
    fade_in: u32,
    fade_out: u32,
) -> CueOptions {
    CueOptions {
        repeat,
        shuffle,
        volume,
        fade_in_ms: fade_in,
        fade_out_ms: fade_out,
    }
}

struct Rig {
    api: Arc<FakeSpotify>,
    obs: Arc<RecObserver>,
    clock: MockClock,
    engine: Arc<Engine>,
}

fn rig() -> Rig {
    let api = Arc::new(FakeSpotify::default());
    let obs = Arc::new(RecObserver::default());
    let clock = MockClock::new(1_000_000);
    let engine = Engine::new(
        api.clone(),
        Arc::new(StaticTokens),
        Arc::new(clock.clone()),
        obs.clone(),
        EngineConfig::default(),
    );
    Rig {
        api,
        obs,
        clock,
        engine,
    }
}

fn pos_of(calls: &[String], prefix: &str) -> usize {
    calls
        .iter()
        .position(|c| c.starts_with(prefix))
        .unwrap_or_else(|| panic!("no {prefix} in {calls:?}"))
}

// ------------------------------------------------------------------ play

#[tokio::test(start_paused = true)]
async fn play_track_cue_sequence() {
    let r = rig();
    let c = cue(
        7,
        &[("spotify:track:a", 500), ("spotify:track:b", 0)],
        opts(RepeatMode::Track, false, Some(70), 1500, 1500),
    );
    r.engine.play_cue(&c).await.unwrap();

    // Nothing was playing (204): no fades, target volume set once after play.
    assert_eq!(
        r.api.calls(),
        vec![
            "playback_state",
            "play uris=spotify:track:a,spotify:track:b offset=0 pos=500 dev=-",
            "repeat track",
            "shuffle false",
            "volume 70",
        ]
    );
    let s = r.engine.state();
    assert_eq!(s.active_cue_id, Some(7));
    assert_eq!(s.active_environment_id, None);
    assert!(s.is_playing);
    assert_eq!(s.repeat, RepeatMode::Track);
    assert_eq!(
        r.obs.changes.lock().unwrap().last().unwrap().active_cue_id,
        Some(7)
    );
    assert!(r.obs.errors().is_empty());
}

#[tokio::test(start_paused = true)]
async fn play_shuffled_track_cue() {
    let r = rig();
    let uris = [
        "spotify:track:a",
        "spotify:track:b",
        "spotify:track:c",
        "spotify:track:d",
    ];
    let items: Vec<(&str, u32)> = uris.iter().map(|u| (*u, 0)).collect();
    let c = cue(1, &items, opts(RepeatMode::Context, true, None, 0, 0));
    r.engine.play_cue(&c).await.unwrap();
    let calls = r.api.calls();
    let play = &calls[pos_of(&calls, "play ")];
    let list = play
        .strip_prefix("play uris=")
        .unwrap()
        .split(' ')
        .next()
        .unwrap();
    let mut got: Vec<&str> = list.split(',').collect();
    got.sort();
    assert_eq!(got, uris.to_vec());
    assert!(pos_of(&calls, "play ") < pos_of(&calls, "repeat context"));
    assert!(pos_of(&calls, "repeat context") < pos_of(&calls, "shuffle true"));
    // No volume known, none requested → no volume calls.
    assert_eq!(r.api.count("volume"), 0);
}

#[tokio::test(start_paused = true)]
async fn play_context_cue() {
    let r = rig();
    let c = cue(
        3,
        &[("spotify:playlist:p1", 9000)],
        opts(RepeatMode::Context, false, None, 0, 0),
    );
    r.engine.play_cue(&c).await.unwrap();
    assert!(r
        .api
        .calls()
        .contains(&"play context=spotify:playlist:p1 pos=0 dev=-".to_string()));
}

#[tokio::test(start_paused = true)]
async fn mixed_cue_plays_tracks_only() {
    let r = rig();
    let c = cue(
        3,
        &[("spotify:playlist:p1", 0), ("spotify:track:t", 0)],
        opts(RepeatMode::Off, false, None, 0, 0),
    );
    r.engine.play_cue(&c).await.unwrap();
    assert!(r
        .api
        .calls()
        .contains(&"play uris=spotify:track:t offset=0 pos=0 dev=-".to_string()));
}

#[tokio::test(start_paused = true)]
async fn empty_cue_is_an_error() {
    let r = rig();
    let c = cue(3, &[], CueOptions::default());
    assert!(matches!(
        r.engine.play_cue(&c).await,
        Err(BardicError::Invalid(_))
    ));
    assert_eq!(r.obs.errors().len(), 1);
    assert_eq!(r.api.count("play "), 0);
}

#[tokio::test(start_paused = true)]
async fn fade_out_then_fade_in() {
    let r = rig();
    r.api
        .set_state(Some(playing("spotify:track:old", None, 80)));
    r.engine.refresh().await.unwrap();
    r.api.clear();

    let c = cue(
        1,
        &[("spotify:track:new", 0)],
        opts(RepeatMode::Track, false, Some(60), 1500, 1500),
    );
    let t0 = Instant::now();
    r.engine.play_cue(&c).await.unwrap();
    let elapsed = t0.elapsed();

    let calls = r.api.calls();
    let play_at = pos_of(&calls, "play ");
    let before: Vec<_> = calls[..play_at]
        .iter()
        .filter(|c| c.starts_with("volume"))
        .collect();
    let after: Vec<_> = calls[play_at..]
        .iter()
        .filter(|c| c.starts_with("volume"))
        .collect();
    assert_eq!(before.len(), 8, "{calls:?}");
    assert_eq!(after.len(), 8, "{calls:?}");
    assert_eq!(
        r.api.volumes(),
        vec![70, 60, 50, 40, 30, 20, 10, 0, 7, 15, 22, 30, 37, 45, 52, 60]
    );
    // repeat/shuffle sit between play and fade-in
    assert!(pos_of(&calls, "shuffle") < play_at + 3);
    assert_eq!(r.engine.state().device.unwrap().volume_percent, Some(60));
    // 2 × 7 intervals of 1500/7 ms
    assert!(
        elapsed >= Duration::from_millis(2900) && elapsed <= Duration::from_millis(3100),
        "{elapsed:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn fade_in_from_silence_when_nothing_playing() {
    let r = rig();
    let mut s = playing("spotify:track:old", None, 50);
    s.is_playing = false;
    r.api.set_state(Some(s));
    r.engine.refresh().await.unwrap();
    r.api.clear();
    let c = cue(
        1,
        &[("spotify:track:new", 0)],
        opts(RepeatMode::Off, false, None, 1500, 1500),
    );
    r.engine.play_cue(&c).await.unwrap();
    let calls = r.api.calls();
    assert_eq!(calls[0], "volume 0");
    assert_eq!(
        *r.api.volumes().last().unwrap(),
        50,
        "fades up to the pre-fade volume"
    );
    assert_eq!(r.api.count("volume"), 9);
}

#[tokio::test(start_paused = true)]
async fn no_volume_device_skips_fades() {
    let r = rig();
    let mut s = playing("spotify:track:old", None, 80);
    s.device.as_mut().unwrap().supports_volume = false;
    r.api.set_state(Some(s));
    r.engine.refresh().await.unwrap();
    r.api.clear();
    let c = cue(
        1,
        &[("spotify:track:new", 0)],
        opts(RepeatMode::Off, false, Some(30), 1500, 1500),
    );
    r.engine.play_cue(&c).await.unwrap();
    assert_eq!(r.api.count("volume"), 0);
    assert_eq!(r.api.count("play "), 1);
}

#[tokio::test(start_paused = true)]
async fn rate_limit_during_fade_jumps_to_target() {
    let r = rig();
    r.api
        .set_state(Some(playing("spotify:track:old", None, 80)));
    r.engine.refresh().await.unwrap();
    r.api.clear();
    r.api.fail(
        "set_volume",
        BardicError::RateLimited {
            retry_after_secs: 3,
        },
    ); // 1st step
    let c = cue(
        1,
        &[("spotify:track:new", 0)],
        opts(RepeatMode::Off, false, Some(40), 0, 1500),
    );
    r.engine.play_cue(&c).await.unwrap();
    let calls = r.api.calls();
    // step 70 (rate limited) → target 0 once → play → ... → 40 (fade_in 0: set once)
    assert_eq!(calls[0], "volume 70");
    assert_eq!(calls[1], "volume 0");
    assert!(calls[2].starts_with("play "));
    assert_eq!(r.api.count("volume"), 3);
    assert_eq!(calls.last().unwrap(), "volume 40");
}

#[tokio::test(start_paused = true)]
async fn newer_play_supersedes_fade_in_progress() {
    let r = rig();
    r.api
        .set_state(Some(playing("spotify:track:old", None, 80)));
    r.engine.refresh().await.unwrap();
    r.api.clear();

    let a = cue(
        1,
        &[("spotify:track:a", 0)],
        opts(RepeatMode::Off, false, None, 1500, 1500),
    );
    let b = cue(
        2,
        &[("spotify:track:b", 0)],
        opts(RepeatMode::Track, false, None, 1500, 1500),
    );
    let engine = r.engine.clone();
    let first = tokio::spawn(async move { engine.play_cue(&a).await });
    sleep(Duration::from_millis(300)).await;
    let a_steps = r.api.count("volume");
    assert!(
        (1..=3).contains(&a_steps),
        "A should be mid-fade: {:?}",
        r.api.calls()
    );

    r.engine.play_cue(&b).await.unwrap();
    first.await.unwrap().unwrap(); // superseded call returns Ok

    let calls = r.api.calls();
    let plays: Vec<_> = calls.iter().filter(|c| c.starts_with("play ")).collect();
    assert_eq!(
        plays,
        vec!["play uris=spotify:track:b offset=0 pos=0 dev=-"]
    );
    // B fades up to the volume from before A's fade, not A's intermediate value.
    assert_eq!(*r.api.volumes().last().unwrap(), 80);
    assert_eq!(r.engine.state().active_cue_id, Some(2));
    assert!(r.obs.errors().is_empty());
}

#[tokio::test(start_paused = true)]
async fn set_volume_cancels_fade() {
    let r = rig();
    r.api
        .set_state(Some(playing("spotify:track:old", None, 80)));
    r.engine.refresh().await.unwrap();
    r.api.clear();
    let a = cue(
        1,
        &[("spotify:track:a", 0)],
        opts(RepeatMode::Off, false, None, 1500, 1500),
    );
    let engine = r.engine.clone();
    let first = tokio::spawn(async move { engine.play_cue(&a).await });
    sleep(Duration::from_millis(300)).await;
    r.engine.set_volume(55).await.unwrap();
    first.await.unwrap().unwrap();
    sleep(Duration::from_secs(5)).await;
    assert_eq!(r.api.count("play "), 0);
    assert_eq!(r.api.calls().last().unwrap(), "volume 55");
}

#[tokio::test(start_paused = true)]
async fn fade_out_and_pause_restores_volume_without_resuming() {
    let r = rig();
    r.api.set_state(Some(playing("spotify:track:x", None, 80)));
    r.engine.refresh().await.unwrap();
    r.api.clear();
    r.engine.fade_out_and_pause(1000).await.unwrap();
    let calls = r.api.calls();
    let pause_at = pos_of(&calls, "pause");
    // 1000ms / 150ms min spacing → 7 steps
    assert_eq!(calls[..pause_at].len(), 7, "{calls:?}");
    assert_eq!(calls[pause_at - 1], "volume 0");
    assert_eq!(&calls[pause_at + 1..], ["volume 80"]);
    assert_eq!(r.api.count("resume"), 0);
    assert_eq!(r.api.count("play"), 0);
    let s = r.engine.state();
    assert!(!s.is_playing);
    assert_eq!(s.device.unwrap().volume_percent, Some(80));
}

#[tokio::test(start_paused = true)]
async fn no_active_device_with_selected_device_transfers_and_retries() {
    let r = rig();
    r.api.set_state(Some(playing("spotify:track:x", None, 50)));
    r.engine.refresh().await.unwrap();
    r.engine.select_device(Some("d1".into())); // already active → no lazy transfer
    r.api.clear();
    r.api.fail("play", BardicError::NoActiveDevice);
    let c = cue(
        1,
        &[("spotify:track:a", 0)],
        opts(RepeatMode::Off, false, None, 0, 0),
    );
    r.engine.play_cue(&c).await.unwrap();
    let calls = r.api.calls();
    assert_eq!(
        &calls[..3],
        [
            "play uris=spotify:track:a offset=0 pos=0 dev=d1",
            "transfer d1 play=false",
            "play uris=spotify:track:a offset=0 pos=0 dev=d1",
        ]
    );
    assert!(r.obs.errors().is_empty());
}

#[tokio::test(start_paused = true)]
async fn no_active_device_without_selection_is_reported() {
    let r = rig();
    r.api.fail("play", BardicError::NoActiveDevice);
    let c = cue(
        1,
        &[("spotify:track:a", 0)],
        opts(RepeatMode::Off, false, None, 0, 0),
    );
    assert!(matches!(
        r.engine.play_cue(&c).await,
        Err(BardicError::NoActiveDevice)
    ));
    assert_eq!(r.api.count("transfer"), 0);
    assert_eq!(r.obs.errors(), vec!["no active Spotify device".to_string()]);
}

#[tokio::test(start_paused = true)]
async fn selecting_other_device_transfers_lazily_before_play() {
    let r = rig();
    r.api.set_state(Some(playing("spotify:track:x", None, 50)));
    r.engine.refresh().await.unwrap();
    r.engine.select_device(Some("d2".into()));
    r.api.clear();
    let c = cue(
        1,
        &[("spotify:track:a", 0)],
        opts(RepeatMode::Off, false, None, 1500, 1500),
    );
    r.engine.play_cue(&c).await.unwrap();
    let calls = r.api.calls();
    assert_eq!(
        calls[0], "transfer d2 play=false",
        "no fade on the old device: {calls:?}"
    );
    assert_eq!(calls[1], "play uris=spotify:track:a offset=0 pos=0 dev=d2");
    // Second play: no transfer anymore.
    r.api.clear();
    r.engine.play_cue(&c).await.unwrap();
    assert_eq!(r.api.count("transfer"), 0);
}

// ------------------------------------------------------------------ ownership

#[tokio::test(start_paused = true)]
async fn refresh_preserves_then_clears_active_cue() {
    let r = rig();
    let c = cue(
        9,
        &[("spotify:track:a", 0), ("spotify:track:b", 0)],
        opts(RepeatMode::Context, false, None, 0, 0),
    );
    r.engine.play_cue(&c).await.unwrap();

    r.api.set_state(Some(playing("spotify:track:b", None, 50)));
    r.clock.advance_ms(60_000);
    let s = r.engine.refresh().await.unwrap();
    assert_eq!(s.active_cue_id, Some(9), "track still belongs to the cue");
    assert_eq!(s.fetched_at_ms, 1_060_000);

    // Stale snapshot right after a play is tolerated (grace period).
    r.engine.play_cue(&c).await.unwrap();
    r.api
        .set_state(Some(playing("spotify:track:other", None, 50)));
    r.clock.advance_ms(1000);
    assert_eq!(r.engine.refresh().await.unwrap().active_cue_id, Some(9));

    // User started something else.
    r.clock.advance_ms(10_000);
    assert_eq!(r.engine.refresh().await.unwrap().active_cue_id, None);
    // Dropped for good, even if the track comes back.
    r.api.set_state(Some(playing("spotify:track:a", None, 50)));
    assert_eq!(r.engine.refresh().await.unwrap().active_cue_id, None);
}

#[tokio::test(start_paused = true)]
async fn ambience_ownership_by_context() {
    let r = rig();
    let tracks = vec![AmbienceTrack {
        id: 1,
        environment_id: 42,
        track: tref("spotify:playlist:forest", 0),
        emotion_id: None,
        position: 0,
    }];
    let res = Resolution::Ambience {
        environment_id: 42,
        from: bardic_core::model::FallbackLevel::Global,
        tracks,
        options: opts(RepeatMode::Context, true, None, 0, 0),
    };
    r.engine.play_resolution(&res).await.unwrap();
    assert!(r
        .api
        .calls()
        .contains(&"play context=spotify:playlist:forest pos=0 dev=-".to_string()));
    assert!(r.api.calls().contains(&"shuffle true".to_string()));

    r.clock.advance_ms(60_000);
    r.api.set_state(Some(playing(
        "spotify:track:any",
        Some("spotify:playlist:forest"),
        50,
    )));
    let s = r.engine.refresh().await.unwrap();
    assert_eq!((s.active_cue_id, s.active_environment_id), (None, Some(42)));
    r.api.set_state(Some(playing(
        "spotify:track:any",
        Some("spotify:album:else"),
        50,
    )));
    assert_eq!(
        r.engine.refresh().await.unwrap().active_environment_id,
        None
    );

    // Nothing resolution is a no-op.
    r.api.clear();
    r.engine
        .play_resolution(&Resolution::Nothing)
        .await
        .unwrap();
    assert!(r.api.calls().is_empty());
}

// ------------------------------------------------------------------ poll loop

#[tokio::test(start_paused = true)]
async fn poll_loop_notifies_on_track_change_not_progress_drift() {
    let r = rig();
    r.api.set_state(Some(playing("spotify:track:a", None, 50)));
    let handle = r.engine.spawn_poll_loop();
    sleep(Duration::from_millis(100)).await;
    assert_eq!(r.api.count("playback_state"), 1);
    assert_eq!(r.obs.changes(), 1);

    for i in 1..=3 {
        r.api.edit_state(|s| s.progress_ms = i * 1000);
        sleep(Duration::from_millis(1000)).await;
    }
    assert_eq!(
        r.api.count("playback_state"),
        4,
        "1s interval while playing"
    );
    assert_eq!(r.obs.changes(), 1, "progress drift alone must not notify");

    r.api
        .edit_state(|s| s.track_uri = Some("spotify:track:b".into()));
    sleep(Duration::from_millis(1000)).await;
    assert_eq!(r.obs.changes(), 2);
    assert_eq!(
        r.obs
            .changes
            .lock()
            .unwrap()
            .last()
            .unwrap()
            .track_uri
            .as_deref(),
        Some("spotify:track:b")
    );

    // A seek (big jump) notifies.
    r.api.edit_state(|s| s.progress_ms = 150_000);
    sleep(Duration::from_millis(1000)).await;
    assert_eq!(r.obs.changes(), 3);

    // Paused → idle interval.
    r.api.edit_state(|s| s.is_playing = false);
    sleep(Duration::from_millis(1000)).await;
    let n = r.api.count("playback_state");
    sleep(Duration::from_millis(4000)).await;
    assert_eq!(r.api.count("playback_state"), n);
    sleep(Duration::from_millis(1100)).await;
    assert_eq!(r.api.count("playback_state"), n + 1);

    // Stopped → no polls; Drop also stops.
    handle.set_mode(PollMode::Stopped);
    let n = r.api.count("playback_state");
    sleep(Duration::from_secs(30)).await;
    assert_eq!(r.api.count("playback_state"), n);
    handle.set_mode(PollMode::Active);
    sleep(Duration::from_millis(10)).await;
    assert_eq!(
        r.api.count("playback_state"),
        n + 1,
        "resuming polls immediately"
    );
    drop(handle);
    sleep(Duration::from_secs(30)).await;
    assert_eq!(r.api.count("playback_state"), n + 1);
}

#[tokio::test(start_paused = true)]
async fn poll_loop_honours_rate_limit() {
    let r = rig();
    r.api.set_state(Some(playing("spotify:track:a", None, 50)));
    r.api.fail(
        "playback_state",
        BardicError::RateLimited {
            retry_after_secs: 10,
        },
    );
    let _handle = r.engine.spawn_poll_loop();
    sleep(Duration::from_millis(100)).await;
    assert_eq!(r.api.count("playback_state"), 1);
    sleep(Duration::from_millis(9000)).await;
    assert_eq!(r.api.count("playback_state"), 1, "must wait retry_after");
    sleep(Duration::from_millis(1000)).await;
    assert_eq!(r.api.count("playback_state"), 2);
    assert!(r.obs.errors().is_empty());
    assert_eq!(r.obs.changes(), 1);
}

#[tokio::test(start_paused = true)]
async fn poll_loop_not_authenticated_reports_once_and_idles() {
    let r = rig();
    r.api.set_state(Some(playing("spotify:track:a", None, 50)));
    for _ in 0..3 {
        r.api.fail("playback_state", BardicError::NotAuthenticated);
    }
    let _handle = r.engine.spawn_poll_loop();
    sleep(Duration::from_millis(100)).await;
    assert_eq!(r.obs.errors(), vec!["not authenticated".to_string()]);
    sleep(Duration::from_millis(4800)).await;
    assert_eq!(
        r.api.count("playback_state"),
        1,
        "idle interval while unauthenticated"
    );
    sleep(Duration::from_millis(10_200)).await; // polls at 5s, 10s fail; 15s succeeds
    assert_eq!(r.api.count("playback_state"), 4);
    assert_eq!(r.obs.errors().len(), 1, "same error is not repeated");
    sleep(Duration::from_millis(1000)).await;
    assert_eq!(r.api.count("playback_state"), 5, "back to active interval");
}

// ------------------------------------------------------------------ misc transport

#[tokio::test(start_paused = true)]
async fn transport_commands_update_state_and_report_errors() {
    let r = rig();
    r.api.set_state(Some(playing("spotify:track:a", None, 50)));
    r.engine.refresh().await.unwrap();
    r.engine.select_device(Some("d1".into()));
    r.api.clear();

    r.engine.pause().await.unwrap();
    assert!(!r.engine.state().is_playing);
    r.engine.resume().await.unwrap();
    assert!(r.engine.state().is_playing);
    r.engine.set_repeat(RepeatMode::Context).await.unwrap();
    r.engine.set_shuffle(true).await.unwrap();
    r.engine.seek(12_000).await.unwrap();
    r.engine.next().await.unwrap();
    r.engine.previous().await.unwrap();
    r.engine.set_volume(150).await.unwrap();
    let s = r.engine.state();
    assert_eq!(
        (s.repeat, s.shuffle, s.progress_ms),
        (RepeatMode::Context, true, 12_000)
    );
    assert_eq!(s.device.unwrap().volume_percent, Some(100));
    assert_eq!(
        r.api.calls(),
        vec![
            "pause dev=d1",
            "resume dev=d1",
            "repeat context",
            "shuffle true",
            "seek 12000",
            "next",
            "previous",
            "volume 100",
        ]
    );
    let devices = r.engine.devices().await.unwrap();
    assert_eq!(devices.len(), 2);

    r.api.fail("next", BardicError::Forbidden("premium".into()));
    assert!(r.engine.next().await.is_err());
    assert_eq!(r.obs.errors().len(), 1);
}
