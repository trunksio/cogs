use zed_extension_api::settings::LspSettings;
use zed_extension_api::{
    self as zed, Result, SlashCommand, SlashCommandOutput, SlashCommandOutputSection,
};

struct CogsExtension {
    cached_binary_path: Option<String>,
}

impl CogsExtension {
    fn binary(&mut self, worktree: &zed::Worktree, args: Vec<String>) -> Result<zed::Command> {
        let settings = LspSettings::for_worktree("cogs", worktree).ok();

        // 1. Explicit user override: lsp.cogs.binary.{path,arguments} in Zed
        //    settings — also the dev-iteration hook. Explicit arguments only
        //    make sense for the LSP role.
        if let Some(binary) = settings.as_ref().and_then(|s| s.binary.as_ref()) {
            if let Some(path) = &binary.path {
                let args = match (&binary.arguments, args.first().map(String::as_str)) {
                    (Some(custom), Some("lsp")) => custom.clone(),
                    _ => args,
                };
                return Ok(zed::Command { command: path.clone(), args, env: Default::default() });
            }
        }

        // 2. A `cogs` on PATH.
        if let Some(path) = worktree.which("cogs") {
            return Ok(zed::Command { command: path, args, env: Default::default() });
        }

        // 3. Previously downloaded binary.
        if let Some(path) = &self.cached_binary_path {
            if std::fs::metadata(path).is_ok_and(|m| m.is_file()) {
                return Ok(zed::Command {
                    command: path.clone(),
                    args,
                    env: Default::default(),
                });
            }
        }

        // 4. GitHub release download (once releases exist).
        Err(
            "cogs binary not found. Install it with `cargo install --git \
             https://github.com/trunksio/cogs` or set lsp.cogs.binary.path \
             in your Zed settings."
                .into(),
        )
    }
}

const TEMPLATES: &[(&str, &str)] = &[
    ("cogs.toml", include_str!("../../templates/karpathy/cogs.toml")),
    ("AGENTS.md", include_str!("../../templates/karpathy/AGENTS.md")),
    ("raw/README.md", include_str!("../../templates/karpathy/raw-README.md")),
    ("wiki/index.md", include_str!("../../templates/karpathy/wiki-index.md")),
    ("wiki/log.md", include_str!("../../templates/karpathy/wiki-log.md")),
    (".zed/settings.json", include_str!("../../templates/karpathy/zed-settings.json")),
    (".gitignore", include_str!("../../templates/karpathy/gitignore")),
];

const SCAFFOLD_DIRS: &str = "wiki/concepts wiki/entities wiki/positions wiki/questions \
                             wiki/sources wiki/_lint raw/clips raw/research raw/files";

impl zed::Extension for CogsExtension {
    fn new() -> Self {
        Self { cached_binary_path: None }
    }

    fn language_server_command(
        &mut self,
        _language_server_id: &zed::LanguageServerId,
        worktree: &zed::Worktree,
    ) -> Result<zed::Command> {
        self.binary(worktree, vec!["lsp".into()])
    }

    fn language_server_initialization_options(
        &mut self,
        _language_server_id: &zed::LanguageServerId,
        worktree: &zed::Worktree,
    ) -> Result<Option<zed::serde_json::Value>> {
        // Forward lsp.cogs.initialization_options verbatim (vault_root etc.).
        Ok(LspSettings::for_worktree("cogs", worktree)
            .ok()
            .and_then(|s| s.initialization_options))
    }

    fn context_server_command(
        &mut self,
        _context_server_id: &zed::ContextServerId,
        project: &zed::Project,
    ) -> Result<zed::Command> {
        // No worktree handle here; rely on PATH (the MCP server discovers the
        // vault by walking up from its cwd, which Zed sets to the project).
        let _ = project;
        Ok(zed::Command {
            command: "cogs".into(),
            args: vec!["mcp".into()],
            env: Default::default(),
        })
    }

    fn run_slash_command(
        &self,
        command: SlashCommand,
        _args: Vec<String>,
        worktree: Option<&zed::Worktree>,
    ) -> Result<SlashCommandOutput, String> {
        match command.name.as_str() {
            "cogs-init" => {
                let root = worktree
                    .map(|w| w.root_path())
                    .unwrap_or_else(|| "the current project".into());
                let mut text = format!(
                    "Scaffold {root} as a Karpathy-style cogs wiki. Follow these steps \
                     exactly, creating files with precisely the content given below.\n\n\
                     IMPORTANT: if cogs.toml, wiki/, raw/, or AGENTS.md already exist in \
                     the project, STOP and report that the vault is already initialised — \
                     do not overwrite anything. If a .gitignore exists, append the cogs \
                     entries to it instead of replacing it.\n\n\
                     1. Create each file below with the exact content shown.\n\
                     2. Create these empty directories, each containing an empty .gitkeep \
                     file: {SCAFFOLD_DIRS}.\n\
                     3. If a `cogs` binary is available on PATH, run `cogs sync` to build \
                     the graph index.\n\
                     4. Tell the user to re-open the project (or restart the language \
                     server) so the cogs LSP picks up cogs.toml, and to read AGENTS.md — \
                     it is the operating manual you (the agent) will follow when working \
                     in this wiki.\n"
                );
                let mut sections = vec![SlashCommandOutputSection {
                    range: (0..text.len() as u32).into(),
                    label: "cogs-init: scaffold instructions".into(),
                }];
                for (path, content) in TEMPLATES {
                    let start = text.len() as u32;
                    text.push_str(&format!(
                        "\n=== FILE: {path} ===\n```\n{content}\n```\n"
                    ));
                    sections.push(SlashCommandOutputSection {
                        range: (start..text.len() as u32).into(),
                        label: format!("file: {path}"),
                    });
                }
                Ok(SlashCommandOutput { text, sections })
            }
            "cogs-graph" => {
                let text = "To open the cogs knowledge-graph visualization, run \
                            `cogs viz --toggle` from the vault directory (native window, \
                            toggleable), or `cogs serve` and open http://127.0.0.1:7117 \
                            in a browser. Modes: semantic (embedding-similarity edges + \
                            unlinked-note suggestions), health (orphans/contradictions/\
                            stale), time (recency). If asked to open it, run the command \
                            for the user."
                    .to_string();
                let len = text.len() as u32;
                Ok(SlashCommandOutput {
                    text,
                    sections: vec![SlashCommandOutputSection {
                        range: (0..len).into(),
                        label: "cogs graph viz".into(),
                    }],
                })
            }
            other => Err(format!("unknown slash command {other:?}")),
        }
    }
}

zed::register_extension!(CogsExtension);
