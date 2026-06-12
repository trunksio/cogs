use zed_extension_api::settings::LspSettings;
use zed_extension_api::{self as zed, Result};

struct CogsExtension {
    cached_binary_path: Option<String>,
}

impl CogsExtension {
    fn binary(&mut self, worktree: &zed::Worktree) -> Result<zed::Command> {
        let settings = LspSettings::for_worktree("cogs", worktree).ok();

        // 1. Explicit user override: lsp.cogs.binary.{path,arguments} in Zed
        //    settings — also the dev-iteration hook.
        if let Some(binary) = settings.as_ref().and_then(|s| s.binary.as_ref()) {
            if let Some(path) = &binary.path {
                return Ok(zed::Command {
                    command: path.clone(),
                    args: binary.arguments.clone().unwrap_or_else(|| vec!["lsp".into()]),
                    env: Default::default(),
                });
            }
        }

        // 2. A `cogs` on PATH.
        if let Some(path) = worktree.which("cogs") {
            return Ok(zed::Command {
                command: path,
                args: vec!["lsp".into()],
                env: Default::default(),
            });
        }

        // 3. Previously downloaded binary.
        if let Some(path) = &self.cached_binary_path {
            if std::fs::metadata(path).is_ok_and(|m| m.is_file()) {
                return Ok(zed::Command {
                    command: path.clone(),
                    args: vec!["lsp".into()],
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

impl zed::Extension for CogsExtension {
    fn new() -> Self {
        Self { cached_binary_path: None }
    }

    fn language_server_command(
        &mut self,
        _language_server_id: &zed::LanguageServerId,
        worktree: &zed::Worktree,
    ) -> Result<zed::Command> {
        self.binary(worktree)
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
}

zed::register_extension!(CogsExtension);
