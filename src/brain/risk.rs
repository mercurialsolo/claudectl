#![allow(dead_code)]

// ────────────────────────────────────────────────────────────────────────────
// Risk tier classification
// ────────────────────────────────────────────────────────────────────────────

/// Risk tier for a decision, based on tool and command patterns.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RiskTier {
    /// Read, Glob, Grep — no side effects
    Low,
    /// Edit, Write (non-config) — reversible changes
    Medium,
    /// Bash (non-destructive), file operations
    High,
    /// rm -rf, force push, DROP, production deploys
    Critical,
}

impl RiskTier {
    pub fn label(&self) -> &'static str {
        match self {
            RiskTier::Low => "low",
            RiskTier::Medium => "medium",
            RiskTier::High => "high",
            RiskTier::Critical => "critical",
        }
    }
}

impl std::fmt::Display for RiskTier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

/// Classify a decision into a risk tier based on tool and command.
pub fn classify_risk(tool: Option<&str>, command: Option<&str>) -> RiskTier {
    let tool = tool.unwrap_or("");
    let cmd = command.unwrap_or("").to_lowercase();

    // Critical: destructive patterns regardless of tool
    const CRITICAL_PATTERNS: &[&str] = &[
        "rm -rf",
        "rm -fr",
        "git push --force",
        "git push -f",
        "git reset --hard",
        "drop table",
        "drop database",
        "truncate table",
        "kubectl delete",
        "docker rm",
        "format c:",
        "> /dev/",
        ":(){ :|:& };:",
        "chmod -r 777",
        "chmod 777",
        "--no-verify",
    ];
    for pat in CRITICAL_PATTERNS {
        if cmd.contains(pat) {
            return RiskTier::Critical;
        }
    }

    match tool {
        // Low risk: read-only tools
        "Read" | "Glob" | "Grep" | "LS" | "Explore" => RiskTier::Low,

        // Medium risk: file modifications
        "Edit" | "Write" | "NotebookEdit" => {
            // Config files are higher risk
            if cmd.contains("config")
                || cmd.contains(".env")
                || cmd.contains("deploy")
                || cmd.contains("production")
                || cmd.contains("Dockerfile")
                || cmd.contains("ci.yml")
                || cmd.contains("ci.yaml")
            {
                RiskTier::High
            } else {
                RiskTier::Medium
            }
        }

        // Bash: depends on command
        "Bash" => {
            // High-risk bash patterns
            const HIGH_RISK_BASH: &[&str] = &[
                "git push",
                "git merge",
                "git rebase",
                "npm publish",
                "cargo publish",
                "pip install",
                "npm install -g",
                "brew install",
                "sudo ",
                "curl ",
                "wget ",
            ];
            for pat in HIGH_RISK_BASH {
                if cmd.contains(pat) {
                    return RiskTier::High;
                }
            }
            // Safe bash commands
            const SAFE_BASH: &[&str] = &[
                "cargo test",
                "cargo build",
                "cargo check",
                "cargo clippy",
                "cargo fmt",
                "npm test",
                "npm run",
                "pytest",
                "go test",
                "make test",
                "ls",
                "pwd",
                "cat ",
                "head ",
                "tail ",
                "wc ",
                "git status",
                "git log",
                "git diff",
                "git branch",
                "echo ",
            ];
            for pat in SAFE_BASH {
                if cmd.starts_with(pat) || cmd.contains(pat) {
                    return RiskTier::Low;
                }
            }
            RiskTier::Medium
        }

        // Unknown tools default to medium
        _ => RiskTier::Medium,
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Tests
// ────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_read_as_low() {
        assert_eq!(
            classify_risk(Some("Read"), Some("src/main.rs")),
            RiskTier::Low
        );
        assert_eq!(classify_risk(Some("Glob"), Some("**/*.rs")), RiskTier::Low);
        assert_eq!(classify_risk(Some("Grep"), Some("TODO")), RiskTier::Low);
    }

    #[test]
    fn classify_edit_as_medium() {
        assert_eq!(
            classify_risk(Some("Edit"), Some("src/lib.rs")),
            RiskTier::Medium
        );
        assert_eq!(
            classify_risk(Some("Write"), Some("tests/test.rs")),
            RiskTier::Medium
        );
    }

    #[test]
    fn classify_config_write_as_high() {
        assert_eq!(
            classify_risk(Some("Write"), Some("config.toml")),
            RiskTier::High
        );
        assert_eq!(classify_risk(Some("Edit"), Some(".env")), RiskTier::High);
    }

    #[test]
    fn classify_destructive_as_critical() {
        assert_eq!(
            classify_risk(Some("Bash"), Some("rm -rf /tmp")),
            RiskTier::Critical
        );
        assert_eq!(
            classify_risk(Some("Bash"), Some("git push --force origin main")),
            RiskTier::Critical
        );
        assert_eq!(
            classify_risk(Some("Bash"), Some("DROP TABLE users")),
            RiskTier::Critical
        );
    }

    #[test]
    fn classify_safe_bash_as_low() {
        assert_eq!(
            classify_risk(Some("Bash"), Some("cargo test --release")),
            RiskTier::Low
        );
        assert_eq!(
            classify_risk(Some("Bash"), Some("git status")),
            RiskTier::Low
        );
        assert_eq!(classify_risk(Some("Bash"), Some("ls -la")), RiskTier::Low);
    }

    #[test]
    fn classify_risky_bash_as_high() {
        assert_eq!(
            classify_risk(Some("Bash"), Some("git push origin main")),
            RiskTier::High
        );
        assert_eq!(
            classify_risk(Some("Bash"), Some("npm publish")),
            RiskTier::High
        );
    }

    #[test]
    fn classify_unknown_tool_as_medium() {
        assert_eq!(
            classify_risk(Some("CustomTool"), Some("anything")),
            RiskTier::Medium
        );
        assert_eq!(classify_risk(None, None), RiskTier::Medium);
    }

    #[test]
    fn risk_tier_labels() {
        assert_eq!(RiskTier::Low.label(), "low");
        assert_eq!(RiskTier::Critical.label(), "critical");
        assert_eq!(format!("{}", RiskTier::High), "high");
    }
}
