// Skill discovery: scans the standard Claude Code skill locations and returns
// metadata parsed from each skill's YAML frontmatter.
//
// Skills can live in three shapes:
//   1. ~/.claude/skills/<name>/SKILL.md            (user-installed)
//   2. ~/.claude/plugins/<plugin>/skills/<name>/   (shipped by a plugin)
//   3. <cwd>/.claude/skills/<name>/SKILL.md        (project-local)
//
// We also accept the flat form ~/.claude/skills/<name>.md for compatibility
// with older skill layouts.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::helpers::dirs_home;

/// Where a discovered skill came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillSource {
    /// `~/.claude/skills/...`
    User,
    /// `~/.claude/plugins/<plugin>/skills/...`
    Plugin,
    /// `<cwd>/.claude/skills/...`
    Project,
}

impl SkillSource {
    pub fn label(&self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Plugin => "plugin",
            Self::Project => "project",
        }
    }
}

#[derive(Debug, Clone)]
pub struct DiscoveredSkill {
    /// Skill name (from frontmatter `name`, falls back to directory name).
    pub name: String,
    /// One-line description from frontmatter (empty if not present).
    pub description: String,
    /// Path to the skill's markdown file (the one we'd share).
    pub path: PathBuf,
    /// Where it was discovered.
    pub source: SkillSource,
    /// Plugin name, if `source == Plugin`.
    pub plugin: Option<String>,
    /// File size in bytes.
    pub size_bytes: u64,
}

impl DiscoveredSkill {
    /// Stable key used to compare against shared knowledge units. Matches
    /// the semantic key shape in src/hive/mod.rs (`skill:<lowercased-name>`).
    pub fn semantic_key(&self) -> String {
        format!("skill:{}", self.name.to_lowercase().replace(' ', "-"))
    }

    /// True if the skill body is small enough to fit in a hive `Skill` unit.
    pub fn within_share_limit(&self) -> bool {
        // Mirrors `hive::MAX_SKILL_BYTES` (32 KiB) but we keep this module
        // free of the hive feature gate so the TUI can list skills even when
        // hive is disabled.
        self.size_bytes <= 32 * 1024
    }
}

/// Scan all known skill locations.
///
/// `project_root` is the directory used for project-local skills — pass the
/// current working directory (or None to skip the project sweep).
pub fn discover(project_root: Option<&Path>) -> Vec<DiscoveredSkill> {
    let mut out = Vec::new();
    let home = dirs_home();

    scan_skill_root(
        &home.join(".claude/skills"),
        SkillSource::User,
        None,
        &mut out,
    );

    let plugins_root = home.join(".claude/plugins");
    if let Ok(entries) = fs::read_dir(&plugins_root) {
        for entry in entries.flatten() {
            let plugin_dir = entry.path();
            if !plugin_dir.is_dir() {
                continue;
            }
            let plugin_name = plugin_dir
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .to_string();
            scan_skill_root(
                &plugin_dir.join("skills"),
                SkillSource::Plugin,
                Some(plugin_name),
                &mut out,
            );
        }
    }

    if let Some(root) = project_root {
        scan_skill_root(
            &root.join(".claude/skills"),
            SkillSource::Project,
            None,
            &mut out,
        );
    }

    out.sort_by_key(|a| a.name.to_lowercase());
    out
}

fn scan_skill_root(
    root: &Path,
    source: SkillSource,
    plugin: Option<String>,
    out: &mut Vec<DiscoveredSkill>,
) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            // Conventional layout: <dir>/SKILL.md (or skill.md)
            let candidates = [path.join("SKILL.md"), path.join("skill.md")];
            for candidate in &candidates {
                if candidate.exists() {
                    if let Some(skill) = load_skill(candidate, source, plugin.clone(), Some(&path))
                    {
                        out.push(skill);
                    }
                    break;
                }
            }
        } else if path.extension().and_then(|e| e.to_str()) == Some("md") {
            // Flat form: ~/.claude/skills/<name>.md
            if let Some(skill) = load_skill(&path, source, plugin.clone(), None) {
                out.push(skill);
            }
        }
    }
}

fn load_skill(
    path: &Path,
    source: SkillSource,
    plugin: Option<String>,
    dir_for_fallback_name: Option<&Path>,
) -> Option<DiscoveredSkill> {
    let body = fs::read_to_string(path).ok()?;
    let fm = parse_frontmatter(&body);

    let fallback_name = dir_for_fallback_name
        .and_then(|d| d.file_name())
        .or_else(|| path.file_stem())
        .and_then(|n| n.to_str())
        .unwrap_or("(unnamed)")
        .to_string();

    let name = fm.get("name").cloned().unwrap_or(fallback_name);
    let description = fm.get("description").cloned().unwrap_or_default();

    let size_bytes = fs::metadata(path).map(|m| m.len()).unwrap_or(0);

    Some(DiscoveredSkill {
        name,
        description,
        path: path.to_path_buf(),
        source,
        plugin,
        size_bytes,
    })
}

/// Lightweight YAML frontmatter parser. Mirrors hive::cli::parse_frontmatter but
/// kept local so this module has no feature-gated dependencies.
fn parse_frontmatter(content: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    let trimmed = content.trim_start();
    if !trimmed.starts_with("---") {
        return map;
    }
    let after_open = &trimmed[3..].trim_start_matches('\r');
    let after_open = after_open.strip_prefix('\n').unwrap_or(after_open);
    let Some(close_pos) = after_open.find("\n---") else {
        return map;
    };
    let yaml = &after_open[..close_pos];
    for line in yaml.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((k, v)) = line.split_once(':') {
            let key = k.trim().to_string();
            let value = v.trim().trim_matches('"').trim_matches('\'').to_string();
            if !key.is_empty() && !value.is_empty() {
                map.insert(key, value);
            }
        }
    }
    map
}

/// Shared-status lookup against a set of semantic keys already present in the
/// local hive store. Caller passes in the keys (lowercased) for skills that are
/// already shared so we don't take a hive dependency here.
pub fn is_shared(skill: &DiscoveredSkill, shared_keys: &std::collections::HashSet<String>) -> bool {
    shared_keys.contains(&skill.semantic_key())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    fn write_skill(dir: &Path, name: &str, frontmatter: &str, body: &str) -> PathBuf {
        let skill_dir = dir.join(name);
        fs::create_dir_all(&skill_dir).unwrap();
        let path = skill_dir.join("SKILL.md");
        let mut f = fs::File::create(&path).unwrap();
        writeln!(f, "---\n{frontmatter}\n---").unwrap();
        writeln!(f, "{body}").unwrap();
        path
    }

    #[test]
    fn parses_frontmatter_and_falls_back_to_dir_name() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("skills");
        fs::create_dir_all(&root).unwrap();
        write_skill(
            &root,
            "demo-skill",
            "name: Demo Skill\ndescription: A demo.\nversion: 1.0",
            "body",
        );
        write_skill(&root, "no-frontmatter", "", "just body");

        let mut out = Vec::new();
        scan_skill_root(&root, SkillSource::User, None, &mut out);
        out.sort_by(|a, b| a.name.cmp(&b.name));

        assert_eq!(out.len(), 2);
        assert_eq!(out[0].name, "Demo Skill");
        assert_eq!(out[0].description, "A demo.");
        assert_eq!(out[0].source, SkillSource::User);
        assert_eq!(out[1].name, "no-frontmatter");
        assert_eq!(out[1].description, "");
    }

    #[test]
    fn semantic_key_matches_hive_shape() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("skills");
        fs::create_dir_all(&root).unwrap();
        write_skill(&root, "x", "name: Session Monitoring\n", "");
        let mut out = Vec::new();
        scan_skill_root(&root, SkillSource::User, None, &mut out);
        assert_eq!(out[0].semantic_key(), "skill:session-monitoring");
    }

    #[test]
    fn within_share_limit_respects_32k() {
        let mut skill = DiscoveredSkill {
            name: "a".into(),
            description: String::new(),
            path: PathBuf::new(),
            source: SkillSource::User,
            plugin: None,
            size_bytes: 0,
        };
        assert!(skill.within_share_limit());
        skill.size_bytes = 32 * 1024;
        assert!(skill.within_share_limit());
        skill.size_bytes = 32 * 1024 + 1;
        assert!(!skill.within_share_limit());
    }
}
