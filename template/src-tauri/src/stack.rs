use std::fs;
use std::path::{Path, PathBuf};

use crate::compose::ComposeError;
use crate::manifest::{self, ShellManifest};

const ENV_EXAMPLE: &str = ".env.example";
const ENV_FILE: &str = ".env";
const STACK_SUBDIR: &str = "stack";

pub fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..")
}

fn locate_seed_dir(resource_dir: Option<&Path>) -> Option<PathBuf> {
    if let Some(dir) = resource_dir {
        if dir.join(manifest::MANIFEST_FILE).is_file() {
            return Some(dir.to_path_buf());
        }
    }
    let bundled = repo_root();
    if bundled.join(manifest::MANIFEST_FILE).is_file() {
        Some(bundled)
    } else {
        None
    }
}

pub fn resolve_stack_dir(
    debug: bool,
    resource_dir: Option<&Path>,
    app_data_dir: Option<&Path>,
) -> Result<PathBuf, ComposeError> {
    if debug {
        if let Ok(stack_dir) = std::env::var("STACK_DIR") {
            let stack_dir = stack_dir.trim();
            if !stack_dir.is_empty() {
                return resolve_env_stack_dir(stack_dir);
            }
        }
        let root = repo_root();
        return validate_stack_dir(&root);
    }
    let writable = app_data_dir
        .map(|dir| dir.join(STACK_SUBDIR))
        .ok_or_else(|| ComposeError::new("Could not resolve the application data directory."))?;
    let seed = locate_seed_dir(resource_dir).ok_or_else(|| {
        ComposeError::new("Could not find the bundled stack files. Reinstall the application.")
    })?;
    prepare_writable_stack(&writable, &seed)?;
    validate_stack_dir(&writable)
}

pub fn prepare_stack_in_place(
    stack_dir: &Path,
    manifest: &ShellManifest,
) -> Result<(), ComposeError> {
    ensure_env_file(stack_dir)?;
    ensure_secrets(stack_dir, manifest)?;
    Ok(())
}

fn prepare_writable_stack(writable: &Path, seed: &Path) -> Result<(), ComposeError> {
    fs::create_dir_all(writable).map_err(|err| {
        ComposeError::new(format!(
            "Could not create the stack directory at {}: {err}",
            writable.display()
        ))
    })?;
    let manifest = manifest::load(seed)?;
    copy_if_changed(
        &seed.join(manifest::MANIFEST_FILE),
        &writable.join(manifest::MANIFEST_FILE),
    )?;
    copy_if_changed(
        &seed.join(&manifest.compose_file),
        &writable.join(&manifest.compose_file),
    )?;
    copy_if_changed(&seed.join(ENV_EXAMPLE), &writable.join(ENV_EXAMPLE))?;
    ensure_env_file(writable)?;
    ensure_secrets(writable, &manifest)?;
    Ok(())
}

fn resolve_env_stack_dir(stack_dir: &str) -> Result<PathBuf, ComposeError> {
    for candidate in stack_dir_candidates(stack_dir) {
        if candidate.join(manifest::MANIFEST_FILE).is_file() {
            return validate_stack_dir(&candidate);
        }
    }
    Err(ComposeError::new(format!(
        "Could not find {} in {}. Set STACK_DIR to an absolute path or a path relative to the repository root.",
        manifest::MANIFEST_FILE,
        stack_dir
    )))
}

fn stack_dir_candidates(stack_dir: &str) -> Vec<PathBuf> {
    let raw = PathBuf::from(stack_dir);
    let mut candidates = Vec::new();
    let mut push = |path: PathBuf| {
        if !candidates.iter().any(|existing| existing == &path) {
            candidates.push(path);
        }
    };
    push(raw.clone());
    if raw.is_relative() {
        if let Ok(cwd) = std::env::current_dir() {
            push(cwd.join(&raw));
        }
        push(repo_root().join(&raw));
    }
    candidates
}

fn validate_stack_dir(path: &Path) -> Result<PathBuf, ComposeError> {
    if !path.join(manifest::MANIFEST_FILE).is_file() {
        return Err(ComposeError::new(format!(
            "Could not find {} in {}.",
            manifest::MANIFEST_FILE,
            path.display()
        )));
    }
    let manifest = manifest::load(path)?;
    if !path.join(&manifest.compose_file).is_file() {
        return Err(ComposeError::new(format!(
            "Could not find {} in {}.",
            manifest.compose_file,
            path.display()
        )));
    }
    Ok(path.to_path_buf())
}

fn ensure_env_file(stack_dir: &Path) -> Result<(), ComposeError> {
    let env_path = stack_dir.join(ENV_FILE);
    if env_path.is_file() {
        return restrict_env_file(&env_path);
    }
    let example = stack_dir.join(ENV_EXAMPLE);
    if example.is_file() {
        fs::copy(&example, &env_path).map_err(|err| {
            ComposeError::new(format!(
                "Could not create {} from {}: {err}",
                ENV_FILE, ENV_EXAMPLE
            ))
        })?;
        return restrict_env_file(&env_path);
    }
    fs::write(&env_path, "")
        .map_err(|err| ComposeError::new(format!("Could not create {}: {err}", ENV_FILE)))?;
    restrict_env_file(&env_path)
}

pub fn ensure_secrets(stack_dir: &Path, manifest: &ShellManifest) -> Result<(), ComposeError> {
    if manifest.secret_keys.is_empty() {
        return Ok(());
    }
    let env_path = stack_dir.join(ENV_FILE);
    let mut lines = read_env_lines(&env_path)?;
    let mut index_by_key: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
    for (index, line) in lines.iter().enumerate() {
        if let Some((key, _)) = parse_assignment(line) {
            index_by_key.insert(key, index);
        }
    }
    for key in &manifest.secret_keys {
        let current = index_by_key
            .get(key)
            .and_then(|index| lines.get(*index))
            .and_then(|line| parse_assignment(line))
            .map(|(_, value)| value)
            .unwrap_or_default();
        if !current.is_empty() {
            continue;
        }
        let token = random_hex(32)?;
        let assignment = format!("{key}={token}");
        if let Some(index) = index_by_key.get(key) {
            lines[*index] = assignment;
        } else {
            if !lines.is_empty() && !lines.last().map(String::is_empty).unwrap_or(true) {
                lines.push(String::new());
            }
            lines.push(assignment);
            index_by_key.insert(key.clone(), lines.len() - 1);
        }
    }
    write_env_lines(&env_path, &lines)?;
    Ok(())
}

fn read_env_lines(path: &Path) -> Result<Vec<String>, ComposeError> {
    if !path.is_file() {
        return Ok(Vec::new());
    }
    let text = fs::read_to_string(path)
        .map_err(|err| ComposeError::new(format!("Could not read {}: {err}", ENV_FILE)))?;
    Ok(text.lines().map(ToString::to_string).collect())
}

fn write_env_lines(path: &Path, lines: &[String]) -> Result<(), ComposeError> {
    let mut text = lines.join("\n");
    if !text.is_empty() {
        text.push('\n');
    }
    fs::write(path, text)
        .map_err(|err| ComposeError::new(format!("Could not write {}: {err}", ENV_FILE)))?;
    restrict_env_file(path)
}

fn restrict_env_file(path: &Path) -> Result<(), ComposeError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .map_err(|err| ComposeError::new(format!("Could not protect {}: {err}", ENV_FILE)))?;
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}

fn parse_assignment(line: &str) -> Option<(String, String)> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let line = line.strip_prefix("export ").unwrap_or(line);
    let Some((key, value)) = line.split_once('=') else {
        return None;
    };
    let key = key.trim();
    if key.is_empty() {
        return None;
    }
    Some((key.to_string(), unquote(value.trim())))
}

fn unquote(value: &str) -> String {
    let bytes = value.as_bytes();
    if bytes.len() >= 2
        && ((bytes[0] == b'"' && bytes[bytes.len() - 1] == b'"')
            || (bytes[0] == b'\'' && bytes[bytes.len() - 1] == b'\''))
    {
        value[1..value.len() - 1].to_string()
    } else {
        value.to_string()
    }
}

fn random_hex(byte_len: usize) -> Result<String, ComposeError> {
    let mut bytes = vec![0_u8; byte_len];
    getrandom::getrandom(&mut bytes)
        .map_err(|err| ComposeError::new(format!("Could not generate a secret value: {err}")))?;
    Ok(bytes.iter().map(|byte| format!("{:02x}", byte)).collect())
}

fn copy_if_changed(from: &Path, to: &Path) -> Result<(), ComposeError> {
    if !from.is_file() {
        return Err(ComposeError::new(format!(
            "Bundled file is missing: {}",
            from.display()
        )));
    }
    if to.is_file() {
        let from_meta = fs::metadata(from).map_err(io_error("read bundled file metadata"))?;
        let to_meta = fs::metadata(to).map_err(io_error("read stack file metadata"))?;
        if from_meta.len() == to_meta.len() && from_meta.modified().ok() == to_meta.modified().ok()
        {
            let from_bytes = fs::read(from).map_err(io_error("read bundled file"))?;
            let to_bytes = fs::read(to).map_err(io_error("read stack file"))?;
            if from_bytes == to_bytes {
                return Ok(());
            }
        }
    }
    if let Some(parent) = to.parent() {
        fs::create_dir_all(parent).map_err(io_error("create stack directory"))?;
    }
    fs::copy(from, to).map_err(|err| {
        ComposeError::new(format!(
            "Could not copy {} to {}: {err}",
            from.display(),
            to.display()
        ))
    })?;
    Ok(())
}

fn io_error(context: &'static str) -> impl Fn(std::io::Error) -> ComposeError {
    move |err| ComposeError::new(format!("Could not {context}: {err}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn temp_dir() -> PathBuf {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "compose-shell-stack-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    struct RemoveOnDrop(PathBuf);

    impl Drop for RemoveOnDrop {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).ok();
        }
    }

    #[test]
    fn env_stack_dir_resolves_relative_to_repo_root() {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let name = format!(
            ".stack-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        );
        let stack = repo_root().join(&name);
        let _cleanup = RemoveOnDrop(stack.clone());
        fs::create_dir_all(&stack).unwrap();
        fs::write(
            stack.join(manifest::MANIFEST_FILE),
            "product_name = \"Stack Test\"\nproject_name = \"stack-test\"\ncompose_file = \"docker-compose.yml\"\n",
        )
        .unwrap();
        fs::write(stack.join("docker-compose.yml"), "services: {}\n").unwrap();

        let previous = std::env::var("STACK_DIR").ok();
        let mut values = vec![name.clone()];
        if cfg!(windows) {
            values.push(format!(".\\{name}"));
        }
        for value in values {
            std::env::set_var("STACK_DIR", &value);
            let resolved = resolve_stack_dir(true, None, None).unwrap();
            assert_eq!(resolved, stack);
        }
        restore_env("STACK_DIR", previous);
    }

    #[test]
    fn release_ignores_stack_dir() {
        let root = temp_dir();
        let seed = root.join("seed");
        let other = root.join("other");
        let app_data = root.join("appdata");
        for dir in [&seed, &other] {
            fs::create_dir_all(dir).unwrap();
            fs::write(
                dir.join(manifest::MANIFEST_FILE),
                "product_name = \"Stack\"\nproject_name = \"stack\"\ncompose_file = \"docker-compose.yml\"\n",
            )
            .unwrap();
            fs::write(dir.join("docker-compose.yml"), "services: {}\n").unwrap();
            fs::write(dir.join(ENV_EXAMPLE), "").unwrap();
        }
        let previous = std::env::var("STACK_DIR").ok();
        std::env::set_var("STACK_DIR", &other);
        let resolved = resolve_stack_dir(false, Some(&seed), Some(&app_data)).unwrap();
        restore_env("STACK_DIR", previous);
        assert_eq!(resolved, app_data.join(STACK_SUBDIR));
        assert!(resolved.join(manifest::MANIFEST_FILE).is_file());
        fs::remove_dir_all(root).ok();
    }

    fn restore_env(key: &str, value: Option<String>) {
        match value {
            Some(value) => std::env::set_var(key, value),
            None => std::env::remove_var(key),
        }
    }

    #[test]
    fn secrets_fill_empty_keys_without_overwriting_set_values() {
        let dir = temp_dir();
        fs::write(
            dir.join(ENV_FILE),
            "POSTGRES_PASSWORD=keep-me\nAPI_TOKEN=\n",
        )
        .unwrap();
        let manifest = ShellManifest {
            secret_keys: vec!["POSTGRES_PASSWORD".to_string(), "API_TOKEN".to_string()],
            ..ShellManifest::default()
        };
        ensure_secrets(&dir, &manifest).unwrap();
        let text = fs::read_to_string(dir.join(ENV_FILE)).unwrap();
        assert!(text.contains("POSTGRES_PASSWORD=keep-me"));
        assert!(text.contains("API_TOKEN="));
        assert!(!text.contains("API_TOKEN=\n"));
        let token_line = text
            .lines()
            .find(|line| line.starts_with("API_TOKEN="))
            .unwrap();
        assert!(token_line.len() > "API_TOKEN=".len());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(dir.join(ENV_FILE))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600);
        }
        fs::remove_dir_all(dir).ok();
    }
}
