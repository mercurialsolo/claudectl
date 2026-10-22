//! Plugin assets embedded in the binary (#325).
//!
//! Every file under `claude-plugin/` is compiled into the binary via
//! `include_str!` so `claudectl init` can write a working plugin to
//! `~/.claude/plugins/claudectl/` without the user cloning the repo.
//! Total embed cost: ~35 KB across 18 files — well below the binary's
//! 1 MB target.
//!
//! Source of truth stays on disk at `claude-plugin/`; this module just
//! gives the binary a way to re-emit the same contents at install time.
//! `brew upgrade claudectl` automatically ships the latest plugin files
//! because they're baked into the new binary.

use std::io;
use std::path::{Path, PathBuf};

/// One embedded plugin file: where to write it (relative to the install
/// root) and what to write.
pub struct Asset {
    /// Relative path inside `~/.claude/plugins/claudectl/`. Always uses
    /// forward slashes — converted to platform separators at write time.
    pub rel_path: &'static str,
    /// File contents baked in by `include_str!`. Always text — the
    /// plugin doesn't carry binary assets today.
    pub contents: &'static str,
}

/// Every file under `claude-plugin/` that needs to land on disk. Order
/// doesn't matter; each file is written independently and parent dirs
/// are created as needed.
pub const ASSETS: &[Asset] = &[
    Asset {
        rel_path: ".claude-plugin/plugin.json",
        contents: include_str!("../../claude-plugin/.claude-plugin/plugin.json"),
    },
    Asset {
        rel_path: ".mcp.json",
        contents: include_str!("../../claude-plugin/.mcp.json"),
    },
    Asset {
        rel_path: "agents/supervisor.md",
        contents: include_str!("../../claude-plugin/agents/supervisor.md"),
    },
    Asset {
        rel_path: "commands/auto-insights.md",
        contents: include_str!("../../claude-plugin/commands/auto-insights.md"),
    },
    Asset {
        rel_path: "commands/brain-stats.md",
        contents: include_str!("../../claude-plugin/commands/brain-stats.md"),
    },
    Asset {
        rel_path: "commands/brain.md",
        contents: include_str!("../../claude-plugin/commands/brain.md"),
    },
    Asset {
        rel_path: "commands/inbox.md",
        contents: include_str!("../../claude-plugin/commands/inbox.md"),
    },
    Asset {
        rel_path: "commands/role.md",
        contents: include_str!("../../claude-plugin/commands/role.md"),
    },
    Asset {
        rel_path: "commands/sessions.md",
        contents: include_str!("../../claude-plugin/commands/sessions.md"),
    },
    Asset {
        rel_path: "commands/spend.md",
        contents: include_str!("../../claude-plugin/commands/spend.md"),
    },
    Asset {
        rel_path: "hooks/hooks.json",
        contents: include_str!("../../claude-plugin/hooks/hooks.json"),
    },
    Asset {
        rel_path: "hooks/scripts/brain-gate.sh",
        contents: include_str!("../../claude-plugin/hooks/scripts/brain-gate.sh"),
    },
    Asset {
        rel_path: "hooks/scripts/budget-check.sh",
        contents: include_str!("../../claude-plugin/hooks/scripts/budget-check.sh"),
    },
    Asset {
        rel_path: "hooks/scripts/inbox-drain.sh",
        contents: include_str!("../../claude-plugin/hooks/scripts/inbox-drain.sh"),
    },
    Asset {
        rel_path: "hooks/scripts/outcome-record.sh",
        contents: include_str!("../../claude-plugin/hooks/scripts/outcome-record.sh"),
    },
    Asset {
        rel_path: "hooks/scripts/session-briefing.sh",
        contents: include_str!("../../claude-plugin/hooks/scripts/session-briefing.sh"),
    },
    Asset {
        rel_path: "skills/session-monitoring/SKILL.md",
        contents: include_str!("../../claude-plugin/skills/session-monitoring/SKILL.md"),
    },
    Asset {
        rel_path: "skills/hive/SKILL.md",
        contents: include_str!("../../claude-plugin/skills/hive/SKILL.md"),
    },
];

/// Where the plugin lives by default — `~/.claude/plugins/claudectl/`.
/// Returns `None` when `$HOME` is unset (CI, sandboxed environments)
/// so the caller can fall through gracefully.
pub fn default_install_dir() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(
        PathBuf::from(home)
            .join(".claude")
            .join("plugins")
            .join("claudectl"),
    )
}

/// Write every embedded asset to `dest`, creating parent dirs as needed.
/// Returns the list of files written (with their absolute paths) so the
/// init wizard can report what landed where.
///
/// Idempotent: existing files are overwritten (this is how
/// `brew upgrade` re-syncs the plugin). The wizard's `--check` path can
/// compare on-disk hashes to detect manual edits.
pub fn write_assets(dest: &Path) -> io::Result<Vec<PathBuf>> {
    let mut written = Vec::with_capacity(ASSETS.len());
    for asset in ASSETS {
        let target = dest.join(asset.rel_path);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&target, asset.contents)?;
        // Shell hook scripts need to be executable on POSIX. On Windows
        // the perm-set is a no-op.
        #[cfg(unix)]
        if asset.rel_path.ends_with(".sh") {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&target)?.permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&target, perms)?;
        }
        written.push(target);
    }
    Ok(written)
}

/// Remove the plugin install dir entirely. Idempotent — missing dir is
/// not an error. Used by `init --remove` so the soft uninstall actually
/// removes the plugin files it installed.
pub fn remove_assets(dest: &Path) -> io::Result<()> {
    match std::fs::remove_dir_all(dest) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The embedded copy of one asset, by the path it installs to.
    fn asset(rel_path: &str) -> &'static str {
        ASSETS
            .iter()
            .find(|a| a.rel_path == rel_path)
            .unwrap_or_else(|| panic!("{rel_path} is not an embedded asset"))
            .contents
    }

    /// The manifest is baked into the binary, so its version is what a user
    /// sees after `claudectl init` or `brew upgrade` — not what the repo said
    /// when the plugin was last touched. It sat at 0.53.0 for 27 releases
    /// because the release bump only moved the three `Cargo.toml` files and
    /// nothing read this one. Lockstep with the crate is the rule; this test
    /// is what makes forgetting it fail.
    #[test]
    fn plugin_manifest_version_tracks_the_crate() {
        let parsed: serde_json::Value =
            serde_json::from_str(asset(".claude-plugin/plugin.json")).unwrap();
        assert_eq!(
            parsed["version"].as_str().unwrap(),
            env!("CARGO_PKG_VERSION"),
            "claude-plugin/.claude-plugin/plugin.json is out of step with \
             Cargo.toml — bump it in the release commit"
        );
    }

    /// Same reasoning for the skills, which drifted further still (0.48.0).
    #[test]
    fn skill_versions_track_the_crate() {
        let skills: Vec<&Asset> = ASSETS
            .iter()
            .filter(|a| a.rel_path.ends_with("/SKILL.md"))
            .collect();
        assert!(skills.len() >= 2, "expected session-monitoring and hive");
        for skill in skills {
            let frontmatter_version = skill
                .contents
                .lines()
                .take_while(|l| !l.starts_with("# "))
                .find_map(|l| l.strip_prefix("version:"))
                .unwrap_or_else(|| panic!("{} frontmatter should carry a version", skill.rel_path))
                .trim();
            assert_eq!(
                frontmatter_version,
                env!("CARGO_PKG_VERSION"),
                "claude-plugin/{} is out of step with Cargo.toml — bump it in the \
                 release commit",
                skill.rel_path
            );
        }
    }

    /// Every file under `claude-plugin/` has to be listed in `ASSETS`, or
    /// `claudectl init` installs a plugin missing it.
    #[test]
    fn every_plugin_file_is_embedded() {
        fn walk(dir: &Path, root: &Path, out: &mut Vec<String>) {
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    walk(&path, root, out);
                } else {
                    let rel = path.strip_prefix(root).unwrap();
                    out.push(rel.to_string_lossy().replace('\\', "/"));
                }
            }
        }
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("claude-plugin");
        let mut on_disk = Vec::new();
        walk(&root, &root, &mut on_disk);
        for rel in on_disk {
            assert!(
                ASSETS.iter().any(|a| a.rel_path == rel),
                "claude-plugin/{rel} is not embedded in ASSETS"
            );
        }
    }

    #[test]
    fn write_assets_creates_every_file_with_expected_contents() {
        let tmp = tempfile::tempdir().unwrap();
        let written = write_assets(tmp.path()).unwrap();
        assert_eq!(written.len(), ASSETS.len());
        for asset in ASSETS {
            let path = tmp.path().join(asset.rel_path);
            assert!(path.exists(), "expected {} to be written", path.display());
            let on_disk = std::fs::read_to_string(&path).unwrap();
            assert_eq!(
                on_disk,
                asset.contents,
                "content mismatch at {}",
                path.display()
            );
        }
    }

    #[test]
    fn write_assets_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        write_assets(tmp.path()).unwrap();
        // Second write — should not error, contents identical.
        write_assets(tmp.path()).unwrap();
        for asset in ASSETS {
            let path = tmp.path().join(asset.rel_path);
            assert_eq!(std::fs::read_to_string(&path).unwrap(), asset.contents);
        }
    }

    #[cfg(unix)]
    #[test]
    fn shell_scripts_get_executable_bit() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        write_assets(tmp.path()).unwrap();
        for asset in ASSETS {
            if asset.rel_path.ends_with(".sh") {
                let path = tmp.path().join(asset.rel_path);
                let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
                assert_eq!(
                    mode,
                    0o755,
                    "expected 0o755 on {}, got {:o}",
                    path.display(),
                    mode
                );
            }
        }
    }

    #[test]
    fn remove_assets_wipes_the_install() {
        let tmp = tempfile::tempdir().unwrap();
        let install = tmp.path().join("plugin");
        write_assets(&install).unwrap();
        assert!(install.exists());
        remove_assets(&install).unwrap();
        assert!(!install.exists());
    }

    #[test]
    fn remove_assets_tolerates_missing_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let nope = tmp.path().join("nope");
        // Should not error even though the dir was never created.
        remove_assets(&nope).unwrap();
    }

    #[test]
    fn every_asset_has_non_empty_contents() {
        // Catches a build-time include path going stale.
        for asset in ASSETS {
            assert!(
                !asset.contents.is_empty(),
                "asset {} was embedded empty — check include_str! path",
                asset.rel_path
            );
        }
    }
}
