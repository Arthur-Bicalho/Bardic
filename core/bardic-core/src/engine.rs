//! Playback state machine, poll loop and fades. OWNER: resolver/engine agent.
//!
//! The [`Engine`] is the only component that talks to Spotify for playback. It keeps a cached
//! [`PlaybackState`] snapshot (see [`Engine::state`]), pushes changes to an [`EngineObserver`],
//! and runs volume fades as stepped `set_volume` calls.
//!
//! # Supersession (cancellation)
//! Every *transport* action — `play_*`, `pause`, `resume`, `set_volume`,
//! `fade_out_and_pause` — bumps a generation counter. Long sequences (fade → play → repeat →
//! shuffle → fade) re-check the generation before every Spotify call and between fade steps;
//! when a newer transport action has started, the older call stops issuing requests and
//! returns `Ok(())`. `next`, `previous`, `seek`, `set_repeat`, `set_shuffle`, `refresh` and
//! `devices` do **not** supersede anything (e.g. toggling shuffle does not cut a fade-in short).
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
//! The engine tracks a *base volume*: the volume the GM wants when nothing is fading. It is
//! updated by `set_volume`, by a completed cue fade-in, and by polled snapshots while no
//! sequence is running and no fade was interrupted. Fades that get superseded midway therefore
//! never leak their intermediate volume into the next cue's "pre-fade volume".
//!
//! Never holds the std `Mutex` across an `.await`.
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::Duration;

use rand::seq::SliceRandom;
use rand::Rng;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::auth::AccessTokenProvider;
use crate::clock::Clock;
use crate::model::{
    AmbienceTrack, Cue, CueOptions, Device, Id, PlaybackState, RepeatMode, Resolution, TrackRef,
};
use crate::spotify::{PlayRequest, SpotifyApi};
use crate::{BardicError, Result};

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
    /// A fade was interrupted; polled volumes are not adopted as base until fixed.
    volume_dirty: bool,
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

struct Plan {
    req: PlayRequest,
    context_uri: Option<String>,
    track_uris: Vec<String>,
}

/// Build the play request. Tracks win over contexts (mixed lists play tracks only); a list of
/// only contexts plays the first one (a random one when `shuffle`).
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

pub struct Engine {
    api: Arc<dyn SpotifyApi>,
    tokens: Arc<dyn AccessTokenProvider>,
    clock: Arc<dyn Clock>,
    observer: Arc<dyn EngineObserver>,
    config: EngineConfig,
    generation: AtomicU64,
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

    /// Start a new transport action; supersedes older ones.
    fn bump(&self) -> u64 {
        self.generation.fetch_add(1, Ordering::SeqCst) + 1
    }

    fn is_current(&self, gen: u64) -> bool {
        self.generation.load(Ordering::SeqCst) == gen
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

    // ------------------------------------------------------------ state

    /// Cached snapshot (no network).
    pub fn state(&self) -> PlaybackState {
        self.lock().state.clone()
    }

    /// Poll Spotify once. Notifies the observer if the state meaningfully changed.
    pub async fn refresh(&self) -> Result<PlaybackState> {
        let r = self.refresh_inner().await;
        self.report(r)
    }

    async fn refresh_inner(&self) -> Result<PlaybackState> {
        let token = self.tokens.access_token().await?;
        let fetched = self.api.playback_state(&token).await?;
        let now = self.clock.now_ms();
        let (snapshot, changed) = {
            let mut i = self.lock();
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
            if i.busy == 0 && !i.volume_dirty {
                if let Some(v) = s.device.as_ref().and_then(|d| d.volume_percent) {
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
        let r = async {
            let token = self.tokens.access_token().await?;
            self.api.devices(&token).await
        }
        .await;
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
        let gen = self.bump();
        let _busy = BusyGuard::new(&self.inner);
        self.ensure_fresh().await;
        if !self.is_current(gen) {
            return Ok(());
        }
        let token = self.tokens.access_token().await?;
        let snap = self.snap();
        let device = snap.selected.clone();
        let dev = device.as_deref();
        let target = opts
            .volume
            .map(|v| v.min(100))
            .or(snap.base.or(snap.cur_vol));
        let can_fade = snap.supports_volume && !snap.pending_transfer;
        let mut level = snap.cur_vol;

        // 1. fade down (or pre-silence before a fade-in when nothing is playing)
        if let Some(v) = level.filter(|v| can_fade && *v > 0) {
            if snap.playing && opts.fade_out_ms > 0 {
                if !self.fade(gen, &token, dev, v, 0, opts.fade_out_ms).await? {
                    return Ok(());
                }
                level = Some(0);
            } else if !snap.playing && opts.fade_in_ms > 0 && target.is_some_and(|t| t > 0) {
                self.api.set_volume(&token, dev, 0).await?;
                self.set_cached_volume(0);
                level = Some(0);
            }
        }
        if !self.is_current(gen) {
            return Ok(());
        }

        // 2. device transfer + play
        if snap.pending_transfer {
            if let Some(d) = dev {
                self.api.transfer(&token, d, false).await?;
                self.lock().pending_transfer = false;
            }
        }
        self.play_with_retry(&token, dev, &plan.req).await?;
        {
            let mut i = self.lock();
            if self.is_current(gen) {
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
            }
        }

        // 3. repeat + shuffle
        if !self.is_current(gen) {
            return Ok(());
        }
        self.api.set_repeat(&token, dev, opts.repeat).await?;
        self.lock().state.repeat = opts.repeat;
        if !self.is_current(gen) {
            return Ok(());
        }
        self.api.set_shuffle(&token, dev, opts.shuffle).await?;
        self.lock().state.shuffle = opts.shuffle;

        // 4. fade up / set target volume
        if let Some(t) = target.filter(|_| snap.supports_volume) {
            if !self.is_current(gen) {
                return Ok(());
            }
            let completed = match level {
                Some(l) if can_fade && opts.fade_in_ms > 0 => {
                    self.fade(gen, &token, dev, l, t, opts.fade_in_ms).await?
                }
                Some(l) if l == t => true,
                _ => {
                    self.api.set_volume(&token, dev, t).await?;
                    self.set_cached_volume(t);
                    true
                }
            };
            if !completed {
                return Ok(());
            }
            let mut i = self.lock();
            i.base_volume = Some(t);
            i.volume_dirty = false;
        }
        if self.is_current(gen) {
            self.notify_current();
        }
        Ok(())
    }

    async fn play_with_retry(
        &self,
        token: &str,
        dev: Option<&str>,
        req: &PlayRequest,
    ) -> Result<()> {
        match self.api.play(token, dev, req).await {
            Err(BardicError::NoActiveDevice) => match dev {
                Some(d) => {
                    tracing::info!(device = d, "no active device; transferring and retrying");
                    self.api.transfer(token, d, false).await?;
                    self.lock().pending_transfer = false;
                    self.api.play(token, Some(d), req).await
                }
                None => Err(BardicError::NoActiveDevice),
            },
            r => r,
        }
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

    /// Stepped volume ramp `from → to` over ~`duration_ms`. Returns `Ok(false)` if a newer
    /// transport action superseded it (stops stepping immediately).
    async fn fade(
        &self,
        gen: u64,
        token: &str,
        dev: Option<&str>,
        from: u8,
        to: u8,
        duration_ms: u32,
    ) -> Result<bool> {
        if from == to {
            return Ok(self.is_current(gen));
        }
        self.lock().volume_dirty = true;
        let steps = self.step_count(duration_ms);
        let interval = if steps > 1 {
            duration_ms as u64 / (steps as u64 - 1)
        } else {
            0
        };
        let (from_i, to_i) = (from as i64, to as i64);
        for i in 1..=steps {
            if !self.is_current(gen) {
                return Ok(false);
            }
            let v = (from_i + (to_i - from_i) * i as i64 / steps as i64) as u8;
            match self.api.set_volume(token, dev, v).await {
                Ok(()) => self.set_cached_volume(v),
                Err(BardicError::RateLimited { retry_after_secs }) => {
                    tracing::warn!(
                        retry_after_secs,
                        "rate limited during fade; jumping to target"
                    );
                    if !self.is_current(gen) {
                        return Ok(false);
                    }
                    match self.api.set_volume(token, dev, to).await {
                        Ok(()) => self.set_cached_volume(to),
                        Err(e) => tracing::warn!(error = %e, "could not set fade target volume"),
                    }
                    return Ok(self.is_current(gen));
                }
                Err(e) => return Err(e),
            }
            if i < steps {
                tokio::time::sleep(Duration::from_millis(interval)).await;
            }
        }
        Ok(self.is_current(gen))
    }

    /// GM "panic / silence": fade to 0, pause, then restore the pre-fade volume *without*
    /// resuming (Spotify accepts volume changes while paused), so the next play isn't silent.
    pub async fn fade_out_and_pause(&self, duration_ms: u32) -> Result<()> {
        let r = self.fade_out_and_pause_inner(duration_ms).await;
        self.report(r)
    }

    async fn fade_out_and_pause_inner(&self, duration_ms: u32) -> Result<()> {
        let gen = self.bump();
        let _busy = BusyGuard::new(&self.inner);
        self.ensure_fresh().await;
        let token = self.tokens.access_token().await?;
        let snap = self.snap();
        let dev = self.cmd_device();
        let dev = dev.as_deref();
        let mut restore = None;
        if let Some(v) = snap
            .cur_vol
            .filter(|v| *v > 0 && snap.playing && snap.supports_volume && !snap.pending_transfer)
        {
            let restore_to = snap.base.unwrap_or(v);
            if !self.fade(gen, &token, dev, v, 0, duration_ms).await? {
                return Ok(());
            }
            restore = Some(restore_to);
        }
        if !self.is_current(gen) {
            return Ok(());
        }
        match self.api.pause(&token, dev).await {
            Ok(()) => {}
            // Spotify rejects pausing an already-paused player; harmless for a panic button.
            Err(BardicError::Forbidden(_)) if !snap.playing => {}
            Err(e) => return Err(e),
        }
        self.lock().state.is_playing = false;
        if let Some(r) = restore {
            if self.is_current(gen) {
                self.api.set_volume(&token, dev, r).await?;
                self.set_cached_volume(r);
                let mut i = self.lock();
                i.base_volume = Some(r);
                i.volume_dirty = false;
            }
        }
        self.notify_current();
        Ok(())
    }

    // ------------------------------------------------------------ transport

    pub async fn pause(&self) -> Result<()> {
        self.bump();
        let r = async {
            let token = self.tokens.access_token().await?;
            self.api.pause(&token, self.cmd_device().as_deref()).await?;
            self.lock().state.is_playing = false;
            Ok(())
        }
        .await;
        self.finish(r)
    }

    pub async fn resume(&self) -> Result<()> {
        self.bump();
        let r = async {
            let token = self.tokens.access_token().await?;
            self.api
                .resume(&token, self.cmd_device().as_deref())
                .await?;
            self.lock().state.is_playing = true;
            Ok(())
        }
        .await;
        self.finish(r)
    }

    pub async fn next(&self) -> Result<()> {
        let r = async {
            let token = self.tokens.access_token().await?;
            self.api.next(&token, self.cmd_device().as_deref()).await
        }
        .await;
        self.report(r)
    }

    pub async fn previous(&self) -> Result<()> {
        let r = async {
            let token = self.tokens.access_token().await?;
            self.api
                .previous(&token, self.cmd_device().as_deref())
                .await
        }
        .await;
        self.report(r)
    }

    pub async fn seek(&self, position_ms: u32) -> Result<()> {
        let r = async {
            let token = self.tokens.access_token().await?;
            self.api
                .seek(&token, self.cmd_device().as_deref(), position_ms)
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
            let token = self.tokens.access_token().await?;
            self.api
                .set_repeat(&token, self.cmd_device().as_deref(), mode)
                .await?;
            self.lock().state.repeat = mode;
            Ok(())
        }
        .await;
        self.finish(r)
    }

    pub async fn set_shuffle(&self, on: bool) -> Result<()> {
        let r = async {
            let token = self.tokens.access_token().await?;
            self.api
                .set_shuffle(&token, self.cmd_device().as_deref(), on)
                .await?;
            self.lock().state.shuffle = on;
            Ok(())
        }
        .await;
        self.finish(r)
    }

    /// User volume change: cancels running fades and becomes the new base volume.
    pub async fn set_volume(&self, percent: u8) -> Result<()> {
        self.bump();
        let percent = percent.min(100);
        let r = async {
            let token = self.tokens.access_token().await?;
            self.api
                .set_volume(&token, self.cmd_device().as_deref(), percent)
                .await?;
            self.set_cached_volume(percent);
            let mut i = self.lock();
            i.base_volume = Some(percent);
            i.volume_dirty = false;
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
    /// - `NotAuthenticated`: reported, then idle-rate polling until a refresh succeeds;
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
