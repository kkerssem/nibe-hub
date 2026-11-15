//! SQLite storage. The collector is the only writer; the MCP server opens the database read-only.
//!
//! Only *changes* are stored (a value holds until the next sample).
//! The `polls` log records when the collector actually measured, so that
//! "value unchanged" can be told apart from "collector was down".

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

// ---------------------------------------------------------------- writer

/// Sample statistics kept in `meta` by the writer, so the readers never have to scan `samples`.
const META_SAMPLES: &str = "samples_count";
const META_FIRST: &str = "first_sample_ms";
const META_LAST: &str = "last_sample_ms";

pub struct Store {
    conn: Connection,
    /// Latest stored sample per point: (timestamp, raw value).
    last: HashMap<i64, (i64, Option<i64>)>,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating directory {dir:?}"))?;
        }
        let conn = Connection::open(path).with_context(|| format!("opening database {path:?}"))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.busy_timeout(std::time::Duration::from_secs(10))?;
        conn.execute_batch(SCHEMA)?;
        let mut last = HashMap::new();
        {
            // One index seek per catalogue point instead of a scan of the whole samples table.
            let ids: Vec<i64> = conn.prepare("SELECT id FROM points")?.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
            let mut latest = conn.prepare("SELECT ts_ms, raw FROM samples WHERE point_id = ?1 ORDER BY ts_ms DESC LIMIT 1")?;
            for id in ids {
                if let Some(v) = latest.query_row(params![id], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Option<i64>>(1)?))).optional()? {
                    last.insert(id, v);
                }
            }
        }
        // Databases from before the counters were kept get them once (a one-time scan).
        let has_counters: bool =
            conn.query_row("SELECT EXISTS(SELECT 1 FROM meta WHERE key = ?1)", params![META_SAMPLES], |r| r.get(0))?;
        if !has_counters {
            let (n, first, lastts): (i64, Option<i64>, Option<i64>) =
                conn.query_row("SELECT COUNT(*), MIN(ts_ms), MAX(ts_ms) FROM samples", [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
            let set = |k: &str, v: i64| conn.execute("INSERT OR REPLACE INTO meta(key, value) VALUES (?1, ?2)", params![k, v.to_string()]);
            set(META_SAMPLES, n)?;
            if let Some(f) = first {
                set(META_FIRST, f)?;
            }
            if let Some(l) = lastts {
                set(META_LAST, l)?;
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

    /// Stores only values that changed. Returns the number of new samples.
    ///
    /// A value older than (or as old as) the point's latest stored sample is ignored, so a
    /// slow full poll can never overwrite a newer fast-poll value. The in-memory cache is
    /// updated only after the transaction commits, so a failed write is retried next time.
    pub fn record(&mut self, ts_ms: i64, values: &[(i64, Option<i64>)]) -> Result<usize> {
        let mut pending = Vec::new();
        for (id, raw) in values {
            match self.last.get(id) {
                Some((t, _)) if *t >= ts_ms => continue,
                Some((_, v)) if v == raw => continue,
                _ => pending.push((*id, *raw)),
            }
        }
        if pending.is_empty() {
            return Ok(0);
        }
        let tx = self.conn.transaction()?;
        {
            let mut st = tx.prepare_cached("INSERT INTO samples(point_id, ts_ms, raw) VALUES (?1,?2,?3)")?;
            for (id, raw) in &pending {
                st.execute(params![id, ts_ms, raw])?;
            }
            tx.execute(
                "INSERT INTO meta(key, value) VALUES (?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = CAST(value AS INTEGER) + CAST(excluded.value AS INTEGER)",
                params![META_SAMPLES, pending.len().to_string()],
            )?;
            tx.execute("INSERT OR IGNORE INTO meta(key, value) VALUES (?1, ?2)", params![META_FIRST, ts_ms.to_string()])?;
            tx.execute(
                "INSERT INTO meta(key, value) VALUES (?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = MAX(CAST(value AS INTEGER), CAST(excluded.value AS INTEGER))",
                params![META_LAST, ts_ms.to_string()],
            )?;
        }
        tx.commit()?;
        let changed = pending.len();
        for (id, raw) in pending {
            self.last.insert(id, (ts_ms, raw));
        }
        Ok(changed)
    }

    pub fn log_poll(&self, ts_ms: i64, kind: &str, ok: bool, points: usize, changed: usize, duration_ms: i64, err: Option<&str>) {
        let r = self.conn.execute(
            "INSERT OR REPLACE INTO polls(ts_ms, kind, ok, points, changed, duration_ms, error)
             VALUES (?1,?2,?3,?4,?5,?6,?7)",
            params![ts_ms, kind, ok as i64, points as i64, changed as i64, duration_ms, err],
        );
        if let Err(e) = r {
            tracing::warn!("writing poll log failed: {e}");
        }
    }

    /// Deletes old failed polls and fast-poll entries. Successful full polls are kept for as
    /// long as the samples: gap detection needs them for every range that can be queried.
    pub fn prune_polls(&self, older_than_ms: i64) -> Result<usize> {
        Ok(self
            .conn
            .execute("DELETE FROM polls WHERE ts_ms < ?1 AND (ok = 0 OR kind != 'full')", params![older_than_ms])?)
    }
}

// ----------------------------------------------------------------- reader

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
            .with_context(|| format!("opening database {path:?} read-only (is the collector running?)"))?;
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

    /// Searches by title (all words must match, case-insensitive), id or unit.
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

    /// Latest sample of a point: (time of change, raw value).
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

    /// The sample in effect at `from` (baseline) plus the samples in `(from, to)`, at most
    /// `limit` of them in total when a limit is given.
    pub fn samples(&self, id: i64, from: i64, to: i64, limit: Option<usize>) -> Result<Vec<(i64, Option<i64>)>> {
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
        // LIMIT -1 means no limit in SQLite
        let rest = limit.map(|l| l.saturating_sub(out.len()) as i64).unwrap_or(-1);
        let mut st = self.conn.prepare_cached(
            "SELECT ts_ms, raw FROM samples WHERE point_id = ?1 AND ts_ms > ?2 AND ts_ms < ?3 ORDER BY ts_ms LIMIT ?4",
        )?;
        let rows = st.query_map(params![id, from, to, rest], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Option<i64>>(1)?)))?;
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// Times of successful full polls in `[from, to]`, plus the last one before `from`.
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

    /// Time of the latest successful full poll (walks the polls primary key backwards).
    pub fn last_ok_poll_ms(&self) -> Result<Option<i64>> {
        Ok(self
            .conn
            .query_row("SELECT ts_ms FROM polls WHERE kind='full' AND ok=1 ORDER BY ts_ms DESC LIMIT 1", [], |r| r.get(0))
            .optional()?)
    }

    fn meta_i64(&self, key: &str) -> Result<Option<i64>> {
        Ok(self.meta(key)?.and_then(|s| s.parse().ok()))
    }

    /// Cheap statistics: sample counts and range come from `meta`, which the writer maintains.
    pub fn stats(&self) -> Result<DbStats> {
        let one = |sql: &str| -> Result<i64> { Ok(self.conn.query_row(sql, [], |r| r.get(0))?) };
        let last_poll: Option<(i64, bool, Option<String>)> = self
            .conn
            .query_row("SELECT ts_ms, ok, error FROM polls ORDER BY ts_ms DESC LIMIT 1", [], |r| {
                Ok((r.get(0)?, r.get::<_, i64>(1)? != 0, r.get(2)?))
            })
            .optional()?;
        Ok(DbStats {
            points: one("SELECT COUNT(*) FROM points")?,
            samples: self.meta_i64(META_SAMPLES)?.unwrap_or(0),
            db_bytes: one("SELECT page_count * page_size FROM pragma_page_count, pragma_page_size")?,
            first_sample_ms: self.meta_i64(META_FIRST)?,
            last_sample_ms: self.meta_i64(META_LAST)?,
            last_ok_poll_ms: self.last_ok_poll_ms()?,
            last_poll_ms: last_poll.as_ref().map(|p| p.0),
            last_poll_ok: last_poll.as_ref().map(|p| p.1),
            last_poll_error: last_poll.and_then(|p| p.2),
            poll_interval_secs: self.meta("poll_interval_secs")?.and_then(|s| s.parse().ok()),
        })
    }
}
