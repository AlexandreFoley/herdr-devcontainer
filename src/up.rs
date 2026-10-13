use std::path::Path;
use std::time::Duration;

use crate::error::Error;
use crate::run::{run, StderrMode};
use crate::util::tail;

#[derive(Debug, PartialEq, serde::Deserialize)]
pub struct UpResult {
    #[serde(rename = "containerId", default)]
    pub container_id: String,
    #[serde(rename = "remoteUser", default)]
    pub remote_user: Option<String>,
    #[serde(rename = "remoteWorkspaceFolder", default)]
    pub remote_workspace_folder: String,
    #[serde(default)]
    pub outcome: String,
    #[serde(default)]
    pub message: Option<String>,
}

/// Preserve absolute Git paths while keeping the repository's container identity.
fn up_argv(
    devcontainer_bin: &Path,
    repo_root: &Path,
    worktrees_dir: &Path,
    config_arg: Option<&Path>,
) -> Vec<String> {
    let mut argv = vec![
        devcontainer_bin.display().to_string(),
        "up".to_string(),
        "--workspace-folder".to_string(),
        repo_root.display().to_string(),
    ];
    if let Some(cfg) = config_arg {
        argv.push("--config".to_string());
        argv.push(cfg.display().to_string());
    }
    for path in [repo_root, worktrees_dir] {
        argv.push("--mount".into());
        argv.push(format!("type=bind,source={0},target={0}", path.display()));
    }
    argv
}

pub fn parse_up_output(stdout: &str) -> Result<UpResult, Error> {
    let last = stdout
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .ok_or_else(|| Error::UpOutputUnparseable {
            detail: "empty stdout".into(),
            last_line: String::new(),
        })?;
    let parsed: UpResult =
        serde_json::from_str(last.trim()).map_err(|e| Error::UpOutputUnparseable {
            detail: e.to_string(),
            last_line: last.to_string(),
        })?;
    if parsed.outcome != "success" {
        return Err(Error::UpFailed {
            exit_code: None,
            output_tail: parsed.message.clone().unwrap_or_else(|| last.to_string()),
        });
    }
    if parsed.container_id.is_empty() || parsed.remote_workspace_folder.is_empty() {
        return Err(Error::UpOutputUnparseable {
            detail: "success result missing containerId or remoteWorkspaceFolder".into(),
            last_line: last.to_string(),
        });
    }
    Ok(parsed)
}

/// Mount the shared Herdr worktree root so future checkouts are available too.
pub fn bring_up(
    devcontainer_bin: &Path,
    repo_root: &Path,
    worktrees_dir: &Path,
    config_arg: Option<&Path>,
    timeout: Duration,
) -> Result<UpResult, Error> {
    eprintln!("bringing up dev container for {} ...", repo_root.display());
    for path in [repo_root, worktrees_dir] {
        if !path.is_absolute() || path.to_str().map_or(true, |s| s.contains(',')) {
            return Err(Error::Other(format!(
                "bind mount path must be absolute UTF-8 without commas: {}", path.display()
            )));
        }
    }
    let argv = up_argv(devcontainer_bin, repo_root, worktrees_dir, config_arg);
    let res = run(&argv, timeout, StderrMode::Inherit)?;
    if res.timed_out {
        return Err(Error::UpTimeout {
            secs: timeout.as_secs(),
            output_tail: tail(&res.stdout, 2000),
        });
    }
    if res.exit_code != Some(0) {
        return Err(Error::UpFailed {
            exit_code: res.exit_code,
            output_tail: tail(&res.stdout, 2000),
        });
    }
    let result = parse_up_output(&res.stdout)?;
    verify_mounts(&result.container_id, repo_root, worktrees_dir)?;
    Ok(result)
}

/// Reused containers cannot gain mounts; refuse rather than enter the wrong checkout.
fn verify_mounts(container_id: &str, repo_root: &Path, worktrees_dir: &Path) -> Result<(), Error> {
    let result = run(
        &["docker".into(), "inspect".into(), "--format".into(),
            "{{json .Mounts}}".into(), container_id.into()],
        Duration::from_secs(15),
        StderrMode::Capture,
    )?;
    if result.timed_out || result.exit_code != Some(0) || result.stdout_incomplete {
        return Err(Error::DockerCommandFailed { detail: result.stderr });
    }
    let mounts: Vec<serde_json::Value> = serde_json::from_str(&result.stdout)
        .map_err(|e| Error::DockerCommandFailed { detail: e.to_string() })?;
    for path in [repo_root, worktrees_dir] {
        let host = path.to_str().unwrap();
        if !mounts.iter().any(|m| m["Type"] == "bind" && m["Source"] == host
            && m["Destination"] == host && m["RW"] == true) {
            return Err(Error::Other(format!(
                "container {container_id} lacks the writable same-path bind mount for {host}; \
                 recreate it with the plugin's repository and Herdr worktree mounts \
                 (removing it interrupts existing container panes)"
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const OK: &str = r#"{"outcome":"success","containerId":"c0ffee","remoteUser":"vscode","remoteWorkspaceFolder":"/workspaces/proj"}"#;

    #[test]
    fn parses_a_success_line() {
        let up = parse_up_output(OK).unwrap();
        assert_eq!(up.container_id, "c0ffee");
        assert_eq!(up.remote_user.as_deref(), Some("vscode"));
        assert_eq!(up.remote_workspace_folder, "/workspaces/proj");
    }

    #[test]
    fn takes_the_last_nonempty_line() {
        let noisy = format!("progress 1\nprogress 2\n{OK}\n\n");
        assert!(parse_up_output(&noisy).is_ok());
    }

    #[test]
    fn empty_stdout_is_unparseable() {
        assert!(matches!(
            parse_up_output("  \n \n"),
            Err(crate::error::Error::UpOutputUnparseable { .. })
        ));
    }

    #[test]
    fn garbage_last_line_is_unparseable() {
        assert!(matches!(
            parse_up_output("something went wrong"),
            Err(crate::error::Error::UpOutputUnparseable { .. })
        ));
    }

    #[test]
    fn non_success_outcome_is_up_failed() {
        let line = r#"{"outcome":"error","message":"build failed"}"#;
        assert!(matches!(
            parse_up_output(line),
            Err(crate::error::Error::UpFailed { .. })
        ));
    }

    #[test]
    fn success_without_container_id_is_unparseable() {
        let line = r#"{"outcome":"success","remoteWorkspaceFolder":"/w"}"#;
        assert!(matches!(
            parse_up_output(line),
            Err(crate::error::Error::UpOutputUnparseable { .. })
        ));
    }
}
