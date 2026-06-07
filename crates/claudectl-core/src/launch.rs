use std::path::{Path, PathBuf};

use crate::terminals;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchRequest {
    pub cwd_path: PathBuf,
    pub prompt: Option<String>,
    pub resume: Option<String>,
}

impl LaunchRequest {
    pub fn option_summary(&self) -> String {
        let mut parts = Vec::new();
        if let Some(resume) = &self.resume {
            parts.push(format!("resume {resume}"));
        }
        if self.prompt.is_some() {
            parts.push("prompt set".to_string());
        }

        if parts.is_empty() {
            String::new()
        } else {
            format!(" ({})", parts.join(", "))
        }
    }
}

pub fn prepare(
    cwd: &str,
    prompt: Option<&str>,
    resume: Option<&str>,
) -> Result<LaunchRequest, String> {
    let raw_cwd = if cwd.trim().is_empty() {
        "."
    } else {
        cwd.trim()
    };
    let cwd_path = Path::new(raw_cwd)
        .canonicalize()
        .map_err(|_| format!("Directory not found: {raw_cwd}"))?;

    if !cwd_path.is_dir() {
        return Err(format!(
            "Launch path is not a directory: {}",
            cwd_path.display()
        ));
    }

    let prompt = prompt
        .filter(|value| !value.trim().is_empty())
        .map(ToOwned::to_owned);
    let resume = resume
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned);

    Ok(LaunchRequest {
        cwd_path,
        prompt,
        resume,
    })
}

pub fn launch(request: &LaunchRequest) -> Result<String, String> {
    terminals::launch_session(
        request.cwd_path.to_string_lossy().as_ref(),
        request.prompt.as_deref(),
        request.resume.as_deref(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prepare_normalizes_optional_fields() {
        let temp = tempfile::tempdir().unwrap();
        let request = prepare(
            temp.path().to_string_lossy().as_ref(),
            Some("ship it"),
            Some("  session-123  "),
        )
        .unwrap();

        assert_eq!(request.cwd_path, temp.path().canonicalize().unwrap());
        assert_eq!(request.prompt.as_deref(), Some("ship it"));
        assert_eq!(request.resume.as_deref(), Some("session-123"));
    }

    #[test]
    fn prepare_rejects_missing_directory() {
        let missing = "/tmp/claudectl-this-path-should-not-exist";
        let err = prepare(missing, None, None).unwrap_err();
        assert_eq!(err, format!("Directory not found: {missing}"));
    }

    #[test]
    fn prepare_treats_blank_prompt_and_resume_as_none() {
        let temp = tempfile::tempdir().unwrap();
        let request = prepare(
            temp.path().to_string_lossy().as_ref(),
            Some("   "),
            Some(""),
        )
        .unwrap();

        assert_eq!(request.prompt, None);
        assert_eq!(request.resume, None);
    }
}
