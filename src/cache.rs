use anyhow::Result;
use rusqlite::{params, Connection};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};
use sha2::{Sha256, Digest};

pub struct Cache {
    conn: Connection,
}

impl Cache {
    pub fn new() -> Result<Self> {
        let db_path = get_cache_db_path()?;
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let conn = Connection::open(&db_path)?;
        conn.execute(
            "CREATE TABLE IF NOT EXISTS cache (
                key TEXT PRIMARY KEY,
                output TEXT NOT NULL,
                timestamp INTEGER NOT NULL,
                trigger_hash TEXT
            )",
            [],
        )?;

        Ok(Self { conn })
    }

    pub fn get(&self, key: &str, trigger_hash: Option<&str>) -> Result<Option<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT output, trigger_hash FROM cache WHERE key = ?1",
        )?;

        let mut rows = stmt.query(params![key])?;
        if let Some(row) = rows.next()? {
            let cached_output: String = row.get(0)?;
            let cached_trigger: Option<String> = row.get(1)?;

            if let Some(current_trigger) = trigger_hash {
                if Some(current_trigger.to_string()) != cached_trigger {
                    return Ok(None); // Invalidated
                }
            }
            return Ok(Some(cached_output));
        }

        Ok(None)
    }

    pub fn set(&self, key: &str, output: &str, trigger_hash: Option<&str>) -> Result<()> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)?
            .as_secs();

        self.conn.execute(
            "INSERT OR REPLACE INTO cache (key, output, timestamp, trigger_hash)
             VALUES (?1, ?2, ?3, ?4)",
            params![key, output, now, trigger_hash],
        )?;

        Ok(())
    }

    pub fn generate_key(cwd: &str, cmd: &str, args: &[String]) -> String {
        let mut hasher = Sha256::new();
        hasher.update(cwd.as_bytes());
        hasher.update(cmd.as_bytes());
        for arg in args {
            hasher.update(arg.as_bytes());
        }
        format!("{:x}", hasher.finalize())
    }
}

fn get_cache_db_path() -> Result<PathBuf> {
    let data_dir = dirs::data_local_dir().unwrap_or_else(|| PathBuf::from("."));
    Ok(data_dir.join("rtk").join("cache.db"))
}

pub fn hash_files(paths: &[PathBuf]) -> Result<String> {
    let mut hasher = Sha256::new();
    for path in paths {
        if path.exists() {
            let metadata = std::fs::metadata(path)?;
            let mtime = metadata.modified()?
                .duration_since(UNIX_EPOCH)?
                .as_secs();
            let size = metadata.len();
            hasher.update(path.to_string_lossy().as_bytes());
            hasher.update(mtime.to_be_bytes());
            hasher.update(size.to_be_bytes());
        }
    }
    Ok(format!("{:x}", hasher.finalize()))
}
