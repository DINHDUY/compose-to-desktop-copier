use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use crate::boot_progress::BootReport;

pub const STATUS_MACHINE_INIT: &str = "Downloading and starting the Podman machine";
pub const STATUS_MACHINE_START: &str = "Starting the Podman machine";
pub const STATUS_PULL: &str = "Pulling images and starting containers";

const MIN_PROVIDER_BYTES: u64 = 1_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EngineKind {
    Podman,
    Docker,
}

#[derive(Clone, Debug)]
pub struct Engine {
    kind: EngineKind,
    program: String,
    compose_provider: Option<String>,
    wsl_program: Option<String>,
    manage_machine: bool,
}

impl Engine {
    pub fn docker() -> Self {
        Self {
            kind: EngineKind::Docker,
            program: "docker".to_string(),
            compose_provider: None,
            wsl_program: None,
            manage_machine: false,
        }
    }

    #[cfg(test)]
    pub fn docker_with_program(program: impl Into<String>) -> Self {
        Self {
            program: program.into(),
            ..Self::docker()
        }
    }

    #[cfg(test)]
    pub fn podman_for_test(
        program: impl Into<String>,
        compose_provider: impl Into<String>,
        wsl_program: Option<String>,
    ) -> Self {
        Self {
            kind: EngineKind::Podman,
            program: program.into(),
            compose_provider: Some(compose_provider.into()),
            wsl_program,
            manage_machine: true,
        }
    }

    #[cfg(test)]
    pub fn program(&self) -> &str {
        &self.program
    }

    #[cfg(test)]
    pub fn compose_provider(&self) -> Option<&str> {
        self.compose_provider.as_deref()
    }
}

#[derive(Debug)]
pub struct ComposeError {
    message: String,
}

impl ComposeError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: truncate_message(crate::boot_progress::redact_sensitive(&message.into())),
        }
    }
}

impl std::fmt::Display for ComposeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ComposeError {}

pub struct ResolveRequest<'a> {
    pub debug: bool,
    pub container_runtime: Option<&'a str>,
    pub podman_binaries: &'a [PathBuf],
    pub path_podman: Option<&'a str>,
    pub compose_provider: Option<&'a Path>,
    pub manage_machine: bool,
    pub wsl_program: Option<&'a str>,
}

pub fn resolve(
    resource_dir: Option<&Path>,
    manifest_engine: Option<&str>,
) -> Result<Engine, ComposeError> {
    let runtime = std::env::var("CONTAINER_RUNTIME").ok();
    let debug = cfg!(debug_assertions);
    let normalized = runtime
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| value.to_ascii_lowercase());
    let preference = normalized
        .as_deref()
        .or_else(|| {
            manifest_engine
                .map(str::trim)
                .filter(|value| !value.is_empty() && !value.eq_ignore_ascii_case("auto"))
        })
        .map(|value| value.to_ascii_lowercase());
    let want_podman = match preference.as_deref() {
        Some("podman") => true,
        Some("docker") => false,
        Some(other) => {
            return Err(ComposeError::new(format!(
                "CONTAINER_RUNTIME must be docker or podman, got {other}"
            )));
        }
        None if debug => false,
        None => true,
    };
    let binaries = if want_podman {
        existing_podman_binaries(debug)
    } else {
        Vec::new()
    };
    let responds = debug && want_podman && binaries.is_empty() && podman_responds();
    let path_podman = path_podman_program(debug, binaries.is_empty(), responds).map(str::to_string);
    let provider = if want_podman {
        locate_compose_provider(resource_dir, debug)
    } else {
        None
    };
    resolve_from(ResolveRequest {
        debug,
        container_runtime: runtime.as_deref(),
        podman_binaries: &binaries,
        path_podman: path_podman.as_deref(),
        compose_provider: provider.as_deref(),
        manage_machine: cfg!(any(windows, target_os = "macos")),
        wsl_program: if cfg!(windows) { Some("wsl") } else { None },
    })
}

pub fn resolve_from(request: ResolveRequest<'_>) -> Result<Engine, ComposeError> {
    let runtime = request
        .container_runtime
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| value.to_ascii_lowercase());
    let want_podman = if request.debug {
        match runtime.as_deref() {
            None | Some("docker") => false,
            Some("podman") => true,
            Some(other) => {
                return Err(ComposeError::new(format!(
                    "CONTAINER_RUNTIME must be docker or podman, got {other}"
                )));
            }
        }
    } else {
        true
    };
    if !want_podman {
        return Ok(Engine::docker());
    }
    let program = request
        .podman_binaries
        .first()
        .map(|path| path.to_string_lossy().into_owned())
        .or_else(|| request.path_podman.map(str::to_string))
        .ok_or_else(|| ComposeError::new("Podman is not installed. Reinstall the application."))?;
    let provider = request.compose_provider.ok_or_else(|| {
        ComposeError::new("Compose support is missing. Reinstall the application.")
    })?;
    Ok(Engine {
        kind: EngineKind::Podman,
        program,
        compose_provider: Some(provider.to_string_lossy().into_owned()),
        wsl_program: request.wsl_program.map(str::to_string),
        manage_machine: request.manage_machine,
    })
}

pub fn podman_candidates(
    podman_path: Option<&str>,
    local_app_data: Option<&Path>,
    program_files: Option<&Path>,
) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(path) = podman_path.map(str::trim).filter(|value| !value.is_empty()) {
        paths.push(PathBuf::from(path));
    }
    if let Some(dir) = local_app_data {
        paths.push(dir.join("Programs").join("Podman").join(podman_file_name()));
    }
    if let Some(dir) = program_files {
        paths.push(dir.join("Podman").join(podman_file_name()));
    }
    paths
}

pub fn compose_provider_candidates(
    resource_dir: Option<&Path>,
    exe_dir: Option<&Path>,
) -> Vec<PathBuf> {
    let name = compose_provider_file_name();
    let mut paths = Vec::new();
    if let Some(dir) = resource_dir {
        paths.push(dir.join(name));
    }
    if let Some(dir) = exe_dir {
        paths.push(dir.join("resources").join(name));
        paths.push(dir.join(name));
    }
    paths
}

pub fn usable_binary(path: &Path) -> bool {
    std::fs::metadata(path)
        .map(|meta| meta.is_file() && meta.len() >= MIN_PROVIDER_BYTES)
        .unwrap_or(false)
}

pub fn prepare(engine: &Engine, reporter: &dyn BootReport) -> Result<(), ComposeError> {
    reporter.report_phase(
        crate::boot_progress::BootPhase::Engine,
        "Checking the container engine",
        "Verifying Docker or Podman",
    );
    match engine.kind {
        EngineKind::Docker => ensure_info(
            engine,
            "Docker is not running. Start Docker Desktop and open the app again.",
        ),
        EngineKind::Podman => prepare_podman(engine, reporter),
    }
}

pub fn run(engine: &Engine, args: &[String]) -> Result<Output, ComposeError> {
    run_program(engine, &engine.program, args, true)
}

pub fn run_streaming(
    engine: &Engine,
    args: &[String],
    on_line: &dyn Fn(&str),
) -> Result<Output, ComposeError> {
    run_program_streaming(engine, &engine.program, args, true, on_line)
}

fn prepare_podman(engine: &Engine, reporter: &dyn BootReport) -> Result<(), ComposeError> {
    if let Some(wsl) = &engine.wsl_program {
        reporter.report_detail("Checking Windows subsystem for Linux");
        ensure_wsl(engine, wsl)?;
    }
    if engine.manage_machine {
        ensure_machine(engine, reporter)?;
    }
    reporter.report_detail("Checking the Podman engine");
    ensure_info(engine, "Podman is installed but the engine did not start.")
}

fn ensure_wsl(engine: &Engine, wsl: &str) -> Result<(), ComposeError> {
    let output = run_program(engine, wsl, &["--status".to_string()], false)
        .map_err(|_| ComposeError::new(wsl_message()))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(ComposeError::new(wsl_message()))
    }
}

fn wsl_message() -> &'static str {
    "Windows needs WSL before Podman can start.\n\n\
Open an administrator PowerShell and run:\n\
wsl --install --no-distribution\n\n\
Restart Windows, then open the application again."
}

fn ensure_machine(engine: &Engine, reporter: &dyn BootReport) -> Result<(), ComposeError> {
    reporter.report_detail("Checking the Podman machine");
    let listed = run(
        engine,
        &[
            "machine".to_string(),
            "list".to_string(),
            "--format".to_string(),
            "json".to_string(),
        ],
    )?;
    if !listed.status.success() {
        return Err(ComposeError::new(format!(
            "Podman could not list its machines.\n\n{}",
            command_text(&listed)
        )));
    }
    let presence = machine_presence(&String::from_utf8_lossy(&listed.stdout))?;
    match presence {
        MachinePresence::Running => Ok(()),
        MachinePresence::None => {
            reporter.report_detail(STATUS_MACHINE_INIT);
            let output = run(
                engine,
                &[
                    "machine".to_string(),
                    "init".to_string(),
                    "--now".to_string(),
                ],
            )?;
            if output.status.success() {
                Ok(())
            } else {
                Err(ComposeError::new(format!(
                    "Podman could not create its machine.\n\n{}",
                    command_text(&output)
                )))
            }
        }
        MachinePresence::Stopped => {
            reporter.report_detail(STATUS_MACHINE_START);
            let output = run(engine, &["machine".to_string(), "start".to_string()])?;
            if output.status.success() {
                Ok(())
            } else {
                Err(ComposeError::new(format!(
                    "Podman could not start its machine.\n\n{}",
                    command_text(&output)
                )))
            }
        }
    }
}

fn ensure_info(engine: &Engine, summary: &str) -> Result<(), ComposeError> {
    let output = run(engine, &["info".to_string()])?;
    if output.status.success() {
        Ok(())
    } else {
        Err(ComposeError::new(format!(
            "{summary}\n\n{}",
            command_text(&output)
        )))
    }
}

#[derive(Debug, PartialEq, Eq)]
enum MachinePresence {
    None,
    Stopped,
    Running,
}

fn machine_presence(text: &str) -> Result<MachinePresence, ComposeError> {
    let text = text.trim();
    if text.is_empty() || text == "[]" || text == "null" {
        return Ok(MachinePresence::None);
    }
    let value: serde_json::Value = serde_json::from_str(text)
        .map_err(|_| ComposeError::new("Podman returned an unreadable machine list."))?;
    let machines = match value {
        serde_json::Value::Array(items) => items,
        _ => {
            return Err(ComposeError::new(
                "Podman returned an unreadable machine list.",
            ))
        }
    };
    if machines.is_empty() {
        return Ok(MachinePresence::None);
    }
    let chosen = machines
        .iter()
        .find(|machine| machine.get("Default") == Some(&serde_json::Value::Bool(true)))
        .unwrap_or(&machines[0]);
    let running = chosen.get("Running") == Some(&serde_json::Value::Bool(true));
    Ok(if running {
        MachinePresence::Running
    } else {
        MachinePresence::Stopped
    })
}

fn run_program(
    engine: &Engine,
    program: &str,
    args: &[String],
    podman_env: bool,
) -> Result<Output, ComposeError> {
    let mut command = configure_command(engine, program, args, podman_env);
    command
        .output()
        .map_err(|err| run_error(program, &engine.kind, err))
}

fn run_program_streaming(
    engine: &Engine,
    program: &str,
    args: &[String],
    podman_env: bool,
    on_line: &dyn Fn(&str),
) -> Result<Output, ComposeError> {
    let mut command = configure_command(engine, program, args, podman_env);
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .map_err(|err| run_error(program, &engine.kind, err))?;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let (line_tx, line_rx) = mpsc::channel();
    if let Some(stdout) = stdout {
        let sender = line_tx.clone();
        thread::spawn(move || pipe_lines(stdout, sender));
    }
    if let Some(stderr) = stderr {
        thread::spawn(move || pipe_lines(stderr, line_tx));
    }
    let mut combined = Vec::new();
    loop {
        match line_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(line) => {
                on_line(&line);
                combined.extend_from_slice(line.as_bytes());
                combined.push(b'\n');
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if child
                    .try_wait()
                    .map_err(|err| run_error(program, &engine.kind, err))?
                    .is_some()
                {
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    while let Ok(line) = line_rx.try_recv() {
        on_line(&line);
        combined.extend_from_slice(line.as_bytes());
        combined.push(b'\n');
    }
    let status = child
        .wait()
        .map_err(|err| run_error(program, &engine.kind, err))?;
    Ok(Output {
        status,
        stdout: combined,
        stderr: Vec::new(),
    })
}

fn pipe_lines<R: std::io::Read + Send + 'static>(reader: R, sender: mpsc::Sender<String>) {
    for line in BufReader::new(reader).lines().map_while(Result::ok) {
        if sender.send(line).is_err() {
            break;
        }
    }
}

fn configure_command(engine: &Engine, program: &str, args: &[String], podman_env: bool) -> Command {
    let mut command = Command::new(program);
    hide_window(&mut command, program);
    if podman_env {
        if let Some(provider) = &engine.compose_provider {
            command.env("PODMAN_COMPOSE_PROVIDER", provider);
            command.env("PODMAN_COMPOSE_WARNING_LOGS", "false");
        }
    }
    command.args(args);
    command
}

fn run_error(program: &str, kind: &EngineKind, err: std::io::Error) -> ComposeError {
    let hint = match kind {
        EngineKind::Docker => "Install Docker and make sure it is on PATH.",
        EngineKind::Podman => "Reinstall the application so it can install Podman.",
    };
    ComposeError::new(format!("Could not run '{program}'. {hint}\n\n{err}"))
}

fn hide_window(command: &mut Command, program: &str) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        let program = program.to_ascii_lowercase();
        if !program.ends_with(".cmd") && !program.ends_with(".bat") {
            command.creation_flags(CREATE_NO_WINDOW);
        }
    }
    #[cfg(not(windows))]
    {
        let _ = (command, program);
    }
}

pub fn command_text(output: &Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let text = if stderr.trim().is_empty() {
        stdout.trim()
    } else {
        stderr.trim()
    };
    if text.is_empty() {
        format!("command exited with {}", output.status)
    } else {
        text.to_string()
    }
}

fn existing_podman_binaries(debug: bool) -> Vec<PathBuf> {
    let podman_path = std::env::var("PODMAN_PATH").ok();
    let local_app_data = std::env::var_os("LOCALAPPDATA").map(PathBuf::from);
    let program_files = std::env::var_os("PROGRAMFILES").map(PathBuf::from);
    podman_search_paths(
        debug,
        podman_path.as_deref(),
        local_app_data.as_deref(),
        program_files.as_deref(),
    )
    .into_iter()
    .filter(|path| path.is_file())
    .collect()
}

fn podman_search_paths(
    debug: bool,
    podman_path: Option<&str>,
    local_app_data: Option<&Path>,
    program_files: Option<&Path>,
) -> Vec<PathBuf> {
    let podman_path = if debug {
        podman_path.map(str::trim).filter(|value| !value.is_empty())
    } else {
        None
    };
    podman_candidates(podman_path, local_app_data, program_files)
}

fn path_podman_program(debug: bool, binaries_empty: bool, responds: bool) -> Option<&'static str> {
    if debug && binaries_empty && responds {
        Some(podman_file_name())
    } else {
        None
    }
}

fn ordered_compose_providers(
    resource_dir: Option<&Path>,
    exe_dir: Option<&Path>,
    env_provider: Option<&str>,
    path_provider: Option<&Path>,
) -> Vec<PathBuf> {
    let mut paths = compose_provider_candidates(resource_dir, exe_dir);
    if let Some(value) = env_provider
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        paths.push(PathBuf::from(value));
    }
    if let Some(path) = path_provider {
        paths.push(path.to_path_buf());
    }
    paths
}

fn compose_provider_search_paths(
    debug: bool,
    resource_dir: Option<&Path>,
    exe_dir: Option<&Path>,
    env_provider: Option<&str>,
    path_provider: Option<&Path>,
) -> Vec<PathBuf> {
    let (env_provider, path_provider) = if debug {
        (env_provider, path_provider)
    } else {
        (None, None)
    };
    ordered_compose_providers(resource_dir, exe_dir, env_provider, path_provider)
}

fn locate_compose_provider(resource_dir: Option<&Path>, debug: bool) -> Option<PathBuf> {
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(Path::to_path_buf));
    let env_provider = if debug {
        std::env::var("PODMAN_COMPOSE_PROVIDER").ok()
    } else {
        None
    };
    let path_provider = if debug {
        compose_provider_from_path()
    } else {
        None
    };
    compose_provider_search_paths(
        debug,
        resource_dir,
        exe_dir.as_deref(),
        env_provider.as_deref(),
        path_provider.as_deref(),
    )
    .into_iter()
    .find(|path| usable_binary(path))
}

fn compose_provider_from_path() -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    let name = compose_provider_file_name();
    std::env::split_paths(&path_var)
        .map(|dir| dir.join(name))
        .find(|path| usable_binary(path))
}

fn podman_responds() -> bool {
    let mut command = Command::new(podman_file_name());
    hide_window(&mut command, podman_file_name());
    command
        .arg("--version")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

fn podman_file_name() -> &'static str {
    if cfg!(windows) {
        "podman.exe"
    } else {
        "podman"
    }
}

fn compose_provider_file_name() -> &'static str {
    if cfg!(windows) {
        "docker-compose.exe"
    } else {
        "docker-compose"
    }
}

fn truncate_message(mut message: String) -> String {
    const MAX: usize = 4000;
    if message.len() <= MAX {
        return message;
    }
    let mut end = MAX;
    while !message.is_char_boundary(end) {
        end -= 1;
    }
    message.truncate(end);
    message.push_str("\n...");
    message
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_defaults_to_docker() {
        let engine = resolve_from(ResolveRequest {
            debug: true,
            container_runtime: None,
            podman_binaries: &[],
            path_podman: None,
            compose_provider: None,
            manage_machine: false,
            wsl_program: None,
        })
        .unwrap();
        assert_eq!(engine.program(), "docker");
        assert!(engine.compose_provider().is_none());
        assert_eq!(engine.kind, EngineKind::Docker);
    }

    #[test]
    fn release_requires_podman_and_compose() {
        let missing = resolve_from(ResolveRequest {
            debug: false,
            container_runtime: None,
            podman_binaries: &[],
            path_podman: None,
            compose_provider: None,
            manage_machine: true,
            wsl_program: Some("wsl"),
        })
        .unwrap_err();
        assert!(missing.to_string().contains("Podman is not installed"));

        let podman = PathBuf::from("podman.exe");
        let missing_compose = resolve_from(ResolveRequest {
            debug: false,
            container_runtime: Some("docker"),
            podman_binaries: std::slice::from_ref(&podman),
            path_podman: None,
            compose_provider: None,
            manage_machine: true,
            wsl_program: Some("wsl"),
        })
        .unwrap_err();
        assert!(missing_compose
            .to_string()
            .contains("Compose support is missing"));
    }

    #[test]
    fn release_ignores_podman_path_and_path_podman() {
        let paths = podman_search_paths(
            false,
            Some(r"D:\custom\podman.exe"),
            Some(Path::new(r"C:\Users\me\AppData\Local")),
            Some(Path::new(r"C:\Program Files")),
        );
        assert!(!paths
            .iter()
            .any(|path| path.ends_with(r"D:\custom\podman.exe")));
        assert_eq!(paths.len(), 2);
        assert!(path_podman_program(false, true, true).is_none());
        assert_eq!(
            path_podman_program(true, true, true),
            Some(podman_file_name())
        );
    }

    #[test]
    fn release_ignores_compose_provider_overrides() {
        let name = compose_provider_file_name();
        let paths = compose_provider_search_paths(
            false,
            Some(Path::new("/res")),
            Some(Path::new("/exe")),
            Some(r"D:\tools\docker-compose.exe"),
            Some(Path::new(r"C:\bin\docker-compose.exe")),
        );
        assert_eq!(
            paths,
            vec![
                Path::new("/res").join(name),
                Path::new("/exe").join("resources").join(name),
                Path::new("/exe").join(name),
            ]
        );
    }

    #[test]
    fn podman_path_wins_over_default_locations() {
        let name = podman_file_name();
        let paths = podman_candidates(
            Some(r"D:\custom\podman.exe"),
            Some(Path::new(r"C:\Users\me\AppData\Local")),
            Some(Path::new(r"C:\Program Files")),
        );
        let local = Path::new(r"C:\Users\me\AppData\Local");
        let program_files = Path::new(r"C:\Program Files");
        assert_eq!(paths[0], PathBuf::from(r"D:\custom\podman.exe"));
        assert_eq!(paths[1], local.join("Programs").join("Podman").join(name));
        assert_eq!(paths[2], program_files.join("Podman").join(name));
    }

    #[test]
    fn empty_podman_path_is_skipped() {
        let paths = podman_candidates(Some("  "), None, None);
        assert!(paths.is_empty());
    }

    #[test]
    fn compose_provider_checks_resource_dir_then_the_executable_dir() {
        let name = compose_provider_file_name();
        let paths = compose_provider_candidates(Some(Path::new("/res")), Some(Path::new("/exe")));
        assert_eq!(
            paths,
            vec![
                Path::new("/res").join(name),
                Path::new("/exe").join("resources").join(name),
                Path::new("/exe").join(name),
            ]
        );
    }

    #[test]
    fn compose_provider_lookup_checks_bundled_paths_before_env_and_path() {
        let name = compose_provider_file_name();
        let paths = ordered_compose_providers(
            Some(Path::new("/res")),
            Some(Path::new("/exe")),
            Some(r"D:\tools\docker-compose.exe"),
            Some(Path::new(r"C:\bin\docker-compose.exe")),
        );
        assert_eq!(
            paths,
            vec![
                Path::new("/res").join(name),
                Path::new("/exe").join("resources").join(name),
                Path::new("/exe").join(name),
                PathBuf::from(r"D:\tools\docker-compose.exe"),
                PathBuf::from(r"C:\bin\docker-compose.exe"),
            ]
        );
    }

    #[test]
    fn blank_compose_provider_env_is_skipped() {
        let paths = ordered_compose_providers(None, None, Some("  "), None);
        assert!(paths.is_empty());
    }

    #[test]
    fn small_files_are_not_compose_providers() {
        let dir =
            std::env::temp_dir().join(format!("compose-shell-provider-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("docker-compose.exe");
        std::fs::write(&path, b"not a compose binary").unwrap();
        assert!(!usable_binary(&path));
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn machine_list_uses_the_default_machine() {
        let json = r#"[
            {"Name":"other","Default":false,"Running":true},
            {"Name":"podman-machine-default","Default":true,"Running":false}
        ]"#;
        assert_eq!(machine_presence(json).unwrap(), MachinePresence::Stopped);
        assert_eq!(machine_presence("[]").unwrap(), MachinePresence::None);
        assert_eq!(machine_presence("").unwrap(), MachinePresence::None);
        assert_eq!(
            machine_presence(
                r#"[{"Name":"podman-machine-default","Default":true,"Running":true}]"#
            )
            .unwrap(),
            MachinePresence::Running
        );
    }
}
