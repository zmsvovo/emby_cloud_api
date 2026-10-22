use std::{
    collections::HashMap,
    path::Path,
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};

use rusqlite::{Connection, OptionalExtension, params};
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use uuid::Uuid;

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct UserDataPatch {
    pub playback_position_ticks: Option<i64>,
    pub played: Option<bool>,
    pub is_favorite: Option<bool>,
}

#[derive(Clone, Debug, Default)]
pub struct UserItemState {
    pub position_ticks: i64,
    pub played: bool,
    pub favorite: bool,
    pub hidden_from_resume: bool,
    pub play_count: i64,
    pub last_played: Option<String>,
}

pub struct Database {
    connection: Mutex<Connection>,
    server_id: String,
}

impl Database {
    pub fn open(path: &Path) -> Result<Self, String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let connection = Connection::open(path).map_err(|e| e.to_string())?;
        connection
            .execute_batch(
                "PRAGMA journal_mode=WAL;
                 CREATE TABLE IF NOT EXISTS settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);
                 CREATE TABLE IF NOT EXISTS tokens (hash TEXT PRIMARY KEY, user_id TEXT NOT NULL);
                 CREATE TABLE IF NOT EXISTS stream_tickets (
                    hash TEXT PRIMARY KEY,
                    item_id TEXT NOT NULL,
                    expires_at INTEGER NOT NULL
                 );
                 CREATE TABLE IF NOT EXISTS progress (
                    user_id TEXT NOT NULL,
                    item_id TEXT NOT NULL,
                    position_ticks INTEGER NOT NULL DEFAULT 0,
                    played INTEGER NOT NULL DEFAULT 0,
                    PRIMARY KEY(user_id, item_id)
                 );",
            )
            .map_err(|e| e.to_string())?;
        for (column, definition) in [
            ("favorite", "INTEGER NOT NULL DEFAULT 0"),
            ("hidden_from_resume", "INTEGER NOT NULL DEFAULT 0"),
            ("play_count", "INTEGER NOT NULL DEFAULT 0"),
            ("last_played", "TEXT"),
        ] {
            let exists: bool = connection
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM pragma_table_info('progress') WHERE name=?1)",
                    [column],
                    |row| row.get(0),
                )
                .map_err(|e| e.to_string())?;
            if !exists {
                connection
                    .execute_batch(&format!(
                        "ALTER TABLE progress ADD COLUMN {column} {definition}"
                    ))
                    .map_err(|e| e.to_string())?;
            }
        }
        let server_id: Option<String> = connection
            .query_row(
                "SELECT value FROM settings WHERE key='server_id'",
                [],
                |row| row.get(0),
            )
            .optional()
            .map_err(|e| e.to_string())?;
        let server_id = match server_id {
            Some(id) => id,
            None => {
                let id = Uuid::new_v4().simple().to_string();
                connection
                    .execute(
                        "INSERT INTO settings(key,value) VALUES('server_id',?1)",
                        [&id],
                    )
                    .map_err(|e| e.to_string())?;
                id
            }
        };
        Ok(Self {
            connection: Mutex::new(connection),
            server_id,
        })
    }

    pub fn server_id(&self) -> &str {
        &self.server_id
    }

    pub fn json_setting(&self, key: &str) -> Option<Value> {
        let connection = self.connection.lock().ok()?;
        let text: String = connection
            .query_row("SELECT value FROM settings WHERE key=?1", [key], |row| {
                row.get(0)
            })
            .optional()
            .ok()??;
        serde_json::from_str(&text).ok()
    }

    pub fn set_json_setting(&self, key: &str, value: &Value) -> Result<(), String> {
        self.connection
            .lock()
            .map_err(|e| e.to_string())?
            .execute(
                "INSERT INTO settings(key,value) VALUES(?1,?2)
                 ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                params![key, value.to_string()],
            )
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn user_states(&self) -> HashMap<String, UserItemState> {
        let Ok(connection) = self.connection.lock() else {
            return HashMap::new();
        };
        let Ok(mut statement) = connection.prepare(
            "SELECT item_id,position_ticks,played,favorite,hidden_from_resume,play_count,last_played
             FROM progress WHERE user_id='1'"
        ) else { return HashMap::new() };
        statement
            .query_map([], |row| {
                Ok((
                    row.get(0)?,
                    UserItemState {
                        position_ticks: row.get(1)?,
                        played: row.get(2)?,
                        favorite: row.get(3)?,
                        hidden_from_resume: row.get(4)?,
                        play_count: row.get(5)?,
                        last_played: row.get(6)?,
                    },
                ))
            })
            .map(|rows| rows.flatten().collect())
            .unwrap_or_default()
    }

    pub fn set_favorite(&self, id: &str, favorite: bool) -> Result<(), String> {
        self.connection
            .lock()
            .map_err(|e| e.to_string())?
            .execute(
                "INSERT INTO progress(user_id,item_id,favorite) VALUES('1',?1,?2)
             ON CONFLICT(user_id,item_id) DO UPDATE SET favorite=excluded.favorite",
                params![id, favorite],
            )
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn update_user_data(&self, id: &str, patch: &UserDataPatch) -> Result<(), String> {
        let mut connection = self.connection.lock().map_err(|e| e.to_string())?;
        let transaction = connection.transaction().map_err(|e| e.to_string())?;
        transaction
            .execute(
                "INSERT OR IGNORE INTO progress(user_id,item_id) VALUES('1',?1)",
                [id],
            )
            .map_err(|e| e.to_string())?;
        transaction
            .execute(
                "UPDATE progress SET
             position_ticks=COALESCE(?2,CASE WHEN ?3=1 THEN 0 ELSE position_ticks END),
             played=COALESCE(?3,played),favorite=COALESCE(?4,favorite),
             play_count=CASE WHEN ?3=1 THEN MAX(play_count,1) WHEN ?3=0 THEN 0 ELSE play_count END
             WHERE user_id='1' AND item_id=?1",
                params![
                    id,
                    patch.playback_position_ticks,
                    patch.played,
                    patch.is_favorite
                ],
            )
            .map_err(|e| e.to_string())?;
        transaction.commit().map_err(|e| e.to_string())
    }

    pub fn set_hidden_from_resume(&self, id: &str, hidden: bool) -> Result<(), String> {
        self.connection.lock().map_err(|e| e.to_string())?.execute(
            "INSERT INTO progress(user_id,item_id,hidden_from_resume) VALUES('1',?1,?2)
             ON CONFLICT(user_id,item_id) DO UPDATE SET hidden_from_resume=excluded.hidden_from_resume",
            params![id, hidden],
        ).map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn record_play(&self, id: &str) -> Result<(), String> {
        self.connection
            .lock()
            .map_err(|e| e.to_string())?
            .execute(
                "INSERT INTO progress(user_id,item_id,play_count,last_played)
             VALUES('1',?1,1,strftime('%Y-%m-%dT%H:%M:%fZ','now'))
             ON CONFLICT(user_id,item_id) DO UPDATE SET play_count=play_count+1,
             last_played=strftime('%Y-%m-%dT%H:%M:%fZ','now')",
                [id],
            )
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn create_token(&self) -> Result<String, String> {
        let token = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
        self.connection
            .lock()
            .map_err(|e| e.to_string())?
            .execute(
                "INSERT INTO tokens(hash,user_id) VALUES(?1,'1')",
                [digest(&token)],
            )
            .map_err(|e| e.to_string())?;
        Ok(token)
    }

    pub fn has_token(&self, token: &str) -> bool {
        self.connection.lock().is_ok_and(|connection| {
            connection
                .query_row(
                    "SELECT 1 FROM tokens WHERE hash=?1",
                    [digest(token)],
                    |_| Ok(()),
                )
                .optional()
                .is_ok_and(|entry| entry.is_some())
        })
    }

    pub fn revoke_token(&self, token: &str) {
        if let Ok(connection) = self.connection.lock() {
            let _ = connection.execute("DELETE FROM tokens WHERE hash=?1", [digest(token)]);
        }
    }

    pub fn create_stream_ticket(&self, item_id: &str) -> Result<String, String> {
        let ticket = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
        let expires_at = unix_seconds() + 2 * 60 * 60;
        let connection = self.connection.lock().map_err(|e| e.to_string())?;
        connection
            .execute(
                "DELETE FROM stream_tickets WHERE expires_at < ?1",
                [unix_seconds()],
            )
            .map_err(|e| e.to_string())?;
        connection
            .execute(
                "INSERT INTO stream_tickets(hash,item_id,expires_at) VALUES(?1,?2,?3)",
                params![digest(&ticket), item_id, expires_at],
            )
            .map_err(|e| e.to_string())?;
        Ok(ticket)
    }

    pub fn has_stream_ticket(&self, ticket: &str, item_id: &str) -> bool {
        self.connection.lock().is_ok_and(|connection| {
            connection
                .query_row(
                    "SELECT 1 FROM stream_tickets WHERE hash=?1 AND item_id=?2 AND expires_at>=?3",
                    params![digest(ticket), item_id, unix_seconds()],
                    |_| Ok(()),
                )
                .optional()
                .is_ok_and(|entry| entry.is_some())
        })
    }

    pub fn progress(&self, item_id: &str) -> i64 {
        self.connection.lock().map_or(0, |connection| {
            connection
                .query_row(
                    "SELECT position_ticks FROM progress WHERE user_id='1' AND item_id=?1",
                    [item_id],
                    |row| row.get(0),
                )
                .unwrap_or(0)
        })
    }

    pub fn set_progress(&self, item_id: &str, ticks: i64) -> Result<(), String> {
        self.connection
            .lock()
            .map_err(|e| e.to_string())?
            .execute(
                "INSERT INTO progress(user_id,item_id,position_ticks,played) VALUES('1',?1,?2,0)
                 ON CONFLICT(user_id,item_id) DO UPDATE SET
                 position_ticks=excluded.position_ticks,hidden_from_resume=0",
                params![item_id, ticks.max(0)],
            )
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn set_played(&self, item_id: &str, played: bool) -> Result<(), String> {
        self.connection
            .lock()
            .map_err(|e| e.to_string())?
            .execute(
                "INSERT INTO progress(user_id,item_id,position_ticks,played,play_count,last_played)
                 VALUES('1',?1,0,?2,?2,CASE WHEN ?2 THEN strftime('%Y-%m-%dT%H:%M:%fZ','now') END)
                 ON CONFLICT(user_id,item_id) DO UPDATE SET
                 position_ticks=0,played=excluded.played,
                 play_count=CASE WHEN excluded.played THEN MAX(play_count,1) ELSE 0 END,
                 last_played=CASE WHEN excluded.played THEN excluded.last_played ELSE last_played END",
                params![item_id, played],
            )
            .map_err(|e| e.to_string())?;
        Ok(())
    }
}

fn digest(value: &str) -> String {
    hex::encode(Sha256::digest(value.as_bytes()))
}

fn unix_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrates_old_database_and_keeps_user_preferences() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("bridge.db");
        Connection::open(&path)
            .unwrap()
            .execute_batch(
                "CREATE TABLE progress(user_id TEXT NOT NULL,item_id TEXT NOT NULL,
             position_ticks INTEGER NOT NULL DEFAULT 0,played INTEGER NOT NULL DEFAULT 0,
             PRIMARY KEY(user_id,item_id));
             INSERT INTO progress VALUES('1','video',42,0);",
            )
            .unwrap();
        let db = Database::open(&path).unwrap();
        assert_eq!(db.progress("video"), 42);
        db.set_favorite("video", true).unwrap();
        db.set_json_setting("preferences:test", &serde_json::json!({"ViewType":"List"}))
            .unwrap();
        let server_id = db.server_id().to_owned();
        drop(db);
        let db = Database::open(&path).unwrap();
        assert_eq!(db.server_id(), server_id);
        assert!(db.user_states()["video"].favorite);
        assert_eq!(db.progress("video"), 42);
        assert_eq!(
            db.json_setting("preferences:test").unwrap()["ViewType"],
            "List"
        );
    }
}
