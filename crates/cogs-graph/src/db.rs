use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use lbug::{Connection, Database, SystemConfig, Value};
use tracing::{info, warn};

use cogs_core::config::Vault;

use crate::schema;

/// Owns the embedded Ladybug database for a vault. The DB is a regenerable
/// cache: a config-hash mismatch (or `force_rebuild`) wipes and recreates it.
pub struct GraphDb {
    db: Database,
    path: PathBuf,
    read_only: bool,
    /// True when this open performed a wipe — callers must do a full sync.
    pub rebuilt: bool,
}

impl GraphDb {
    pub fn open_rw(vault: &Vault, force_rebuild: bool) -> Result<Self> {
        let path = vault.db_path();
        std::fs::create_dir_all(path.parent().unwrap())?;

        let mut rebuilt = force_rebuild || !path.exists();
        if force_rebuild {
            wipe_db_files(&path)?;
        }

        let mut db = open(&path, false)?;

        if !rebuilt {
            let hash_matches = {
                let conn = Connection::new(&db)?;
                stored_config_hash(&conn).as_deref() == Some(vault.config_hash.as_str())
            };
            if !hash_matches {
                info!("config hash changed; rebuilding graph db");
                drop(db);
                wipe_db_files(&path)?;
                db = open(&path, false)?;
                rebuilt = true;
            }
        }

        {
            let conn = Connection::new(&db)?;
            schema::ensure_schema(&conn, &vault.config)?;
            schema::write_meta(&conn, vault)?;
        }

        Ok(Self { db, path, read_only: false, rebuilt })
    }

    pub fn open_ro(vault: &Vault) -> Result<Self> {
        let path = vault.db_path();
        let db = open(&path, true)?;
        // FTS queries need the extension loaded per-database; harmless if absent.
        if let Ok(conn) = Connection::new(&db) {
            let _ = conn.query("LOAD EXTENSION FTS");
        }
        Ok(Self { db, path, read_only: true, rebuilt: false })
    }

    pub fn conn(&self) -> Result<Connection<'_>> {
        Ok(Connection::new(&self.db)?)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn is_read_only(&self) -> bool {
        self.read_only
    }

    /// Force WAL contents into the main file so read-only opens see fresh data.
    pub fn checkpoint(&self) -> Result<()> {
        let conn = self.conn()?;
        if let Err(e) = conn.query("CHECKPOINT") {
            warn!("checkpoint failed (continuing): {e}");
        }
        Ok(())
    }
}

fn open(path: &Path, read_only: bool) -> Result<Database> {
    Database::new(path, SystemConfig::default().read_only(read_only))
        .with_context(|| format!("opening graph db at {}", path.display()))
}

/// Delete the DB file plus lbug's lock/WAL sidecars (`graph.db*`).
fn wipe_db_files(path: &Path) -> Result<()> {
    let Some(parent) = path.parent() else { return Ok(()) };
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else { return Ok(()) };
    if !parent.exists() {
        return Ok(());
    }
    for entry in std::fs::read_dir(parent)? {
        let entry = entry?;
        let fname = entry.file_name();
        let fname = fname.to_string_lossy();
        if fname.starts_with(name) {
            let p = entry.path();
            if p.is_dir() {
                std::fs::remove_dir_all(&p)?;
            } else {
                std::fs::remove_file(&p)?;
            }
        }
    }
    Ok(())
}

fn stored_config_hash(conn: &Connection) -> Option<String> {
    let result = conn
        .query("MATCH (m:Meta {key: 'config_hash'}) RETURN m.value")
        .ok()?;
    for row in result {
        if let Some(Value::String(s)) = row.into_iter().next() {
            return Some(s);
        }
    }
    None
}

impl GraphDb {
    /// kNN over stored embeddings. `table` is "Note" or "Resource"; returns
    /// (key, distance) pairs, nearest first. Errors if no vector index exists.
    pub fn vector_search(
        &self,
        table: &str,
        query_vec: &[f32],
        k: usize,
    ) -> Result<Vec<(String, f64)>> {
        let conn = self.conn()?;
        let _ = conn.query("LOAD EXTENSION VECTOR");
        let index = match table {
            "Resource" => crate::schema::RESOURCE_VEC_INDEX,
            _ => crate::schema::NOTE_VEC_INDEX,
        };
        let key_col = if table == "Resource" { "node.path" } else { "node.id" };
        let mut stmt = conn.prepare(&format!(
            "CALL QUERY_VECTOR_INDEX('{table}', '{index}', $vec, {k}) \
             RETURN {key_col} AS key, distance ORDER BY distance"
        ))?;
        let value = Value::Array(
            lbug::LogicalType::Float,
            query_vec.iter().map(|f| Value::Float(*f)).collect(),
        );
        let result = conn.execute(&mut stmt, vec![("vec", value)])?;
        let mut out = Vec::new();
        for row in result {
            let mut it = row.into_iter();
            if let (Some(Value::String(key)), Some(dist)) = (it.next(), it.next()) {
                let d = match dist {
                    Value::Double(d) => d,
                    Value::Float(f) => f as f64,
                    _ => continue,
                };
                out.push((key, d));
            }
        }
        Ok(out)
    }

    /// Stored embedding for a note, if present.
    pub fn note_embedding(&self, id: &str) -> Result<Option<Vec<f32>>> {
        let conn = self.conn()?;
        let mut stmt =
            conn.prepare("MATCH (n:Note {id: $id}) RETURN n.embedding")?;
        let result = conn.execute(&mut stmt, vec![("id", Value::String(id.into()))])?;
        for row in result {
            if let Some(Value::Array(_, items) | Value::List(_, items)) = row.into_iter().next() {
                let vec = items
                    .into_iter()
                    .filter_map(|v| match v {
                        Value::Float(f) => Some(f),
                        Value::Double(d) => Some(d as f32),
                        _ => None,
                    })
                    .collect();
                return Ok(Some(vec));
            }
        }
        Ok(None)
    }

    /// Run a Cypher query and return rows as JSON objects keyed by column name.
    /// Powers `cogs query` and the HTTP layer.
    pub fn query_json(&self, cypher: &str) -> Result<Vec<serde_json::Value>> {
        let conn = self.conn()?;
        let result = conn.query(cypher)?;
        let columns = result.get_column_names();
        let mut rows = Vec::new();
        for tuple in result {
            let mut obj = serde_json::Map::new();
            for (name, value) in columns.iter().zip(tuple) {
                obj.insert(name.clone(), value_to_json(value));
            }
            rows.push(serde_json::Value::Object(obj));
        }
        Ok(rows)
    }
}

pub fn value_to_json(v: Value) -> serde_json::Value {
    use serde_json::json;
    match v {
        Value::Null(_) => serde_json::Value::Null,
        Value::Bool(b) => json!(b),
        Value::Int64(n) => json!(n),
        Value::Int32(n) => json!(n),
        Value::Int16(n) => json!(n),
        Value::Int8(n) => json!(n),
        Value::UInt64(n) => json!(n),
        Value::UInt32(n) => json!(n),
        Value::UInt16(n) => json!(n),
        Value::UInt8(n) => json!(n),
        Value::Int128(n) => json!(n.to_string()),
        Value::Double(f) => json!(f),
        Value::Float(f) => json!(f),
        Value::String(s) => json!(s),
        Value::Json(j) => j,
        Value::Date(d) => json!(d.to_string()),
        Value::List(_, items) | Value::Array(_, items) => {
            serde_json::Value::Array(items.into_iter().map(value_to_json).collect())
        }
        Value::Struct(fields) => serde_json::Value::Object(
            fields.into_iter().map(|(k, v)| (k, value_to_json(v))).collect(),
        ),
        other => json!(other.to_string()),
    }
}

/// Helpers for building parameter values.
pub fn opt_string(v: Option<String>) -> Value {
    match v {
        Some(s) => Value::String(s),
        None => Value::Null(lbug::LogicalType::String),
    }
}

pub fn opt_date(v: Option<chrono::NaiveDate>) -> Value {
    use chrono::Datelike;
    match v.and_then(|d| {
        let month = time::Month::try_from(d.month() as u8).ok()?;
        time::Date::from_calendar_date(d.year(), month, d.day() as u8).ok()
    }) {
        Some(d) => Value::Date(d),
        None => Value::Null(lbug::LogicalType::Date),
    }
}
