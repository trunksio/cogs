//! File templates for `cogs init`. The karpathy template scaffolds a full
//! three-layer LLM-native wiki (raw/ immutable sources → wiki/ synthesis),
//! including the AGENTS.md operating manual that tells agents how to ingest
//! and which tools they have.
//!
//! Shared with the Zed extension's /cogs-init slash command — both sides
//! include_str! the same files under templates/karpathy/.

pub const KARPATHY_COGS_TOML: &str = include_str!("../templates/karpathy/cogs.toml");
pub const KARPATHY_AGENTS_MD: &str = include_str!("../templates/karpathy/AGENTS.md");
pub const KARPATHY_RAW_README: &str = include_str!("../templates/karpathy/raw-README.md");
pub const KARPATHY_WIKI_INDEX: &str = include_str!("../templates/karpathy/wiki-index.md");
pub const KARPATHY_WIKI_LOG: &str = include_str!("../templates/karpathy/wiki-log.md");
pub const KARPATHY_ZED_SETTINGS: &str = include_str!("../templates/karpathy/zed-settings.json");
pub const KARPATHY_GITIGNORE: &str = include_str!("../templates/karpathy/gitignore");
