//! `anchor-service.exe`: runs Anchor as a Windows service.
//!
//! `anchor.exe` is an ordinary console program. A Windows service must speak the Service Control
//! Manager's protocol, so this small host registers with it, starts `anchor.exe daemon` (or
//! `anchor.exe server`) as a child process, restarts it if it exits, and stops it when the
//! service is stopped. Output of the child goes to a log file.
//!
//! ```text
//! anchor-service install [--mode daemon|server] [--config PATH] [--name NAME]
//! anchor-service start | stop | status | uninstall [--name NAME]
//! ```
//!
//! `install`, `start`, `stop` and `uninstall` need an elevated (Administrator) prompt. The
//! service runs as LocalSystem and starts automatically at boot. Its configuration defaults to
//! `%ProgramData%\Anchor\anchor.toml` (created on install if missing, with the data directory
//! next to it) and its log is `%ProgramData%\Anchor\logs\anchor-service.log`.

#[cfg(not(windows))]
fn main() {
    eprintln!(
        "anchor-service is the Windows service host for Anchor; on this platform run `anchor daemon` under systemd or launchd instead."
    );
    std::process::exit(2);
}

#[cfg(windows)]
fn main() {
    if let Err(message) = windows_host::run() {
        eprintln!("anchor-service: {message}");
        std::process::exit(1);
    }
}

/// Command-line options shared by the management commands and the service itself.
#[cfg_attr(not(windows), allow(dead_code))]
#[derive(Clone, Debug, PartialEq, Eq)]
struct Options {
    /// Service name as registered with the Service Control Manager.
    name: String,
    /// `daemon` or `server`: the `anchor` subcommand the service runs.
    mode: String,
    /// Configuration file passed to `anchor --config`.
    config: Option<std::path::PathBuf>,
    /// Path of `anchor.exe`; defaults to the one next to this program.
    exe: Option<std::path::PathBuf>,
}

/// Parses `--name`, `--mode`, `--config` and `--exe` (each followed by its value).
#[cfg_attr(not(windows), allow(dead_code))]
fn parse_options(args: &[String]) -> Result<Options, String> {
    let mut options = Options {
        name: "Anchor".to_owned(),
        mode: "daemon".to_owned(),
        config: None,
        exe: None,
    };
    let mut iter = args.iter();
    while let Some(flag) = iter.next() {
        let mut value = || {
            iter.next()
                .cloned()
                .ok_or_else(|| format!("{flag} needs a value"))
        };
        match flag.as_str() {
            "--name" => options.name = value()?,
            "--mode" => {
                let mode = value()?;
                if mode != "daemon" && mode != "server" {
                    return Err(format!("--mode must be `daemon` or `server`, not `{mode}`"));
                }
                options.mode = mode;
            }
            "--config" => options.config = Some(value()?.into()),
            "--exe" => options.exe = Some(value()?.into()),
            other => return Err(format!("unknown option `{other}`")),
        }
    }
    if options.name.is_empty() || options.name.contains(['/', '\\']) {
        return Err("--name must be a non-empty name without slashes".to_owned());
    }
    Ok(options)
}

/// How long to wait before restarting a child that exited: 2 s doubling to 60 s, reset once a
/// run has lasted a minute.
#[cfg_attr(not(windows), allow(dead_code))]
fn next_backoff(
    previous: std::time::Duration,
    ran_for: std::time::Duration,
) -> std::time::Duration {
    use std::time::Duration;
    if ran_for >= Duration::from_secs(60) {
        Duration::from_secs(2)
    } else {
        (previous * 2).clamp(Duration::from_secs(2), Duration::from_secs(60))
    }
}

#[cfg(windows)]
mod windows_host {
    use std::ffi::OsString;
    use std::fs::OpenOptions;
    use std::io::Write as _;
    use std::path::{Path, PathBuf};
    use std::process::{Child, Command, Stdio};
    use std::sync::OnceLock;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    use windows_service::service::{
        ServiceAccess, ServiceControl, ServiceControlAccept, ServiceErrorControl, ServiceExitCode,
        ServiceInfo, ServiceStartType, ServiceState, ServiceStatus, ServiceType,
    };
    use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
    use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
    use windows_service::{define_windows_service, service_dispatcher};

    use super::{Options, next_backoff, parse_options};

    static OPTIONS: OnceLock<Options> = OnceLock::new();

    define_windows_service!(ffi_service_main, service_main);

    fn program_data() -> PathBuf {
        std::env::var_os("ProgramData")
            .map_or_else(|| PathBuf::from(r"C:\ProgramData"), PathBuf::from)
            .join("Anchor")
    }

    fn default_config() -> PathBuf {
        program_data().join("anchor.toml")
    }

    fn log_path() -> PathBuf {
        program_data().join("logs").join("anchor-service.log")
    }

    fn log(message: &str) {
        let path = log_path();
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(&path) {
            let _ = writeln!(file, "[anchor-service] {message}");
        }
    }

    fn anchor_exe(options: &Options) -> Result<PathBuf, String> {
        if let Some(exe) = &options.exe {
            return Ok(exe.clone());
        }
        let own =
            std::env::current_exe().map_err(|err| format!("cannot find this program: {err}"))?;
        let exe = own.with_file_name("anchor.exe");
        if exe.is_file() {
            Ok(exe)
        } else {
            Err(format!(
                "anchor.exe was not found next to {} (pass --exe)",
                own.display()
            ))
        }
    }

    /// Writes a starter configuration whose data directory is next to it, unless one exists.
    fn ensure_config(path: &Path, mode: &str) -> Result<(), String> {
        if path.exists() {
            return Ok(());
        }
        let dir = path.parent().unwrap_or_else(|| Path::new("."));
        std::fs::create_dir_all(dir)
            .map_err(|err| format!("cannot create {}: {err}", dir.display()))?;
        let data = dir.join("data");
        // TOML literal strings (single quotes) keep backslashes as they are.
        let mut text = format!("data_dir = '{}'\n", data.display());
        if mode == "server" {
            text.push_str("mode = \"server\"\n\n[sync]\nlisten = \"0.0.0.0:7891\"\nauth_token_env = \"ANCHOR_SYNC_TOKEN\"\n");
        }
        std::fs::write(path, text).map_err(|err| format!("cannot write {}: {err}", path.display()))
    }

    fn manager(access: ServiceManagerAccess) -> Result<ServiceManager, String> {
        ServiceManager::local_computer(None::<&str>, access).map_err(|err| {
            format!("cannot reach the Service Control Manager ({err}); run from an elevated prompt")
        })
    }

    fn install(options: &Options) -> Result<(), String> {
        let exe = anchor_exe(options)?;
        let config = options.config.clone().unwrap_or_else(default_config);
        ensure_config(&config, &options.mode)?;
        let own = std::env::current_exe().map_err(|err| err.to_string())?;
        let info = ServiceInfo {
            name: OsString::from(&options.name),
            display_name: OsString::from(format!("Anchor ({})", options.mode)),
            service_type: ServiceType::OWN_PROCESS,
            start_type: ServiceStartType::AutoStart,
            error_control: ServiceErrorControl::Normal,
            executable_path: own,
            launch_arguments: vec![
                OsString::from("run"),
                OsString::from("--name"),
                OsString::from(&options.name),
                OsString::from("--mode"),
                OsString::from(&options.mode),
                OsString::from("--config"),
                config.clone().into_os_string(),
                OsString::from("--exe"),
                exe.clone().into_os_string(),
            ],
            dependencies: vec![],
            account_name: None,
            account_password: None,
        };
        let manager =
            manager(ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE)?;
        let service = manager
            .create_service(&info, ServiceAccess::CHANGE_CONFIG)
            .map_err(|err| format!("cannot create the service `{}`: {err}", options.name))?;
        let _ = service.set_description(
            "Anchor context engine: keeps the knowledge graph current and serves it to local tools.",
        );
        println!(
            "Installed service `{}` running `{} --config {} {}`.\nStart it with: anchor-service start",
            options.name,
            exe.display(),
            config.display(),
            options.mode
        );
        Ok(())
    }

    fn open(
        options: &Options,
        access: ServiceAccess,
    ) -> Result<windows_service::service::Service, String> {
        manager(ServiceManagerAccess::CONNECT)?
            .open_service(&options.name, access)
            .map_err(|err| format!("cannot open the service `{}`: {err}", options.name))
    }

    fn wait_for(
        service: &windows_service::service::Service,
        wanted: ServiceState,
    ) -> Result<(), String> {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let state = service
                .query_status()
                .map_err(|err| err.to_string())?
                .current_state;
            if state == wanted {
                return Ok(());
            }
            if Instant::now() > deadline {
                return Err(format!("the service is still {state:?} after 30 s"));
            }
            std::thread::sleep(Duration::from_millis(250));
        }
    }

    fn start(options: &Options) -> Result<(), String> {
        let service = open(options, ServiceAccess::START | ServiceAccess::QUERY_STATUS)?;
        service
            .start::<&str>(&[])
            .map_err(|err| format!("cannot start the service: {err}"))?;
        wait_for(&service, ServiceState::Running)?;
        println!("Service `{}` is running.", options.name);
        Ok(())
    }

    fn stop(options: &Options) -> Result<(), String> {
        let service = open(options, ServiceAccess::STOP | ServiceAccess::QUERY_STATUS)?;
        let state = service
            .query_status()
            .map_err(|err| err.to_string())?
            .current_state;
        if state != ServiceState::Stopped {
            service
                .stop()
                .map_err(|err| format!("cannot stop the service: {err}"))?;
            wait_for(&service, ServiceState::Stopped)?;
        }
        println!("Service `{}` is stopped.", options.name);
        Ok(())
    }

    fn status(options: &Options) -> Result<(), String> {
        let service = open(options, ServiceAccess::QUERY_STATUS)?;
        let status = service.query_status().map_err(|err| err.to_string())?;
        println!("{:?}", status.current_state);
        Ok(())
    }

    fn uninstall(options: &Options) -> Result<(), String> {
        let service = open(
            options,
            ServiceAccess::STOP | ServiceAccess::QUERY_STATUS | ServiceAccess::DELETE,
        )?;
        let state = service
            .query_status()
            .map_err(|err| err.to_string())?
            .current_state;
        if state != ServiceState::Stopped {
            let _ = service.stop();
            let _ = wait_for(&service, ServiceState::Stopped);
        }
        service
            .delete()
            .map_err(|err| format!("cannot delete the service: {err}"))?;
        println!(
            "Removed service `{}`. Configuration and data under {} were kept.",
            options.name,
            program_data().display()
        );
        Ok(())
    }

    fn spawn(options: &Options) -> Result<Child, String> {
        let exe = anchor_exe(options)?;
        let config = options.config.clone().unwrap_or_else(default_config);
        let log_file = |suffix: &str| {
            let path = log_path().with_file_name(format!("anchor-{suffix}.log"));
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .map(Stdio::from)
                .unwrap_or_else(|_| Stdio::null())
        };
        Command::new(&exe)
            .arg("--config")
            .arg(&config)
            .arg(&options.mode)
            .stdin(Stdio::null())
            .stdout(log_file("out"))
            .stderr(log_file("err"))
            .spawn()
            .map_err(|err| format!("cannot start {}: {err}", exe.display()))
    }

    fn report(
        handle: &service_control_handler::ServiceStatusHandle,
        state: ServiceState,
        exit_code: u32,
    ) {
        let accepted = if state == ServiceState::Running {
            ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN
        } else {
            ServiceControlAccept::empty()
        };
        let _ = handle.set_service_status(ServiceStatus {
            service_type: ServiceType::OWN_PROCESS,
            current_state: state,
            controls_accepted: accepted,
            exit_code: ServiceExitCode::Win32(exit_code),
            checkpoint: 0,
            wait_hint: Duration::from_secs(10),
            process_id: None,
        });
    }

    fn service_main(_arguments: Vec<OsString>) {
        let Some(options) = OPTIONS.get() else {
            return;
        };
        let (stop_tx, stop_rx) = mpsc::channel::<()>();
        let handler = move |control| match control {
            ServiceControl::Stop | ServiceControl::Shutdown => {
                let _ = stop_tx.send(());
                ServiceControlHandlerResult::NoError
            }
            ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
            _ => ServiceControlHandlerResult::NotImplemented,
        };
        let Ok(handle) = service_control_handler::register(&options.name, handler) else {
            log("cannot register the service control handler");
            return;
        };
        report(&handle, ServiceState::StartPending, 0);
        let mut child = match spawn(options) {
            Ok(child) => child,
            Err(message) => {
                log(&message);
                report(&handle, ServiceState::Stopped, 1);
                return;
            }
        };
        log(&format!(
            "started `anchor {}` (pid {})",
            options.mode,
            child.id()
        ));
        report(&handle, ServiceState::Running, 0);

        let mut started = Instant::now();
        let mut backoff = Duration::from_secs(1);
        loop {
            if stop_rx.recv_timeout(Duration::from_millis(500)).is_ok() {
                break;
            }
            match child.try_wait() {
                Ok(None) => {}
                Ok(Some(exit)) => {
                    backoff = next_backoff(backoff, started.elapsed());
                    log(&format!(
                        "anchor exited ({exit}); restarting in {} s",
                        backoff.as_secs()
                    ));
                    if stop_rx.recv_timeout(backoff).is_ok() {
                        report(&handle, ServiceState::Stopped, 0);
                        return;
                    }
                    match spawn(options) {
                        Ok(next) => {
                            child = next;
                            started = Instant::now();
                            log(&format!("restarted (pid {})", child.id()));
                        }
                        Err(message) => log(&message),
                    }
                }
                Err(err) => log(&format!("cannot query the child process: {err}")),
            }
        }
        report(&handle, ServiceState::StopPending, 0);
        // The database is crash-safe (SQLite write-ahead log), so the child is simply ended.
        let _ = child.kill();
        let _ = child.wait();
        log("stopped");
        report(&handle, ServiceState::Stopped, 0);
    }

    pub fn run() -> Result<(), String> {
        let args: Vec<String> = std::env::args().skip(1).collect();
        let Some((command, rest)) = args.split_first() else {
            return Err(usage());
        };
        let options = parse_options(rest)?;
        match command.as_str() {
            "install" => install(&options),
            "uninstall" | "remove" => uninstall(&options),
            "start" => start(&options),
            "stop" => stop(&options),
            "status" => status(&options),
            "run" => {
                let name = options.name.clone();
                let _ = OPTIONS.set(options);
                service_dispatcher::start(name, ffi_service_main).map_err(|err| {
                    format!("`run` is started by the Service Control Manager, not by hand ({err})")
                })
            }
            "--help" | "-h" | "help" => {
                println!("{}", usage());
                Ok(())
            }
            other => Err(format!("unknown command `{other}`\n{}", usage())),
        }
    }

    fn usage() -> String {
        "usage: anchor-service install [--mode daemon|server] [--config PATH] [--name NAME]\n       anchor-service start | stop | status | uninstall [--name NAME]".to_owned()
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn options_default_to_the_daemon_under_the_name_anchor() {
        let options = parse_options(&[]).unwrap();
        assert_eq!(options.name, "Anchor");
        assert_eq!(options.mode, "daemon");
        assert_eq!(options.config, None);
    }

    #[test]
    fn options_are_parsed_and_validated() {
        let options = parse_options(&args(&[
            "--mode",
            "server",
            "--config",
            r"C:\ProgramData\Anchor\anchor.toml",
            "--name",
            "AnchorSync",
        ]))
        .unwrap();
        assert_eq!(options.mode, "server");
        assert_eq!(options.name, "AnchorSync");
        assert!(options.config.is_some());
        assert!(parse_options(&args(&["--mode", "web"])).is_err());
        assert!(parse_options(&args(&["--mode"])).is_err());
        assert!(parse_options(&args(&["--bogus"])).is_err());
        assert!(parse_options(&args(&["--name", r"a\b"])).is_err());
    }

    #[test]
    fn restarts_back_off_and_recover() {
        let mut wait = Duration::from_secs(1);
        let quick = Duration::from_secs(1);
        let mut seen = Vec::new();
        for _ in 0..7 {
            wait = next_backoff(wait, quick);
            seen.push(wait.as_secs());
        }
        assert_eq!(seen, [2, 4, 8, 16, 32, 60, 60]);
        assert_eq!(next_backoff(wait, Duration::from_secs(120)).as_secs(), 2);
    }
}
