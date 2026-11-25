//! SQLite library: schema, migrations, CRUD, seed data. OWNER: db agent.
//!
//! One `rusqlite::Connection` sits behind a `Mutex`, giving a sync API that is
//! `Send + Sync`. Foreign keys are enforced: deleting a parent cascades to cues,
//! cue items, ambience tracks and layers, or nulls references (default
//! environments, cue tags).

use std::path::Path;
use std::sync::{Mutex, MutexGuard};

use rusqlite::{params, Connection, OptionalExtension, Params, Row};
use rusqlite_migration::{Migrations, M};

use crate::error::{BardicError, Result};
use crate::model::{
    AmbienceLayer, AmbienceTrack, Campaign, Chapter, Cue, CueItem, CueOptions, CueOwner,
    Environment, Id, PlayContext, RepeatMode, ResolverInput, Scene, Tag, TagKind, TrackRef,
};

const SCHEMA: &str = r#"
CREATE TABLE settings (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE TABLE environments (
    id       INTEGER PRIMARY KEY,
    name     TEXT NOT NULL,
    icon     TEXT NOT NULL,
    color    TEXT NOT NULL,
    position INTEGER NOT NULL
);

CREATE TABLE campaigns (
    id                     INTEGER PRIMARY KEY,
    name                   TEXT NOT NULL,
    description            TEXT NOT NULL DEFAULT '',
    default_environment_id INTEGER REFERENCES environments(id) ON DELETE SET NULL,
    position               INTEGER NOT NULL
);

CREATE TABLE chapters (
    id                     INTEGER PRIMARY KEY,
    campaign_id            INTEGER NOT NULL REFERENCES campaigns(id) ON DELETE CASCADE,
    name                   TEXT NOT NULL,
    description            TEXT NOT NULL DEFAULT '',
    default_environment_id INTEGER REFERENCES environments(id) ON DELETE SET NULL,
    position               INTEGER NOT NULL
);
CREATE INDEX chapters_by_campaign ON chapters(campaign_id, position);

CREATE TABLE scenes (
    id                     INTEGER PRIMARY KEY,
    chapter_id             INTEGER NOT NULL REFERENCES chapters(id) ON DELETE CASCADE,
    name                   TEXT NOT NULL,
    description            TEXT NOT NULL DEFAULT '',
    default_environment_id INTEGER REFERENCES environments(id) ON DELETE SET NULL,
    position               INTEGER NOT NULL
);
CREATE INDEX scenes_by_chapter ON scenes(chapter_id, position);

CREATE TABLE tags (
    id       INTEGER PRIMARY KEY,
    kind     TEXT NOT NULL CHECK (kind IN ('situation', 'emotion')),
    name     TEXT NOT NULL,
    color    TEXT NOT NULL,
    icon     TEXT NOT NULL,
    position INTEGER NOT NULL
);

CREATE TABLE cues (
    id           INTEGER PRIMARY KEY,
    owner_level  TEXT NOT NULL CHECK (owner_level IN ('global', 'campaign', 'chapter', 'scene')),
    campaign_id  INTEGER REFERENCES campaigns(id) ON DELETE CASCADE,
    chapter_id   INTEGER REFERENCES chapters(id) ON DELETE CASCADE,
    scene_id     INTEGER REFERENCES scenes(id) ON DELETE CASCADE,
    name         TEXT NOT NULL,
    situation_id INTEGER REFERENCES tags(id) ON DELETE SET NULL,
    emotion_id   INTEGER REFERENCES tags(id) ON DELETE SET NULL,
    repeat_mode  TEXT NOT NULL CHECK (repeat_mode IN ('off', 'track', 'context')),
    shuffle      INTEGER NOT NULL CHECK (shuffle IN (0, 1)),
    volume       INTEGER CHECK (volume IS NULL OR volume BETWEEN 0 AND 100),
    fade_in_ms   INTEGER NOT NULL,
    fade_out_ms  INTEGER NOT NULL,
    position     INTEGER NOT NULL,
    CHECK (
        (owner_level = 'global'   AND campaign_id IS NULL     AND chapter_id IS NULL     AND scene_id IS NULL)
     OR (owner_level = 'campaign' AND campaign_id IS NOT NULL AND chapter_id IS NULL     AND scene_id IS NULL)
     OR (owner_level = 'chapter'  AND campaign_id IS NULL     AND chapter_id IS NOT NULL AND scene_id IS NULL)
     OR (owner_level = 'scene'    AND campaign_id IS NULL     AND chapter_id IS NULL     AND scene_id IS NOT NULL)
    )
);
CREATE INDEX cues_by_owner ON cues(owner_level, campaign_id, chapter_id, scene_id, position);

CREATE TABLE cue_items (
    id              INTEGER PRIMARY KEY,
    cue_id          INTEGER NOT NULL REFERENCES cues(id) ON DELETE CASCADE,
    spotify_uri     TEXT NOT NULL,
    title           TEXT NOT NULL,
    subtitle        TEXT NOT NULL DEFAULT '',
    art_url         TEXT,
    duration_ms     INTEGER,
    start_offset_ms INTEGER NOT NULL DEFAULT 0,
    position        INTEGER NOT NULL
);
CREATE INDEX cue_items_by_cue ON cue_items(cue_id, position);

CREATE TABLE ambience_tracks (
    id              INTEGER PRIMARY KEY,
    environment_id  INTEGER NOT NULL REFERENCES environments(id) ON DELETE CASCADE,
    spotify_uri     TEXT NOT NULL,
    title           TEXT NOT NULL,
    subtitle        TEXT NOT NULL DEFAULT '',
    art_url         TEXT,
    duration_ms     INTEGER,
    start_offset_ms INTEGER NOT NULL DEFAULT 0,
    emotion_id      INTEGER REFERENCES tags(id) ON DELETE SET NULL,
    position        INTEGER NOT NULL
);
CREATE INDEX ambience_tracks_by_env ON ambience_tracks(environment_id, position);

CREATE TABLE ambience_layers (
    id             INTEGER PRIMARY KEY,
    environment_id INTEGER NOT NULL REFERENCES environments(id) ON DELETE CASCADE,
    name           TEXT NOT NULL,
    file_path      TEXT NOT NULL,
    default_volume REAL NOT NULL CHECK (default_volume BETWEEN 0.0 AND 1.0),
    position       INTEGER NOT NULL
);
CREATE INDEX ambience_layers_by_env ON ambience_layers(environment_id, position);
"#;

const GLOBAL_DEFAULT_ENV_KEY: &str = "global_default_environment";

const SEED_SITUATIONS: &[(&str, &str, &str)] = &[
    // (name, icon, color)
    ("Combat", "swords", "#C0392B"),
    ("Exploration", "compass", "#27AE60"),
    ("Tavern", "beer", "#D35400"),
    ("Travel", "horse", "#8E6E53"),
    ("Rest", "campfire", "#E67E22"),
    ("Boss", "skull", "#6C1B1B"),
    ("Stealth", "mask", "#34495E"),
    ("Social", "chat", "#2980B9"),
];

const SEED_EMOTIONS: &[(&str, &str, &str)] = &[
    ("Tension", "pulse", "#8E44AD"),
    ("Sorrow", "tear", "#5D6D7E"),
    ("Triumph", "crown", "#F1C40F"),
    ("Mystery", "eye", "#1ABC9C"),
    ("Dread", "ghost", "#2C3E50"),
    ("Joy", "sun", "#F39C12"),
    ("Wonder", "sparkles", "#3498DB"),
];

const SEED_ENVIRONMENTS: &[(&str, &str, &str)] = &[
    ("Forest", "tree", "#2E7D32"),
    ("Cave", "mountain", "#5D4037"),
    ("City", "castle", "#607D8B"),
    ("Tavern", "beer", "#A1887F"),
    ("Sea", "waves", "#0277BD"),
    ("Storm", "cloud-lightning", "#37474F"),
    ("Night", "moon", "#1A237E"),
    ("Temple", "temple", "#B8860B"),
    ("Desert", "sun", "#E1A95F"),
    ("Snow", "snowflake", "#B0BEC5"),
];

const CAMPAIGN_COLS: &str = "id, name, description, default_environment_id, position";
const CHAPTER_COLS: &str =
    "id, campaign_id, name, description, default_environment_id, position";
const SCENE_COLS: &str = "id, chapter_id, name, description, default_environment_id, position";
const TAG_COLS: &str = "id, kind, name, color, icon, position";
const ENV_COLS: &str = "id, name, icon, color, position";
const CUE_COLS: &str = "id, owner_level, campaign_id, chapter_id, scene_id, name, situation_id, \
     emotion_id, repeat_mode, shuffle, volume, fade_in_ms, fade_out_ms, position";
const ITEM_COLS: &str =
    "id, cue_id, spotify_uri, title, subtitle, art_url, duration_ms, start_offset_ms, position";
const AMBIENCE_TRACK_COLS: &str = "id, environment_id, spotify_uri, title, subtitle, art_url, \
     duration_ms, start_offset_ms, emotion_id, position";
const LAYER_COLS: &str = "id, environment_id, name, file_path, default_volume, position";

fn migrations() -> Migrations<'static> {
    Migrations::new(vec![M::up(SCHEMA)])
}

/// The campaign library: one SQLite database, sync API.
pub struct Library {
    conn: Mutex<Connection>,
}

impl Library {
    /// Open (or create) a database file, migrate it and seed defaults.
    pub fn open(path: &Path) -> Result<Library> {
        Self::from_connection(Connection::open(path)?)
    }

    /// In-memory database (tests, previews). Migrated and seeded like a file DB.
    pub fn open_in_memory() -> Result<Library> {
        Self::from_connection(Connection::open_in_memory()?)
    }

    fn from_connection(mut conn: Connection) -> Result<Library> {
        conn.execute_batch("PRAGMA foreign_keys = ON;")?;
        migrations().to_latest(&mut conn)?;
        let lib = Library { conn: Mutex::new(conn) };
        lib.seed_defaults()?;
        Ok(lib)
    }

    fn lock(&self) -> Result<MutexGuard<'_, Connection>> {
        self.conn
            .lock()
            .map_err(|_| BardicError::Db("connection mutex poisoned".into()))
    }

    // ------------------------------------------------------------ campaigns

    pub fn create_campaign(&self, name: &str, description: &str) -> Result<Campaign> {
        let conn = self.lock()?;
        let position = next_position(&conn, "SELECT COALESCE(MAX(position) + 1, 0) FROM campaigns", [])?;
        conn.execute(
            "INSERT INTO campaigns (name, description, position) VALUES (?1, ?2, ?3)",
            params![name, description, position],
        )?;
        fetch_campaign(&conn, conn.last_insert_rowid())
    }

    pub fn get_campaign(&self, id: Id) -> Result<Campaign> {
        let conn = self.lock()?;
        fetch_campaign(&conn, id)
    }

    pub fn list_campaigns(&self) -> Result<Vec<Campaign>> {
        let conn = self.lock()?;
        query_all(
            &conn,
            &format!("SELECT {CAMPAIGN_COLS} FROM campaigns ORDER BY position, id"),
            [],
            campaign_from_row,
        )
    }

    pub fn update_campaign(&self, c: &Campaign) -> Result<()> {
        let conn = self.lock()?;
        let n = conn.execute(
            "UPDATE campaigns SET name = ?1, description = ?2, default_environment_id = ?3, position = ?4 WHERE id = ?5",
            params![c.name, c.description, c.default_environment_id, c.position, c.id],
        )?;
        expect_one(n, "campaign", c.id)
    }

    pub fn delete_campaign(&self, id: Id) -> Result<()> {
        let conn = self.lock()?;
        let n = conn.execute("DELETE FROM campaigns WHERE id = ?1", params![id])?;
        expect_one(n, "campaign", id)
    }

    // ------------------------------------------------------------ chapters

    pub fn create_chapter(&self, campaign_id: Id, name: &str, description: &str) -> Result<Chapter> {
        let conn = self.lock()?;
        let position = next_position(
            &conn,
            "SELECT COALESCE(MAX(position) + 1, 0) FROM chapters WHERE campaign_id = ?1",
            params![campaign_id],
        )?;
        conn.execute(
            "INSERT INTO chapters (campaign_id, name, description, position) VALUES (?1, ?2, ?3, ?4)",
            params![campaign_id, name, description, position],
        )?;
        fetch_chapter(&conn, conn.last_insert_rowid())
    }

    pub fn get_chapter(&self, id: Id) -> Result<Chapter> {
        let conn = self.lock()?;
        fetch_chapter(&conn, id)
    }

    pub fn list_chapters(&self, campaign_id: Id) -> Result<Vec<Chapter>> {
        let conn = self.lock()?;
        query_all(
            &conn,
            &format!("SELECT {CHAPTER_COLS} FROM chapters WHERE campaign_id = ?1 ORDER BY position, id"),
            params![campaign_id],
            chapter_from_row,
        )
    }

    /// Updates name, description, default environment and position. `campaign_id` is not changed.
    pub fn update_chapter(&self, c: &Chapter) -> Result<()> {
        let conn = self.lock()?;
        let n = conn.execute(
            "UPDATE chapters SET name = ?1, description = ?2, default_environment_id = ?3, position = ?4 WHERE id = ?5",
            params![c.name, c.description, c.default_environment_id, c.position, c.id],
        )?;
        expect_one(n, "chapter", c.id)
    }

    pub fn delete_chapter(&self, id: Id) -> Result<()> {
        let conn = self.lock()?;
        let n = conn.execute("DELETE FROM chapters WHERE id = ?1", params![id])?;
        expect_one(n, "chapter", id)
    }

    // ------------------------------------------------------------ scenes

    pub fn create_scene(&self, chapter_id: Id, name: &str, description: &str) -> Result<Scene> {
        let conn = self.lock()?;
        let position = next_position(
            &conn,
            "SELECT COALESCE(MAX(position) + 1, 0) FROM scenes WHERE chapter_id = ?1",
            params![chapter_id],
        )?;
        conn.execute(
            "INSERT INTO scenes (chapter_id, name, description, position) VALUES (?1, ?2, ?3, ?4)",
            params![chapter_id, name, description, position],
        )?;
        fetch_scene(&conn, conn.last_insert_rowid())
    }

    pub fn get_scene(&self, id: Id) -> Result<Scene> {
        let conn = self.lock()?;
        fetch_scene(&conn, id)
    }

    pub fn list_scenes(&self, chapter_id: Id) -> Result<Vec<Scene>> {
        let conn = self.lock()?;
        query_all(
            &conn,
            &format!("SELECT {SCENE_COLS} FROM scenes WHERE chapter_id = ?1 ORDER BY position, id"),
            params![chapter_id],
            scene_from_row,
        )
    }

    /// Updates name, description, default environment and position. `chapter_id` is not changed.
    pub fn update_scene(&self, s: &Scene) -> Result<()> {
        let conn = self.lock()?;
        let n = conn.execute(
            "UPDATE scenes SET name = ?1, description = ?2, default_environment_id = ?3, position = ?4 WHERE id = ?5",
            params![s.name, s.description, s.default_environment_id, s.position, s.id],
        )?;
        expect_one(n, "scene", s.id)
    }

    pub fn delete_scene(&self, id: Id) -> Result<()> {
        let conn = self.lock()?;
        let n = conn.execute("DELETE FROM scenes WHERE id = ?1", params![id])?;
        expect_one(n, "scene", id)
    }

    // ------------------------------------------------------------ tags

    /// New tags get the next position within their kind.
    pub fn create_tag(&self, kind: TagKind, name: &str, color: &str, icon: &str) -> Result<Tag> {
        check_color(color)?;
        let conn = self.lock()?;
        let position = next_position(
            &conn,
            "SELECT COALESCE(MAX(position) + 1, 0) FROM tags WHERE kind = ?1",
            params![kind.as_str()],
        )?;
        conn.execute(
            "INSERT INTO tags (kind, name, color, icon, position) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![kind.as_str(), name, color, icon, position],
        )?;
        fetch_one(&conn, &format!("SELECT {TAG_COLS} FROM tags WHERE id = ?1"), conn.last_insert_rowid(), "tag", tag_from_row)
    }

    /// Ordered by kind, then position. `None` lists both kinds.
    pub fn list_tags(&self, kind: Option<TagKind>) -> Result<Vec<Tag>> {
        let conn = self.lock()?;
        query_all(
            &conn,
            &format!(
                "SELECT {TAG_COLS} FROM tags WHERE ?1 IS NULL OR kind = ?1 ORDER BY kind, position, id"
            ),
            params![kind.map(TagKind::as_str)],
            tag_from_row,
        )
    }

    /// Updates name, color, icon and position. The tag's kind is immutable.
    pub fn update_tag(&self, t: &Tag) -> Result<()> {
        check_color(&t.color)?;
        let conn = self.lock()?;
        let n = conn.execute(
            "UPDATE tags SET name = ?1, color = ?2, icon = ?3, position = ?4 WHERE id = ?5",
            params![t.name, t.color, t.icon, t.position, t.id],
        )?;
        expect_one(n, "tag", t.id)
    }

    /// Cues referencing the tag keep existing with the tag cleared (ON DELETE SET NULL).
    pub fn delete_tag(&self, id: Id) -> Result<()> {
        let conn = self.lock()?;
        let n = conn.execute("DELETE FROM tags WHERE id = ?1", params![id])?;
        expect_one(n, "tag", id)
    }

    // ------------------------------------------------------------ cues

    /// New cues get the next position within their owner.
    pub fn create_cue(
        &self,
        owner: CueOwner,
        name: &str,
        situation_id: Option<Id>,
        emotion_id: Option<Id>,
        options: &CueOptions,
    ) -> Result<Cue> {
        check_cue_options(options)?;
        let conn = self.lock()?;
        let (level, campaign, chapter, scene) = owner_parts(owner);
        let position = next_position(
            &conn,
            "SELECT COALESCE(MAX(position) + 1, 0) FROM cues WHERE owner_level = ?1 \
             AND campaign_id IS ?2 AND chapter_id IS ?3 AND scene_id IS ?4",
            params![level, campaign, chapter, scene],
        )?;
        conn.execute(
            "INSERT INTO cues (owner_level, campaign_id, chapter_id, scene_id, name, situation_id, \
             emotion_id, repeat_mode, shuffle, volume, fade_in_ms, fade_out_ms, position) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            params![
                level,
                campaign,
                chapter,
                scene,
                name,
                situation_id,
                emotion_id,
                options.repeat.as_spotify(),
                options.shuffle,
                options.volume.map(i64::from),
                options.fade_in_ms,
                options.fade_out_ms,
                position,
            ],
        )?;
        load_cue(&conn, conn.last_insert_rowid())
    }

    /// Cue with its items loaded in position order.
    pub fn get_cue(&self, id: Id) -> Result<Cue> {
        let conn = self.lock()?;
        load_cue(&conn, id)
    }

    /// Cues owned by exactly this owner, ordered by position, with items.
    pub fn list_cues(&self, owner: CueOwner) -> Result<Vec<Cue>> {
        let conn = self.lock()?;
        list_cues_on(&conn, owner)
    }

    /// Updates name, tags, options and position. Owner and items are not changed;
    /// use `add_cue_item`, `remove_cue_item` and `reorder_cue_items` for items.
    pub fn update_cue(&self, cue: &Cue) -> Result<()> {
        check_cue_options(&cue.options)?;
        let conn = self.lock()?;
        let o = &cue.options;
        let n = conn.execute(
            "UPDATE cues SET name = ?1, situation_id = ?2, emotion_id = ?3, repeat_mode = ?4, \
             shuffle = ?5, volume = ?6, fade_in_ms = ?7, fade_out_ms = ?8, position = ?9 WHERE id = ?10",
            params![
                cue.name,
                cue.situation_id,
                cue.emotion_id,
                o.repeat.as_spotify(),
                o.shuffle,
                o.volume.map(i64::from),
                o.fade_in_ms,
                o.fade_out_ms,
                cue.position,
                cue.id,
            ],
        )?;
        expect_one(n, "cue", cue.id)
    }

    pub fn delete_cue(&self, id: Id) -> Result<()> {
        let conn = self.lock()?;
        let n = conn.execute("DELETE FROM cues WHERE id = ?1", params![id])?;
        expect_one(n, "cue", id)
    }

    // ------------------------------------------------------------ cue items

    /// Appends a track to the cue; it gets the next position.
    pub fn add_cue_item(&self, cue_id: Id, track: &TrackRef) -> Result<CueItem> {
        let conn = self.lock()?;
        let position = next_position(
            &conn,
            "SELECT COALESCE(MAX(position) + 1, 0) FROM cue_items WHERE cue_id = ?1",
            params![cue_id],
        )?;
        conn.execute(
            "INSERT INTO cue_items (cue_id, spotify_uri, title, subtitle, art_url, duration_ms, \
             start_offset_ms, position) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                cue_id,
                track.spotify_uri,
                track.title,
                track.subtitle,
                track.art_url,
                track.duration_ms,
                track.start_offset_ms,
                position,
            ],
        )?;
        let item_id = conn.last_insert_rowid();
        fetch_one(
            &conn,
            &format!("SELECT {ITEM_COLS} FROM cue_items WHERE id = ?1"),
            item_id,
            "cue item",
            cue_item_from_row,
        )
    }

    pub fn remove_cue_item(&self, id: Id) -> Result<()> {
        let conn = self.lock()?;
        let n = conn.execute("DELETE FROM cue_items WHERE id = ?1", params![id])?;
        expect_one(n, "cue item", id)
    }

    /// Sets the item order to `ordered_ids`. The ids must be exactly the cue's
    /// current items (no missing, extra or duplicate ids), otherwise `Invalid`.
    pub fn reorder_cue_items(&self, cue_id: Id, ordered_ids: &[Id]) -> Result<()> {
        let mut conn = self.lock()?;
        let tx = conn.transaction()?;
        let cue_exists: i64 =
            tx.query_row("SELECT COUNT(*) FROM cues WHERE id = ?1", params![cue_id], |r| r.get(0))?;
        if cue_exists == 0 {
            return Err(BardicError::NotFound(format!("cue {cue_id}")));
        }
        let mut current: Vec<Id> = query_all(
            &tx,
            "SELECT id FROM cue_items WHERE cue_id = ?1",
            params![cue_id],
            |r: &Row<'_>| Ok(r.get::<_, Id>(0)?),
        )?;
        let mut wanted = ordered_ids.to_vec();
        current.sort_unstable();
        wanted.sort_unstable();
        if current != wanted {
            return Err(BardicError::Invalid(format!(
                "ordered_ids must be exactly the items of cue {cue_id}"
            )));
        }
        for (pos, item_id) in ordered_ids.iter().enumerate() {
            tx.execute(
                "UPDATE cue_items SET position = ?1 WHERE id = ?2 AND cue_id = ?3",
                params![pos as i64, item_id, cue_id],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    // ------------------------------------------------------------ environments

    pub fn create_environment(&self, name: &str, icon: &str, color: &str) -> Result<Environment> {
        check_color(color)?;
        let conn = self.lock()?;
        let position = next_position(&conn, "SELECT COALESCE(MAX(position) + 1, 0) FROM environments", [])?;
        conn.execute(
            "INSERT INTO environments (name, icon, color, position) VALUES (?1, ?2, ?3, ?4)",
            params![name, icon, color, position],
        )?;
        fetch_one(
            &conn,
            &format!("SELECT {ENV_COLS} FROM environments WHERE id = ?1"),
            conn.last_insert_rowid(),
            "environment",
            environment_from_row,
        )
    }

    pub fn list_environments(&self) -> Result<Vec<Environment>> {
        let conn = self.lock()?;
        query_all(
            &conn,
            &format!("SELECT {ENV_COLS} FROM environments ORDER BY position, id"),
            [],
            environment_from_row,
        )
    }

    pub fn update_environment(&self, e: &Environment) -> Result<()> {
        check_color(&e.color)?;
        let conn = self.lock()?;
        let n = conn.execute(
            "UPDATE environments SET name = ?1, icon = ?2, color = ?3, position = ?4 WHERE id = ?5",
            params![e.name, e.icon, e.color, e.position, e.id],
        )?;
        expect_one(n, "environment", e.id)
    }

    /// Deletes the environment and its ambience tracks and layers. Campaigns,
    /// chapters and scenes using it are cleared, and so is the global default.
    pub fn delete_environment(&self, id: Id) -> Result<()> {
        let mut conn = self.lock()?;
        let tx = conn.transaction()?;
        let n = tx.execute("DELETE FROM environments WHERE id = ?1", params![id])?;
        expect_one(n, "environment", id)?;
        tx.execute(
            "DELETE FROM settings WHERE key = ?1 AND value = ?2",
            params![GLOBAL_DEFAULT_ENV_KEY, id.to_string()],
        )?;
        tx.commit()?;
        Ok(())
    }

    // ------------------------------------------------------------ spotify ambience

    /// Appends a Spotify ambience track to the environment.
    pub fn add_ambience_track(
        &self,
        environment_id: Id,
        track: &TrackRef,
        emotion_id: Option<Id>,
    ) -> Result<AmbienceTrack> {
        let conn = self.lock()?;
        let position = next_position(
            &conn,
            "SELECT COALESCE(MAX(position) + 1, 0) FROM ambience_tracks WHERE environment_id = ?1",
            params![environment_id],
        )?;
        conn.execute(
            "INSERT INTO ambience_tracks (environment_id, spotify_uri, title, subtitle, art_url, \
             duration_ms, start_offset_ms, emotion_id, position) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                environment_id,
                track.spotify_uri,
                track.title,
                track.subtitle,
                track.art_url,
                track.duration_ms,
                track.start_offset_ms,
                emotion_id,
                position,
            ],
        )?;
        fetch_one(
            &conn,
            &format!("SELECT {AMBIENCE_TRACK_COLS} FROM ambience_tracks WHERE id = ?1"),
            conn.last_insert_rowid(),
            "ambience track",
            ambience_track_from_row,
        )
    }

    /// `None` lists every environment's tracks. Ordered by environment, position.
    pub fn list_ambience_tracks(&self, environment_id: Option<Id>) -> Result<Vec<AmbienceTrack>> {
        let conn = self.lock()?;
        list_ambience_on(&conn, environment_id)
    }

    pub fn remove_ambience_track(&self, id: Id) -> Result<()> {
        let conn = self.lock()?;
        let n = conn.execute("DELETE FROM ambience_tracks WHERE id = ?1", params![id])?;
        expect_one(n, "ambience track", id)
    }

    // ------------------------------------------------------------ local layers

    /// Appends a local audio layer. `default_volume` must be in `0.0..=1.0`.
    pub fn add_ambience_layer(
        &self,
        environment_id: Id,
        name: &str,
        file_path: &str,
        default_volume: f32,
    ) -> Result<AmbienceLayer> {
        check_unit_volume(default_volume)?;
        let conn = self.lock()?;
        let position = next_position(
            &conn,
            "SELECT COALESCE(MAX(position) + 1, 0) FROM ambience_layers WHERE environment_id = ?1",
            params![environment_id],
        )?;
        conn.execute(
            "INSERT INTO ambience_layers (environment_id, name, file_path, default_volume, position) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![environment_id, name, file_path, default_volume, position],
        )?;
        fetch_one(
            &conn,
            &format!("SELECT {LAYER_COLS} FROM ambience_layers WHERE id = ?1"),
            conn.last_insert_rowid(),
            "ambience layer",
            layer_from_row,
        )
    }

    pub fn list_ambience_layers(&self, environment_id: Id) -> Result<Vec<AmbienceLayer>> {
        let conn = self.lock()?;
        query_all(
            &conn,
            &format!(
                "SELECT {LAYER_COLS} FROM ambience_layers WHERE environment_id = ?1 ORDER BY position, id"
            ),
            params![environment_id],
            layer_from_row,
        )
    }

    /// Updates name, file path, volume and position. `environment_id` is not changed.
    pub fn update_ambience_layer(&self, l: &AmbienceLayer) -> Result<()> {
        check_unit_volume(l.default_volume)?;
        let conn = self.lock()?;
        let n = conn.execute(
            "UPDATE ambience_layers SET name = ?1, file_path = ?2, default_volume = ?3, position = ?4 \
             WHERE id = ?5",
            params![l.name, l.file_path, l.default_volume, l.position, l.id],
        )?;
        expect_one(n, "ambience layer", l.id)
    }

    pub fn remove_ambience_layer(&self, id: Id) -> Result<()> {
        let conn = self.lock()?;
        let n = conn.execute("DELETE FROM ambience_layers WHERE id = ?1", params![id])?;
        expect_one(n, "ambience layer", id)
    }

    // ------------------------------------------------------------ settings

    pub fn global_default_environment(&self) -> Result<Option<Id>> {
        let conn = self.lock()?;
        global_default_on(&conn)
    }

    /// `None` clears the default. `Some(id)` must name an existing environment.
    pub fn set_global_default_environment(&self, environment_id: Option<Id>) -> Result<()> {
        let conn = self.lock()?;
        match environment_id {
            Some(id) => {
                let exists: i64 = conn.query_row(
                    "SELECT COUNT(*) FROM environments WHERE id = ?1",
                    params![id],
                    |r| r.get(0),
                )?;
                if exists == 0 {
                    return Err(BardicError::NotFound(format!("environment {id}")));
                }
                conn.execute(
                    "INSERT INTO settings (key, value) VALUES (?1, ?2) \
                     ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                    params![GLOBAL_DEFAULT_ENV_KEY, id.to_string()],
                )?;
            }
            None => {
                conn.execute("DELETE FROM settings WHERE key = ?1", params![GLOBAL_DEFAULT_ENV_KEY])?;
            }
        }
        Ok(())
    }

    // ------------------------------------------------------------ resolver

    /// Normalizes `ctx` (a scene id fills in its chapter and campaign; a chapter
    /// id fills in its campaign; unknown ids are `NotFound`) and gathers what the
    /// pure resolver needs: default environments per level, the applicable cues
    /// (Global plus the levels of the context) and all ambience tracks.
    pub fn resolver_input(&self, ctx: PlayContext) -> Result<ResolverInput> {
        let conn = self.lock()?;
        let mut context = PlayContext::default();
        let mut scene_default_env = None;
        let mut chapter_default_env = None;
        let mut campaign_default_env = None;

        if let Some(scene_id) = ctx.scene_id {
            let scene = fetch_scene(&conn, scene_id)?;
            let chapter = fetch_chapter(&conn, scene.chapter_id)?;
            let campaign = fetch_campaign(&conn, chapter.campaign_id)?;
            context = PlayContext {
                campaign_id: Some(campaign.id),
                chapter_id: Some(chapter.id),
                scene_id: Some(scene.id),
            };
            scene_default_env = scene.default_environment_id;
            chapter_default_env = chapter.default_environment_id;
            campaign_default_env = campaign.default_environment_id;
        } else if let Some(chapter_id) = ctx.chapter_id {
            let chapter = fetch_chapter(&conn, chapter_id)?;
            let campaign = fetch_campaign(&conn, chapter.campaign_id)?;
            context = PlayContext {
                campaign_id: Some(campaign.id),
                chapter_id: Some(chapter.id),
                scene_id: None,
            };
            chapter_default_env = chapter.default_environment_id;
            campaign_default_env = campaign.default_environment_id;
        } else if let Some(campaign_id) = ctx.campaign_id {
            let campaign = fetch_campaign(&conn, campaign_id)?;
            context = PlayContext { campaign_id: Some(campaign.id), chapter_id: None, scene_id: None };
            campaign_default_env = campaign.default_environment_id;
        }

        let mut cues = list_cues_on(&conn, CueOwner::Global)?;
        if let Some(id) = context.campaign_id {
            cues.extend(list_cues_on(&conn, CueOwner::Campaign(id))?);
        }
        if let Some(id) = context.chapter_id {
            cues.extend(list_cues_on(&conn, CueOwner::Chapter(id))?);
        }
        if let Some(id) = context.scene_id {
            cues.extend(list_cues_on(&conn, CueOwner::Scene(id))?);
        }

        Ok(ResolverInput {
            context,
            scene_default_env,
            chapter_default_env,
            campaign_default_env,
            global_default_env: global_default_on(&conn)?,
            cues,
            ambience: list_ambience_on(&conn, None)?,
        })
    }

    // ------------------------------------------------------------ seeding

    /// Inserts the default situations, emotions and environments, but only when
    /// both the tags and environments tables are empty. Safe to call repeatedly.
    pub fn seed_defaults(&self) -> Result<()> {
        let mut conn = self.lock()?;
        let tx = conn.transaction()?;
        let tags: i64 = tx.query_row("SELECT COUNT(*) FROM tags", [], |r| r.get(0))?;
        let envs: i64 = tx.query_row("SELECT COUNT(*) FROM environments", [], |r| r.get(0))?;
        if tags > 0 || envs > 0 {
            return Ok(());
        }
        for (i, (name, icon, color)) in SEED_SITUATIONS.iter().enumerate() {
            tx.execute(
                "INSERT INTO tags (kind, name, color, icon, position) VALUES ('situation', ?1, ?2, ?3, ?4)",
                params![name, color, icon, i as i64],
            )?;
        }
        for (i, (name, icon, color)) in SEED_EMOTIONS.iter().enumerate() {
            tx.execute(
                "INSERT INTO tags (kind, name, color, icon, position) VALUES ('emotion', ?1, ?2, ?3, ?4)",
                params![name, color, icon, i as i64],
            )?;
        }
        for (i, (name, icon, color)) in SEED_ENVIRONMENTS.iter().enumerate() {
            tx.execute(
                "INSERT INTO environments (name, icon, color, position) VALUES (?1, ?2, ?3, ?4)",
                params![name, icon, color, i as i64],
            )?;
        }
        tx.commit()?;
        Ok(())
    }
}

// ---------------------------------------------------------------- helpers

fn corrupt(what: &str) -> BardicError {
    BardicError::Db(format!("corrupt row: {what}"))
}

fn expect_one(changed: usize, what: &str, id: Id) -> Result<()> {
    if changed == 0 {
        Err(BardicError::NotFound(format!("{what} {id}")))
    } else {
        Ok(())
    }
}

fn next_position<P: Params>(conn: &Connection, sql: &str, p: P) -> Result<i64> {
    Ok(conn.query_row(sql, p, |r| r.get(0))?)
}

/// Runs a query and maps every row, propagating the first error.
fn query_all<T, P: Params>(
    conn: &Connection,
    sql: &str,
    p: P,
    map: impl FnMut(&Row<'_>) -> Result<T>,
) -> Result<Vec<T>> {
    let mut stmt = conn.prepare_cached(sql)?;
    let rows = stmt.query_and_then(p, map)?;
    rows.collect()
}

/// Fetches exactly one row by id, or `NotFound`.
fn fetch_one<T>(
    conn: &Connection,
    sql: &str,
    id: Id,
    what: &str,
    map: impl FnMut(&Row<'_>) -> Result<T>,
) -> Result<T> {
    query_all(conn, sql, params![id], map)?
        .pop()
        .ok_or_else(|| BardicError::NotFound(format!("{what} {id}")))
}

fn fetch_campaign(conn: &Connection, id: Id) -> Result<Campaign> {
    fetch_one(
        conn,
        &format!("SELECT {CAMPAIGN_COLS} FROM campaigns WHERE id = ?1"),
        id,
        "campaign",
        campaign_from_row,
    )
}

fn fetch_chapter(conn: &Connection, id: Id) -> Result<Chapter> {
    fetch_one(
        conn,
        &format!("SELECT {CHAPTER_COLS} FROM chapters WHERE id = ?1"),
        id,
        "chapter",
        chapter_from_row,
    )
}

fn fetch_scene(conn: &Connection, id: Id) -> Result<Scene> {
    fetch_one(
        conn,
        &format!("SELECT {SCENE_COLS} FROM scenes WHERE id = ?1"),
        id,
        "scene",
        scene_from_row,
    )
}

fn check_color(color: &str) -> Result<()> {
    let ok = color.len() == 7
        && color.starts_with('#')
        && color[1..].bytes().all(|b| b.is_ascii_hexdigit());
    if ok {
        Ok(())
    } else {
        Err(BardicError::Invalid(format!("color must be #RRGGBB, got {color:?}")))
    }
}

fn check_cue_options(o: &CueOptions) -> Result<()> {
    match o.volume {
        Some(v) if v > 100 => Err(BardicError::Invalid(format!("cue volume {v} exceeds 100"))),
        _ => Ok(()),
    }
}

fn check_unit_volume(v: f32) -> Result<()> {
    if (0.0..=1.0).contains(&v) {
        Ok(())
    } else {
        Err(BardicError::Invalid(format!("layer volume {v} not in 0..=1")))
    }
}

/// Column values for the owner: (owner_level, campaign_id, chapter_id, scene_id).
fn owner_parts(owner: CueOwner) -> (&'static str, Option<Id>, Option<Id>, Option<Id>) {
    match owner {
        CueOwner::Global => ("global", None, None, None),
        CueOwner::Campaign(id) => ("campaign", Some(id), None, None),
        CueOwner::Chapter(id) => ("chapter", None, Some(id), None),
        CueOwner::Scene(id) => ("scene", None, None, Some(id)),
    }
}

fn global_default_on(conn: &Connection) -> Result<Option<Id>> {
    let value: Option<String> = conn
        .query_row(
            "SELECT value FROM settings WHERE key = ?1",
            params![GLOBAL_DEFAULT_ENV_KEY],
            |r| r.get(0),
        )
        .optional()?;
    value
        .map(|v| v.parse::<Id>().map_err(|_| corrupt("global default environment")))
        .transpose()
}

fn list_cues_on(conn: &Connection, owner: CueOwner) -> Result<Vec<Cue>> {
    let (level, campaign, chapter, scene) = owner_parts(owner);
    let ids: Vec<Id> = query_all(
        conn,
        "SELECT id FROM cues WHERE owner_level = ?1 AND campaign_id IS ?2 \
         AND chapter_id IS ?3 AND scene_id IS ?4 ORDER BY position, id",
        params![level, campaign, chapter, scene],
        |r: &Row<'_>| Ok(r.get::<_, Id>(0)?),
    )?;
    ids.into_iter().map(|id| load_cue(conn, id)).collect()
}

fn list_ambience_on(conn: &Connection, environment_id: Option<Id>) -> Result<Vec<AmbienceTrack>> {
    query_all(
        conn,
        &format!(
            "SELECT {AMBIENCE_TRACK_COLS} FROM ambience_tracks \
             WHERE ?1 IS NULL OR environment_id = ?1 ORDER BY environment_id, position, id"
        ),
        params![environment_id],
        ambience_track_from_row,
    )
}

fn load_cue(conn: &Connection, id: Id) -> Result<Cue> {
    let mut cue = fetch_one(
        conn,
        &format!("SELECT {CUE_COLS} FROM cues WHERE id = ?1"),
        id,
        "cue",
        cue_from_row,
    )?;
    cue.items = query_all(
        conn,
        &format!("SELECT {ITEM_COLS} FROM cue_items WHERE cue_id = ?1 ORDER BY position, id"),
        params![id],
        cue_item_from_row,
    )?;
    Ok(cue)
}

// ---------------------------------------------------------------- row mappers

fn campaign_from_row(row: &Row<'_>) -> Result<Campaign> {
    Ok(Campaign {
        id: row.get("id")?,
        name: row.get("name")?,
        description: row.get("description")?,
        default_environment_id: row.get("default_environment_id")?,
        position: row.get("position")?,
    })
}

fn chapter_from_row(row: &Row<'_>) -> Result<Chapter> {
    Ok(Chapter {
        id: row.get("id")?,
        campaign_id: row.get("campaign_id")?,
        name: row.get("name")?,
        description: row.get("description")?,
        default_environment_id: row.get("default_environment_id")?,
        position: row.get("position")?,
    })
}

fn scene_from_row(row: &Row<'_>) -> Result<Scene> {
    Ok(Scene {
        id: row.get("id")?,
        chapter_id: row.get("chapter_id")?,
        name: row.get("name")?,
        description: row.get("description")?,
        default_environment_id: row.get("default_environment_id")?,
        position: row.get("position")?,
    })
}

fn tag_from_row(row: &Row<'_>) -> Result<Tag> {
    let kind: String = row.get("kind")?;
    Ok(Tag {
        id: row.get("id")?,
        kind: TagKind::parse(&kind).ok_or_else(|| corrupt("tag kind"))?,
        name: row.get("name")?,
        color: row.get("color")?,
        icon: row.get("icon")?,
        position: row.get("position")?,
    })
}

fn environment_from_row(row: &Row<'_>) -> Result<Environment> {
    Ok(Environment {
        id: row.get("id")?,
        name: row.get("name")?,
        icon: row.get("icon")?,
        color: row.get("color")?,
        position: row.get("position")?,
    })
}

/// Reads the track columns shared by cue items and ambience tracks.
fn track_from_row(row: &Row<'_>) -> Result<TrackRef> {
    Ok(TrackRef {
        spotify_uri: row.get("spotify_uri")?,
        title: row.get("title")?,
        subtitle: row.get("subtitle")?,
        art_url: row.get("art_url")?,
        duration_ms: row.get("duration_ms")?,
        start_offset_ms: row.get("start_offset_ms")?,
    })
}

fn cue_item_from_row(row: &Row<'_>) -> Result<CueItem> {
    Ok(CueItem {
        id: row.get("id")?,
        cue_id: row.get("cue_id")?,
        track: track_from_row(row)?,
        position: row.get("position")?,
    })
}

fn ambience_track_from_row(row: &Row<'_>) -> Result<AmbienceTrack> {
    Ok(AmbienceTrack {
        id: row.get("id")?,
        environment_id: row.get("environment_id")?,
        track: track_from_row(row)?,
        emotion_id: row.get("emotion_id")?,
        position: row.get("position")?,
    })
}

fn layer_from_row(row: &Row<'_>) -> Result<AmbienceLayer> {
    Ok(AmbienceLayer {
        id: row.get("id")?,
        environment_id: row.get("environment_id")?,
        name: row.get("name")?,
        file_path: row.get("file_path")?,
        default_volume: row.get("default_volume")?,
        position: row.get("position")?,
    })
}

fn owner_from_row(row: &Row<'_>) -> Result<CueOwner> {
    let level: String = row.get("owner_level")?;
    let campaign: Option<Id> = row.get("campaign_id")?;
    let chapter: Option<Id> = row.get("chapter_id")?;
    let scene: Option<Id> = row.get("scene_id")?;
    match (level.as_str(), campaign, chapter, scene) {
        ("global", None, None, None) => Ok(CueOwner::Global),
        ("campaign", Some(id), None, None) => Ok(CueOwner::Campaign(id)),
        ("chapter", None, Some(id), None) => Ok(CueOwner::Chapter(id)),
        ("scene", None, None, Some(id)) => Ok(CueOwner::Scene(id)),
        _ => Err(corrupt("cue owner")),
    }
}

fn cue_from_row(row: &Row<'_>) -> Result<Cue> {
    let repeat: String = row.get("repeat_mode")?;
    Ok(Cue {
        id: row.get("id")?,
        owner: owner_from_row(row)?,
        name: row.get("name")?,
        situation_id: row.get("situation_id")?,
        emotion_id: row.get("emotion_id")?,
        options: CueOptions {
            repeat: RepeatMode::from_spotify(&repeat),
            shuffle: row.get("shuffle")?,
            volume: row.get::<_, Option<i64>>("volume")?.map(|v| v as u8),
            fade_in_ms: row.get("fade_in_ms")?,
            fade_out_ms: row.get("fade_out_ms")?,
        },
        position: row.get("position")?,
        items: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lib() -> Library {
        Library::open_in_memory().expect("open in-memory library")
    }

    fn track(uri: &str) -> TrackRef {
        TrackRef {
            spotify_uri: uri.into(),
            title: format!("Title {uri}"),
            subtitle: "Artist".into(),
            art_url: None,
            duration_ms: Some(180_000),
            start_offset_ms: 0,
        }
    }

    fn count(lib: &Library, table: &str) -> i64 {
        let conn = lib.lock().unwrap();
        conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0)).unwrap()
    }

    fn tag_id(lib: &Library, kind: TagKind, name: &str) -> Id {
        lib.list_tags(Some(kind)).unwrap().into_iter().find(|t| t.name == name).unwrap().id
    }

    fn env_id(lib: &Library, name: &str) -> Id {
        lib.list_environments().unwrap().into_iter().find(|e| e.name == name).unwrap().id
    }

    fn is_not_found<T: std::fmt::Debug>(r: Result<T>) -> bool {
        matches!(r, Err(BardicError::NotFound(_)))
    }

    #[test]
    fn library_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Library>();
    }

    #[test]
    fn seeding_populates_defaults_once() {
        let lib = lib();
        assert_eq!(lib.list_tags(Some(TagKind::Situation)).unwrap().len(), 8);
        assert_eq!(lib.list_tags(Some(TagKind::Emotion)).unwrap().len(), 7);
        assert_eq!(lib.list_environments().unwrap().len(), 10);
        assert!(lib.list_ambience_tracks(None).unwrap().is_empty());

        let combat = &lib.list_tags(Some(TagKind::Situation)).unwrap()[0];
        assert_eq!(
            (combat.name.as_str(), combat.icon.as_str(), combat.color.as_str(), combat.position),
            ("Combat", "swords", "#C0392B", 0)
        );

        lib.seed_defaults().unwrap();
        assert_eq!(count(&lib, "tags"), 15);
        assert_eq!(count(&lib, "environments"), 10);
    }

    #[test]
    fn seeding_skips_when_tags_or_environments_exist() {
        let lib = lib();
        for t in lib.list_tags(None).unwrap() {
            lib.delete_tag(t.id).unwrap();
        }
        // Environments still exist, so re-seeding must not run.
        lib.seed_defaults().unwrap();
        assert_eq!(count(&lib, "tags"), 0);
    }

    #[test]
    fn campaign_crud_and_positions() {
        let lib = lib();
        let a = lib.create_campaign("Strahd", "Vampire").unwrap();
        let b = lib.create_campaign("Homebrew", "").unwrap();
        assert_eq!((a.position, b.position), (0, 1));
        assert_eq!(lib.get_campaign(a.id).unwrap(), a);

        let forest = env_id(&lib, "Forest");
        let mut updated = b.clone();
        updated.name = "Homebrew 2".into();
        updated.position = -1;
        updated.default_environment_id = Some(forest);
        lib.update_campaign(&updated).unwrap();
        let names: Vec<String> = lib.list_campaigns().unwrap().into_iter().map(|c| c.name).collect();
        assert_eq!(names, vec!["Homebrew 2", "Strahd"]);
        assert_eq!(lib.get_campaign(b.id).unwrap().default_environment_id, Some(forest));

        lib.delete_campaign(a.id).unwrap();
        assert!(is_not_found(lib.get_campaign(a.id)));
        assert!(is_not_found(lib.delete_campaign(a.id)));
        assert!(is_not_found(lib.update_campaign(&a)));
    }

    #[test]
    fn chapter_and_scene_crud() {
        let lib = lib();
        let c = lib.create_campaign("C", "").unwrap();
        let c1 = lib.create_chapter(c.id, "Ch1", "").unwrap();
        let c2 = lib.create_chapter(c.id, "Ch2", "").unwrap();
        assert_eq!((c1.position, c2.position), (0, 1));
        assert_eq!(lib.get_chapter(c2.id).unwrap(), c2);
        assert_eq!(lib.list_chapters(c.id).unwrap().len(), 2);

        let mut ch = c1.clone();
        ch.name = "Renamed".into();
        lib.update_chapter(&ch).unwrap();
        assert_eq!(lib.get_chapter(c1.id).unwrap().name, "Renamed");
        lib.delete_chapter(c1.id).unwrap();
        assert!(is_not_found(lib.get_chapter(c1.id)));

        let s1 = lib.create_scene(c2.id, "S1", "desc").unwrap();
        let s2 = lib.create_scene(c2.id, "S2", "").unwrap();
        assert_eq!((s1.position, s2.position), (0, 1));
        let mut scene = s2.clone();
        scene.position = -1;
        scene.default_environment_id = Some(env_id(&lib, "Cave"));
        lib.update_scene(&scene).unwrap();
        let ordered: Vec<Id> = lib.list_scenes(c2.id).unwrap().into_iter().map(|s| s.id).collect();
        assert_eq!(ordered, vec![s2.id, s1.id]);
        lib.delete_scene(s1.id).unwrap();
        assert!(is_not_found(lib.get_scene(s1.id)));
        assert!(is_not_found(lib.delete_scene(s1.id)));
    }

    #[test]
    fn tag_crud_and_cue_tag_is_cleared_on_delete() {
        let lib = lib();
        let t = lib.create_tag(TagKind::Situation, "Heist", "#123456", "mask").unwrap();
        assert_eq!(t.position, 8, "position is per kind, after the 8 seeded situations");
        let emo = lib.create_tag(TagKind::Emotion, "Hope", "#00FF00", "sun").unwrap();
        assert_eq!(emo.position, 7);
        assert!(lib.create_tag(TagKind::Emotion, "Bad", "red", "x").is_err());

        let mut renamed = t.clone();
        renamed.name = "Heist 2".into();
        renamed.color = "#654321".into();
        lib.update_tag(&renamed).unwrap();
        assert_eq!(lib.list_tags(Some(TagKind::Situation)).unwrap().last().unwrap().name, "Heist 2");

        let cue = lib
            .create_cue(CueOwner::Global, "Job", Some(t.id), Some(emo.id), &CueOptions::default())
            .unwrap();
        lib.delete_tag(t.id).unwrap();
        let got = lib.get_cue(cue.id).unwrap();
        assert_eq!(got.situation_id, None);
        assert_eq!(got.emotion_id, Some(emo.id));
        assert!(is_not_found(lib.delete_tag(t.id)));
    }

    #[test]
    fn cue_crud_and_options_roundtrip() {
        let lib = lib();
        let campaign = lib.create_campaign("C", "").unwrap();
        let combat = tag_id(&lib, TagKind::Situation, "Combat");
        let tension = tag_id(&lib, TagKind::Emotion, "Tension");
        let opts = CueOptions {
            repeat: RepeatMode::Context,
            shuffle: true,
            volume: Some(70),
            fade_in_ms: 2000,
            fade_out_ms: 500,
        };
        let cue = lib
            .create_cue(CueOwner::Campaign(campaign.id), "Ambush", Some(combat), None, &opts)
            .unwrap();
        assert_eq!(cue.position, 0);
        assert_eq!(cue.owner, CueOwner::Campaign(campaign.id));
        assert_eq!(cue.options, opts);
        assert_eq!(lib.get_cue(cue.id).unwrap(), cue);

        let global = lib
            .create_cue(CueOwner::Global, "Default", None, None, &CueOptions::default())
            .unwrap();
        assert_eq!(lib.list_cues(CueOwner::Campaign(campaign.id)).unwrap(), vec![cue.clone()]);
        assert_eq!(lib.list_cues(CueOwner::Global).unwrap(), vec![global.clone()]);

        let mut edited = cue.clone();
        edited.name = "Ambush!".into();
        edited.options.volume = None;
        edited.options.repeat = RepeatMode::Off;
        edited.emotion_id = Some(tension);
        lib.update_cue(&edited).unwrap();
        let got = lib.get_cue(cue.id).unwrap();
        assert_eq!(got.name, "Ambush!");
        assert_eq!(got.emotion_id, Some(tension));
        assert_eq!(got.options.volume, None);
        assert_eq!(got.options.repeat, RepeatMode::Off);

        let too_loud = CueOptions { volume: Some(101), ..CueOptions::default() };
        assert!(matches!(
            lib.create_cue(CueOwner::Global, "x", None, None, &too_loud),
            Err(BardicError::Invalid(_))
        ));

        lib.delete_cue(cue.id).unwrap();
        assert!(is_not_found(lib.get_cue(cue.id)));
        assert!(is_not_found(lib.delete_cue(cue.id)));
    }

    #[test]
    fn cue_items_add_remove_reorder() {
        let lib = lib();
        let cue = lib
            .create_cue(CueOwner::Global, "Playlist cue", None, None, &CueOptions::default())
            .unwrap();
        let a = lib.add_cue_item(cue.id, &track("spotify:track:a")).unwrap();
        let b = lib.add_cue_item(cue.id, &track("spotify:track:b")).unwrap();
        let c = lib.add_cue_item(cue.id, &track("spotify:track:c")).unwrap();
        assert_eq!(lib.get_cue(cue.id).unwrap().items.iter().map(|i| i.id).collect::<Vec<_>>(), vec![a.id, b.id, c.id]);

        lib.reorder_cue_items(cue.id, &[c.id, a.id, b.id]).unwrap();
        let got = lib.get_cue(cue.id).unwrap();
        assert_eq!(got.items.iter().map(|i| i.id).collect::<Vec<_>>(), vec![c.id, a.id, b.id]);
        assert_eq!(got.items[0].track, track("spotify:track:c"));

        assert!(matches!(
            lib.reorder_cue_items(cue.id, &[c.id, a.id]),
            Err(BardicError::Invalid(_))
        ));
        assert!(matches!(
            lib.reorder_cue_items(cue.id, &[a.id, a.id, b.id]),
            Err(BardicError::Invalid(_))
        ));
        // Failed reorders must not change anything.
        let still = lib.get_cue(cue.id).unwrap();
        assert_eq!(still.items.iter().map(|i| i.id).collect::<Vec<_>>(), vec![c.id, a.id, b.id]);
        assert!(is_not_found(lib.reorder_cue_items(9999, &[])));

        lib.remove_cue_item(a.id).unwrap();
        assert_eq!(lib.get_cue(cue.id).unwrap().items.len(), 2);
        assert!(is_not_found(lib.remove_cue_item(a.id)));
    }

    #[test]
    fn environment_ambience_and_layers_crud() {
        let lib = lib();
        let env = lib.create_environment("Moors", "tree", "#112233").unwrap();
        assert_eq!(env.position, 10);
        assert!(lib.create_environment("Bad", "x", "112233").is_err());

        let mut renamed = env.clone();
        renamed.name = "Fens".into();
        lib.update_environment(&renamed).unwrap();
        assert_eq!(lib.list_environments().unwrap().last().unwrap().name, "Fens");

        let mystery = tag_id(&lib, TagKind::Emotion, "Mystery");
        let t = lib.add_ambience_track(env.id, &track("spotify:playlist:x"), Some(mystery)).unwrap();
        lib.add_ambience_track(env_id(&lib, "Cave"), &track("spotify:track:y"), None).unwrap();
        assert_eq!(lib.list_ambience_tracks(Some(env.id)).unwrap(), vec![t.clone()]);
        assert_eq!(lib.list_ambience_tracks(None).unwrap().len(), 2);
        assert!(lib.list_ambience_tracks(Some(env_id(&lib, "Sea"))).unwrap().is_empty());
        lib.remove_ambience_track(t.id).unwrap();
        assert!(is_not_found(lib.remove_ambience_track(t.id)));

        let layer = lib.add_ambience_layer(env.id, "Rain", "/app/rain.ogg", 0.5).unwrap();
        assert!(matches!(
            lib.add_ambience_layer(env.id, "Loud", "/x.ogg", 1.5),
            Err(BardicError::Invalid(_))
        ));
        assert!(matches!(
            lib.add_ambience_layer(env.id, "NaN", "/x.ogg", f32::NAN),
            Err(BardicError::Invalid(_))
        ));
        let mut edited = layer.clone();
        edited.default_volume = 0.8;
        edited.name = "Heavy rain".into();
        lib.update_ambience_layer(&edited).unwrap();
        assert_eq!(lib.list_ambience_layers(env.id).unwrap(), vec![edited.clone()]);
        lib.remove_ambience_layer(layer.id).unwrap();
        assert!(lib.list_ambience_layers(env.id).unwrap().is_empty());
        assert!(is_not_found(lib.remove_ambience_layer(layer.id)));
    }

    #[test]
    fn delete_environment_nulls_defaults_and_cascades() {
        let lib = lib();
        let env = lib.create_environment("Moors", "tree", "#112233").unwrap();
        lib.add_ambience_track(env.id, &track("spotify:track:m"), None).unwrap();
        lib.add_ambience_layer(env.id, "Wind", "/wind.ogg", 0.3).unwrap();

        let campaign = lib.create_campaign("C", "").unwrap();
        let chapter = lib.create_chapter(campaign.id, "Ch", "").unwrap();
        let scene = lib.create_scene(chapter.id, "S", "").unwrap();
        let mut c = campaign.clone();
        c.default_environment_id = Some(env.id);
        lib.update_campaign(&c).unwrap();
        let mut ch = chapter.clone();
        ch.default_environment_id = Some(env.id);
        lib.update_chapter(&ch).unwrap();
        let mut s = scene.clone();
        s.default_environment_id = Some(env.id);
        lib.update_scene(&s).unwrap();
        lib.set_global_default_environment(Some(env.id)).unwrap();

        lib.delete_environment(env.id).unwrap();
        assert_eq!(lib.get_campaign(campaign.id).unwrap().default_environment_id, None);
        assert_eq!(lib.get_chapter(chapter.id).unwrap().default_environment_id, None);
        assert_eq!(lib.get_scene(scene.id).unwrap().default_environment_id, None);
        assert_eq!(lib.global_default_environment().unwrap(), None);
        assert!(lib.list_ambience_tracks(None).unwrap().is_empty());
        assert_eq!(count(&lib, "ambience_layers"), 0);
        assert!(is_not_found(lib.delete_environment(env.id)));
    }

    #[test]
    fn global_default_environment_roundtrip() {
        let lib = lib();
        assert_eq!(lib.global_default_environment().unwrap(), None);
        let forest = env_id(&lib, "Forest");
        lib.set_global_default_environment(Some(forest)).unwrap();
        assert_eq!(lib.global_default_environment().unwrap(), Some(forest));
        lib.set_global_default_environment(None).unwrap();
        assert_eq!(lib.global_default_environment().unwrap(), None);
        assert!(is_not_found(lib.set_global_default_environment(Some(9999))));
    }

    #[test]
    fn deleting_campaign_cascades_to_chapters_scenes_and_cues() {
        let lib = lib();
        let global = lib
            .create_cue(CueOwner::Global, "Global", None, None, &CueOptions::default())
            .unwrap();
        let campaign = lib.create_campaign("C", "").unwrap();
        let chapter = lib.create_chapter(campaign.id, "Ch", "").unwrap();
        let scene = lib.create_scene(chapter.id, "S", "").unwrap();
        lib.create_cue(CueOwner::Campaign(campaign.id), "cc", None, None, &CueOptions::default()).unwrap();
        lib.create_cue(CueOwner::Chapter(chapter.id), "chc", None, None, &CueOptions::default()).unwrap();
        let sc = lib.create_cue(CueOwner::Scene(scene.id), "scc", None, None, &CueOptions::default()).unwrap();
        lib.add_cue_item(sc.id, &track("spotify:track:z")).unwrap();

        lib.delete_campaign(campaign.id).unwrap();
        assert_eq!(count(&lib, "chapters"), 0);
        assert_eq!(count(&lib, "scenes"), 0);
        assert_eq!(count(&lib, "cue_items"), 0);
        assert_eq!(lib.list_cues(CueOwner::Global).unwrap(), vec![global]);
        assert_eq!(count(&lib, "cues"), 1);
    }

    #[test]
    fn deleting_chapter_cascades_to_its_scene_cues_only() {
        let lib = lib();
        let campaign = lib.create_campaign("C", "").unwrap();
        let ch1 = lib.create_chapter(campaign.id, "Ch1", "").unwrap();
        let ch2 = lib.create_chapter(campaign.id, "Ch2", "").unwrap();
        let s1 = lib.create_scene(ch1.id, "S1", "").unwrap();
        let s2 = lib.create_scene(ch2.id, "S2", "").unwrap();
        lib.create_cue(CueOwner::Scene(s1.id), "gone", None, None, &CueOptions::default()).unwrap();
        let kept = lib.create_cue(CueOwner::Scene(s2.id), "kept", None, None, &CueOptions::default()).unwrap();

        lib.delete_chapter(ch1.id).unwrap();
        assert_eq!(lib.list_cues(CueOwner::Scene(s2.id)).unwrap(), vec![kept]);
        assert_eq!(count(&lib, "cues"), 1);
        assert!(is_not_found(lib.get_scene(s1.id)));
    }

    #[test]
    fn resolver_input_normalizes_scene_and_collects_levels() {
        let lib = lib();
        let forest = env_id(&lib, "Forest");
        let snow = env_id(&lib, "Snow");
        lib.set_global_default_environment(Some(snow)).unwrap();

        let campaign = lib.create_campaign("C", "").unwrap();
        let mut c = campaign.clone();
        c.default_environment_id = Some(env_id(&lib, "City"));
        lib.update_campaign(&c).unwrap();

        let ch1 = lib.create_chapter(campaign.id, "Ch1", "").unwrap();
        let mut ch1_edit = ch1.clone();
        ch1_edit.default_environment_id = Some(forest);
        lib.update_chapter(&ch1_edit).unwrap();
        let ch2 = lib.create_chapter(campaign.id, "Ch2", "").unwrap();

        let s1 = lib.create_scene(ch1.id, "S1", "").unwrap();
        let s2 = lib.create_scene(ch2.id, "S2", "").unwrap();

        let g = lib.create_cue(CueOwner::Global, "g", None, None, &CueOptions::default()).unwrap();
        let cp = lib.create_cue(CueOwner::Campaign(campaign.id), "cp", None, None, &CueOptions::default()).unwrap();
        let c1 = lib.create_cue(CueOwner::Chapter(ch1.id), "c1", None, None, &CueOptions::default()).unwrap();
        let c2 = lib.create_cue(CueOwner::Chapter(ch2.id), "c2", None, None, &CueOptions::default()).unwrap();
        let s1c = lib.create_cue(CueOwner::Scene(s1.id), "s1c", None, None, &CueOptions::default()).unwrap();
        let s2c = lib.create_cue(CueOwner::Scene(s2.id), "s2c", None, None, &CueOptions::default()).unwrap();
        lib.add_ambience_track(forest, &track("spotify:playlist:amb"), None).unwrap();

        let input = lib
            .resolver_input(PlayContext { campaign_id: None, chapter_id: None, scene_id: Some(s1.id) })
            .unwrap();
        assert_eq!(
            input.context,
            PlayContext { campaign_id: Some(campaign.id), chapter_id: Some(ch1.id), scene_id: Some(s1.id) }
        );
        assert_eq!(input.scene_default_env, None);
        assert_eq!(input.chapter_default_env, Some(forest));
        assert_eq!(input.campaign_default_env, c.default_environment_id);
        assert_eq!(input.global_default_env, Some(snow));
        assert_eq!(input.ambience.len(), 1);

        let mut ids: Vec<Id> = input.cues.iter().map(|c| c.id).collect();
        ids.sort_unstable();
        let mut expected = vec![g.id, cp.id, c1.id, s1c.id];
        expected.sort_unstable();
        assert_eq!(ids, expected, "cues of other chapters and scenes must be excluded");
        assert!(!ids.contains(&c2.id) && !ids.contains(&s2c.id));

        let chapter_only = lib
            .resolver_input(PlayContext { campaign_id: None, chapter_id: Some(ch2.id), scene_id: None })
            .unwrap();
        assert_eq!(
            chapter_only.context,
            PlayContext { campaign_id: Some(campaign.id), chapter_id: Some(ch2.id), scene_id: None }
        );
        assert_eq!(chapter_only.chapter_default_env, None);
        let mut ids: Vec<Id> = chapter_only.cues.iter().map(|c| c.id).collect();
        ids.sort_unstable();
        let mut expected = vec![g.id, cp.id, c2.id];
        expected.sort_unstable();
        assert_eq!(ids, expected);

        assert!(is_not_found(
            lib.resolver_input(PlayContext { campaign_id: None, chapter_id: None, scene_id: Some(9999) })
        ));
    }

    #[test]
    fn file_database_survives_reopen_without_reseeding() {
        let path = std::env::temp_dir().join(format!("bardic-db-test-{}.sqlite", std::process::id()));
        let _ = std::fs::remove_file(&path);
        {
            let lib = Library::open(&path).unwrap();
            lib.create_campaign("Persisted", "").unwrap();
        }
        let lib = Library::open(&path).unwrap();
        assert_eq!(lib.list_campaigns().unwrap().len(), 1);
        assert_eq!(count(&lib, "tags"), 15);
        drop(lib);
        let _ = std::fs::remove_file(&path);
    }
}
