//! 本地记录：SQLite 三表（design.md §3）
//! rusqlite（bundled）：SQL 全部收在 Rust 侧，经 IPC 暴露给前端。

use std::path::Path;

use rusqlite::Connection;
use serde::Serialize;

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RoundTarget {
    pub site_id: String,
    pub status: String,
    pub rating: Option<i64>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Round {
    pub id: i64,
    pub question: String,
    pub created_at: i64,
    pub targets: Vec<RoundTarget>,
}

pub struct Store {
    conn: std::sync::Mutex<Connection>,
}

const SCHEMA_V1: &str = "
CREATE TABLE rounds (
  id         INTEGER PRIMARY KEY AUTOINCREMENT,
  question   TEXT    NOT NULL,
  created_at INTEGER NOT NULL
);
CREATE TABLE round_targets (
  round_id     INTEGER NOT NULL REFERENCES rounds(id) ON DELETE CASCADE,
  site_id      TEXT    NOT NULL,
  status       TEXT    NOT NULL,
  injector_ver TEXT,
  error        TEXT,
  created_at   INTEGER NOT NULL,
  PRIMARY KEY (round_id, site_id)
);
CREATE INDEX idx_rounds_created ON rounds(created_at DESC);
CREATE TABLE ratings (
  round_id   INTEGER NOT NULL,
  site_id    TEXT    NOT NULL,
  rating     INTEGER NOT NULL CHECK (rating IN (1, -1)),
  updated_at INTEGER NOT NULL,
  PRIMARY KEY (round_id, site_id)
);
";

impl Store {
    pub fn open(path: &Path) -> Result<Self, String> {
        let conn = Connection::open(path).map_err(|e| e.to_string())?;
        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(|e| e.to_string())?;
        conn.pragma_update(None, "foreign_keys", "ON")
            .map_err(|e| e.to_string())?;
        let store = Self {
            conn: std::sync::Mutex::new(conn),
        };
        store.migrate()?;
        Ok(store)
    }

    fn migrate(&self) -> Result<(), String> {
        let conn = self.conn.lock().unwrap();
        let v: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .map_err(|e| e.to_string())?;
        if v < 1 {
            conn.execute_batch(SCHEMA_V1).map_err(|e| e.to_string())?;
            conn.pragma_update(None, "user_version", 1)
                .map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    pub fn insert_round(&self, question: &str) -> Result<i64, String> {
        let conn = self.conn.lock().unwrap();
        let now = now_ms();
        conn.execute(
            "INSERT INTO rounds (question, created_at) VALUES (?1, ?2)",
            rusqlite::params![question, now],
        )
        .map_err(|e| e.to_string())?;
        Ok(conn.last_insert_rowid())
    }

    pub fn upsert_target(
        &self,
        round_id: i64,
        site_id: &str,
        status: &str,
        injector_ver: Option<&str>,
        error: Option<&str>,
    ) -> Result<(), String> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO round_targets (round_id, site_id, status, injector_ver, error, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT (round_id, site_id) DO UPDATE SET
               status = excluded.status,
               injector_ver = excluded.injector_ver,
               error = excluded.error",
            rusqlite::params![round_id, site_id, status, injector_ver, error, now_ms()],
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn list_rounds(&self, limit: i64, before: Option<i64>) -> Result<Vec<Round>, String> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT id, question, created_at FROM rounds
                 WHERE (?1 IS NULL OR created_at < ?1)
                 ORDER BY created_at DESC LIMIT ?2",
            )
            .map_err(|e| e.to_string())?;
        let mut rows = stmt
            .query(rusqlite::params![before, limit])
            .map_err(|e| e.to_string())?;

        let mut rounds = Vec::new();
        while let Ok(Some(row)) = rows.next() {
            rounds.push(Round {
                id: row.get(0).map_err(|e| e.to_string())?,
                question: row.get(1).map_err(|e| e.to_string())?,
                created_at: row.get(2).map_err(|e| e.to_string())?,
                targets: Vec::new(),
            });
        }
        drop(rows);

        // 批量取 targets（页面 size ≤ 100，两次查询足够）
        for r in rounds.iter_mut() {
            let mut stmt = conn
                .prepare(
                    "SELECT t.site_id, t.status, g.rating
                     FROM round_targets t
                     LEFT JOIN ratings g
                       ON g.round_id = t.round_id AND g.site_id = t.site_id
                     WHERE t.round_id = ?1",
                )
                .map_err(|e| e.to_string())?;
            let mut rows = stmt
                .query(rusqlite::params![r.id])
                .map_err(|e| e.to_string())?;
            while let Ok(Some(row)) = rows.next() {
                r.targets.push(RoundTarget {
                    site_id: row.get(0).map_err(|e| e.to_string())?,
                    status: row.get(1).map_err(|e| e.to_string())?,
                    rating: row.get(2).map_err(|e| e.to_string())?,
                });
            }
        }
        Ok(rounds)
    }

    /// 同值再点 = 撤销（删除行）
    pub fn set_rating(&self, round_id: i64, site_id: &str, rating: i64) -> Result<(), String> {
        let conn = self.conn.lock().unwrap();
        let existing: Option<i64> = conn
            .query_row(
                "SELECT rating FROM ratings WHERE round_id = ?1 AND site_id = ?2",
                rusqlite::params![round_id, site_id],
                |r| r.get(0),
            )
            .map(Some)
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                other => Err(other),
            })
            .map_err(|e| e.to_string())?;

        match existing {
            Some(v) if v == rating => {
                conn.execute(
                    "DELETE FROM ratings WHERE round_id = ?1 AND site_id = ?2",
                    rusqlite::params![round_id, site_id],
                )
                .map_err(|e| e.to_string())?;
            }
            _ => {
                conn.execute(
                    "INSERT INTO ratings (round_id, site_id, rating, updated_at)
                     VALUES (?1, ?2, ?3, ?4)
                     ON CONFLICT (round_id, site_id) DO UPDATE SET
                       rating = excluded.rating, updated_at = excluded.updated_at",
                    rusqlite::params![round_id, site_id, rating, now_ms()],
                )
                .map_err(|e| e.to_string())?;
            }
        }
        Ok(())
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem() -> Store {
        Store {
            conn: std::sync::Mutex::new(Connection::open_in_memory().unwrap()),
        }
    }
    // open() 之外的内测路径：手动迁移
    fn mem_migrated() -> Store {
        let s = mem();
        {
            let conn = s.conn.lock().unwrap();
            conn.execute_batch(SCHEMA_V1).unwrap();
        }
        s
    }

    #[test]
    fn round_trip_and_rating_toggle() {
        let s = mem_migrated();
        let id = s.insert_round("hello").unwrap();
        s.upsert_target(id, "chatgpt", "sent", Some("1"), None)
            .unwrap();

        let rs = s.list_rounds(10, None).unwrap();
        assert_eq!(rs.len(), 1);
        assert_eq!(rs[0].question, "hello");
        assert_eq!(rs[0].targets[0].site_id, "chatgpt");
        assert_eq!(rs[0].targets[0].rating, None);

        s.set_rating(id, "chatgpt", 1).unwrap();
        s.set_rating(id, "chatgpt", -1).unwrap(); // 改判
        assert_eq!(
            s.list_rounds(10, None).unwrap()[0].targets[0].rating,
            Some(-1)
        );
        s.set_rating(id, "chatgpt", -1).unwrap(); // 同值=撤销
        assert_eq!(s.list_rounds(10, None).unwrap()[0].targets[0].rating, None);
    }
}
