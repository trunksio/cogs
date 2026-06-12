use anyhow::Result;
use lbug::{Connection, Value};
use tracing::warn;

use cogs_core::config::{EdgeTarget, Vault, VaultConfig, SCHEMA_VERSION};

pub const FTS_INDEX: &str = "note_fts";

/// Generated DDL: one Note table (kind as property), one REL table per
/// configured edge type, plus Resource/Tag/Meta. The embedding column exists
/// even with embeddings off (stays NULL) so enabling them later only changes
/// the config hash when `dim` changes.
pub fn ddl(cfg: &VaultConfig) -> Vec<String> {
    let dim = cfg.embeddings.dim;
    let mut stmts = vec![
        format!(
            "CREATE NODE TABLE IF NOT EXISTS Note (
                id STRING,
                path STRING,
                slug STRING,
                dir STRING,
                title STRING,
                kind STRING,
                status STRING,
                created DATE,
                updated DATE,
                tags STRING[],
                body_text STRING,
                body_hash STRING,
                frontmatter_json STRING,
                embedding FLOAT[{dim}],
                embedded_hash STRING,
                PRIMARY KEY (id)
            )"
        ),
        "CREATE NODE TABLE IF NOT EXISTS Tag (name STRING, PRIMARY KEY (name))".into(),
        "CREATE NODE TABLE IF NOT EXISTS Meta (key STRING, value STRING, PRIMARY KEY (key))"
            .into(),
    ];
    if cfg.resources.is_some() {
        stmts.push(format!(
            "CREATE NODE TABLE IF NOT EXISTS Resource (
                path STRING,
                title STRING,
                captured DATE,
                source_date DATE,
                url STRING,
                body_text STRING,
                body_hash STRING,
                embedding FLOAT[{dim}],
                embedded_hash STRING,
                PRIMARY KEY (path)
            )"
        ));
    }
    for e in &cfg.edges {
        let to = match e.target {
            EdgeTarget::Resource => "Resource",
            EdgeTarget::Note => "Note",
        };
        stmts.push(format!(
            "CREATE REL TABLE IF NOT EXISTS {} (FROM Note TO {to}, raw_target STRING)",
            e.name
        ));
    }
    stmts.push("CREATE REL TABLE IF NOT EXISTS TAGGED (FROM Note TO Tag)".into());
    stmts
}

pub fn ensure_schema(conn: &Connection, cfg: &VaultConfig) -> Result<()> {
    // Extension install needs network on first run; the DB is still fully
    // useful without FTS, so tolerate failure with a warning.
    for stmt in ["INSTALL FTS", "LOAD EXTENSION FTS"] {
        if let Err(e) = conn.query(stmt) {
            warn!("{stmt} failed (FTS search may be unavailable): {e}");
        }
    }
    if cfg.embeddings.enabled {
        for stmt in ["INSTALL VECTOR", "LOAD EXTENSION VECTOR"] {
            if let Err(e) = conn.query(stmt) {
                warn!("{stmt} failed (semantic search may be unavailable): {e}");
            }
        }
    }
    for stmt in ddl(cfg) {
        conn.query(&stmt)?;
    }
    ensure_fts_index(conn);
    Ok(())
}

fn ensure_fts_index(conn: &Connection) {
    if let Err(e) = conn.query(&format!(
        "CALL CREATE_FTS_INDEX('Note', '{FTS_INDEX}', ['title', 'body_text'])"
    )) {
        let msg = e.to_string();
        if !msg.contains("already exists") {
            warn!("could not create FTS index: {msg}");
        }
    }
}

/// Ladybug's FTS index does not pick up row updates — drop and recreate after
/// a sync batch (cheap at target scale).
pub fn refresh_fts(conn: &Connection) {
    if let Err(e) = conn.query(&format!("CALL DROP_FTS_INDEX('Note', '{FTS_INDEX}')")) {
        let msg = e.to_string();
        if !msg.contains("does not exist") && !msg.contains("doesn't exist") {
            warn!("dropping FTS index failed: {msg}");
        }
    }
    ensure_fts_index(conn);
}

pub const NOTE_VEC_INDEX: &str = "note_vec";
pub const RESOURCE_VEC_INDEX: &str = "resource_vec";

/// Ladybug locks the embedding column against SET while a vector index
/// exists — drop before writing embeddings, recreate after the batch.
pub fn drop_vector_indices(conn: &Connection) {
    for (table, idx) in [("Note", NOTE_VEC_INDEX), ("Resource", RESOURCE_VEC_INDEX)] {
        let _ = conn.query(&format!("CALL DROP_VECTOR_INDEX('{table}', '{idx}')"));
    }
}

pub fn create_vector_indices(conn: &Connection, has_resources: bool) {
    let mut tables = vec![("Note", NOTE_VEC_INDEX)];
    if has_resources {
        tables.push(("Resource", RESOURCE_VEC_INDEX));
    }
    for (table, idx) in tables {
        if let Err(e) = conn.query(&format!(
            "CALL CREATE_VECTOR_INDEX('{table}', '{idx}', 'embedding', metric := 'cosine')"
        )) {
            let msg = e.to_string();
            if !msg.contains("already exists") {
                warn!("could not create vector index {idx}: {msg}");
            }
        }
    }
}

pub fn write_meta(conn: &Connection, vault: &Vault) -> Result<()> {
    let mut stmt =
        conn.prepare("MERGE (m:Meta {key: $key}) SET m.value = $value")?;
    for (key, value) in [
        ("config_hash", vault.config_hash.clone()),
        ("schema_version", SCHEMA_VERSION.to_string()),
        ("embed_dim", vault.config.embeddings.dim.to_string()),
    ] {
        conn.execute(
            &mut stmt,
            vec![("key", Value::String(key.into())), ("value", Value::String(value))],
        )?;
    }
    Ok(())
}
