//! `anchor mcp install` / `anchor mcp uninstall`: register this binary as an MCP server with
//! agent hosts (Claude Code, Claude Desktop, Cursor).
//!
//! Everything that touches the outside world is injectable ([`Locations`], [`CommandRunner`]) so
//! tests run against temporary directories and a fake runner, never the real home directory.

use std::path::{Path, PathBuf};

use clap::{Args, ValueEnum};
use serde_json::{Map, Value, json};

use crate::base::{Error, Result};

/// An agent host Anchor can be registered with.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum Client {
    /// Claude Code, through the `claude` CLI.
    ClaudeCode,
    /// The Claude Desktop app, through its JSON config file.
    ClaudeDesktop,
    /// Cursor, through `~/.cursor/mcp.json`.
    Cursor,
    /// Every client above.
    All,
}

impl Client {
    fn expand(self) -> Vec<Client> {
        match self {
            Client::All => vec![Client::ClaudeCode, Client::ClaudeDesktop, Client::Cursor],
            one => vec![one],
        }
    }
}

/// Arguments of `anchor mcp install`.
#[derive(Args, Debug, Clone)]
pub struct InstallArgs {
    /// Which agent host to register with.
    #[arg(long, value_enum, default_value = "claude-code")]
    pub client: Client,
    /// Server name shown in the host.
    #[arg(long, default_value = "anchor")]
    pub name: String,
    /// Show what would be done and change nothing.
    #[arg(long)]
    pub print: bool,
    /// Claude Code scope (`local`, `user` or `project`); the `claude` default when omitted.
    #[arg(long)]
    pub scope: Option<String>,
}

/// Arguments of `anchor mcp uninstall`.
#[derive(Args, Debug, Clone)]
pub struct UninstallArgs {
    /// Which agent host to remove the registration from.
    #[arg(long, value_enum, default_value = "claude-code")]
    pub client: Client,
    /// Server name to remove.
    #[arg(long, default_value = "anchor")]
    pub name: String,
    /// Show what would be done and change nothing.
    #[arg(long)]
    pub print: bool,
    /// Claude Code scope (`local`, `user` or `project`); the `claude` default when omitted.
    #[arg(long)]
    pub scope: Option<String>,
}

/// Where the JSON config files live.
#[derive(Clone, Debug)]
pub struct Locations {
    /// Claude Desktop's `claude_desktop_config.json`.
    pub claude_desktop: PathBuf,
    /// Cursor's `mcp.json`.
    pub cursor: PathBuf,
}

impl Locations {
    /// The real per-user locations for this platform.
    pub fn system() -> Result<Self> {
        let dirs = directories::BaseDirs::new()
            .ok_or_else(|| Error::config("cannot determine the home directory"))?;
        Ok(Self {
            claude_desktop: dirs
                .config_dir()
                .join("Claude")
                .join("claude_desktop_config.json"),
            cursor: dirs.home_dir().join(".cursor").join("mcp.json"),
        })
    }
}

/// Runs the `claude` CLI (faked in tests).
pub trait CommandRunner {
    /// Whether the `claude` CLI is available.
    fn available(&self) -> bool;
    /// Run `claude <args>`; returns its combined output.
    fn run(&self, args: &[String]) -> Result<String>;
}

/// Runs the real `claude` executable found on `PATH`.
pub struct SystemRunner;

impl CommandRunner for SystemRunner {
    fn available(&self) -> bool {
        which::which("claude").is_ok()
    }

    fn run(&self, args: &[String]) -> Result<String> {
        let out = std::process::Command::new("claude")
            .args(args)
            .output()
            .map_err(|err| Error::config(format!("cannot run claude: {err}")))?;
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        if out.status.success() {
            Ok(text.trim().to_owned())
        } else {
            Err(Error::config(format!(
                "`claude {}` failed: {}",
                args.join(" "),
                text.trim()
            )))
        }
    }
}

/// The `args` array of the registered server: `[--config <path>] mcp`.
fn server_args(config: Option<&Path>) -> Vec<String> {
    let mut args = Vec::new();
    if let Some(config) = config {
        args.push("--config".to_owned());
        args.push(config.display().to_string());
    }
    args.push("mcp".to_owned());
    args
}

/// Shell-quote one word for display.
fn quote(word: &str) -> String {
    if !word.is_empty()
        && word
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "/._-:=+\\".contains(c))
    {
        word.to_owned()
    } else {
        format!("'{}'", word.replace('\'', "'\\''"))
    }
}

fn claude_add_args(
    name: &str,
    exe: &Path,
    config: Option<&Path>,
    scope: Option<&str>,
) -> Vec<String> {
    let mut args = vec!["mcp".to_owned(), "add".to_owned()];
    if let Some(scope) = scope {
        args.push("--scope".to_owned());
        args.push(scope.to_owned());
    }
    args.push(name.to_owned());
    args.push("--".to_owned());
    args.push(exe.display().to_string());
    args.extend(server_args(config));
    args
}

fn display_command(args: &[String]) -> String {
    let mut line = String::from("claude");
    for a in args {
        line.push(' ');
        line.push_str(&quote(a));
    }
    line
}

/// Read a JSON object config; `None` when the file does not exist.
fn read_config(path: &Path) -> Result<Option<Map<String, Value>>> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => {
            return Err(Error::config(format!(
                "cannot read {}: {err}",
                path.display()
            )));
        }
    };
    if text.trim().is_empty() {
        return Ok(Some(Map::new()));
    }
    match serde_json::from_str::<Value>(&text) {
        Ok(Value::Object(map)) => Ok(Some(map)),
        Ok(_) => Err(Error::config(format!(
            "{} is not a JSON object; refusing to modify it",
            path.display()
        ))),
        Err(err) => Err(Error::config(format!(
            "{} is not valid JSON ({err}); refusing to modify it. Fix or remove the file and retry",
            path.display()
        ))),
    }
}

/// Copy `path` to `path.bak` when it exists.
fn backup(path: &Path) -> Result<()> {
    if path.exists() {
        let mut bak = path.as_os_str().to_owned();
        bak.push(".bak");
        std::fs::copy(path, PathBuf::from(bak))
            .map_err(|err| Error::config(format!("cannot back up {}: {err}", path.display())))?;
    }
    Ok(())
}

fn write_config(path: &Path, map: Map<String, Value>) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|err| Error::config(format!("cannot create {}: {err}", parent.display())))?;
    }
    let mut text = serde_json::to_string_pretty(&Value::Object(map))
        .map_err(|err| Error::config(err.to_string()))?;
    text.push('\n');
    std::fs::write(path, text)
        .map_err(|err| Error::config(format!("cannot write {}: {err}", path.display())))
}

/// The JSON entry registered under `mcpServers`.
fn json_entry(exe: &Path, config: Option<&Path>) -> Value {
    json!({ "command": exe.display().to_string(), "args": server_args(config) })
}

/// Merge or remove one entry in a JSON host config. Returns a human-readable outcome line.
fn edit_json(path: &Path, name: &str, entry: Option<Value>, print: bool) -> Result<String> {
    let existing = read_config(path)?;
    let mut map = existing.clone().unwrap_or_default();
    match &entry {
        Some(entry) => {
            let servers = map
                .entry("mcpServers")
                .or_insert_with(|| Value::Object(Map::new()));
            let Value::Object(servers) = servers else {
                return Err(Error::config(format!(
                    "`mcpServers` in {} is not an object; refusing to modify it",
                    path.display()
                )));
            };
            if servers.get(name) == Some(entry) {
                return Ok(format!(
                    "{}: `{name}` already registered, nothing to do",
                    path.display()
                ));
            }
            servers.insert(name.to_owned(), entry.clone());
        }
        None => {
            let removed = map
                .get_mut("mcpServers")
                .and_then(Value::as_object_mut)
                .is_some_and(|s| s.remove(name).is_some());
            if !removed {
                return Ok(format!(
                    "{}: `{name}` not registered, nothing to do",
                    path.display()
                ));
            }
        }
    }
    let verb = if entry.is_some() { "add" } else { "remove" };
    if print {
        let text = serde_json::to_string_pretty(&Value::Object(map))
            .map_err(|err| Error::config(err.to_string()))?;
        return Ok(format!(
            "Would {verb} `{name}` in {}:\n{text}",
            path.display()
        ));
    }
    if existing.is_some() {
        backup(path)?;
    }
    write_config(path, map)?;
    Ok(format!(
        "{} `{name}` in {}",
        if entry.is_some() {
            "Registered"
        } else {
            "Removed"
        },
        path.display()
    ))
}

/// Run (or print) a `claude` command, or print it when the CLI is missing.
fn claude_step(cmd: &[String], print: bool, runner: &dyn CommandRunner) -> Result<String> {
    if print {
        Ok(format!("Would run: {}", display_command(cmd)))
    } else if runner.available() {
        Ok(format!("Claude Code: {}", runner.run(cmd)?))
    } else {
        Ok(format!(
            "The `claude` CLI was not found on PATH. Run this yourself:\n  {}",
            display_command(cmd)
        ))
    }
}

/// Register Anchor with the requested hosts. Returns the lines to show the user.
pub fn install(
    args: &InstallArgs,
    exe: &Path,
    config: Option<&Path>,
    locations: &Locations,
    runner: &dyn CommandRunner,
) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for client in args.client.expand() {
        match client {
            Client::ClaudeCode => {
                let cmd = claude_add_args(&args.name, exe, config, args.scope.as_deref());
                out.push(claude_step(&cmd, args.print, runner)?);
            }
            Client::ClaudeDesktop => out.push(edit_json(
                &locations.claude_desktop,
                &args.name,
                Some(json_entry(exe, config)),
                args.print,
            )?),
            Client::Cursor => out.push(edit_json(
                &locations.cursor,
                &args.name,
                Some(json_entry(exe, config)),
                args.print,
            )?),
            Client::All => {}
        }
    }
    if !args.print {
        out.push("Restart the host application to pick up the change.".to_owned());
    }
    Ok(out)
}

/// Remove the registration made by [`install`].
pub fn uninstall(
    args: &UninstallArgs,
    locations: &Locations,
    runner: &dyn CommandRunner,
) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for client in args.client.expand() {
        match client {
            Client::ClaudeCode => {
                let mut cmd = vec!["mcp".to_owned(), "remove".to_owned()];
                if let Some(scope) = &args.scope {
                    cmd.push("--scope".to_owned());
                    cmd.push(scope.clone());
                }
                cmd.push(args.name.clone());
                out.push(claude_step(&cmd, args.print, runner)?);
            }
            Client::ClaudeDesktop => out.push(edit_json(
                &locations.claude_desktop,
                &args.name,
                None,
                args.print,
            )?),
            Client::Cursor => out.push(edit_json(&locations.cursor, &args.name, None, args.print)?),
            Client::All => {}
        }
    }
    Ok(out)
}

crate::base::opaque_debug!(SystemRunner);

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;

    struct Fake {
        available: bool,
        calls: RefCell<Vec<Vec<String>>>,
    }

    impl CommandRunner for Fake {
        fn available(&self) -> bool {
            self.available
        }
        fn run(&self, args: &[String]) -> Result<String> {
            self.calls.borrow_mut().push(args.to_vec());
            Ok("ok".to_owned())
        }
    }

    fn fake(available: bool) -> Fake {
        Fake {
            available,
            calls: RefCell::new(Vec::new()),
        }
    }

    fn locs(dir: &Path) -> Locations {
        Locations {
            claude_desktop: dir.join("Claude").join("claude_desktop_config.json"),
            cursor: dir.join(".cursor").join("mcp.json"),
        }
    }

    fn args(client: Client, print: bool) -> InstallArgs {
        InstallArgs {
            client,
            name: "anchor".into(),
            print,
            scope: None,
        }
    }

    fn exe() -> PathBuf {
        PathBuf::from("/opt/anchor/anchor")
    }

    fn read(path: &Path) -> Value {
        serde_json::from_str(&std::fs::read_to_string(path).expect("read")).expect("json")
    }

    #[test]
    fn fresh_file_is_created() {
        let dir = tempfile::tempdir().expect("tmp");
        let l = locs(dir.path());
        install(&args(Client::Cursor, false), &exe(), None, &l, &fake(false)).expect("install");
        let v = read(&l.cursor);
        assert_eq!(v["mcpServers"]["anchor"]["command"], "/opt/anchor/anchor");
        assert_eq!(v["mcpServers"]["anchor"]["args"], json!(["mcp"]));
        assert!(!l.cursor.with_extension("json.bak").exists());
    }

    #[test]
    fn merge_preserves_other_servers_and_keys_and_writes_backup() {
        let dir = tempfile::tempdir().expect("tmp");
        let l = locs(dir.path());
        std::fs::create_dir_all(l.claude_desktop.parent().expect("p")).expect("mk");
        let original = json!({"theme": "dark", "mcpServers": {"other": {"command": "x"}}});
        std::fs::write(&l.claude_desktop, original.to_string()).expect("w");
        let cfg = PathBuf::from("/etc/anchor.toml");
        install(
            &args(Client::ClaudeDesktop, false),
            &exe(),
            Some(&cfg),
            &l,
            &fake(false),
        )
        .expect("install");
        let v = read(&l.claude_desktop);
        assert_eq!(v["theme"], "dark");
        assert_eq!(v["mcpServers"]["other"]["command"], "x");
        assert_eq!(
            v["mcpServers"]["anchor"]["args"],
            json!(["--config", "/etc/anchor.toml", "mcp"])
        );
        let bak = l.claude_desktop.with_extension("json.bak");
        assert_eq!(read(&bak), original);
    }

    #[test]
    fn reinstall_is_idempotent() {
        let dir = tempfile::tempdir().expect("tmp");
        let l = locs(dir.path());
        install(&args(Client::Cursor, false), &exe(), None, &l, &fake(false)).expect("1");
        let first = std::fs::read_to_string(&l.cursor).expect("r");
        let out = install(&args(Client::Cursor, false), &exe(), None, &l, &fake(false)).expect("2");
        assert!(out[0].contains("already registered"));
        assert_eq!(std::fs::read_to_string(&l.cursor).expect("r"), first);
    }

    #[test]
    fn invalid_json_is_refused_and_untouched() {
        let dir = tempfile::tempdir().expect("tmp");
        let l = locs(dir.path());
        std::fs::create_dir_all(l.cursor.parent().expect("p")).expect("mk");
        std::fs::write(&l.cursor, "{ not json").expect("w");
        let err = install(&args(Client::Cursor, false), &exe(), None, &l, &fake(false))
            .expect_err("must refuse");
        assert!(err.to_string().contains("not valid JSON"), "{err}");
        assert_eq!(std::fs::read_to_string(&l.cursor).expect("r"), "{ not json");
        assert!(!l.cursor.with_extension("json.bak").exists());
    }

    #[test]
    fn print_mode_writes_nothing_and_runs_nothing() {
        let dir = tempfile::tempdir().expect("tmp");
        let l = locs(dir.path());
        let runner = fake(true);
        let out = install(&args(Client::All, true), &exe(), None, &l, &runner).expect("print");
        assert!(!l.cursor.exists() && !l.claude_desktop.exists());
        assert!(!l.cursor.parent().expect("p").exists());
        assert!(runner.calls.borrow().is_empty());
        assert!(
            out.iter()
                .any(|l| l.starts_with("Would run: claude mcp add"))
        );
    }

    #[test]
    fn claude_code_uses_runner_or_prints_command() {
        let dir = tempfile::tempdir().expect("tmp");
        let l = locs(dir.path());
        let runner = fake(true);
        let mut a = args(Client::ClaudeCode, false);
        a.scope = Some("user".into());
        let cfg = PathBuf::from("/my cfg.toml");
        install(&a, &exe(), Some(&cfg), &l, &runner).expect("install");
        assert_eq!(
            runner.calls.borrow()[0],
            [
                "mcp",
                "add",
                "--scope",
                "user",
                "anchor",
                "--",
                "/opt/anchor/anchor",
                "--config",
                "/my cfg.toml",
                "mcp"
            ]
        );
        let out = install(
            &args(Client::ClaudeCode, false),
            &exe(),
            None,
            &l,
            &fake(false),
        )
        .expect("fallback");
        assert!(out[0].contains("claude mcp add anchor -- /opt/anchor/anchor mcp"));
    }

    #[test]
    fn uninstall_removes_only_ours() {
        let dir = tempfile::tempdir().expect("tmp");
        let l = locs(dir.path());
        std::fs::create_dir_all(l.cursor.parent().expect("p")).expect("mk");
        std::fs::write(
            &l.cursor,
            json!({"mcpServers": {"other": {"command": "x"}}}).to_string(),
        )
        .expect("w");
        install(&args(Client::Cursor, false), &exe(), None, &l, &fake(false)).expect("i");
        let u = UninstallArgs {
            client: Client::Cursor,
            name: "anchor".into(),
            print: false,
            scope: None,
        };
        uninstall(&u, &l, &fake(false)).expect("u");
        let v = read(&l.cursor);
        assert!(v["mcpServers"].get("anchor").is_none());
        assert_eq!(v["mcpServers"]["other"]["command"], "x");
    }
}
