//! Clean-tree preflight: ingest writes straight into the working tree, so the
//! note tree must be reviewable — i.e. clean — before we add to it.

use std::path::Path;
use std::process::Command;

use anyhow::{bail, Result};

/// Refuse to proceed when the note tree (`scope`, e.g. "wiki"; None = whole
/// vault) has uncommitted changes. `force` downgrades the refusal to a
/// warning. Returns human-readable warnings (not-a-repo, forced-through).
pub fn ensure_clean(root: &Path, scope: Option<&str>, force: bool) -> Result<Vec<String>> {
    let mut warnings = Vec::new();

    let inside = Command::new("git")
        .args(["rev-parse", "--is-inside-work-tree"])
        .current_dir(root)
        .output();
    match inside {
        Ok(out) if out.status.success() => {}
        _ => {
            warnings.push(
                "vault is not a git repository — skipping the clean-tree check \
                 (you won't be able to review the ingest as a diff)"
                    .into(),
            );
            return Ok(warnings);
        }
    }

    let mut cmd = Command::new("git");
    cmd.args(["status", "--porcelain"]).current_dir(root);
    if let Some(scope) = scope {
        cmd.arg("--").arg(scope);
    }
    let out = cmd.output()?;
    let status = String::from_utf8_lossy(&out.stdout);
    let dirty: Vec<&str> = status.lines().filter(|l| !l.trim().is_empty()).collect();
    if dirty.is_empty() {
        return Ok(warnings);
    }

    let listing = dirty.join("\n  ");
    if force {
        warnings.push(format!(
            "proceeding over a dirty tree (--force); uncommitted changes:\n  {listing}"
        ));
        Ok(warnings)
    } else {
        bail!(
            "the note tree has uncommitted changes — commit or stash them first so the \
             ingest is reviewable as a clean diff (or pass --force):\n  {listing}"
        );
    }
}
