use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use crate::error::Error;
use crate::open::resolve_herdr_bin;
use crate::run::{run, StderrMode};

/// Staging directory on the remote host. `plugin link` needs an absolute path;
/// the caller resolves it from the remote `$HOME`.
const REMOTE_PLUGIN_DIR: &str = ".herdr/devcontainer-plugin";
const SSH_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, serde::Deserialize)]
struct Machine {
    id: String,
    label: String,
    target: String,
    #[serde(default = "enabled_by_default")]
    enabled: bool,
}

fn enabled_by_default() -> bool {
    true
}

pub fn run_remote(args: &[String]) -> Result<(), Error> {
    match args.first().map(String::as_str) {
        Some("install") => install(&args[1..]),
        _ => Err(Error::Other(
            "usage: herdr-devc remote install [<machine>...]".into(),
        )),
    }
}

/// Herdr keeps plugins per server; a plugin installed locally is invisible to
/// a saved SSH machine. Copy this installation to the remote and link it there,
/// so the same manifest and binary run on both sides.
fn install(selectors: &[String]) -> Result<(), Error> {
    let herdr_bin = resolve_herdr_bin(
        std::env::var("HERDR_BIN_PATH").ok().as_deref(),
        &std::env::var("PATH").unwrap_or_default(),
    )?;
    let machines = enabled_machines(&herdr_bin)?;
    let targets = select(&machines, selectors)?;
    if targets.is_empty() {
        println!("no saved remote machines; add one with `herdr machine add <ssh-target>`");
        return Ok(());
    }
    let plugin_root = plugin_root()?;
    let mut failures = 0;
    for machine in targets {
        match install_one(&herdr_bin, &plugin_root, machine) {
            Ok(path) => println!(
                "installed devcontainer plugin on {} ({path})",
                machine.label
            ),
            Err(err) => {
                eprintln!("error: remote machine {}: {err}", machine.label);
                failures += 1;
            }
        }
    }
    if failures > 0 {
        return Err(Error::Other(format!(
            "failed on {failures} of the selected remote machines"
        )));
    }
    println!(
        "remote machines keep their own Docker and Dev Containers CLI; the first pane reports anything missing there"
    );
    Ok(())
}

fn enabled_machines(herdr_bin: &str) -> Result<Vec<Machine>, Error> {
    let argv = vec![
        herdr_bin.to_string(),
        "machine".into(),
        "list".into(),
        "--json".into(),
    ];
    let res = run(&argv, SSH_TIMEOUT, StderrMode::Capture)?;
    if res.timed_out {
        return Err(Error::Other("`herdr machine list` timed out".into()));
    }
    if res.exit_code != Some(0) || res.stdout_incomplete {
        return Err(Error::Other(format!(
            "`herdr machine list --json` failed: {}",
            res.stderr.trim()
        )));
    }
    let machines: Vec<Machine> = serde_json::from_str(&res.stdout)
        .map_err(|e| Error::Other(format!("could not parse `herdr machine list`: {e}")))?;
    Ok(machines.into_iter().filter(|m| m.enabled).collect())
}

fn select<'a>(machines: &'a [Machine], selectors: &[String]) -> Result<Vec<&'a Machine>, Error> {
    if selectors.is_empty() {
        return Ok(machines.iter().collect());
    }
    let mut selected = Vec::new();
    for selector in selectors {
        let mut matches = machines
            .iter()
            .filter(|m| m.id == *selector || m.label == *selector)
            .peekable();
        if matches.peek().is_none() {
            return Err(Error::Other(format!(
                "unknown or disabled machine `{selector}`; see `herdr machine list`"
            )));
        }
        for machine in matches {
            if !selected.iter().any(|m: &&Machine| m.id == machine.id) {
                selected.push(machine);
            }
        }
    }
    Ok(selected)
}

fn plugin_root() -> Result<PathBuf, Error> {
    let root = std::env::var_os("HERDR_PLUGIN_ROOT")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or(std::env::current_dir()?);
    for relative in ["herdr-plugin.toml", "target/release/herdr-devc"] {
        let path = root.join(relative);
        if !path.is_file() {
            return Err(Error::Other(format!(
                "{} is missing; build the plugin with `cargo build --release` first",
                path.display()
            )));
        }
    }
    Ok(root)
}

fn install_one(herdr_bin: &str, root: &Path, machine: &Machine) -> Result<String, Error> {
    let ssh = SshTarget::parse(&machine.target)?;
    let uname = ssh_capture(&ssh, "printf '%s %s' \"$(uname -s)\" \"$(uname -m)\"")?;
    check_platform(&uname)?;
    let home = ssh_capture(&ssh, "printf %s \"$HOME\"")?;
    let home = home.trim();
    if !home.starts_with('/') {
        return Err(Error::Other(format!(
            "remote HOME is not an absolute path: {home:?}"
        )));
    }
    let dest = format!("{home}/{REMOTE_PLUGIN_DIR}");
    ssh_status(
        &ssh,
        &format!("mkdir -p {}", quote(&format!("{dest}/target/release"))),
    )?;
    copy_plugin(root, &ssh, &dest)?;
    link(herdr_bin, &machine.id, &dest)?;
    Ok(dest)
}

fn check_platform(uname: &str) -> Result<(), Error> {
    let mut parts = uname.split_whitespace();
    let os = parts.next().unwrap_or("");
    let arch = parts.next().unwrap_or("");
    if std::env::consts::OS != "linux" {
        return Err(Error::Other(format!(
            "this installation runs on {}; build the plugin in the remote's own Linux environment instead",
            std::env::consts::OS
        )));
    }
    if os != "Linux" {
        return Err(Error::Other(format!(
            "remote OS is {os}; this plugin supports Linux only"
        )));
    }
    if arch != std::env::consts::ARCH {
        return Err(Error::Other(format!(
            "remote architecture is {arch} but this binary is {}; install from source on the remote with `herdr plugin install gambtho/herdr-devcontainer`",
            std::env::consts::ARCH
        )));
    }
    Ok(())
}

fn copy_plugin(root: &Path, ssh: &SshTarget, dest: &str) -> Result<(), Error> {
    let mut tar = Command::new("tar")
        .arg("-C")
        .arg(root)
        .args(["-cf", "-", "herdr-plugin.toml", "target/release/herdr-devc"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let payload = tar.stdout.take().expect("tar stdout is piped");
    let ssh_cmd = Command::new("ssh")
        .args(ssh.args())
        .arg(format!("tar -C {} -xf -", quote(dest)))
        .stdin(Stdio::from(payload))
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn();
    let ssh_child = match ssh_cmd {
        Ok(child) => child,
        Err(err) => {
            let _ = tar.kill();
            let _ = tar.wait();
            return Err(err.into());
        }
    };
    let ssh_out = ssh_child.wait_with_output()?;
    let tar_out = tar.wait_with_output()?;
    if !ssh_out.status.success() {
        return Err(Error::Other(format!(
            "copying the plugin failed: {}",
            String::from_utf8_lossy(&ssh_out.stderr).trim()
        )));
    }
    if !tar_out.status.success() {
        return Err(Error::Other(format!(
            "local tar failed: {}",
            String::from_utf8_lossy(&tar_out.stderr).trim()
        )));
    }
    Ok(())
}

fn link(herdr_bin: &str, machine_id: &str, dest: &str) -> Result<(), Error> {
    let argv = vec![
        herdr_bin.to_string(),
        "--machine".into(),
        machine_id.into(),
        "plugin".into(),
        "link".into(),
        dest.into(),
    ];
    let res = run(&argv, SSH_TIMEOUT, StderrMode::Capture)?;
    if res.timed_out {
        return Err(Error::Other("`herdr plugin link` timed out".into()));
    }
    if res.exit_code != Some(0) || res.stdout_incomplete {
        let detail = res.stderr.trim();
        return Err(Error::Other(if detail.is_empty() {
            "`herdr plugin link` failed".into()
        } else {
            format!("`herdr plugin link` failed: {detail}")
        }));
    }
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
struct SshTarget {
    argv: Vec<String>,
}

impl SshTarget {
    fn parse(target: &str) -> Result<Self, Error> {
        let mut argv = vec![
            "-o".to_string(),
            "BatchMode=yes".to_string(),
            "-o".to_string(),
            "ConnectTimeout=10".to_string(),
            "-o".to_string(),
            "ServerAliveInterval=5".to_string(),
            "-o".to_string(),
            "ServerAliveCountMax=3".to_string(),
        ];
        match target.strip_prefix("ssh://") {
            Some(rest) => {
                let rest = rest.split('/').next().unwrap_or(rest);
                if rest.is_empty() {
                    return Err(Error::Other("empty SSH target".into()));
                }
                if let Some((host, port)) = split_port(rest) {
                    argv.push("-p".into());
                    argv.push(port.into());
                    argv.push(host.into());
                } else {
                    argv.push(rest.into());
                }
            }
            None => {
                if target.is_empty() {
                    return Err(Error::Other("empty SSH target".into()));
                }
                argv.push(target.into());
            }
        }
        Ok(Self { argv })
    }

    fn args(&self) -> impl Iterator<Item = &str> {
        self.argv.iter().map(String::as_str)
    }
}

/// `ssh://user@host:2222` carries its port in the URL; plain `user@host` is an
/// SSH config alias and must be handed to `ssh` untouched.
fn split_port(value: &str) -> Option<(&str, &str)> {
    let (host, port) = value.rsplit_once(':')?;
    if host.is_empty() || port.is_empty() || !port.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    Some((host, port))
}

fn ssh_capture(ssh: &SshTarget, command: &str) -> Result<String, Error> {
    let mut argv = vec!["ssh".to_string()];
    argv.extend(ssh.args().map(str::to_string));
    argv.push(command.to_string());
    let res = run(&argv, SSH_TIMEOUT, StderrMode::Capture)?;
    if res.timed_out {
        return Err(Error::Other(
            "SSH timed out; run `herdr machine reconnect` if authentication is needed".into(),
        ));
    }
    if res.exit_code != Some(0) || res.stdout_incomplete {
        return Err(Error::Other(format!("SSH failed: {}", res.stderr.trim())));
    }
    Ok(res.stdout)
}

fn ssh_status(ssh: &SshTarget, command: &str) -> Result<(), Error> {
    let mut argv = vec!["ssh".to_string()];
    argv.extend(ssh.args().map(str::to_string));
    argv.push(command.to_string());
    let res = run(&argv, SSH_TIMEOUT, StderrMode::Capture)?;
    if res.timed_out {
        return Err(Error::Other("SSH timed out".into()));
    }
    if res.exit_code != Some(0) {
        return Err(Error::Other(format!("SSH failed: {}", res.stderr.trim())));
    }
    Ok(())
}

/// POSIX single-quote quoting for a value interpolated into a remote command.
fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_targets_are_ssh_config_aliases() {
        let ssh = SshTarget::parse("workbox").unwrap();
        assert_eq!(ssh.argv.last().map(String::as_str), Some("workbox"));
        assert!(!ssh.argv.iter().any(|arg| arg == "-p"));
    }

    #[test]
    fn ssh_url_with_a_port_becomes_p_and_host() {
        let ssh = SshTarget::parse("ssh://you@server:2222").unwrap();
        assert_eq!(
            ssh.argv[ssh.argv.len() - 3..],
            ["-p".to_string(), "2222".into(), "you@server".to_string()]
        );
    }

    #[test]
    fn ssh_url_without_a_port_is_untouched() {
        let ssh = SshTarget::parse("ssh://you@server").unwrap();
        assert_eq!(ssh.argv.last().map(String::as_str), Some("you@server"));
    }

    #[test]
    fn an_empty_target_is_rejected() {
        assert!(SshTarget::parse("").is_err());
        assert!(SshTarget::parse("ssh://").is_err());
    }

    #[test]
    fn quote_escapes_posix_single_quotes() {
        assert_eq!(quote("/home/o'brien/x"), "'/home/o'\\''brien/x'");
    }

    fn machine(id: &str, label: &str) -> Machine {
        Machine {
            id: id.into(),
            label: label.into(),
            target: label.into(),
            enabled: true,
        }
    }

    #[test]
    fn no_selectors_means_every_enabled_machine() {
        let machines = [machine("a", "alpha"), machine("b", "beta")];
        let picked = select(&machines, &[]).unwrap();
        assert_eq!(picked.len(), 2);
    }

    #[test]
    fn selectors_match_ids_and_labels_without_duplicates() {
        let machines = [machine("a", "alpha"), machine("b", "beta")];
        let picked = select(&machines, &["alpha".into(), "a".into()]).unwrap();
        assert_eq!(picked.len(), 1);
        assert_eq!(picked[0].id, "a");
    }

    #[test]
    fn an_unknown_selector_is_an_error() {
        let machines = [machine("a", "alpha")];
        assert!(select(&machines, &["nope".into()]).is_err());
    }

    #[test]
    fn platform_mismatches_are_refused_before_copying() {
        assert!(check_platform("Darwin arm64").is_err());
        let other_arch = if std::env::consts::ARCH == "x86_64" {
            "riscv64"
        } else {
            "x86_64"
        };
        assert!(check_platform(&format!("Linux {other_arch}")).is_err());
        if std::env::consts::OS == "linux" {
            assert!(check_platform(&format!("Linux {}", std::env::consts::ARCH)).is_ok());
        }
    }
}
