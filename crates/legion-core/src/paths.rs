//! Where Legion keeps its configuration and its state.
//!
//! This is the only place in the workspace that decides those directories, so a test that sets
//! `LEGION_HOME` can be sure nothing it runs reads or writes the real ones.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use legion_proto::{ErrorBody, ErrorCode};

/// The directories a node reads and writes, and the home directory `~` stands for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Paths {
    config: PathBuf,
    state: PathBuf,
    home: Option<PathBuf>,
}

impl Paths {
    /// The directories for this process, from its environment:
    ///
    /// | | Unix | Windows |
    /// | --- | --- | --- |
    /// | configuration | `$XDG_CONFIG_HOME/legion` or `~/.config/legion` | `%APPDATA%\legion` |
    /// | state | `$XDG_STATE_HOME/legion` or `~/.local/state/legion` | `%LOCALAPPDATA%\legion` |
    ///
    /// `LEGION_HOME`, when set, replaces both with `$LEGION_HOME/config` and `$LEGION_HOME/state`.
    /// It does not change what `~` means.
    pub fn from_env() -> Result<Paths, ErrorBody> {
        let var = |name: &str| std::env::var_os(name);
        let home = var(home_variable(cfg!(windows)))
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .or_else(std::env::home_dir);
        resolve(&var, home, cfg!(windows))
    }

    /// The directories `LEGION_HOME=<root>` would give, without reading the environment. A test
    /// uses this to run a node inside its own temporary directory.
    pub fn under(root: impl Into<PathBuf>) -> Paths {
        let root = root.into();
        Paths {
            config: root.join("config"),
            state: root.join("state"),
            home: None,
        }
    }

    /// The same directories, with `~` standing for `home`.
    #[must_use]
    pub fn with_home(mut self, home: impl Into<PathBuf>) -> Paths {
        self.home = Some(home.into());
        self
    }

    /// Where the configuration files live. It may not exist: every file then takes its default.
    pub fn config_dir(&self) -> &Path {
        &self.config
    }

    /// Where the node writes what it keeps: the audit log and, later, jobs.
    pub fn state_dir(&self) -> &Path {
        &self.state
    }

    /// The directory `~` stands for, when one is known.
    pub fn home(&self) -> Option<&Path> {
        self.home.as_deref()
    }

    /// A configuration file, by its name, such as `policy.toml`.
    pub fn config_file(&self, name: &str) -> PathBuf {
        self.config.join(name)
    }

    /// Where user-defined extension tools live (`tools.d/*.toml`).
    pub fn tools_dir(&self) -> PathBuf {
        self.config.join("tools.d")
    }

    /// The audit log, one JSON object per line.
    pub fn audit_log(&self) -> PathBuf {
        self.state.join("audit.jsonl")
    }

    /// The file whose kernel lock orders writers and readers of the audit log.
    pub fn audit_lock(&self) -> PathBuf {
        self.state.join("audit.lock")
    }
}

fn home_variable(windows: bool) -> &'static str {
    if windows { "USERPROFILE" } else { "HOME" }
}

/// The directories for an environment given as a lookup, so both platforms' rules can be tested on
/// either.
fn resolve(
    var: &dyn Fn(&str) -> Option<OsString>,
    home: Option<PathBuf>,
    windows: bool,
) -> Result<Paths, ErrorBody> {
    let set = |name: &str| var(name).filter(|value| !value.is_empty());

    if let Some(root) = set("LEGION_HOME") {
        let root = std::path::absolute(PathBuf::from(&root)).map_err(|error| {
            ErrorBody::new(
                ErrorCode::Failed,
                format!(
                    "LEGION_HOME is {}, which is relative, and the current directory cannot be read to resolve it: {error}.",
                    PathBuf::from(root).display()
                ),
            )
            .with_hint("Set LEGION_HOME to an absolute path.")
        })?;
        let mut paths = Paths::under(root);
        paths.home = home;
        return Ok(paths);
    }

    let (config, state) = if windows {
        let config = set("APPDATA").map(|dir| PathBuf::from(dir).join("legion"));
        let state = set("LOCALAPPDATA").map(|dir| PathBuf::from(dir).join("legion"));
        (config, state)
    } else {
        // The XDG specification says a relative value is invalid and is to be ignored.
        let xdg = |name: &str| set(name).map(PathBuf::from).filter(|dir| dir.is_absolute());
        let under_home = |rest: &[&str]| {
            home.as_ref()
                .map(|home| rest.iter().fold(home.clone(), |path, part| path.join(part)))
        };
        let config = xdg("XDG_CONFIG_HOME")
            .map(|dir| dir.join("legion"))
            .or_else(|| under_home(&[".config", "legion"]));
        let state = xdg("XDG_STATE_HOME")
            .map(|dir| dir.join("legion"))
            .or_else(|| under_home(&[".local", "state", "legion"]));
        (config, state)
    };

    match (config, state) {
        (Some(config), Some(state)) => Ok(Paths {
            config,
            state,
            home,
        }),
        _ => {
            let missing = if windows {
                "APPDATA and LOCALAPPDATA are"
            } else {
                "HOME is"
            };
            Err(ErrorBody::new(
                ErrorCode::Failed,
                format!("Legion cannot tell where its files live: {missing} not set."),
            )
            .with_hint("Set LEGION_HOME to the directory Legion should keep its files in."))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<OsString> + use<> {
        let map: HashMap<String, OsString> = pairs
            .iter()
            .map(|(key, value)| ((*key).to_owned(), OsString::from(value)))
            .collect();
        move |name| map.get(name).cloned()
    }

    #[test]
    fn legion_home_replaces_both_directories() {
        let root = std::env::temp_dir().join("legion-test-root");
        let vars = env(&[
            ("LEGION_HOME", root.to_str().unwrap_or_default()),
            ("XDG_CONFIG_HOME", "/elsewhere"),
        ]);
        let paths = resolve(&vars, Some(PathBuf::from("/home/me")), false);
        assert_eq!(
            paths,
            Ok(Paths {
                config: root.join("config"),
                state: root.join("state"),
                home: Some(PathBuf::from("/home/me")),
            })
        );
    }

    // Whether an XDG directory is absolute is the host's call, and /cfg is not absolute on Windows,
    // where these rules never apply.
    #[cfg(unix)]
    #[test]
    fn unix_follows_xdg_and_falls_back_to_the_home_directory() {
        let home = Some(PathBuf::from("/home/me"));
        let plain = resolve(&env(&[]), home.clone(), false).ok();
        assert_eq!(
            plain.as_ref().map(Paths::config_dir),
            Some(Path::new("/home/me/.config/legion"))
        );
        assert_eq!(
            plain.as_ref().map(Paths::state_dir),
            Some(Path::new("/home/me/.local/state/legion"))
        );

        let xdg = env(&[("XDG_CONFIG_HOME", "/cfg"), ("XDG_STATE_HOME", "/st")]);
        let moved = resolve(&xdg, home.clone(), false).ok();
        assert_eq!(
            moved.as_ref().map(Paths::config_dir),
            Some(Path::new("/cfg/legion"))
        );
        assert_eq!(
            moved.as_ref().map(Paths::state_dir),
            Some(Path::new("/st/legion"))
        );

        let relative = env(&[("XDG_CONFIG_HOME", "cfg")]);
        let ignored = resolve(&relative, home, false).ok();
        assert_eq!(
            ignored.as_ref().map(Paths::config_dir),
            Some(Path::new("/home/me/.config/legion"))
        );
    }

    #[test]
    fn windows_uses_the_roaming_and_local_application_data() {
        let vars = env(&[
            ("APPDATA", r"C:\Users\me\AppData\Roaming"),
            ("LOCALAPPDATA", r"C:\Users\me\AppData\Local"),
        ]);
        let paths = resolve(&vars, Some(PathBuf::from(r"C:\Users\me")), true).ok();
        assert_eq!(
            paths.as_ref().map(Paths::config_dir),
            Some(
                PathBuf::from(r"C:\Users\me\AppData\Roaming")
                    .join("legion")
                    .as_path()
            )
        );
        assert_eq!(
            paths.as_ref().map(Paths::state_dir),
            Some(
                PathBuf::from(r"C:\Users\me\AppData\Local")
                    .join("legion")
                    .as_path()
            )
        );
    }

    #[test]
    fn nowhere_to_live_is_an_error_that_names_the_way_out() {
        let error = resolve(&env(&[]), None, false).err();
        assert_eq!(
            error.as_ref().map(|error| error.code),
            Some(ErrorCode::Failed)
        );
        assert!(
            error
                .and_then(|error| error.hint)
                .is_some_and(|hint| hint.contains("LEGION_HOME"))
        );
        assert!(resolve(&env(&[("APPDATA", r"C:\x")]), None, true).is_err());
    }

    #[test]
    fn the_files_have_fixed_names_under_their_directories() {
        let paths = Paths::under("/legion");
        assert_eq!(
            paths.config_file("policy.toml"),
            Path::new("/legion").join("config").join("policy.toml")
        );
        assert_eq!(
            paths.tools_dir(),
            Path::new("/legion").join("config").join("tools.d")
        );
        assert_eq!(
            paths.audit_log(),
            Path::new("/legion").join("state").join("audit.jsonl")
        );
        assert_eq!(paths.home(), None);
        assert_eq!(
            paths.with_home("/home/me").home(),
            Some(Path::new("/home/me"))
        );
    }
}
