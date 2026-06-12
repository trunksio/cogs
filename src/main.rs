use std::path::PathBuf;

mod mcp;
mod viz_window;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use cogs_core::config::Vault;
use cogs_graph::{GraphDb, SyncEngine};

#[derive(Parser)]
#[command(name = "cogs", version, about = "Graph wiki engine for wikilinked markdown vaults")]
struct Cli {
    /// Vault root (default: discover by walking up from the current directory)
    #[arg(long, global = true)]
    vault: Option<PathBuf>,

    /// Explicit config file (default: <vault>/cogs.toml or zero-config defaults)
    #[arg(long, global = true)]
    config: Option<PathBuf>,

    /// Override where derived state (.cogs/) is stored — useful for indexing
    /// a vault without writing into it
    #[arg(long, global = true)]
    state_dir: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Write a commented default cogs.toml in the current directory
    Init,
    /// Index the vault into the graph database
    Sync {
        /// Wipe the database and reprocess every file
        #[arg(long)]
        full: bool,
        /// Compute embeddings even if [embeddings].enabled is false
        #[arg(long)]
        with_embeddings: bool,
    },
    /// Show vault and database status
    Status,
    /// Run a read-only Cypher query and print rows as JSON
    Query { cypher: String },
    /// Run the LSP server on stdio (launched by editors)
    Lsp,
    /// Run the MCP server on stdio (for AI agents)
    Mcp,
    /// Serve the graph-visualization web app + JSON API on localhost
    Serve {
        /// Port (default: from config, usually 7117)
        #[arg(long)]
        port: Option<u16>,
    },
    /// Open the graph viz in a native window (toggleable from Zed)
    Viz {
        /// Toggle visibility if a window is already open (else launch)
        #[arg(long)]
        toggle: bool,
        /// Quit a running viz window
        #[arg(long)]
        quit: bool,
        /// Port (default: from config, usually 7117)
        #[arg(long)]
        port: Option<u16>,
    },
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "cogs=info,cogs_core=info,cogs_graph=info".into()),
        )
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    match &cli.command {
        Command::Init => init(),
        Command::Sync { full, with_embeddings } => sync(&cli, *full, *with_embeddings),
        Command::Status => status(&cli),
        Command::Query { cypher } => query(&cli, cypher),
        Command::Lsp => {
            let vault_override = cli.vault.clone();
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?
                .block_on(cogs_lsp::run_stdio(vault_override));
            Ok(())
        }
        Command::Mcp => {
            let vault = open_vault(&cli)?;
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?
                .block_on(mcp::run_stdio(vault))
        }
        Command::Serve { port } => {
            let vault = open_vault(&cli)?;
            let port = port.unwrap_or(vault.config.server.port);
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?
                .block_on(cogs_server::serve(vault, port))
        }
        Command::Viz { toggle, quit, port } => {
            let vault = open_vault(&cli)?;
            let port = port.unwrap_or(vault.config.server.port);
            let verb = if *quit {
                viz_window::Verb::Quit
            } else if *toggle {
                viz_window::Verb::Toggle
            } else {
                viz_window::Verb::Show
            };
            viz_window::run(vault, port, verb)
        }
    }
}

fn open_vault(cli: &Cli) -> Result<Vault> {
    let start = cli.vault.clone().unwrap_or(std::env::current_dir()?);
    let mut vault = match &cli.config {
        Some(cfg) => Vault::load(&start, cfg)?,
        None => Vault::discover(&start)?,
    };
    if let Some(dir) = &cli.state_dir {
        vault = vault.with_state_dir(dir.clone());
    }
    Ok(vault)
}

fn init() -> Result<()> {
    let path = std::env::current_dir()?.join("cogs.toml");
    if path.exists() {
        bail!("{} already exists", path.display());
    }
    std::fs::write(&path, DEFAULT_CONFIG_TEMPLATE)?;
    println!("wrote {}", path.display());
    println!("add `.cogs/` to your .gitignore — it holds the regenerable graph cache");
    Ok(())
}

fn sync(cli: &Cli, full: bool, with_embeddings: bool) -> Result<()> {
    let vault = open_vault(cli)?;
    let engine = SyncEngine::new(&vault)?;
    let db = GraphDb::open_rw(&vault, full).context("opening graph db read-write")?;
    let provider = if with_embeddings || vault.config.embeddings.enabled {
        Some(cogs_graph::make_provider(&vault.config.embeddings)?)
    } else {
        None
    };
    let out = engine.sync_with(&db, full, provider.as_deref())?;
    println!(
        "cogs sync: mode={:?} notes={} relinked={} resources={} deleted={} edges={} embeddings={} | total: {} notes, {} resources",
        out.mode,
        out.notes_synced,
        out.notes_relinked,
        out.resources_synced,
        out.deleted,
        out.edges_written,
        out.embeddings_written,
        out.total_notes,
        out.total_resources,
    );
    println!("graph db at {}", db.path().display());
    Ok(())
}

fn print_count(db: &GraphDb, label: &str, cypher: &str) {
    if let Ok(rows) = db.query_json(cypher) {
        if let Some(v) = rows
            .first()
            .and_then(|r| r.as_object())
            .and_then(|o| o.values().next())
        {
            println!("{label:<14} {v}");
        }
    }
}

fn status(cli: &Cli) -> Result<()> {
    let vault = open_vault(cli)?;
    println!("vault root:    {}", vault.root.display());
    println!("config hash:   {}", &vault.config_hash[..16]);
    println!("state dir:     {}", vault.state_dir().display());
    let db_path = vault.db_path();
    if !db_path.exists() {
        println!("graph db:      not built yet (run `cogs sync`)");
        return Ok(());
    }
    let size_mb = std::fs::metadata(&db_path)
        .map(|m| m.len() as f64 / 1_048_576.0)
        .unwrap_or(0.0);
    println!("graph db:      {} ({size_mb:.1} MB)", db_path.display());
    let db = GraphDb::open_ro(&vault).context("opening graph db read-only")?;
    print_count(&db, "notes:", "MATCH (n:Note) RETURN count(n)");
    print_count(&db, "tags:", "MATCH (t:Tag) RETURN count(t)");
    if vault.config.resources.is_some() {
        print_count(&db, "resources:", "MATCH (r:Resource) RETURN count(r)");
    }
    let mut edge_names: Vec<String> =
        vault.config.edges.iter().map(|e| e.name.clone()).collect();
    edge_names.push("TAGGED".into());
    for name in edge_names {
        print_count(&db, &format!("{name}:"), &format!("MATCH ()-[r:{name}]->() RETURN count(r)"));
    }
    Ok(())
}

fn query(cli: &Cli, cypher: &str) -> Result<()> {
    let vault = open_vault(cli)?;
    let db = GraphDb::open_ro(&vault)
        .context("opening graph db read-only (run `cogs sync` first if it doesn't exist)")?;
    let rows = db.query_json(cypher)?;
    println!("{}", serde_json::to_string_pretty(&rows)?);
    Ok(())
}

const DEFAULT_CONFIG_TEMPLATE: &str = r#"# cogs vault configuration
# Delete this file to fall back to zero-config defaults (every *.md is a note,
# wikilinks become LINKS_TO edges, tags from frontmatter + inline #tags).

[vault]
notes = ["**/*.md"]
exclude = [".obsidian/**", ".cogs/**"]
# Strip a leading directory from note ids, e.g. "wiki/" turns
# wiki/concepts/x.md into the id "concepts-x".
id_strip_prefix = ""

# Uncomment to track an immutable source layer (binary files describe
# themselves via a sibling <name>.meta.md):
# [resources]
# paths = ["raw/**/*"]
# exclude = ["raw/README.md"]

[kinds]
# Known values for the frontmatter `kind` field; empty = kinds unused.
values = []
unknown = "allow"   # allow | warn | error

# The edge derived from body [[wikilinks]] (at most one):
[[edges]]
name = "LINKS_TO"
source = "wikilinks"

# Frontmatter-driven typed edges:
# [[edges]]
# name = "SOURCE_OF"
# source = "frontmatter"
# field = "source_refs"
# target = "resource"     # or "note" (default)

[tags]
field = "tags"
inline = true

[diagnostics]
broken_link = "warn"      # allow | warn | error
ambiguous_link = "warn"
# required_fields = { source = ["source_refs"] }
# stale_after_days = 180

[embeddings]
enabled = false
provider = "ollama"        # ollama | openai
model = "nomic-embed-text"
dim = 768
endpoint = "http://localhost:11434"

[server]
port = 7117
"#;
