//! SQLite-opslag. De collector is de enige schrijver; de MCP-server opent de database read-only.
//!
//! Alleen *veranderingen* worden opgeslagen (een waarde geldt tot het volgende sample).
//! Het logboek `polls` legt vast wanneer de collector echt gemeten heeft, zodat
//! "waarde onveranderd" te onderscheiden is van "collector lag eruit".

use crate::api::{clean_title, ApiPoint};
use anyhow::{Context, Result};
use rusqlite::{params, Connection, OpenFlags, OptionalExtension};
use serde::Serialize;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS points (
    id          INTEGER PRIMARY KEY,
    title       TEXT NOT NULL,
    description TEXT NOT NULL DEFAULT '',
    unit        TEXT NOT NULL DEFAULT '',
    divisor     INTEGER NOT NULL DEFAULT 1,
    size        TEXT NOT NULL DEFAULT '',
    vtype       TEXT NOT NULL DEFAULT '',
    writable    INTEGER NOT NULL DEFAULT 0,
    modbus_type TEXT NOT NULL DEFAULT '',
    modbus_reg  INTEGER,
    first_seen  INTEGER NOT NULL,
    last_seen   INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS samples (
    point_id INTEGER NOT NULL,
    ts_ms    INTEGER NOT NULL,
    raw      INTEGER,
    PRIMARY KEY (point_id, ts_ms)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS polls (
    ts_ms       INTEGER PRIMARY KEY,
    kind        TEXT NOT NULL,
    ok          INTEGER NOT NULL,
    points      INTEGER NOT NULL DEFAULT 0,
    changed     INTEGER NOT NULL DEFAULT 0,
    duration_ms INTEGER NOT NULL DEFAULT 0,
    error       TEXT
);
CREATE TABLE IF NOT EXISTS meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
"#;

#[derive(Debug, Clone, Serialize)]
pub struct PointInfo {
    pub id: i64,
    pub title: String,
    pub description: String,
    pub unit: String,
    pub divisor: i64,
    pub size: String,
    pub writable: bool,
    pub modbus_type: String,
    pub modbus_register: Option<i64>,
}

impl PointInfo {
    pub fn scale(&self, raw: i64) -> f64 {
        raw as f64 / self.divisor.max(1) as f64
    }
}

// ---------------------------------------------------------------- schrijver

pub struct Store {
    conn: Connection,
    last: HashMap<i64, Option<i64>>,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("map {dir:?} aanmaken"))?;
        }
        let conn = Connection::open(path).with_context(|| format!("database {path:?} openen"))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.busy_timeout(std::time::Duration::from_secs(10))?;
        conn.execute_batch(SCHEMA)?;
        let mut last = HashMap::new();
        {
            let mut stmt = conn.prepare(
                "SELECT s.point_id, s.raw FROM samples s
                 WHERE s.ts_ms = (SELECT MAX(ts_ms) FROM samples WHERE point_id = s.point_id)",
            )?;
            let rows = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Option<i64>>(1)?)))?;
            for r in rows {
                let (id, raw) = r?;
                last.insert(id, raw);
            }
        }
        Ok(Self { conn, last })
    }

    pub fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO meta(key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    pub fn known_ids(&self) -> HashSet<i64> {
        self.conn
            .prepare("SELECT id FROM points")
            .and_then(|mut s| s.query_map([], |r| r.get::<_, i64>(0))?.collect())
            .unwrap_or_default()
    }

    pub fn upsert_catalog(&mut self, pts: &BTreeMap<i64, ApiPoint>, now_ms: i64) -> Result<()> {
        let tx = self.conn.transaction()?;
        {
            let mut st = tx.prepare(
                "INSERT INTO points(id, title, description, unit, divisor, size, vtype, writable,
                                    modbus_type, modbus_reg, first_seen, last_seen)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?11)
                 ON CONFLICT(id) DO UPDATE SET
                    title=excluded.title, description=excluded.description, unit=excluded.unit,
                    divisor=excluded.divisor, size=excluded.size, vtype=excluded.vtype,
                    writable=excluded.writable, modbus_type=excluded.modbus_type,
                    modbus_reg=excluded.modbus_reg, last_seen=excluded.last_seen",
            )?;
            for (id, p) in pts {
                let m = &p.metadata;
                st.execute(params![
                    id,
                    clean_title(&p.title),
                    clean_title(&p.description),
                    clean_title(&m.unit),
                    m.divisor.max(1),
                    m.variable_size,
                    m.variable_type,
                    m.is_writable as i64,
                    m.modbus_register_type,
                    m.modbus_register_id,
                    now_ms
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Slaat alleen waarden op die veranderd zijn. Geeft het aantal nieuwe samples terug.
    pub fn record(&mut self, ts_ms: i64, values: &[(i64, Option<i64>)]) -> Result<usize> {
        let tx = self.conn.transaction()?;
        let mut changed = 0;
        {
            let mut st = tx.prepare_cached("INSERT OR REPLACE INTO samples(point_id, ts_ms, raw) VALUES (?1,?2,?3)")?;
            for (id, raw) in values {
                if self.last.get(id) == Some(raw) {
                    continue;
                }
                st.execute(params![id, ts_ms, raw])?;
                self.last.insert(*id, *raw);
                changed += 1;
            }
        }
        tx.commit()?;
        Ok(changed)
    }

    pub fn log_poll(&self, ts_ms: i64, kind: &str, ok: bool, points: usize, changed: usize, duration_ms: i64, err: Option<&str>) {
        let r = self.conn.execute(
            "INSERT OR REPLACE INTO polls(ts_ms, kind, ok, points, changed, duration_ms, error)
             VALUES (?1,?2,?3,?4,?5,?6,?7)",
            params![ts_ms, kind, ok as i64, points as i64, changed as i64, duration_ms, err],
        );
        if let Err(e) = r {
            tracing::warn!("poll-log schrijven mislukt: {e}");
        }
    }

    pub fn prune_polls(&self, older_than_ms: i64) -> Result<usize> {
        Ok(self.conn.execute("DELETE FROM polls WHERE ts_ms < ?1", params![older_than_ms])?)
    }
}

// ----------------------------------------------------------------- lezer

pub struct Reader {
    conn: Connection,
}

#[derive(Debug, Serialize)]
pub struct DbStats {
    pub points: i64,
    pub samples: i64,
    pub db_bytes: i64,
    pub first_sample_ms: Option<i64>,
    pub last_sample_ms: Option<i64>,
    pub last_ok_poll_ms: Option<i64>,
    pub last_poll_ms: Option<i64>,
    pub last_poll_ok: Option<bool>,
    pub last_poll_error: Option<String>,
    pub poll_interval_secs: Option<i64>,
}

fn row_to_info(r: &rusqlite::Row<'_>) -> rusqlite::Result<PointInfo> {
    Ok(PointInfo {
        id: r.get(0)?,
        title: r.get(1)?,
        description: r.get(2)?,
        unit: r.get(3)?,
        divisor: r.get(4)?,
        size: r.get(5)?,
        writable: r.get::<_, i64>(6)? != 0,
        modbus_type: r.get(7)?,
        modbus_register: r.get(8)?,
    })
}

const INFO_COLS: &str = "id, title, description, unit, divisor, size, writable, modbus_type, modbus_reg";

impl Reader {
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)
            .with_context(|| format!("database {path:?} read-only openen (draait de collector al?)"))?;
        conn.busy_timeout(std::time::Duration::from_secs(10))?;
        conn.pragma_update(None, "query_only", "ON")?;
        Ok(Self { conn })
    }

    pub fn meta(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row("SELECT value FROM meta WHERE key = ?1", params![key], |r| r.get(0))
            .optional()?)
    }

    pub fn point(&self, id: i64) -> Result<Option<PointInfo>> {
        Ok(self
            .conn
            .query_row(&format!("SELECT {INFO_COLS} FROM points WHERE id = ?1"), params![id], row_to_info)
            .optional()?)
    }

    /// Zoekt op titel (alle woorden moeten voorkomen, hoofdletterongevoelig), id of eenheid.
    pub fn search(&self, query: Option<&str>, unit: Option<&str>, limit: usize) -> Result<Vec<PointInfo>> {
        let mut sql = format!("SELECT {INFO_COLS} FROM points WHERE 1=1");
        let mut args: Vec<String> = Vec::new();
        if let Some(q) = query {
            for w in q.split_whitespace() {
                if let Ok(id) = w.parse::<i64>() {
                    sql.push_str(&format!(" AND (id = {id} OR title LIKE ?{0} ESCAPE '\\')", args.len() + 1));
                } else {
                    sql.push_str(&format!(" AND title LIKE ?{} ESCAPE '\\'", args.len() + 1));
                }
                let esc = w.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_");
                args.push(format!("%{esc}%"));
            }
        }
        if let Some(u) = unit {
            sql.push_str(&format!(" AND unit = ?{}", args.len() + 1));
            args.push(u.to_string());
        }
        sql.push_str(&format!(" ORDER BY id LIMIT {}", limit.min(500)));
        let mut st = self.conn.prepare(&sql)?;
        let rows = st.query_map(rusqlite::params_from_iter(args.iter()), row_to_info)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Laatste sample van een punt: (tijdstip van verandering, ruwe waarde).
    pub fn latest(&self, id: i64) -> Result<Option<(i64, Option<i64>)>> {
        Ok(self
            .conn
            .query_row(
                "SELECT ts_ms, raw FROM samples WHERE point_id = ?1 ORDER BY ts_ms DESC LIMIT 1",
                params![id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?)
    }

    /// Het sample dat op `from` gold (basislijn) plus alle samples in `(from, to)`.
    pub fn samples(&self, id: i64, from: i64, to: i64) -> Result<Vec<(i64, Option<i64>)>> {
        let mut out = Vec::new();
        if let Some(b) = self
            .conn
            .query_row(
                "SELECT ts_ms, raw FROM samples WHERE point_id = ?1 AND ts_ms <= ?2 ORDER BY ts_ms DESC LIMIT 1",
                params![id, from],
                |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Option<i64>>(1)?)),
            )
            .optional()?
        {
            out.push(b);
        }
        let mut st = self
            .conn
            .prepare_cached("SELECT ts_ms, raw FROM samples WHERE point_id = ?1 AND ts_ms > ?2 AND ts_ms < ?3 ORDER BY ts_ms")?;
        let rows = st.query_map(params![id, from, to], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Option<i64>>(1)?)))?;
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// Tijdstippen van geslaagde volledige polls in `[from, to]`, plus de laatste vóór `from`.
    pub fn ok_polls(&self, from: i64, to: i64) -> Result<Vec<i64>> {
        let mut out = Vec::new();
        if let Some(b) = self
            .conn
            .query_row(
                "SELECT ts_ms FROM polls WHERE kind='full' AND ok=1 AND ts_ms < ?1 ORDER BY ts_ms DESC LIMIT 1",
                params![from],
                |r| r.get::<_, i64>(0),
            )
            .optional()?
        {
            out.push(b);
        }
        let mut st = self
            .conn
            .prepare_cached("SELECT ts_ms FROM polls WHERE kind='full' AND ok=1 AND ts_ms >= ?1 AND ts_ms <= ?2 ORDER BY ts_ms")?;
        for r in st.query_map(params![from, to], |r| r.get::<_, i64>(0))? {
            out.push(r?);
        }
        Ok(out)
    }

    pub fn stats(&self) -> Result<DbStats> {
        let one = |sql: &str| -> Result<i64> { Ok(self.conn.query_row(sql, [], |r| r.get(0))?) };
        let opt = |sql: &str| -> Result<Option<i64>> { Ok(self.conn.query_row(sql, [], |r| r.get(0))?) };
        let last_poll: Option<(i64, bool, Option<String>)> = self
            .conn
            .query_row("SELECT ts_ms, ok, error FROM polls ORDER BY ts_ms DESC LIMIT 1", [], |r| {
                Ok((r.get(0)?, r.get::<_, i64>(1)? != 0, r.get(2)?))
            })
            .optional()?;
        Ok(DbStats {
            points: one("SELECT COUNT(*) FROM points")?,
            samples: one("SELECT COUNT(*) FROM samples")?,
            db_bytes: one("SELECT page_count * page_size FROM pragma_page_count, pragma_page_size")?,
            first_sample_ms: opt("SELECT MIN(ts_ms) FROM samples")?,
            last_sample_ms: opt("SELECT MAX(ts_ms) FROM samples")?,
            last_ok_poll_ms: opt("SELECT MAX(ts_ms) FROM polls WHERE kind='full' AND ok=1")?,
            last_poll_ms: last_poll.as_ref().map(|p| p.0),
            last_poll_ok: last_poll.as_ref().map(|p| p.1),
            last_poll_error: last_poll.and_then(|p| p.2),
            poll_interval_secs: self.meta("poll_interval_secs")?.and_then(|s| s.parse().ok()),
        })
    }
}
