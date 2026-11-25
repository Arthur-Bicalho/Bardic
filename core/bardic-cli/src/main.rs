//! `bardic` — developer harness for bardic-core. Exercises the library, the resolver and
//! real Spotify playback from a terminal before the Tauri / Android UIs exist.
//!
//! Spotify commands need `BARDIC_CLIENT_ID` (Client ID from the Spotify dashboard, with
//! redirect URI `http://127.0.0.1:43821/callback`). Data lives in `$XDG_DATA_HOME/bardic`.
use std::path::PathBuf;
use std::sync::Arc;

use bardic_core::auth::{loopback_redirect_uri, AuthConfig, KeyringTokenStore};
use bardic_core::engine::{EngineConfig, EngineObserver};
use bardic_core::model::{
    CueOptions, CueOwner, PlayContext, PlaybackState, RepeatMode, Resolution, TagKind, TrackRef,
    Trigger,
};
use bardic_core::{BardicCore, BardicError, CoreConfig};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const LOGIN_PORT: u16 = 43821;

const USAGE: &str = "\
bardic <command> [args]

Library
  campaigns                                  list campaigns → chapters → scenes
  new-campaign <name>
  new-chapter <campaign_id> <name>
  new-scene <chapter_id> <name>
  default-env <campaign|chapter|scene|global> <id|-> <env_id|->
  tags                                       list situations & emotions
  envs                                       list ambience environments + tracks
  cues <global|campaign|chapter|scene> [id]
  new-cue <global|campaign|chapter|scene> <id|-> <name> [--situation ID] [--emotion ID]
          [--repeat off|track|context] [--shuffle] [--volume N]
  add-item <cue_id> <spotify_uri> [title]
  add-ambience <env_id> <spotify_uri> [title] [--emotion ID]

Resolve / play   (context flags: --campaign ID --chapter ID --scene ID)
  preview [--situation ID] [--emotion ID] [context]   show what would play
  trigger [--situation ID] [--emotion ID] [context]   resolve and play
  play-cue <cue_id>
  ambience <env_id> [--emotion ID]

Spotify   (needs BARDIC_CLIENT_ID)
  login | logout | status | devices [--select ID] | search <query>
  pause | resume | next | prev | silence [ms] | loop <off|track|context>
  shuffle <on|off> | volume <0-100> | watch      (watch = live poll, Ctrl+C to stop)
";

struct PrintObserver;

impl EngineObserver for PrintObserver {
    fn on_playback_changed(&self, s: &PlaybackState) {
        println!("♪ {}", describe(s));
    }
    fn on_error(&self, e: &BardicError) {
        eprintln!("! {e}");
    }
}

fn describe(s: &PlaybackState) -> String {
    let what = match (&s.track_name, &s.artist_name) {
        (Some(t), Some(a)) => format!("{t} — {a}"),
        (Some(t), None) => t.clone(),
        _ => "(nothing)".into(),
    };
    let dev = s.device.as_ref().map(|d| d.name.as_str()).unwrap_or("no device");
    format!(
        "{} {what} [{}/{}s] repeat={:?} shuffle={} on {dev}{}",
        if s.is_playing { "▶" } else { "⏸" },
        s.progress_ms / 1000,
        s.duration_ms / 1000,
        s.repeat,
        s.shuffle,
        s.active_cue_id.map(|c| format!(" (cue {c})")).unwrap_or_default()
    )
}

fn data_dir() -> PathBuf {
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("bardic")
}

/// Minimal `--flag value` / `--switch` parser over the args after the command.
struct Args {
    positional: Vec<String>,
    flags: Vec<(String, Option<String>)>,
}

impl Args {
    fn parse(raw: &[String]) -> Self {
        const SWITCHES: &[&str] = &["--shuffle"];
        let (mut positional, mut flags) = (Vec::new(), Vec::new());
        let mut it = raw.iter();
        while let Some(a) = it.next() {
            if a.starts_with("--") {
                let val = if SWITCHES.contains(&a.as_str()) { None } else { it.next().cloned() };
                flags.push((a.clone(), val));
            } else {
                positional.push(a.clone());
            }
        }
        Self { positional, flags }
    }
    fn pos(&self, i: usize, name: &str) -> Result<&str, BardicError> {
        self.positional
            .get(i)
            .map(String::as_str)
            .ok_or_else(|| BardicError::Invalid(format!("missing <{name}>")))
    }
    fn flag(&self, name: &str) -> Option<&str> {
        self.flags.iter().find(|(k, _)| k == name).and_then(|(_, v)| v.as_deref())
    }
    fn has(&self, name: &str) -> bool {
        self.flags.iter().any(|(k, _)| k == name)
    }
    fn id_flag(&self, name: &str) -> Result<Option<i64>, BardicError> {
        self.flag(name).map(parse_id).transpose()
    }
    fn context(&self) -> Result<PlayContext, BardicError> {
        Ok(PlayContext {
            campaign_id: self.id_flag("--campaign")?,
            chapter_id: self.id_flag("--chapter")?,
            scene_id: self.id_flag("--scene")?,
        })
    }
    fn trigger(&self) -> Result<Trigger, BardicError> {
        Ok(Trigger { situation_id: self.id_flag("--situation")?, emotion_id: self.id_flag("--emotion")? })
    }
}

fn parse_id(s: &str) -> Result<i64, BardicError> {
    s.parse().map_err(|_| BardicError::Invalid(format!("not an id: {s}")))
}

fn opt_id(s: &str) -> Result<Option<i64>, BardicError> {
    if s == "-" { Ok(None) } else { parse_id(s).map(Some) }
}

fn owner(level: &str, id: Option<i64>) -> Result<CueOwner, BardicError> {
    let need = || id.ok_or_else(|| BardicError::Invalid(format!("{level} needs an id")));
    Ok(match level {
        "global" => CueOwner::Global,
        "campaign" => CueOwner::Campaign(need()?),
        "chapter" => CueOwner::Chapter(need()?),
        "scene" => CueOwner::Scene(need()?),
        other => return Err(BardicError::Invalid(format!("unknown level {other}"))),
    })
}

fn track_ref(uri: &str, title: Option<&str>) -> TrackRef {
    TrackRef {
        spotify_uri: uri.to_string(),
        title: title.unwrap_or(uri).to_string(),
        subtitle: String::new(),
        art_url: None,
        duration_ms: None,
        start_offset_ms: 0,
    }
}

fn print_resolution(r: &Resolution) {
    match r {
        Resolution::Cue { cue, matched } => {
            println!("cue {} \"{}\" ({matched:?}, {} item(s))", cue.id, cue.name, cue.items.len())
        }
        Resolution::Ambience { environment_id, from, tracks, .. } => {
            println!("ambience env {environment_id} from {from:?} level ({} track(s))", tracks.len())
        }
        Resolution::Nothing => println!("nothing to play (no cue, no ambience configured)"),
    }
}

fn parse_repeat(s: &str) -> Result<RepeatMode, BardicError> {
    match s {
        "off" | "track" | "context" => Ok(RepeatMode::from_spotify(s)),
        _ => Err(BardicError::Invalid("repeat must be off|track|context".into())),
    }
}

/// One-shot loopback HTTP listener for the OAuth redirect.
async fn wait_for_callback() -> Result<String, BardicError> {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", LOGIN_PORT))
        .await
        .map_err(|e| BardicError::Network(format!("cannot listen on port {LOGIN_PORT}: {e}")))?;
    loop {
        let (mut sock, _) = listener.accept().await.map_err(|e| BardicError::Network(e.to_string()))?;
        let mut buf = vec![0u8; 8192];
        let n = sock.read(&mut buf).await.map_err(|e| BardicError::Network(e.to_string()))?;
        let req = String::from_utf8_lossy(&buf[..n]);
        let path = req.lines().next().and_then(|l| l.split_whitespace().nth(1)).unwrap_or("");
        if !path.starts_with("/callback") {
            let _ = sock.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n").await;
            continue;
        }
        let body = "<html><body style=\"font-family:sans-serif\"><h2>Bardic is connected.</h2>You can close this tab.</body></html>";
        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let _ = sock.write_all(resp.as_bytes()).await;
        return Ok(path.to_string());
    }
}

async fn run(cmd: &str, args: Args) -> Result<(), BardicError> {
    let client_id = std::env::var("BARDIC_CLIENT_ID").unwrap_or_default();
    let config = CoreConfig {
        db_path: Some(data_dir().join("bardic.db")),
        auth: AuthConfig::new(client_id.clone(), loopback_redirect_uri(LOGIN_PORT)),
        engine: EngineConfig::default(),
    };
    let core = BardicCore::new(config, Arc::new(KeyringTokenStore::new()), Arc::new(PrintObserver))?;
    let lib = core.library();
    let needs_spotify = matches!(
        cmd,
        "login" | "status" | "devices" | "search" | "trigger" | "play-cue" | "ambience" | "pause"
            | "resume" | "next" | "prev" | "silence" | "loop" | "shuffle" | "volume" | "watch"
    );
    if needs_spotify && client_id.is_empty() {
        return Err(BardicError::Invalid("set BARDIC_CLIENT_ID to your Spotify app's Client ID".into()));
    }

    match cmd {
        // ------------------------------------------------ library
        "campaigns" => {
            for c in lib.list_campaigns()? {
                println!("[{}] {}{}", c.id, c.name, env_suffix(c.default_environment_id));
                for ch in lib.list_chapters(c.id)? {
                    println!("   [{}] {}{}", ch.id, ch.name, env_suffix(ch.default_environment_id));
                    for s in lib.list_scenes(ch.id)? {
                        println!("      [{}] {}{}", s.id, s.name, env_suffix(s.default_environment_id));
                    }
                }
            }
        }
        "new-campaign" => println!("{:?}", lib.create_campaign(args.pos(0, "name")?, "")?),
        "new-chapter" => println!("{:?}", lib.create_chapter(parse_id(args.pos(0, "campaign_id")?)?, args.pos(1, "name")?, "")?),
        "new-scene" => println!("{:?}", lib.create_scene(parse_id(args.pos(0, "chapter_id")?)?, args.pos(1, "name")?, "")?),
        "default-env" => {
            let env = opt_id(args.pos(2, "env_id")?)?;
            match args.pos(0, "level")? {
                "global" => lib.set_global_default_environment(env)?,
                "campaign" => {
                    let mut c = lib.get_campaign(parse_id(args.pos(1, "id")?)?)?;
                    c.default_environment_id = env;
                    lib.update_campaign(&c)?
                }
                "chapter" => {
                    let mut c = lib.get_chapter(parse_id(args.pos(1, "id")?)?)?;
                    c.default_environment_id = env;
                    lib.update_chapter(&c)?
                }
                "scene" => {
                    let mut s = lib.get_scene(parse_id(args.pos(1, "id")?)?)?;
                    s.default_environment_id = env;
                    lib.update_scene(&s)?
                }
                other => return Err(BardicError::Invalid(format!("unknown level {other}"))),
            }
            println!("ok");
        }
        "tags" => {
            for kind in [TagKind::Situation, TagKind::Emotion] {
                println!("{}s:", kind.as_str());
                for t in lib.list_tags(Some(kind))? {
                    println!("  [{}] {} ({}, {})", t.id, t.name, t.icon, t.color);
                }
            }
        }
        "envs" => {
            let tracks = lib.list_ambience_tracks(None)?;
            let global = lib.global_default_environment()?;
            for e in lib.list_environments()? {
                let star = if Some(e.id) == global { " ★ global default" } else { "" };
                println!("[{}] {} ({}){star}", e.id, e.name, e.icon);
                for t in tracks.iter().filter(|t| t.environment_id == e.id) {
                    println!("     - {} {}", t.track.spotify_uri, t.track.title);
                }
                for l in lib.list_ambience_layers(e.id)? {
                    println!("     ~ layer {} ({})", l.name, l.file_path);
                }
            }
        }
        "cues" => {
            let id = args.positional.get(1).map(|s| parse_id(s)).transpose()?;
            for c in lib.list_cues(owner(args.pos(0, "level")?, id)?)? {
                println!(
                    "[{}] {} situation={:?} emotion={:?} repeat={:?} items={}",
                    c.id, c.name, c.situation_id, c.emotion_id, c.options.repeat, c.items.len()
                );
                for i in &c.items {
                    println!("     - {} {}", i.track.spotify_uri, i.track.title);
                }
            }
        }
        "new-cue" => {
            let own = owner(args.pos(0, "level")?, opt_id(args.pos(1, "id")?)?)?;
            let mut opts = CueOptions::default();
            if let Some(r) = args.flag("--repeat") {
                opts.repeat = parse_repeat(r)?;
            }
            opts.shuffle = args.has("--shuffle");
            opts.volume = args
                .flag("--volume")
                .map(|v| v.parse::<u8>().map_err(|_| BardicError::Invalid("volume 0-100".into())))
                .transpose()?;
            let cue = lib.create_cue(own, args.pos(2, "name")?, args.id_flag("--situation")?, args.id_flag("--emotion")?, &opts)?;
            println!("created cue {}", cue.id);
        }
        "add-item" => {
            let item = lib.add_cue_item(parse_id(args.pos(0, "cue_id")?)?, &track_ref(args.pos(1, "uri")?, args.positional.get(2).map(String::as_str)))?;
            println!("added item {}", item.id);
        }
        "add-ambience" => {
            let t = lib.add_ambience_track(
                parse_id(args.pos(0, "env_id")?)?,
                &track_ref(args.pos(1, "uri")?, args.positional.get(2).map(String::as_str)),
                args.id_flag("--emotion")?,
            )?;
            println!("added ambience track {}", t.id);
        }
        // ------------------------------------------------ resolve / play
        "preview" => {
            core.set_context(args.context()?)?;
            print_resolution(&core.preview(args.trigger()?)?);
        }
        "trigger" => {
            core.set_context(args.context()?)?;
            print_resolution(&core.trigger(args.trigger()?).await?);
        }
        "play-cue" => core.play_cue(parse_id(args.pos(0, "cue_id")?)?).await?,
        "ambience" => core.play_environment(parse_id(args.pos(0, "env_id")?)?, args.id_flag("--emotion")?).await?,
        // ------------------------------------------------ spotify
        "login" => {
            let url = core.login_url()?;
            println!("Open this URL to log in to Spotify:\n\n{url}\n\nWaiting for the redirect on port {LOGIN_PORT}…");
            let callback = wait_for_callback().await?;
            core.complete_login(&callback).await?;
            println!("Logged in.");
        }
        "logout" => {
            core.logout()?;
            println!("Logged out.");
        }
        "status" => println!("{}", describe(&core.engine().refresh().await?)),
        "devices" => {
            if let Some(id) = args.flag("--select") {
                core.engine().select_device(Some(id.to_string()));
            }
            for d in core.engine().devices().await? {
                println!("{} {} [{}] id={}", if d.is_active { "*" } else { " " }, d.name, d.device_type, d.id);
            }
        }
        "search" => {
            let r = core.search(&args.positional.join(" "), 5).await?;
            for (label, list) in [("tracks", &r.tracks), ("playlists", &r.playlists), ("albums", &r.albums)] {
                println!("{label}:");
                for t in list {
                    println!("  {}  {} — {}", t.spotify_uri, t.title, t.subtitle);
                }
            }
        }
        "pause" => core.engine().pause().await?,
        "resume" => core.engine().resume().await?,
        "next" => core.engine().next().await?,
        "prev" => core.engine().previous().await?,
        "silence" => {
            let ms = args.positional.first().map(|s| s.parse().unwrap_or(1500)).unwrap_or(1500);
            core.silence(ms).await?
        }
        "loop" => core.engine().set_repeat(parse_repeat(args.pos(0, "mode")?)?).await?,
        "shuffle" => core.engine().set_shuffle(args.pos(0, "on|off")? == "on").await?,
        "volume" => {
            let v = args.pos(0, "0-100")?.parse::<u8>().map_err(|_| BardicError::Invalid("volume 0-100".into()))?;
            core.engine().set_volume(v.min(100)).await?
        }
        "watch" => {
            core.engine().refresh().await?;
            core.start_polling(bardic_core::engine::PollMode::Active);
            tokio::signal::ctrl_c().await.ok();
            core.stop_polling();
        }
        _ => {
            print!("{USAGE}");
            if cmd != "help" {
                return Err(BardicError::Invalid(format!("unknown command {cmd}")));
            }
        }
    }
    Ok(())
}

fn env_suffix(env: Option<i64>) -> String {
    env.map(|e| format!("  (ambience env {e})")).unwrap_or_default()
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let Some((cmd, rest)) = argv.split_first() else {
        print!("{USAGE}");
        return;
    };
    if let Err(e) = run(cmd, Args::parse(rest)).await {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}
