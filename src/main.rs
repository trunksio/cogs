use std::path::PathBuf;

mod init_templates;
mod mcp;
#[cfg(feature = "viz-window")]
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
    /// Initialise a vault in the current directory
    Init {
        /// Scaffold a full Karpathy-style three-layer wiki (raw/ + wiki/ +
        /// AGENTS.md operating manual) instead of just a config file
        #[arg(long)]
        karpathy: bool,
    },
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
    /// Answer a question using only the wiki, with citations
    Ask {
        /// The question to answer
        question: String,
        /// Emit the full answer (citations, contradictions) as JSON
        #[arg(long)]
        json: bool,
    },
    /// Ingest a raw capture: LLM-drafted source page + wiki updates, written
    /// to the working tree for review via git diff
    Ingest {
        /// Raw file to ingest (vault-relative like raw/clips/x.md, or absolute)
        raw_file: PathBuf,
        /// Proceed even if the note tree has uncommitted changes
        #[arg(long)]
        force: bool,
        /// Print the planned writes (full content) without touching anything
        #[arg(long)]
        dry_run: bool,
        /// Emit the ingest report as JSON
        #[arg(long)]
        json: bool,
        /// Skip writing training-pair records
        #[arg(long)]
        no_training_capture: bool,
        /// Max existing pages to draft updates for
        #[arg(long, default_value_t = 8)]
        pages_cap: usize,
        /// Training-record directory (default <state-dir>/training)
        #[arg(long)]
        training_dir: Option<PathBuf>,
    },
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
        Command::Init { karpathy } => init(*karpathy),
        Command::Sync { full, with_embeddings } => sync(&cli, *full, *with_embeddings),
        Command::Status => status(&cli),
        Command::Query { cypher } => query(&cli, cypher),
        Command::Ask { question, json } => ask(&cli, question, *json),
        Command::Ingest {
            raw_file,
            force,
            dry_run,
            json,
            no_training_capture,
            pages_cap,
            training_dir,
        } => ingest(
            &cli,
            raw_file,
            cogs_ingest::IngestOptions {
                force: *force,
                dry_run: *dry_run,
                pages_cap: *pages_cap,
                capture: !no_training_capture,
                training_dir: training_dir.clone(),
                ..Default::default()
            },
            *json,
        ),
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
        #[cfg(feature = "viz-window")]
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
        #[cfg(not(feature = "viz-window"))]
        Command::Viz { .. } => {
            bail!(
                "this build has no native viz window — run `cogs serve` and open \
                 http://127.0.0.1:7117 in a browser instead"
            );
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

fn init(karpathy: bool) -> Result<()> {
    let root = std::env::current_dir()?;
    let config_path = root.join("cogs.toml");
    if config_path.exists() {
        bail!("{} already exists", config_path.display());
    }
    if !karpathy {
        std::fs::write(&config_path, DEFAULT_CONFIG_TEMPLATE)?;
        println!("wrote {}", config_path.display());
        println!("add `.cogs/` to your .gitignore — it holds the regenerable graph cache");
        println!("(for a full three-layer wiki scaffold, use `cogs init --karpathy`)");
        return Ok(());
    }

    use init_templates::*;
    // Refuse to scaffold over an existing wiki structure.
    for existing in ["wiki", "raw", "AGENTS.md"] {
        if root.join(existing).exists() {
            bail!("{existing} already exists here — refusing to scaffold over it");
        }
    }
    let files: &[(&str, &str)] = &[
        ("cogs.toml", KARPATHY_COGS_TOML),
        ("AGENTS.md", KARPATHY_AGENTS_MD),
        ("raw/README.md", KARPATHY_RAW_README),
        ("wiki/index.md", KARPATHY_WIKI_INDEX),
        ("wiki/log.md", KARPATHY_WIKI_LOG),
        (".zed/settings.json", KARPATHY_ZED_SETTINGS),
    ];
    for (rel, content) in files {
        let path = root.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, content)?;
        println!("wrote {rel}");
    }
    for dir in [
        "wiki/concepts",
        "wiki/entities",
        "wiki/positions",
        "wiki/questions",
        "wiki/sources",
        "wiki/_lint",
        "raw/clips",
        "raw/research",
        "raw/files",
    ] {
        std::fs::create_dir_all(root.join(dir))?;
        std::fs::write(root.join(dir).join(".gitkeep"), "")?;
        println!("created {dir}/");
    }
    // Append to an existing .gitignore rather than clobbering it.
    let gitignore = root.join(".gitignore");
    if gitignore.exists() {
        let current = std::fs::read_to_string(&gitignore)?;
        if !current.contains(".cogs/") {
            std::fs::write(&gitignore, format!("{current}\n{}", KARPATHY_GITIGNORE))?;
            println!("appended .cogs/ to .gitignore");
        }
    } else {
        std::fs::write(&gitignore, KARPATHY_GITIGNORE)?;
        println!("wrote .gitignore");
    }

    println!();
    println!("Vault scaffolded. Next steps:");
    println!("  1. git init && git add -A && git commit  (if not already a repo)");
    println!("  2. cogs sync                              (build the graph)");
    println!("  3. open in Zed — the cogs extension picks up cogs.toml automatically");
    println!("  4. read AGENTS.md — it's the operating manual your AI agents follow");
    println!("  5. optional: enable [embeddings] in cogs.toml for semantic search");
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

fn ask(cli: &Cli, question: &str, as_json: bool) -> Result<()> {
    let vault = open_vault(cli)?;
    let db = GraphDb::open_ro(&vault)
        .context("opening graph db read-only (run `cogs sync` first if it doesn't exist)")?;
    let chat = cogs_llm::make_provider(&vault.config.llm)
        .context("building the LLM provider (check [llm] in cogs.toml)")?;
    // Query embedder for semantic retrieval; optional — FTS still works without.
    let embed = if vault.config.embeddings.enabled {
        cogs_graph::make_provider(&vault.config.embeddings)
            .map_err(|e| tracing::warn!("semantic retrieval disabled: {e:#}"))
            .ok()
    } else {
        None
    };
    let asker = cogs_ask::Asker::new(&vault, &db, chat.as_ref(), embed.as_deref());
    let answer = asker.ask(question)?;

    if as_json {
        println!("{}", serde_json::to_string_pretty(&answer)?);
        return Ok(());
    }
    println!("{}\n", answer.text);
    if !answer.contradictions.is_empty() {
        println!("⚠ contradictions flagged:");
        for c in &answer.contradictions {
            println!("  [{}] ⇄ [{}]", c.source, c.target);
        }
        println!();
    }
    if !answer.citations.is_empty() {
        println!("Sources ({} of {} notes considered):", answer.citations.len(), answer.notes_considered);
        for c in &answer.citations {
            println!("  [{}] {}", c.id, c.title);
        }
    }
    Ok(())
}

fn ingest(cli: &Cli, raw_file: &PathBuf, opts: cogs_ingest::IngestOptions, as_json: bool) -> Result<()> {
    let vault = open_vault(cli)?;
    let chat = cogs_llm::make_provider(&vault.config.llm)
        .context("building the LLM provider (check [llm] in cogs.toml)")?;
    let embed = if vault.config.embeddings.enabled {
        cogs_graph::make_provider(&vault.config.embeddings)
            .map_err(|e| tracing::warn!("semantic retrieval disabled: {e:#}"))
            .ok()
    } else {
        None
    };

    // Freshen the index if we can win the writer; a running cogs process
    // (LSP/MCP primary) keeps it fresh otherwise, so read-only is fine.
    let db = match GraphDb::open_rw(&vault, false) {
        Ok(db) => {
            if let Err(e) = SyncEngine::new(&vault)?.sync_with(&db, false, embed.as_deref()) {
                tracing::warn!("pre-ingest sync failed: {e:#}");
            }
            db
        }
        Err(e) => {
            tracing::info!("graph writer busy ({e:#}); continuing read-only");
            GraphDb::open_ro(&vault)
                .context("opening graph db (run `cogs sync` first if it doesn't exist)")?
        }
    };

    let dry_run = opts.dry_run;
    let ingester = cogs_ingest::Ingester::new(&vault, &db, chat.as_ref(), embed.as_deref(), opts);
    let report = ingester.ingest(raw_file)?;

    if as_json {
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }

    if let Some(existing) = &report.already_ingested {
        println!("already ingested — source page: {existing}");
        return Ok(());
    }
    for w in &report.warnings {
        eprintln!("⚠ {w}");
    }
    if !report.near_duplicates.is_empty() {
        eprintln!("⚠ possible duplicates:");
        for d in &report.near_duplicates {
            eprintln!("    {} ({}, {:.2})", d.id, d.via, d.score);
        }
    }
    if dry_run {
        for p in &report.planned {
            println!("--- {} ({}) ---", p.rel_path, p.action);
            println!("{}", p.content);
        }
        println!("dry run: nothing written");
        return Ok(());
    }
    println!(
        "ingested {} → {}",
        report.raw_path,
        report.source_page.as_deref().unwrap_or("?")
    );
    if !report.pages_updated.is_empty() {
        println!("pages updated: {}", report.pages_updated.join(", "));
    }
    if !report.pages_created.is_empty() {
        println!("pages created: {}", report.pages_created.join(", "));
    }
    if !report.contradictions.is_empty() {
        println!("⚠ contradictions flagged:");
        for c in &report.contradictions {
            println!("    {} — {}", c.page_id, c.explanation);
        }
    }
    if report.training_records > 0 {
        println!("training records captured: {} (run {})", report.training_records, report.run_id);
    }
    if !report.synced {
        println!("note: graph not re-synced here (a running cogs process will, or run `cogs sync`)");
    }
    println!("review with: git diff");
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
