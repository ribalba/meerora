//! Running git on a repository someone else could have written (D16).
//!
//! `GIT_ATTR_SOURCE` replaces the work tree's `.gitattributes`, but git still
//! reads `.git/info/attributes`, and a filter, textconv or merge driver named
//! there runs whatever the repository's configuration says. [`neutralize`]
//! switches every driver the configuration names off, through
//! `GIT_CONFIG_COUNT`, which takes any name. A configuration that includes
//! other files makes git refuse to run, because what those files name can
//! change after this look. The sandbox keeps `.git` read-only for agents, so
//! this matters most when the sandbox is switched off.

use std::path::Path;
use std::process::{Command, Stdio};

/// Where git looks when the repository must not be used.
const REFUSED_GIT_DIR: &str = "/dev/null/meerora-refused-repository";

/// The overrides that switch off every driver named among `keys`, the names
/// of `git config --list --name-only`. `Err` when the configuration includes
/// other files.
pub fn driver_overrides(keys: &[String]) -> Result<Vec<(String, &'static str)>, String> {
    if keys.iter().any(|k| k.starts_with("include.") || k.starts_with("includeif.")) {
        return Err("the git configuration includes other files".into());
    }
    let mut out: Vec<(String, &'static str)> = Vec::new();
    for key in keys {
        for (section, vars) in
            [("filter.", &["clean", "smudge", "process"][..]), ("diff.", &["textconv", "command"][..]), ("merge.", &["driver"][..])]
        {
            if let Some((name, _)) = key.strip_prefix(section).and_then(|rest| rest.rsplit_once('.')) {
                for var in vars {
                    out.push((format!("{section}{name}.{var}"), ""));
                }
                if section == "filter." {
                    out.push((format!("{section}{name}.required"), "false"));
                }
            }
        }
    }
    out.sort();
    out.dedup();
    Ok(out)
}

/// Adds the overrides for the repository at `dir` to `cmd`. When the
/// configuration includes other files, `cmd` is pointed at a git directory
/// that does not exist, so it fails instead of running anything.
pub fn neutralize(cmd: &mut Command, dir: &Path) -> Result<(), String> {
    let listed = Command::new("git")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env_remove("GIT_DIR")
        .env_remove("GIT_CONFIG_COUNT")
        .arg("-C")
        .arg(dir)
        .args(["config", "--local", "--list", "--name-only", "-z"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output();
    // No repository yet, or no local configuration: nothing to switch off.
    let keys: Vec<String> = match listed {
        Ok(out) if out.status.success() => {
            String::from_utf8_lossy(&out.stdout).split('\0').map(|k| k.trim().to_string()).filter(|k| !k.is_empty()).collect()
        }
        _ => Vec::new(),
    };
    match driver_overrides(&keys) {
        Ok(overrides) => {
            for (n, (key, value)) in overrides.iter().enumerate() {
                cmd.env(format!("GIT_CONFIG_KEY_{n}"), key).env(format!("GIT_CONFIG_VALUE_{n}"), value);
            }
            cmd.env("GIT_CONFIG_COUNT", overrides.len().to_string());
            Ok(())
        }
        Err(e) => {
            cmd.env("GIT_DIR", REFUSED_GIT_DIR);
            Err(e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_named_driver_is_switched_off() {
        let keys: Vec<String> = ["core.bare", "filter.lfs.clean", "filter.a=b.smudge", "diff.evil.textconv", "merge.x.driver"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let o = driver_overrides(&keys).unwrap();
        for (key, value) in [
            ("filter.lfs.clean", ""),
            ("filter.lfs.smudge", ""),
            ("filter.lfs.process", ""),
            ("filter.lfs.required", "false"),
            ("filter.a=b.clean", ""),
            ("diff.evil.textconv", ""),
            ("diff.evil.command", ""),
            ("merge.x.driver", ""),
        ] {
            assert!(o.contains(&(key.to_string(), value)), "{key} missing in {o:?}");
        }
        assert!(!o.iter().any(|(k, _)| k.starts_with("core.")));
        assert!(driver_overrides(&["include.path".into()]).is_err());
        assert!(driver_overrides(&["includeif.gitdir:/x.path".into()]).is_err());
    }
}
