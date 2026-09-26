use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use crate::boot_progress::{BootPhase, BootReport};
use crate::engine::{command_text, prepare, run, run_streaming, STATUS_PULL};
pub use crate::engine::{ComposeError, Engine};
use crate::manifest::{self, ResolvedSettings, ShellManifest};

pub const DEFAULT_HEALTH_URL: &str = "http://127.0.0.1:3000/";
pub const DEFAULT_WAIT_SECS: u64 = 120;

#[derive(Clone)]
pub struct StackContext {
    pub dir: PathBuf,
    pub manifest: ShellManifest,
}

pub fn build_context(stack_dir: PathBuf, manifest: ShellManifest) -> StackContext {
    StackContext {
        dir: stack_dir,
        manifest,
    }
}

pub fn up_args(context: &StackContext) -> Vec<String> {
    compose_args(context, &["up", "-d", "--remove-orphans"])
}

pub fn down_args(context: &StackContext) -> Vec<String> {
    let mut command = vec!["down"];
    if context.manifest.remove_volumes {
        command.push("-v");
    }
    compose_args(context, &command)
}

pub fn ps_args(context: &StackContext) -> Vec<String> {
    compose_args(context, &["ps", "--all", "--format", "json"])
}

pub fn start_and_wait(
    engine: &Engine,
    context: &StackContext,
    reporter: Arc<dyn BootReport>,
) -> Result<ResolvedSettings, ComposeError> {
    prepare(engine, reporter.as_ref())?;
    reporter.report_phase(
        BootPhase::Pull,
        "Pulling images and starting containers",
        STATUS_PULL,
    );
    start_stack(engine, context, Arc::clone(&reporter))?;
    let settings = match manifest::resolve_settings(&context.manifest, &context.dir) {
        Ok(settings) => settings,
        Err(err) => {
            let _ = stop_stack(engine, context);
            return Err(err);
        }
    };
    reporter.set_wait_secs(settings.wait_secs);
    reporter.report_phase(
        BootPhase::Health,
        "Waiting for services to become healthy",
        "Checking container health",
    );
    let deadline = Instant::now() + Duration::from_secs(settings.wait_secs);
    if let Err(err) = wait_for_compose_health(engine, context, deadline, reporter.as_ref()) {
        let _ = stop_stack(engine, context);
        return Err(err);
    }
    reporter.report_phase(
        BootPhase::Ui,
        "Waiting for the workspace",
        &settings.health_url,
    );
    if let Err(err) = wait_for_ui(&settings.health_url, deadline, reporter.as_ref()) {
        let _ = stop_stack(engine, context);
        return Err(err);
    }
    Ok(settings)
}

pub fn start_stack(
    engine: &Engine,
    context: &StackContext,
    reporter: Arc<dyn BootReport>,
) -> Result<(), ComposeError> {
    let output = run_streaming(engine, &up_args(context), &|line| {
        reporter.note_compose_line(line);
    })?;
    if output.status.success() {
        Ok(())
    } else {
        let message = format!(
            "Compose could not start the stack.\n\n{}",
            command_text(&output)
        );
        let _ = stop_stack(engine, context);
        Err(ComposeError::new(message))
    }
}

pub fn stop_stack(engine: &Engine, context: &StackContext) -> Result<(), ComposeError> {
    if !context.manifest.down_on_exit {
        return Ok(());
    }
    let output = run(engine, &down_args(context))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(ComposeError::new(format!(
            "Compose could not stop the stack.\n\n{}",
            command_text(&output)
        )))
    }
}

pub fn wait_for_compose_health(
    engine: &Engine,
    context: &StackContext,
    deadline: Instant,
    reporter: &dyn BootReport,
) -> Result<(), ComposeError> {
    loop {
        if Instant::now() >= deadline {
            break;
        }
        let output = run(engine, &ps_args(context))?;
        if !output.status.success() {
            let message = format!(
                "Compose could not report service health.\n\n{}",
                command_text(&output)
            );
            return Err(ComposeError::new(message));
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        reporter.report_detail(&summarize_ps_output(&stdout));
        reporter.tick();
        match evaluate_ps_output(&stdout) {
            PsEvaluation::Ready => return Ok(()),
            PsEvaluation::Waiting => {
                thread::sleep(Duration::from_millis(200));
            }
            PsEvaluation::Failed(message) => return Err(ComposeError::new(message)),
        }
    }
    Err(ComposeError::new(format!(
        "Services did not become healthy within {} seconds.",
        context.manifest.wait_secs
    )))
}

pub fn wait_for_ui(
    health_url: &str,
    deadline: Instant,
    reporter: &dyn BootReport,
) -> Result<(), ComposeError> {
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_millis(400))
        .timeout_read(Duration::from_millis(400))
        .timeout(Duration::from_secs(1))
        .build();
    loop {
        if http_is_success(&agent, health_url) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            break;
        }
        reporter.report_detail(&format!("Waiting for {health_url}"));
        reporter.tick();
        let remaining = deadline.saturating_duration_since(Instant::now());
        thread::sleep(remaining.min(Duration::from_millis(200)));
    }
    Err(ComposeError::new(format!(
        "The UI did not become ready at {health_url} before the wait deadline."
    )))
}

enum PsEvaluation {
    Ready,
    Waiting,
    Failed(String),
}

#[derive(Debug, Default, serde::Deserialize)]
struct PsRow {
    #[serde(default, rename = "Name")]
    name: String,
    #[serde(default, rename = "Service")]
    service: String,
    #[serde(default, rename = "State")]
    state: String,
    #[serde(default, rename = "Health")]
    health: String,
    #[serde(default, rename = "ExitCode")]
    exit_code: i64,
}

pub fn summarize_ps_output(text: &str) -> String {
    let rows = parse_ps_rows(text);
    if rows.is_empty() {
        return "Waiting for containers to start".to_string();
    }
    let total = rows.len();
    let healthy = rows
        .iter()
        .filter(|row| matches!(container_status(row), ContainerStatus::Ready))
        .count();
    if healthy == total {
        return format!("{healthy} of {total} services healthy");
    }
    if let Some(starting) = rows.iter().find(|row| {
        row.health.eq_ignore_ascii_case("starting")
            || row.state.eq_ignore_ascii_case("created")
            || row.state.eq_ignore_ascii_case("restarting")
    }) {
        let label = service_label(starting);
        return format!("{healthy} of {total} services healthy · {label}: starting");
    }
    format!("{healthy} of {total} services healthy")
}

fn service_label(row: &PsRow) -> String {
    if !row.service.is_empty() {
        row.service.clone()
    } else if !row.name.is_empty() {
        row.name.clone()
    } else {
        "service".to_string()
    }
}

fn evaluate_ps_output(text: &str) -> PsEvaluation {
    let rows = parse_ps_rows(text);
    if rows.is_empty() {
        return PsEvaluation::Waiting;
    }
    let mut saw_work = false;
    for row in rows {
        match container_status(&row) {
            ContainerStatus::Failed(message) => return PsEvaluation::Failed(message),
            ContainerStatus::Waiting => return PsEvaluation::Waiting,
            ContainerStatus::Ready => saw_work = true,
        }
    }
    if saw_work {
        PsEvaluation::Ready
    } else {
        PsEvaluation::Waiting
    }
}

fn parse_ps_rows(text: &str) -> Vec<PsRow> {
    let mut rows = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Ok(row) = serde_json::from_str::<PsRow>(line) {
            rows.push(row);
        }
    }
    rows
}

enum ContainerStatus {
    Ready,
    Waiting,
    Failed(String),
}

fn container_status(row: &PsRow) -> ContainerStatus {
    let state = row.state.to_ascii_lowercase();
    let health = row.health.to_ascii_lowercase();
    if health == "unhealthy" {
        return ContainerStatus::Failed(format!(
            "A container exited unhealthy (state: {}, health: {}).",
            row.state, row.health
        ));
    }
    if health == "starting" {
        return ContainerStatus::Waiting;
    }
    if state == "exited" {
        return if row.exit_code == 0 {
            ContainerStatus::Ready
        } else {
            ContainerStatus::Failed(format!("A container exited with code {}.", row.exit_code))
        };
    }
    if state == "running" {
        return if health.is_empty() || health == "healthy" {
            ContainerStatus::Ready
        } else {
            ContainerStatus::Waiting
        };
    }
    if state == "created" || state == "restarting" {
        return ContainerStatus::Waiting;
    }
    if state == "dead" || state == "removing" {
        return ContainerStatus::Failed(format!("A container is in state {}.", row.state));
    }
    ContainerStatus::Waiting
}

fn compose_args(context: &StackContext, command: &[&str]) -> Vec<String> {
    let compose_file = context.dir.join(&context.manifest.compose_file);
    let mut args = vec![
        "compose".to_string(),
        "--project-directory".to_string(),
        path_to_string(&context.dir),
        "-f".to_string(),
        path_to_string(&compose_file),
        "-p".to_string(),
        context.manifest.project_name.clone(),
    ];
    args.extend(command.iter().map(|part| (*part).to_string()));
    args
}

fn http_is_success(agent: &ureq::Agent, url: &str) -> bool {
    match agent.get(url).call() {
        Ok(response) => {
            let status = response.status();
            let mut reader = response.into_reader();
            let mut buf = [0_u8; 256];
            let _ = reader.read(&mut buf);
            (200..300).contains(&status)
        }
        Err(_) => false,
    }
}

fn path_to_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::prepare;
    use super::*;
    use crate::boot_progress::QuietBootReport;
    use crate::manifest;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;
    use std::sync::OnceLock;
    use std::thread;
    use std::time::Duration;

    fn temp_dir() -> PathBuf {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "compose-shell-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_stack(project: &Path, project_name: &str) {
        std::fs::write(
            project.join("shell.toml"),
            format!("project_name = \"{project_name}\"\ncompose_file = \"docker-compose.yml\"\n"),
        )
        .unwrap();
        std::fs::write(project.join("docker-compose.yml"), "services: {}\n").unwrap();
    }

    fn context(project: &Path, project_name: &str) -> StackContext {
        write_stack(project, project_name);
        let manifest = manifest::load(project).unwrap();
        build_context(project.to_path_buf(), manifest)
    }

    fn write_fake_docker(dir: &Path, behavior: &str) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let exe_name = if cfg!(windows) {
            "docker.exe"
        } else {
            "docker"
        };
        let program = dir.join(exe_name);
        std::fs::copy(fake_docker_template(), &program).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        std::fs::write(dir.join("behavior.txt"), behavior).unwrap();
        program
    }

    fn fake_docker_template() -> PathBuf {
        static TEMPLATE: OnceLock<PathBuf> = OnceLock::new();
        TEMPLATE
            .get_or_init(|| {
                let dir = std::env::temp_dir()
                    .join(format!("compose-shell-fake-docker-{}", std::process::id()));
                std::fs::create_dir_all(&dir).unwrap();
                let source = dir.join("main.rs");
                std::fs::write(&source, FAKE_DOCKER_SOURCE).unwrap();
                let program = dir.join(if cfg!(windows) {
                    "docker.exe"
                } else {
                    "docker"
                });
                let status = Command::new("rustc")
                    .arg("-O")
                    .arg("-o")
                    .arg(&program)
                    .arg(&source)
                    .status()
                    .expect("rustc");
                assert!(status.success(), "failed to compile fake docker");
                program
            })
            .clone()
    }

    const FAKE_DOCKER_SOURCE: &str = r#"
use std::io::Write;

fn main() {
    let exe = std::env::current_exe().unwrap();
    let dir = exe.parent().unwrap();
    let behavior = std::fs::read_to_string(dir.join("behavior.txt")).unwrap_or_default();
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Some(log) = behavior.lines().find_map(|line| line.strip_prefix("log=")) {
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log)
            .unwrap();
        let _ = writeln!(file, "{}", args.join(" "));
        if let Ok(provider) = std::env::var("PODMAN_COMPOSE_PROVIDER") {
            let _ = writeln!(file, "provider={provider}");
        }
        if let Ok(warning) = std::env::var("PODMAN_COMPOSE_WARNING_LOGS") {
            let _ = writeln!(file, "warning_logs={warning}");
        }
    }
    let head = args.first().map(String::as_str);
    if head == Some("--status") {
        if behavior.lines().any(|line| line == "wsl-fail") {
            eprintln!("WSL is not installed");
            std::process::exit(1);
        }
        std::process::exit(0);
    }
    if head == Some("machine") {
        let action = args.get(1).map(String::as_str);
        if action == Some("list") {
            let body = if behavior.lines().any(|line| line == "list=stopped") {
                "[{\"Name\":\"podman-machine-default\",\"Default\":true,\"Running\":false}]"
            } else if behavior.lines().any(|line| line == "list=running") {
                "[{\"Name\":\"podman-machine-default\",\"Default\":true,\"Running\":true}]"
            } else {
                "[]"
            };
            println!("{body}");
            std::process::exit(0);
        }
        if action == Some("init") && behavior.lines().any(|line| line == "init-fail") {
            eprintln!("machine init failed");
            std::process::exit(1);
        }
        if action == Some("start") && behavior.lines().any(|line| line == "start-fail") {
            eprintln!("machine start failed");
            std::process::exit(1);
        }
        std::process::exit(0);
    }
    let info = head == Some("info");
    if info && behavior.lines().any(|line| line == "info-fail") {
        eprintln!("Docker daemon is not running");
        std::process::exit(1);
    }
    if args.iter().any(|part| part == "ps") {
        println!("{{\"State\":\"running\",\"Health\":\"healthy\",\"ExitCode\":0}}");
        std::process::exit(0);
    }
    if !info && behavior.lines().any(|line| line == "up-fail") {
        eprintln!("pull access denied for ghcr.io/company/example-ui:1.0");
        std::process::exit(1);
    }
}
"#;

    #[test]
    fn compose_args_target_the_manifest_project() {
        let root = temp_dir();
        let project = root.join("app");
        std::fs::create_dir_all(&project).unwrap();
        let ctx = context(&project, "my-stack");
        let up = up_args(&ctx);
        assert_eq!(up[0], "compose");
        assert_eq!(up[5], "-p");
        assert_eq!(up[6], "my-stack");
        assert!(up.iter().any(|arg| arg == "--remove-orphans"));
        let down = down_args(&ctx);
        assert_eq!(down.last().map(String::as_str), Some("down"));
        assert!(!down.iter().any(|arg| arg == "-v" || arg == "--volumes"));
        std::fs::remove_dir_all(root).ok();
    }

    fn quiet_reporter() -> Arc<dyn BootReport> {
        Arc::new(QuietBootReport)
    }

    fn podman_engine(root: &Path, podman_behavior: &str, wsl_behavior: &str) -> (Engine, PathBuf) {
        let podman = write_fake_docker(&root.join("podman"), podman_behavior);
        let wsl = write_fake_docker(&root.join("wsl"), wsl_behavior);
        let provider = root.join("docker-compose.exe");
        std::fs::write(&provider, b"provider").unwrap();
        let engine = Engine::podman_for_test(
            podman.to_string_lossy().to_string(),
            provider.to_string_lossy().to_string(),
            Some(wsl.to_string_lossy().to_string()),
        );
        (engine, provider)
    }

    #[test]
    fn missing_docker_program_is_reported() {
        let docker = Engine::docker_with_program("docker-binary-that-does-not-exist");
        let err = prepare(&docker, quiet_reporter().as_ref()).unwrap_err();
        assert!(err.to_string().contains("Could not run"));
    }

    #[test]
    fn daemon_failure_is_reported() {
        let root = temp_dir();
        let program = write_fake_docker(&root, "info-fail\n");
        let docker = Engine::docker_with_program(program.to_string_lossy().to_string());
        let err = prepare(&docker, quiet_reporter().as_ref()).unwrap_err();
        assert!(err.to_string().contains("Docker is not running"));
        assert!(err.to_string().contains("daemon is not running"));
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn missing_images_stop_before_the_window_and_run_down() {
        let root = temp_dir();
        let log = root.join("args.log");
        let program = write_fake_docker(&root, &format!("up-fail\nlog={}\n", path_to_string(&log)));
        let project = root.join("project");
        std::fs::create_dir_all(&project).unwrap();
        let ctx = context(&project, "compose-to-desktop-copier");
        let docker = Engine::docker_with_program(program.to_string_lossy().to_string());
        let err = start_and_wait(&docker, &ctx, quiet_reporter()).unwrap_err();
        let message = err.to_string();
        assert!(message.contains("could not start the stack"));
        assert!(message.contains("example-ui"));
        let recorded = std::fs::read_to_string(&log).unwrap();
        assert!(recorded.contains("up"));
        assert!(recorded.contains("down"));
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn ready_ui_returns_the_window_origin() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        thread::spawn(move || {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
            let mut buf = [0_u8; 2048];
            let _ = stream.read(&mut buf);
            let body = b"ok";
            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(header.as_bytes());
            let _ = stream.write_all(body);
        });

        let previous_health = std::env::var("UI_HEALTH_URL").ok();
        let previous_wait = std::env::var("UI_WAIT_SECS").ok();
        std::env::remove_var("UI_HEALTH_URL");
        std::env::remove_var("UI_WAIT_SECS");

        let root = temp_dir();
        let program = write_fake_docker(&root, "");
        let project = root.join("project");
        std::fs::create_dir_all(&project).unwrap();
        write_stack(&project, "compose-to-desktop-copier");
        std::fs::write(
            project.join(".env"),
            format!("UI_HEALTH_URL=http://127.0.0.1:{port}/\nUI_WAIT_SECS=5\n"),
        )
        .unwrap();
        let manifest = manifest::load(&project).unwrap();
        let ctx = build_context(project.clone(), manifest);
        let docker = Engine::docker_with_program(program.to_string_lossy().to_string());
        let settings = start_and_wait(&docker, &ctx, quiet_reporter()).unwrap();
        assert_eq!(settings.window_url, format!("http://127.0.0.1:{port}"));
        std::fs::remove_dir_all(root).ok();
        restore_env("UI_HEALTH_URL", previous_health);
        restore_env("UI_WAIT_SECS", previous_wait);
    }

    fn restore_env(key: &str, value: Option<String>) {
        match value {
            Some(value) => std::env::set_var(key, value),
            None => std::env::remove_var(key),
        }
    }

    #[test]
    fn repo_stack_manifest_loads() {
        let root = crate::stack::repo_root();
        let manifest = manifest::load(&root).unwrap();
        assert!(!manifest.product_name.is_empty());
        assert!(!manifest.project_name.is_empty());
        assert_eq!(manifest.compose_file, "docker-compose.yml");
        assert!(manifest.health_url.starts_with("http://127.0.0.1:"));
    }

    #[test]
    fn summarize_ps_reports_service_counts() {
        let text = concat!(
            "{\"Name\":\"app-db-1\",\"Service\":\"db\",\"State\":\"running\",\"Health\":\"healthy\",\"ExitCode\":0}\n",
            "{\"Name\":\"app-ui-1\",\"Service\":\"ui\",\"State\":\"running\",\"Health\":\"starting\",\"ExitCode\":0}\n",
        );
        let summary = summarize_ps_output(text);
        assert!(summary.contains("1 of 2 services healthy"));
        assert!(summary.contains("ui: starting"));
    }

    #[test]
    fn ps_rows_wait_on_starting_and_fail_on_unhealthy() {
        let waiting = evaluate_ps_output("{\"State\":\"running\",\"Health\":\"starting\"}");
        assert!(matches!(waiting, PsEvaluation::Waiting));
        let failed = evaluate_ps_output("{\"State\":\"running\",\"Health\":\"unhealthy\"}");
        assert!(matches!(failed, PsEvaluation::Failed(_)));
        let ready = evaluate_ps_output("{\"State\":\"running\",\"Health\":\"healthy\"}");
        assert!(matches!(ready, PsEvaluation::Ready));
    }

    #[test]
    fn missing_wsl_stops_before_compose() {
        let root = temp_dir();
        let log = root.join("args.log");
        let (engine, _) = podman_engine(
            &root,
            &format!("list=running\nlog={}\n", path_to_string(&log)),
            "wsl-fail\n",
        );
        let project = root.join("project");
        std::fs::create_dir_all(&project).unwrap();
        let ctx = context(&project, "compose-to-desktop-copier");
        let err = start_and_wait(&engine, &ctx, quiet_reporter()).unwrap_err();
        assert!(err.to_string().contains("wsl --install --no-distribution"));
        assert!(!log.is_file());
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn missing_machine_runs_init_and_records_the_compose_provider() {
        let root = temp_dir();
        let log = root.join("args.log");
        let (engine, provider) = podman_engine(
            &root,
            &format!("list=none\nlog={}\n", path_to_string(&log)),
            "",
        );
        let project = root.join("project");
        std::fs::create_dir_all(&project).unwrap();
        write_stack(&project, "compose-to-desktop-copier");
        std::fs::write(
            project.join(".env"),
            "UI_HEALTH_URL=http://127.0.0.1:9/\nUI_WAIT_SECS=1\n",
        )
        .unwrap();
        let previous_health = std::env::var("UI_HEALTH_URL").ok();
        let previous_wait = std::env::var("UI_WAIT_SECS").ok();
        std::env::remove_var("UI_HEALTH_URL");
        std::env::set_var("UI_WAIT_SECS", "1");
        let manifest = manifest::load(&project).unwrap();
        let ctx = build_context(project, manifest);
        let err = start_and_wait(&engine, &ctx, quiet_reporter()).unwrap_err();
        restore_env("UI_HEALTH_URL", previous_health);
        restore_env("UI_WAIT_SECS", previous_wait);
        assert!(err.to_string().contains("did not become ready"));
        let recorded = std::fs::read_to_string(&log).unwrap();
        assert!(recorded.contains("machine init --now"));
        assert!(!recorded.contains("machine start"));
        assert!(recorded.contains("compose"));
        assert!(recorded.contains("up"));
        assert!(recorded.contains("down"));
        assert!(!recorded.contains(" -v") && !recorded.contains("--volumes"));
        assert!(recorded.contains(&format!("provider={}", path_to_string(&provider))));
        assert!(recorded.contains("warning_logs=false"));
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn stopped_machine_is_started() {
        let root = temp_dir();
        let log = root.join("args.log");
        let (engine, _) = podman_engine(
            &root,
            &format!("list=stopped\nlog={}\n", path_to_string(&log)),
            "",
        );
        let project = root.join("project");
        std::fs::create_dir_all(&project).unwrap();
        write_stack(&project, "compose-to-desktop-copier");
        std::fs::write(
            project.join(".env"),
            "UI_HEALTH_URL=http://127.0.0.1:9/\nUI_WAIT_SECS=1\n",
        )
        .unwrap();
        let previous_health = std::env::var("UI_HEALTH_URL").ok();
        let previous_wait = std::env::var("UI_WAIT_SECS").ok();
        std::env::remove_var("UI_HEALTH_URL");
        std::env::set_var("UI_WAIT_SECS", "1");
        let manifest = manifest::load(&project).unwrap();
        let ctx = build_context(project, manifest);
        let _ = start_and_wait(&engine, &ctx, quiet_reporter()).unwrap_err();
        restore_env("UI_HEALTH_URL", previous_health);
        restore_env("UI_WAIT_SECS", previous_wait);
        let recorded = std::fs::read_to_string(&log).unwrap();
        assert!(recorded.contains("machine start"));
        assert!(!recorded.contains("machine init"));
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn machine_init_failure_does_not_start_the_stack() {
        let root = temp_dir();
        let log = root.join("args.log");
        let (engine, _) = podman_engine(
            &root,
            &format!("list=none\ninit-fail\nlog={}\n", path_to_string(&log)),
            "",
        );
        let project = root.join("project");
        std::fs::create_dir_all(&project).unwrap();
        let ctx = context(&project, "compose-to-desktop-copier");
        let err = start_and_wait(&engine, &ctx, quiet_reporter()).unwrap_err();
        assert!(err.to_string().contains("could not create its machine"));
        let recorded = std::fs::read_to_string(&log).unwrap();
        assert!(!recorded.lines().any(|line| line.starts_with("compose ")));
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn machine_start_failure_does_not_start_the_stack() {
        let root = temp_dir();
        let log = root.join("args.log");
        let (engine, _) = podman_engine(
            &root,
            &format!("list=stopped\nstart-fail\nlog={}\n", path_to_string(&log)),
            "",
        );
        let project = root.join("project");
        std::fs::create_dir_all(&project).unwrap();
        let ctx = context(&project, "compose-to-desktop-copier");
        let err = start_and_wait(&engine, &ctx, quiet_reporter()).unwrap_err();
        assert!(err.to_string().contains("could not start its machine"));
        let recorded = std::fs::read_to_string(&log).unwrap();
        assert!(!recorded.lines().any(|line| line.starts_with("compose ")));
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn podman_info_failure_does_not_start_the_stack() {
        let root = temp_dir();
        let log = root.join("args.log");
        let (engine, _) = podman_engine(
            &root,
            &format!("list=running\ninfo-fail\nlog={}\n", path_to_string(&log)),
            "",
        );
        let project = root.join("project");
        std::fs::create_dir_all(&project).unwrap();
        let ctx = context(&project, "compose-to-desktop-copier");
        let err = start_and_wait(&engine, &ctx, quiet_reporter()).unwrap_err();
        assert!(
            !err.to_string().trim().is_empty(),
            "Podman info failure should return an error"
        );
        let recorded = std::fs::read_to_string(&log).unwrap();
        assert!(recorded.lines().any(|line| line.starts_with("info")));
        assert!(!recorded.lines().any(|line| line.starts_with("compose ")));
        std::fs::remove_dir_all(root).ok();
    }
}
