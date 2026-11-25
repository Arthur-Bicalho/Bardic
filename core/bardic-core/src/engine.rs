//! Playback state machine, poll loop and fades. OWNER: resolver/engine agent.
//!
//! The [`Engine`] is the only component that talks to Spotify for playback. It keeps a cached
//! [`PlaybackState`] snapshot (see [`Engine::state`]), pushes changes to an [`EngineObserver`],
//! and runs volume fades as stepped `set_volume` calls.
//!
//! # Supersession (cancellation)
//! Two counters scope cancellation:
//! - the **transport generation** is bumped by `play_*` and `fade_out_and_pause`. A sequence
//!   (fade → transfer → play → repeat/shuffle → fade) re-checks it before every Spotify call;
//!   once a newer sequence started, the older one stops issuing requests and returns `Ok(())`.
//! - the **fade epoch** is bumped by `set_volume`, `pause` and `resume`. It only cancels an
//!   in-flight fade: the sequence keeps going (play, repeat, …) but skips its remaining fade
//!   steps. A user `set_volume` then owns the volume (the sequence won't touch it again and the
//!   value becomes the new base volume); after `pause`/`resume` the target volume is set
//!   directly instead of ramped.
//!
//! `next`, `previous`, `seek`, `set_repeat`, `set_shuffle`, `refresh` and `devices` cancel
//! nothing. Mutating Spotify calls are serialized (one request in flight at a time), so a stale
//! step of a superseded sequence can never land *after* a newer call.
//!
//! # Active cue / environment ownership
//! When Bardic starts playback it remembers a signature of what it asked for: the context URI
//! (album/playlist cues) or the list of track URIs (track cues). On every refresh the snapshot
//! is still "owned" by that cue/environment if
//! - context playback: the reported `context_uri` equals the requested one, or
//! - track-list playback: the reported `track_uri` is one of the requested URIs,
//!
//! or if the snapshot is younger than `EngineConfig::ownership_grace_ms` since Bardic's play
//! call (Spotify is eventually consistent and may still report the previous track). Otherwise
//! the user started something else (in the Spotify app, or playback ended/disconnected) and the
//! ownership is dropped for good: `active_cue_id`/`active_environment_id` become `None`.
//!
//! # Volume bookkeeping
//! The engine tracks a *base volume*: the (non-zero) volume the GM wants when nothing is
//! fading. It is updated by a non-zero `set_volume`, by a completed cue fade-in, and by polled
//! snapshots taken while no sequence ran and the engine owned no volume change. Before any
//! programmatic volume change the engine marks the volume as *engine-owned* together with the
//! value to settle on; if the sequence fails (or a pause interrupts the fade) that value is
//! restored best-effort, so an error never leaves the device silent. `0` is never adopted as
//! base volume.
//!
//! # Stale polls
//! Each poll is stamped at request start. If any mutating Spotify call or transport/fade action
//! overlapped the request, the snapshot is discarded (the engine's own bookkeeping is fresher).
//!
//! Never holds the std `Mutex` across an `.await`.
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::Duration;

use rand::seq::SliceRandom;
use rand::Rng;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::auth::AccessTokenProvider;
use crate::clock::Clock;
use crate::model::{
    AmbienceTrack, Cue, CueOptions, Device, Id, PlaybackState, RepeatMode, Resolution, TrackRef,
};
use crate::spotify::{PlayRequest, SpotifyApi};
use crate::{BardicError, Result};

/// Spotify accepts at most this many URIs in one `play` call.
const MAX_PLAY_URIS: usize = 100;
/// Longest `Retry-After` a fade waits before jumping to its target.
const MAX_FADE_RATE_LIMIT_WAIT: Duration = Duration::from_secs(2);
/// Wait before re-trying `play` on a device that was just transferred to.
const TRANSFER_SETTLE: Duration = Duration::from_millis(500);

/// UI callback (UniFFI callback interface later). Called from tokio worker tasks; never while
/// the engine holds a lock, so implementations may call back into the engine's sync methods.
pub trait EngineObserver: Send + Sync {
    fn on_playback_changed(&self, state: &PlaybackState);
    fn on_error(&self, error: &BardicError);
}

/// How eagerly the poll loop refreshes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PollMode {
    /// App in foreground: `active_poll_ms` while playing, `idle_poll_ms` otherwise.
    Active,
    /// Background / nothing to show: always `idle_poll_ms`.
    Idle,
    /// No polling until the mode changes again.
    Stopped,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineConfig {
    pub active_poll_ms: u64,
    pub idle_poll_ms: u64,
    /// Number of `set_volume` calls per fade (upper bound).
    pub fade_steps: u32,
    /// Minimum spacing between fade steps; reduces the step count for short fades.
    pub min_step_interval_ms: u64,
    /// After Bardic starts playback, snapshots that don't (yet) show the requested
    /// track/context are tolerated for this long before ownership is dropped.
    pub ownership_grace_ms: u64,
    /// Progress deviating from the locally extrapolated value by more than this counts as a
    /// meaningful change (seek) and notifies the observer; smaller drift does not.
    pub progress_jump_ms: u32,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            active_poll_ms: 1000,
            idle_poll_ms: 5000,
            fade_steps: 8,
            min_step_interval_ms: 150,
            ownership_grace_ms: 5000,
            progress_jump_ms: 2500,
        }
    }
}

/// What Bardic started, used to decide whether a snapshot still belongs to it.
#[derive(Debug, Clone)]
struct Ownership {
    cue_id: Option<Id>,
    environment_id: Option<Id>,
    context_uri: Option<String>,
    track_uris: Vec<String>,
    started_at_ms: u64,
}

impl Ownership {
    fn matches(&self, s: &PlaybackState) -> bool {
        match &self.context_uri {
            Some(c) => s.context_uri.as_deref() == Some(c.as_str()),
            None => s
                .track_uri
                .as_ref()
                .is_some_and(|t| self.track_uris.contains(t)),
        }
    }
}

#[derive(Default)]
struct Inner {
    state: PlaybackState,
    fetched_once: bool,
    selected_device: Option<String>,
    /// Selected device differs from the active one: `transfer(play=false)` before next play.
    pending_transfer: bool,
    devices: Vec<Device>,
    owner: Option<Ownership>,
    base_volume: Option<u8>,
    /// The engine changed the volume programmatically and hasn't settled it yet; polled
    /// volumes are not adopted as base meanwhile.
    vol_engine_owned: bool,
    /// Volume to restore if an engine-owned change is abandoned (error / pause mid-fade).
    settle_to: Option<u8>,
    /// Bumped by every user `set_volume`; sequences leave the volume alone once it changed.
    user_volume_seq: u64,
    /// Number of running play/fade sequences.
    busy: usize,
}

struct BusyGuard<'a>(&'a Mutex<Inner>);

impl<'a> BusyGuard<'a> {
    fn new(m: &'a Mutex<Inner>) -> Self {
        lock(m).busy += 1;
        Self(m)
    }
}

impl Drop for BusyGuard<'_> {
    fn drop(&mut self) {
        let mut i = lock(self.0);
        i.busy = i.busy.saturating_sub(1);
    }
}

/// Marks a mutating Spotify request as in flight (for stale-poll detection).
struct InFlight<'a> {
    seq: &'a AtomicU64,
    count: &'a AtomicU64,
}

impl<'a> InFlight<'a> {
    fn new(seq: &'a AtomicU64, count: &'a AtomicU64) -> Self {
        count.fetch_add(1, Ordering::SeqCst);
        seq.fetch_add(1, Ordering::SeqCst);
        Self { seq, count }
    }
}

impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        self.seq.fetch_add(1, Ordering::SeqCst);
        self.count.fetch_sub(1, Ordering::SeqCst);
    }
}

fn lock(m: &Mutex<Inner>) -> MutexGuard<'_, Inner> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Volume-related facts captured at the start of a sequence.
struct Snap {
    selected: Option<String>,
    pending_transfer: bool,
    playing: bool,
    /// Known current volume of the device that will play (None if unknown / switching).
    cur_vol: Option<u8>,
    supports_volume: bool,
    base: Option<u8>,
}

/// Counters captured when a sequence starts.
#[derive(Debug, Clone, Copy)]
struct Ctx {
    gen: u64,
    epoch: u64,
    user_volume: u64,
}

/// How a fade ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FadeEnd {
    /// Reached the target.
    Done,
    /// Fade epoch changed (`set_volume`/`pause`/`resume`): the sequence may continue.
    Interrupted,
    /// Transport generation changed: the whole sequence must stop.
    Superseded,
    /// Rate limited and the final jump to the target failed too.
    Incomplete,
}

/// Counters a poll is stamped with at request start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PollStamp {
    mutations: u64,
    gen: u64,
    epoch: u64,
}

struct Plan {
    req: PlayRequest,
    context_uri: Option<String>,
    track_uris: Vec<String>,
}

/// Build the play request. Tracks win over contexts (mixed lists play tracks only); a list of
/// only contexts plays the first one (a random one when `shuffle`). Track lists are shuffled
/// locally and capped at Spotify's 100-URI limit.
fn plan(items: &[&TrackRef], shuffle: bool) -> Result<Plan> {
    let (mut tracks, contexts): (Vec<&TrackRef>, Vec<&TrackRef>) =
        items.iter().copied().partition(|t| t.is_track());
    if !tracks.is_empty() {
        if !contexts.is_empty() {
            tracing::warn!(
                dropped = contexts.len(),
                "cue mixes tracks with albums/playlists; playing the tracks only"
            );
        }
        if shuffle {
            tracks.shuffle(&mut rand::thread_rng());
        }
        if tracks.len() > MAX_PLAY_URIS {
            tracing::warn!(
                count = tracks.len(),
                max = MAX_PLAY_URIS,
                "too many tracks for one Spotify play call; truncating"
            );
            tracks.truncate(MAX_PLAY_URIS);
        }
        let uris: Vec<String> = tracks.iter().map(|t| t.spotify_uri.clone()).collect();
        let position_ms = tracks[0].start_offset_ms;
        return Ok(Plan {
            req: PlayRequest::Uris {
                uris: uris.clone(),
                offset_index: 0,
                position_ms,
            },
            context_uri: None,
            track_uris: uris,
        });
    }
    if contexts.is_empty() {
        return Err(BardicError::Invalid("nothing to play: no items".into()));
    }
    let idx = if shuffle && contexts.len() > 1 {
        rand::thread_rng().gen_range(0..contexts.len())
    } else {
        if contexts.len() > 1 {
            tracing::warn!(
                count = contexts.len(),
                "several albums/playlists; playing the first"
            );
        }
        0
    };
    let uri = contexts[idx].spotify_uri.clone();
    Ok(Plan {
        req: PlayRequest::Context {
            context_uri: uri.clone(),
            position_ms: 0,
        },
        context_uri: Some(uri),
        track_uris: Vec::new(),
    })
}

fn meaningful_change(prev: &PlaybackState, new: &PlaybackState, jump_ms: u32) -> bool {
    let strip = |s: &PlaybackState| PlaybackState {
        progress_ms: 0,
        fetched_at_ms: 0,
        ..s.clone()
    };
    if strip(prev) != strip(new) {
        return true;
    }
    let expected = if prev.is_playing {
        let elapsed = new.fetched_at_ms.saturating_sub(prev.fetched_at_ms);
        (prev.progress_ms as u64).saturating_add(elapsed)
    } else {
        prev.progress_ms as u64
    };
    (new.progress_ms as u64).abs_diff(expected) > jump_ms as u64
}

/// `Restricted` means "already in that state" for pause/resume.
fn ok_if_restricted(r: Result<()>) -> Result<()> {
    match r {
        Err(BardicError::Restricted(msg)) => {
            tracing::debug!(%msg, "player command restricted; treating as no-op");
            Ok(())
        }
        r => r,
    }
}

pub struct Engine {
    api: Arc<dyn SpotifyApi>,
    tokens: Arc<dyn AccessTokenProvider>,
    clock: Arc<dyn Clock>,
    observer: Arc<dyn EngineObserver>,
    config: EngineConfig,
    /// Transport generation: cancels play/fade-out sequences.
    generation: AtomicU64,
    /// Fade epoch: cancels only in-flight fades.
    fade_epoch: AtomicU64,
    /// Bumped at start and end of every mutating Spotify request.
    mutations: AtomicU64,
    /// Mutating Spotify requests currently in flight.
    in_flight: AtomicU64,
    /// Serializes mutating Spotify requests (held for one request only).
    call_lock: tokio::sync::Mutex<()>,
    inner: Mutex<Inner>,
}

impl Engine {
    pub fn new(
        api: Arc<dyn SpotifyApi>,
        tokens: Arc<dyn AccessTokenProvider>,
        clock: Arc<dyn Clock>,
        observer: Arc<dyn EngineObserver>,
        config: EngineConfig,
    ) -> Arc<Engine> {
        Arc::new(Engine {
            api,
            tokens,
            clock,
            observer,
            config,
            generation: AtomicU64::new(0),
            fade_epoch: AtomicU64::new(0),
            mutations: AtomicU64::new(0),
            in_flight: AtomicU64::new(0),
            call_lock: tokio::sync::Mutex::new(()),
            inner: Mutex::new(Inner::default()),
        })
    }

    // ------------------------------------------------------------ helpers

    fn lock(&self) -> MutexGuard<'_, Inner> {
        lock(&self.inner)
    }

    fn report<T>(&self, r: Result<T>) -> Result<T> {
        if let Err(e) = &r {
            self.observer.on_error(e);
        }
        r
    }

    /// Start a new transport sequence; supersedes older ones.
    fn begin_sequence(&self) -> Ctx {
        let gen = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        Ctx {
            gen,
            epoch: self.fade_epoch.load(Ordering::SeqCst),
            user_volume: self.lock().user_volume_seq,
        }
    }

    /// Cancel in-flight fades (not the sequences running them).
    fn bump_epoch(&self) {
        self.fade_epoch.fetch_add(1, Ordering::SeqCst);
    }

    fn is_current(&self, gen: u64) -> bool {
        self.generation.load(Ordering::SeqCst) == gen
    }

    fn cancelled(&self, ctx: &Ctx) -> Option<FadeEnd> {
        if !self.is_current(ctx.gen) {
            Some(FadeEnd::Superseded)
        } else if self.fade_epoch.load(Ordering::SeqCst) != ctx.epoch {
            Some(FadeEnd::Interrupted)
        } else {
            None
        }
    }

    fn user_changed_volume(&self, ctx: &Ctx) -> bool {
        self.lock().user_volume_seq != ctx.user_volume
    }

    fn poll_stamp(&self) -> PollStamp {
        PollStamp {
            mutations: self.mutations.load(Ordering::SeqCst),
            gen: self.generation.load(Ordering::SeqCst),
            epoch: self.fade_epoch.load(Ordering::SeqCst),
        }
    }

    /// Device for non-play commands: the selected one, unless it isn't active yet.
    fn cmd_device(&self) -> Option<String> {
        let i = self.lock();
        if i.pending_transfer {
            None
        } else {
            i.selected_device.clone()
        }
    }

    fn notify_current(&self) {
        let s = self.lock().state.clone();
        self.observer.on_playback_changed(&s);
    }

    fn set_cached_volume(&self, v: u8) {
        if let Some(d) = self.lock().state.device.as_mut() {
            d.volume_percent = Some(v);
        }
    }

    fn cached_volume(&self) -> Option<u8> {
        self.lock()
            .state
            .device
            .as_ref()
            .and_then(|d| d.volume_percent)
    }

    /// The engine is about to change the volume; `settle` is what to restore if abandoned.
    fn mark_engine_volume(&self, settle: Option<u8>) {
        let mut i = self.lock();
        i.vol_engine_owned = true;
        if settle.is_some() {
            i.settle_to = settle;
        }
    }

    /// An engine-owned volume change reached `v`, the new base volume.
    fn settle_volume(&self, v: u8) {
        let mut i = self.lock();
        if v > 0 {
            i.base_volume = Some(v);
        }
        i.vol_engine_owned = false;
        i.settle_to = None;
    }

    /// If an engine-owned volume change was abandoned, set the volume it should settle on.
    /// Claims the restore under the lock, so concurrent callers restore at most once.
    async fn restore_volume(&self, dev: Option<&str>) -> Result<()> {
        let v = {
            let mut i = self.lock();
            if !i.vol_engine_owned {
                return Ok(());
            }
            i.vol_engine_owned = false;
            i.settle_to.take()
        };
        let Some(v) = v else { return Ok(()) };
        match self.sp_set_volume(dev, v).await {
            Ok(()) => {
                self.set_cached_volume(v);
                self.settle_volume(v);
                Ok(())
            }
            Err(e) => {
                self.mark_engine_volume(Some(v));
                Err(e)
            }
        }
    }

    async fn restore_volume_quietly(&self, dev: Option<&str>) {
        if let Err(e) = self.restore_volume(dev).await {
            tracing::warn!(error = %e, "could not restore volume");
        }
    }

    fn snap(&self) -> Snap {
        let i = self.lock();
        let (cur_vol, supports_volume) = if i.pending_transfer {
            let d = i
                .devices
                .iter()
                .find(|d| Some(&d.id) == i.selected_device.as_ref());
            (None, d.is_none_or(|d| d.supports_volume))
        } else {
            match &i.state.device {
                Some(d) => (d.volume_percent, d.supports_volume),
                None => (None, true),
            }
        };
        Snap {
            selected: i.selected_device.clone(),
            pending_transfer: i.pending_transfer,
            playing: i.state.is_playing,
            cur_vol,
            supports_volume,
            base: i.base_volume,
        }
    }

    /// Make sure we have looked at Spotify at least once (errors are ignored here; the
    /// following calls surface them).
    async fn ensure_fresh(&self) {
        if !self.lock().fetched_once {
            if let Err(e) = self.refresh_inner().await {
                tracing::debug!(error = %e, "initial refresh failed");
            }
        }
    }

    // ------------------------------------------------------------ Spotify calls

    /// Central Spotify call: fetches a token, retries once with a fresh token on 401, and
    /// (for `mutating` calls) serializes the request and marks it in flight for stale-poll
    /// detection.
    async fn call<T, F, Fut>(&self, mutating: bool, f: F) -> Result<T>
    where
        F: Fn(String) -> Fut,
        Fut: Future<Output = Result<T>>,
    {
        let _serial = if mutating {
            Some(self.call_lock.lock().await)
        } else {
            None
        };
        let _in_flight = mutating.then(|| InFlight::new(&self.mutations, &self.in_flight));
        let token = self.tokens.access_token().await?;
        match f(token.clone()).await {
            Err(BardicError::NotAuthenticated) => {
                tracing::info!("access token rejected; refreshing and retrying once");
                self.tokens.invalidate(&token);
                let fresh = self.tokens.access_token().await?;
                f(fresh).await
            }
            r => r,
        }
    }

    async fn sp_playback_state(&self) -> Result<Option<PlaybackState>> {
        let api = &self.api;
        self.call(false, move |t| async move { api.playback_state(&t).await })
            .await
    }

    async fn sp_devices(&self) -> Result<Vec<Device>> {
        let api = &self.api;
        self.call(false, move |t| async move { api.devices(&t).await })
            .await
    }

    async fn sp_play(&self, dev: Option<&str>, req: &PlayRequest) -> Result<()> {
        let api = &self.api;
        self.call(true, move |t| async move { api.play(&t, dev, req).await })
            .await
    }

    async fn sp_pause(&self, dev: Option<&str>) -> Result<()> {
        let api = &self.api;
        self.call(true, move |t| async move { api.pause(&t, dev).await })
            .await
    }

    async fn sp_resume(&self, dev: Option<&str>) -> Result<()> {
        let api = &self.api;
        self.call(true, move |t| async move { api.resume(&t, dev).await })
            .await
    }

    async fn sp_next(&self, dev: Option<&str>) -> Result<()> {
        let api = &self.api;
        self.call(true, move |t| async move { api.next(&t, dev).await })
            .await
    }

    async fn sp_previous(&self, dev: Option<&str>) -> Result<()> {
        let api = &self.api;
        self.call(true, move |t| async move { api.previous(&t, dev).await })
            .await
    }

    async fn sp_seek(&self, dev: Option<&str>, position_ms: u32) -> Result<()> {
        let api = &self.api;
        self.call(true, move |t| async move {
            api.seek(&t, dev, position_ms).await
        })
        .await
    }

    async fn sp_set_repeat(&self, dev: Option<&str>, mode: RepeatMode) -> Result<()> {
        let api = &self.api;
        self.call(
            true,
            move |t| async move { api.set_repeat(&t, dev, mode).await },
        )
        .await
    }

    async fn sp_set_shuffle(&self, dev: Option<&str>, on: bool) -> Result<()> {
        let api = &self.api;
        self.call(
            true,
            move |t| async move { api.set_shuffle(&t, dev, on).await },
        )
        .await
    }

    async fn sp_set_volume(&self, dev: Option<&str>, percent: u8) -> Result<()> {
        let api = &self.api;
        self.call(true, move |t| async move {
            api.set_volume(&t, dev, percent).await
        })
        .await
    }

    async fn sp_transfer(&self, dev: &str) -> Result<()> {
        let api = &self.api;
        self.call(
            true,
            move |t| async move { api.transfer(&t, dev, false).await },
        )
        .await
    }

    // ------------------------------------------------------------ state

    /// Cached snapshot (no network).
    pub fn state(&self) -> PlaybackState {
        self.lock().state.clone()
    }

    /// Forget all cached playback state (e.g. on logout) and cancel running sequences/fades.
    pub fn reset(&self) {
        self.generation.fetch_add(1, Ordering::SeqCst);
        self.bump_epoch();
        let s = {
            let mut i = self.lock();
            let busy = i.busy;
            let user_volume_seq = i.user_volume_seq;
            *i = Inner::default();
            i.busy = busy;
            i.user_volume_seq = user_volume_seq;
            i.state.clone()
        };
        self.observer.on_playback_changed(&s);
    }

    /// Poll Spotify once. Notifies the observer if the state meaningfully changed.
    pub async fn refresh(&self) -> Result<PlaybackState> {
        let r = self.refresh_inner().await;
        self.report(r)
    }

    async fn refresh_inner(&self) -> Result<PlaybackState> {
        let stamp = self.poll_stamp();
        let in_flight_at_start = self.in_flight.load(Ordering::SeqCst) > 0;
        let busy_at_start = self.lock().busy > 0;
        let fetched = self.sp_playback_state().await?;
        let now = self.clock.now_ms();
        let (snapshot, changed) = {
            let mut i = self.lock();
            if in_flight_at_start || self.poll_stamp() != stamp {
                tracing::debug!("discarding playback snapshot that raced a player command");
                return Ok(i.state.clone());
            }
            let mut s = fetched.unwrap_or_default();
            s.fetched_at_ms = now;
            s.active_cue_id = None;
            s.active_environment_id = None;
            if let Some(o) = &i.owner {
                let in_grace = now.saturating_sub(o.started_at_ms) < self.config.ownership_grace_ms;
                if o.matches(&s) || in_grace {
                    s.active_cue_id = o.cue_id;
                    s.active_environment_id = o.environment_id;
                } else {
                    i.owner = None;
                }
            }
            if let (Some(sel), Some(d)) = (&i.selected_device, &s.device) {
                if &d.id == sel {
                    i.pending_transfer = false;
                }
            }
            if !busy_at_start && i.busy == 0 && !i.vol_engine_owned {
                if let Some(v) = s
                    .device
                    .as_ref()
                    .and_then(|d| d.volume_percent)
                    .filter(|v| *v > 0)
                {
                    i.base_volume = Some(v);
                }
            }
            let changed = meaningful_change(&i.state, &s, self.config.progress_jump_ms);
            i.fetched_once = true;
            i.state = s.clone();
            (s, changed)
        };
        if changed {
            self.observer.on_playback_changed(&snapshot);
        }
        Ok(snapshot)
    }

    // ------------------------------------------------------------ devices

    pub async fn devices(&self) -> Result<Vec<Device>> {
        let r = self.sp_devices().await;
        let r = self.report(r)?;
        self.lock().devices = r.clone();
        Ok(r)
    }

    /// Target device for subsequent calls (`None` = whatever is active). If it differs from
    /// the active device, playback is transferred (paused) right before the next play.
    pub fn select_device(&self, device_id: Option<String>) {
        let mut i = self.lock();
        let active = i.state.device.as_ref().map(|d| d.id.clone());
        i.pending_transfer = device_id.is_some() && device_id != active;
        i.selected_device = device_id;
    }

    // ------------------------------------------------------------ play

    /// Play what the resolver found. `Resolution::Nothing` is a no-op.
    pub async fn play_resolution(&self, resolution: &Resolution) -> Result<()> {
        match resolution {
            Resolution::Cue { cue, .. } => self.play_cue(cue).await,
            Resolution::Ambience {
                environment_id,
                tracks,
                options,
                ..
            } => self.play_ambience(*environment_id, tracks, options).await,
            Resolution::Nothing => Ok(()),
        }
    }

    pub async fn play_cue(&self, cue: &Cue) -> Result<()> {
        let mut items: Vec<_> = cue.items.iter().collect();
        items.sort_by_key(|i| (i.position, i.id));
        let refs: Vec<&TrackRef> = items.iter().map(|i| &i.track).collect();
        let r = match plan(&refs, cue.options.shuffle) {
            Ok(p) => self.play_planned(p, &cue.options, Some(cue.id), None).await,
            Err(e) => Err(e),
        };
        self.report(r)
    }

    pub async fn play_ambience(
        &self,
        environment_id: Id,
        tracks: &[AmbienceTrack],
        options: &CueOptions,
    ) -> Result<()> {
        let mut sorted: Vec<_> = tracks.iter().collect();
        sorted.sort_by_key(|a| (a.position, a.id));
        let refs: Vec<&TrackRef> = sorted.iter().map(|a| &a.track).collect();
        let r = match plan(&refs, options.shuffle) {
            Ok(p) => {
                self.play_planned(p, options, None, Some(environment_id))
                    .await
            }
            Err(e) => Err(e),
        };
        self.report(r)
    }

    async fn play_planned(
        &self,
        plan: Plan,
        opts: &CueOptions,
        cue_id: Option<Id>,
        environment_id: Option<Id>,
    ) -> Result<()> {
        let ctx = self.begin_sequence();
        let _busy = BusyGuard::new(&self.inner);
        self.ensure_fresh().await;
        if !self.is_current(ctx.gen) {
            return Ok(());
        }
        let snap = self.snap();
        let r = self
            .play_sequence(&ctx, &snap, &plan, opts, cue_id, environment_id)
            .await;
        if r.is_err() && self.is_current(ctx.gen) {
            // Never leave the device faded down (old track playing silently).
            self.restore_volume_quietly(snap.selected.as_deref()).await;
        }
        r
    }

    async fn play_sequence(
        &self,
        ctx: &Ctx,
        snap: &Snap,
        plan: &Plan,
        opts: &CueOptions,
        cue_id: Option<Id>,
        environment_id: Option<Id>,
    ) -> Result<()> {
        let dev = snap.selected.as_deref();
        let target = opts
            .volume
            .map(|v| v.min(100))
            .or(snap.base)
            .or(snap.cur_vol.filter(|v| *v > 0));
        // What to restore if we lower the volume and then fail.
        let pre_fade = snap.base.or(snap.cur_vol).filter(|v| *v > 0);
        let can_fade = snap.supports_volume && !snap.pending_transfer;
        let mut level = snap.cur_vol;

        // 1. fade down (or pre-silence before a fade-in when nothing is playing)
        if let Some(v) = level.filter(|v| can_fade && *v > 0) {
            if snap.playing && opts.fade_out_ms > 0 {
                self.mark_engine_volume(pre_fade);
                match self.fade(ctx, dev, v, 0, opts.fade_out_ms).await? {
                    FadeEnd::Superseded => return Ok(()),
                    FadeEnd::Done => level = Some(0),
                    FadeEnd::Interrupted | FadeEnd::Incomplete => level = self.cached_volume(),
                }
            } else if !snap.playing
                && opts.fade_in_ms > 0
                && target.is_some_and(|t| t > 0)
                && self.cancelled(ctx).is_none()
            {
                self.mark_engine_volume(pre_fade);
                self.sp_set_volume(dev, 0).await?;
                self.set_cached_volume(0);
                level = Some(0);
            }
        }
        if !self.is_current(ctx.gen) {
            return Ok(());
        }

        // 2. device transfer
        let mut transferred = false;
        if snap.pending_transfer {
            if let Some(d) = dev {
                self.sp_transfer(d).await?;
                self.lock().pending_transfer = false;
                transferred = true;
            }
        }

        // 3. repeat/shuffle before play for contexts (so a shuffled playlist doesn't start
        //    on track 1), play, then repeat/shuffle for track lists (applies to the new queue).
        let is_context = plan.context_uri.is_some();
        let mut modes_todo = (true, true);
        if is_context {
            modes_todo = self.apply_modes(ctx, dev, opts, modes_todo, false).await;
            if !self.is_current(ctx.gen) {
                return Ok(());
            }
        }
        self.play_with_retry(ctx, dev, &plan.req, transferred)
            .await?;
        {
            let mut i = self.lock();
            if !self.is_current(ctx.gen) {
                return Ok(());
            }
            i.owner = Some(Ownership {
                cue_id,
                environment_id,
                context_uri: plan.context_uri.clone(),
                track_uris: plan.track_uris.clone(),
                started_at_ms: self.clock.now_ms(),
            });
            i.state.is_playing = true;
            i.state.active_cue_id = cue_id;
            i.state.active_environment_id = environment_id;
            // From now on an abandoned volume change should settle on the new cue's level.
            if i.vol_engine_owned && target.is_some() {
                i.settle_to = target;
            }
        }
        self.apply_modes(ctx, dev, opts, modes_todo, true).await;
        if !self.is_current(ctx.gen) {
            return Ok(());
        }

        // 4. fade up / set target volume
        if let Some(t) = target.filter(|_| snap.supports_volume) {
            if self.user_changed_volume(ctx) {
                // The GM moved the slider meanwhile: their volume wins.
                tracing::debug!("user changed volume during cue switch; skipping fade-in");
            } else {
                let interrupted = self.cancelled(ctx).is_some();
                let owned = self.lock().vol_engine_owned;
                let end = match level {
                    Some(l) if can_fade && opts.fade_in_ms > 0 && !interrupted && l != t => {
                        self.mark_engine_volume(Some(t));
                        self.fade(ctx, dev, l, t, opts.fade_in_ms).await?
                    }
                    Some(l) if l == t && !owned => FadeEnd::Done,
                    _ => {
                        self.mark_engine_volume(Some(t));
                        self.sp_set_volume(dev, t).await?;
                        self.set_cached_volume(t);
                        FadeEnd::Done
                    }
                };
                match end {
                    FadeEnd::Superseded => return Ok(()),
                    FadeEnd::Done => self.settle_volume(t),
                    FadeEnd::Interrupted => {
                        if !self.user_changed_volume(ctx) {
                            // pause/resume cut the fade short: land on the target directly.
                            self.restore_volume_quietly(dev).await;
                        }
                    }
                    FadeEnd::Incomplete => {
                        tracing::warn!(target = t, "fade-in did not reach its target volume");
                    }
                }
            }
        }
        if self.is_current(ctx.gen) {
            self.notify_current();
        }
        Ok(())
    }

    /// Apply repeat/shuffle for the items still in `todo` (repeat, shuffle). Failures are not
    /// fatal: returns what still needs doing; on the `last_try` they are logged and reported.
    async fn apply_modes(
        &self,
        ctx: &Ctx,
        dev: Option<&str>,
        opts: &CueOptions,
        todo: (bool, bool),
        last_try: bool,
    ) -> (bool, bool) {
        let (mut repeat, mut shuffle) = todo;
        let failed = |what: &str, e: BardicError| {
            if last_try {
                tracing::warn!(error = %e, "could not set {what}; continuing");
                self.observer.on_error(&e);
            } else {
                tracing::debug!(error = %e, "could not set {what} before play; retrying after");
            }
        };
        if repeat && self.is_current(ctx.gen) {
            match self.sp_set_repeat(dev, opts.repeat).await {
                Ok(()) => {
                    self.lock().state.repeat = opts.repeat;
                    repeat = false;
                }
                Err(e) => failed("repeat", e),
            }
        }
        if shuffle && self.is_current(ctx.gen) {
            match self.sp_set_shuffle(dev, opts.shuffle).await {
                Ok(()) => {
                    self.lock().state.shuffle = opts.shuffle;
                    shuffle = false;
                }
                Err(e) => failed("shuffle", e),
            }
        }
        (repeat, shuffle)
    }

    /// `play`; on `NoActiveDevice` with a known device: transfer (unless just done) and retry;
    /// if a freshly transferred device still isn't ready, wait a moment and retry once more.
    async fn play_with_retry(
        &self,
        ctx: &Ctx,
        dev: Option<&str>,
        req: &PlayRequest,
        transferred: bool,
    ) -> Result<()> {
        let first = self.sp_play(dev, req).await;
        let Some(d) = dev else { return first };
        if !matches!(first, Err(BardicError::NoActiveDevice)) {
            return first;
        }
        if !transferred {
            tracing::info!(device = d, "no active device; transferring and retrying");
            self.sp_transfer(d).await?;
            self.lock().pending_transfer = false;
            let second = self.sp_play(Some(d), req).await;
            if !matches!(second, Err(BardicError::NoActiveDevice)) {
                return second;
            }
        }
        tracing::info!(device = d, "device not ready after transfer; retrying play");
        tokio::time::sleep(TRANSFER_SETTLE).await;
        if !self.is_current(ctx.gen) {
            return Ok(());
        }
        self.sp_play(Some(d), req).await
    }

    // ------------------------------------------------------------ fades

    fn step_count(&self, duration_ms: u32) -> u32 {
        if duration_ms == 0 {
            return 1;
        }
        let mut steps = self.config.fade_steps.max(1);
        let min = self.config.min_step_interval_ms;
        if steps > 1 && min > 0 {
            let max_steps = (duration_ms as u64 / min + 1).min(u32::MAX as u64) as u32;
            steps = steps.min(max_steps);
        }
        steps.max(1)
    }

    /// Stepped volume ramp `from → to` over ~`duration_ms`. Steps are scheduled against
    /// deadlines relative to the fade start (request latency doesn't stretch the fade), and
    /// never exceed `|to − from|` (no repeated volumes). Stops as soon as the transport
    /// generation (`Superseded`) or the fade epoch (`Interrupted`) changes.
    async fn fade(
        &self,
        ctx: &Ctx,
        dev: Option<&str>,
        from: u8,
        to: u8,
        duration_ms: u32,
    ) -> Result<FadeEnd> {
        if let Some(end) = self.cancelled(ctx) {
            return Ok(end);
        }
        if from == to {
            return Ok(FadeEnd::Done);
        }
        let steps = self
            .step_count(duration_ms)
            .min(from.abs_diff(to) as u32)
            .max(1);
        let start = Instant::now();
        let (from_i, to_i) = (from as i64, to as i64);
        for i in 1..=steps {
            if let Some(end) = self.cancelled(ctx) {
                return Ok(end);
            }
            let v = (from_i + (to_i - from_i) * i as i64 / steps as i64) as u8;
            match self.sp_set_volume(dev, v).await {
                Ok(()) => self.set_cached_volume(v),
                Err(BardicError::RateLimited { retry_after_secs }) => {
                    let wait = Duration::from_secs(retry_after_secs).min(MAX_FADE_RATE_LIMIT_WAIT);
                    tracing::warn!(
                        retry_after_secs,
                        "rate limited during fade; jumping to target"
                    );
                    tokio::time::sleep(wait).await;
                    if let Some(end) = self.cancelled(ctx) {
                        return Ok(end);
                    }
                    return match self.sp_set_volume(dev, to).await {
                        Ok(()) => {
                            self.set_cached_volume(to);
                            Ok(FadeEnd::Done)
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "could not set fade target volume");
                            Ok(FadeEnd::Incomplete)
                        }
                    };
                }
                Err(e) => return Err(e),
            }
            if i < steps {
                let due_ms = duration_ms as u64 * i as u64 / (steps as u64 - 1);
                tokio::time::sleep_until(start + Duration::from_millis(due_ms)).await;
            }
        }
        Ok(self.cancelled(ctx).unwrap_or(FadeEnd::Done))
    }

    /// GM "panic / silence": fade to 0, pause, then restore the pre-fade volume *without*
    /// resuming (Spotify accepts volume changes while paused), so the next play isn't silent.
    pub async fn fade_out_and_pause(&self, duration_ms: u32) -> Result<()> {
        let r = self.fade_out_and_pause_inner(duration_ms).await;
        self.report(r)
    }

    async fn fade_out_and_pause_inner(&self, duration_ms: u32) -> Result<()> {
        let ctx = self.begin_sequence();
        let _busy = BusyGuard::new(&self.inner);
        self.ensure_fresh().await;
        if !self.is_current(ctx.gen) {
            return Ok(());
        }
        let dev = self.cmd_device();
        let dev = dev.as_deref();
        let r = self.fade_out_and_pause_seq(&ctx, dev, duration_ms).await;
        if r.is_err() && self.is_current(ctx.gen) {
            self.restore_volume_quietly(dev).await;
        }
        r
    }

    async fn fade_out_and_pause_seq(
        &self,
        ctx: &Ctx,
        dev: Option<&str>,
        duration_ms: u32,
    ) -> Result<()> {
        let snap = self.snap();
        if let Some(v) = snap
            .cur_vol
            .filter(|v| *v > 0 && snap.playing && snap.supports_volume && !snap.pending_transfer)
        {
            self.mark_engine_volume(snap.base.or(Some(v)).filter(|v| *v > 0));
            if self.fade(ctx, dev, v, 0, duration_ms).await? == FadeEnd::Superseded {
                return Ok(());
            }
        }
        if !self.is_current(ctx.gen) {
            return Ok(());
        }
        // Restricted = already paused; harmless for a panic button.
        ok_if_restricted(self.sp_pause(dev).await)?;
        self.lock().state.is_playing = false;
        if self.is_current(ctx.gen) && !self.user_changed_volume(ctx) {
            self.restore_volume(dev).await?;
        }
        self.notify_current();
        Ok(())
    }

    // ------------------------------------------------------------ transport

    /// Pause. Interrupts a running fade (the volume goes back to where it should settle, the
    /// player stays paused); `Restricted` (already paused) counts as success.
    pub async fn pause(&self) -> Result<()> {
        self.bump_epoch();
        let r = async {
            let dev = self.cmd_device();
            ok_if_restricted(self.sp_pause(dev.as_deref()).await)?;
            self.lock().state.is_playing = false;
            self.restore_volume(dev.as_deref()).await
        }
        .await;
        self.finish(r)
    }

    /// Resume. Interrupts a running fade like [`Engine::pause`]; `Restricted` (already
    /// playing) counts as success.
    pub async fn resume(&self) -> Result<()> {
        self.bump_epoch();
        let r = async {
            let dev = self.cmd_device();
            ok_if_restricted(self.sp_resume(dev.as_deref()).await)?;
            self.lock().state.is_playing = true;
            self.restore_volume(dev.as_deref()).await
        }
        .await;
        self.finish(r)
    }

    pub async fn next(&self) -> Result<()> {
        let r = self.sp_next(self.cmd_device().as_deref()).await;
        self.report(r)
    }

    pub async fn previous(&self) -> Result<()> {
        let r = self.sp_previous(self.cmd_device().as_deref()).await;
        self.report(r)
    }

    pub async fn seek(&self, position_ms: u32) -> Result<()> {
        let r = async {
            self.sp_seek(self.cmd_device().as_deref(), position_ms)
                .await?;
            let now = self.clock.now_ms();
            let mut i = self.lock();
            i.state.progress_ms = position_ms;
            i.state.fetched_at_ms = now;
            Ok(())
        }
        .await;
        self.finish(r)
    }

    pub async fn set_repeat(&self, mode: RepeatMode) -> Result<()> {
        let r = async {
            self.sp_set_repeat(self.cmd_device().as_deref(), mode)
                .await?;
            self.lock().state.repeat = mode;
            Ok(())
        }
        .await;
        self.finish(r)
    }

    pub async fn set_shuffle(&self, on: bool) -> Result<()> {
        let r = async {
            self.sp_set_shuffle(self.cmd_device().as_deref(), on)
                .await?;
            self.lock().state.shuffle = on;
            Ok(())
        }
        .await;
        self.finish(r)
    }

    /// User volume change: interrupts a running fade (the cue switch itself continues) and,
    /// unless 0, becomes the new base volume.
    pub async fn set_volume(&self, percent: u8) -> Result<()> {
        self.bump_epoch();
        self.lock().user_volume_seq += 1;
        let percent = percent.min(100);
        let r = async {
            self.sp_set_volume(self.cmd_device().as_deref(), percent)
                .await?;
            self.set_cached_volume(percent);
            self.settle_volume(percent);
            Ok(())
        }
        .await;
        self.finish(r)
    }

    /// Report error, or notify the observer with the updated cached state.
    fn finish(&self, r: Result<()>) -> Result<()> {
        let r = self.report(r);
        if r.is_ok() {
            self.notify_current();
        }
        r
    }

    // ------------------------------------------------------------ polling

    /// Spawn the adaptive poll loop on the current tokio runtime. The loop holds only a weak
    /// reference to the engine and ends when the handle is stopped/dropped or the engine is gone.
    ///
    /// - interval: `active_poll_ms` in `PollMode::Active` while playing, else `idle_poll_ms`;
    /// - `RateLimited`: sleep `retry_after` (no observer error);
    /// - `NotAuthenticated` (after the one automatic token refresh + retry): reported, then
    ///   idle-rate polling until a refresh succeeds;
    /// - other errors are reported once per distinct message (no spam every second).
    pub fn spawn_poll_loop(self: &Arc<Self>) -> PollHandle {
        let (tx, rx) = watch::channel(PollMode::Active);
        let task = tokio::spawn(poll_loop(Arc::downgrade(self), rx));
        PollHandle { mode: tx, task }
    }
}

async fn poll_loop(weak: Weak<Engine>, mut rx: watch::Receiver<PollMode>) {
    let mut forced_idle = false;
    let mut last_error: Option<String> = None;
    loop {
        let mode = *rx.borrow_and_update();
        if mode == PollMode::Stopped {
            if rx.changed().await.is_err() {
                return;
            }
            continue;
        }
        let Some(engine) = weak.upgrade() else { return };
        let cfg = &engine.config;
        let delay_ms = match engine.refresh_inner().await {
            Ok(s) => {
                forced_idle = false;
                last_error = None;
                if mode == PollMode::Active && s.is_playing {
                    cfg.active_poll_ms
                } else {
                    cfg.idle_poll_ms
                }
            }
            Err(BardicError::RateLimited { retry_after_secs }) => {
                tracing::debug!(retry_after_secs, "poll rate limited");
                retry_after_secs
                    .saturating_mul(1000)
                    .max(cfg.active_poll_ms)
            }
            Err(e) => {
                if matches!(e, BardicError::NotAuthenticated) {
                    forced_idle = true;
                }
                let msg = e.to_string();
                if last_error.as_deref() != Some(msg.as_str()) {
                    engine.observer.on_error(&e);
                    last_error = Some(msg);
                }
                cfg.idle_poll_ms
            }
        };
        let delay_ms = if forced_idle {
            delay_ms.max(engine.config.idle_poll_ms)
        } else {
            delay_ms
        };
        drop(engine);
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis(delay_ms)) => {}
            r = rx.changed() => if r.is_err() { return },
        }
    }
}

/// Controls a running poll loop; stops it on drop.
pub struct PollHandle {
    mode: watch::Sender<PollMode>,
    task: JoinHandle<()>,
}

impl PollHandle {
    /// Change the polling mode; takes effect immediately (a non-stopped mode polls right away).
    pub fn set_mode(&self, mode: PollMode) {
        self.mode.send_replace(mode);
    }

    pub fn stop(&self) {
        self.task.abort();
    }
}

impl Drop for PollHandle {
    fn drop(&mut self) {
        self.task.abort();
    }
}
