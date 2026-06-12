use zed_extension_api::settings::LspSettings;
use zed_extension_api::{
    self as zed, Result, SlashCommand, SlashCommandOutput, SlashCommandOutputSection,
};

struct CogsExtension {
    cached_binary_path: Option<String>,
}

const REPO: &str = "trunksio/cogs";

impl CogsExtension {
    fn binary(
        &mut self,
        worktree: &zed::Worktree,
        args: Vec<String>,
        ls_id: Option<&zed::LanguageServerId>,
    ) -> Result<zed::Command> {
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

        // 3. Previously downloaded binary (this run or an earlier one).
        if let Some(path) = &self.cached_binary_path {
            if std::fs::metadata(path).is_ok_and(|m| m.is_file()) {
                return Ok(zed::Command { command: path.clone(), args, env: Default::default() });
            }
        }

        // 4. Download from GitHub Releases. Asset names must match what
        //    .github/workflows/release.yml produces.
        let path = self.download(ls_id)?;
        self.cached_binary_path = Some(path.clone());
        Ok(zed::Command { command: path, args, env: Default::default() })
    }

    fn download(&mut self, ls_id: Option<&zed::LanguageServerId>) -> Result<String> {
        let status = |s: zed::LanguageServerInstallationStatus| {
            if let Some(id) = ls_id {
                zed::set_language_server_installation_status(id, &s);
            }
        };
        status(zed::LanguageServerInstallationStatus::CheckingForUpdate);
        let release = zed::latest_github_release(
            REPO,
            zed::GithubReleaseOptions { require_assets: true, pre_release: false },
        )?;

        let (os, arch) = zed::current_platform();
        let target = match (os, arch) {
            (zed::Os::Mac, zed::Architecture::Aarch64) => "aarch64-apple-darwin",
            (zed::Os::Mac, _) => "x86_64-apple-darwin",
            (zed::Os::Linux, zed::Architecture::Aarch64) => "aarch64-unknown-linux-gnu",
            (zed::Os::Linux, _) => "x86_64-unknown-linux-gnu",
            (zed::Os::Windows, _) => {
                return Err("no Windows builds yet — install cogs manually and put it on PATH \
                            or set lsp.cogs.binary.path"
                    .into())
            }
        };
        let asset_name = format!("cogs-{}-{target}.tar.gz", release.version);
        let asset = release
            .assets
            .iter()
            .find(|a| a.name == asset_name)
            .ok_or_else(|| format!("release {} has no asset {asset_name}", release.version))?;

        let version_dir = format!("cogs-{}", release.version);
        let binary_path = format!("{version_dir}/cogs");
        if !std::fs::metadata(&binary_path).is_ok_and(|m| m.is_file()) {
            status(zed::LanguageServerInstallationStatus::Downloading);
            zed::download_file(
                &asset.download_url,
                &version_dir,
                zed::DownloadedFileType::GzipTar,
            )?;
            zed::make_file_executable(&binary_path)?;
            // Drop older cached versions.
            if let Ok(entries) = std::fs::read_dir(".") {
                for entry in entries.flatten() {
                    let name = entry.file_name();
                    let name = name.to_string_lossy();
                    if name.starts_with("cogs-") && *name != *version_dir {
                        std::fs::remove_dir_all(entry.path()).ok();
                    }
                }
            }
        }
        status(zed::LanguageServerInstallationStatus::None);
        Ok(binary_path)
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
        language_server_id: &zed::LanguageServerId,
        worktree: &zed::Worktree,
    ) -> Result<zed::Command> {
        self.binary(worktree, vec!["lsp".into()], Some(language_server_id))
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
        // No worktree handle here, so PATH lookup isn't available — use the
        // cached/downloaded binary, falling back to bare `cogs` (PATH at
        // spawn time). The MCP server discovers the vault by walking up from
        // its cwd, which Zed sets to the project.
        let _ = project;
        let command = self
            .cached_binary_path
            .clone()
            .filter(|p| std::fs::metadata(p).is_ok_and(|m| m.is_file()))
            .or_else(|| self.download(None).ok())
            .unwrap_or_else(|| "cogs".into());
        Ok(zed::Command { command, args: vec!["mcp".into()], env: Default::default() })
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
