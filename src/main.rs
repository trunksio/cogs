use std::path::PathBuf;

mod init_templates;
mod mcp;
mod okf;
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
        /// Scaffold an OKF (Open Knowledge Format) bundle: `type`-keyed
        /// frontmatter, plain markdown links, index.md/log.md, and an
        /// OKF-compatibility cogs.toml
        #[arg(long, conflicts_with = "karpathy")]
        okf: bool,
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
    /// OKF (Open Knowledge Format) v0.1 interop: import, lint, export bundles
    #[command(subcommand)]
    Okf(OkfCommand),
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

#[derive(Subcommand)]
enum OkfCommand {
    /// Import an OKF bundle (directory, .tar.gz/.tgz tarball, or git URL) into a
    /// graph db using the OKF-compatibility profile
    Import {
        /// Path to a directory/tarball, or a git URL
        source: String,
    },
    /// Check the current vault for OKF v0.1 conformance
    Lint,
    /// Export the current vault as an OKF bundle (kind→type, [[wikilinks]]→
    /// markdown links). Tars the output when --out ends in .tar.gz/.tgz
    Export {
        /// Output directory or tarball (default: ./okf-export)
        #[arg(long)]
        out: Option<PathBuf>,
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
        Command::Init { karpathy, okf } => init(*karpathy, *okf),
        Command::Sync { full, with_embeddings } => sync(&cli, *full, *with_embeddings),
        Command::Status => status(&cli),
        Command::Query { cypher } => query(&cli, cypher),
        Command::Ask { question, json } => ask(&cli, question, *json),
        Command::Okf(cmd) => okf_dispatch(&cli, cmd),
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

fn init(karpathy: bool, okf: bool) -> Result<()> {
    let root = std::env::current_dir()?;
    let config_path = root.join("cogs.toml");
    if config_path.exists() {
        bail!("{} already exists", config_path.display());
    }
    if okf {
        return init_okf(&root);
    }
    if !karpathy {
        std::fs::write(&config_path, DEFAULT_CONFIG_TEMPLATE)?;
        println!("wrote {}", config_path.display());
        println!("add `.cogs/` to your .gitignore — it holds the regenerable graph cache");
        println!("(for a full three-layer wiki scaffold, use `cogs init --karpathy`;");
        println!(" for an OKF interchange bundle, use `cogs init --okf`)");
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

/// Scaffold an OKF (Open Knowledge Format) bundle: `type`-keyed frontmatter,
/// plain markdown links, reserved index.md/log.md, a spec-stub README, and the
/// OKF-compatibility cogs.toml.
fn init_okf(root: &std::path::Path) -> Result<()> {
    use init_templates::*;
    for existing in ["index.md", "log.md", "concepts"] {
        if root.join(existing).exists() {
            bail!("{existing} already exists here — refusing to scaffold over it");
        }
    }
    let files: &[(&str, &str)] = &[
        ("cogs.toml", OKF_COGS_TOML),
        ("README.md", OKF_README),
        ("index.md", OKF_INDEX),
        ("log.md", OKF_LOG),
        ("concepts/example.md", OKF_EXAMPLE),
    ];
    for (rel, content) in files {
        let path = root.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, content)?;
        println!("wrote {rel}");
    }
    let gitignore = root.join(".gitignore");
    if gitignore.exists() {
        let current = std::fs::read_to_string(&gitignore)?;
        if !current.contains(".cogs/") {
            std::fs::write(&gitignore, format!("{current}\n.cogs/\n"))?;
            println!("appended .cogs/ to .gitignore");
        }
    } else {
        std::fs::write(&gitignore, "# cogs graph cache — regenerable, never commit\n.cogs/\n")?;
        println!("wrote .gitignore");
    }
    println!();
    println!("OKF bundle scaffolded. Next steps:");
    println!("  1. cogs sync                 (build the graph)");
    println!("  2. cogs okf lint             (check OKF v0.1 conformance)");
    println!("  3. cogs ask \"...\"            (query it, with citations)");
    println!("  4. add concept files under any directory; link them with");
    println!("     standard markdown links: [label](other/concept.md)");
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

fn okf_dispatch(cli: &Cli, cmd: &OkfCommand) -> Result<()> {
    match cmd {
        OkfCommand::Import { source } => okf::import(source, cli.state_dir.as_deref()),
        OkfCommand::Lint => {
            let vault = open_vault(cli)?;
            let conformant = okf::lint(&vault)?;
            if !conformant {
                bail!("OKF v0.1 conformance check failed");
            }
            Ok(())
        }
        OkfCommand::Export { out } => {
            let vault = open_vault(cli)?;
            let out = out.clone().unwrap_or_else(|| PathBuf::from("okf-export"));
            okf::export(&vault, &out)
        }
    }
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
