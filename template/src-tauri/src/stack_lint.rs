use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::manifest::{self, ShellManifest};

#[derive(Debug, PartialEq, Eq)]
pub struct LintError {
    pub stack: PathBuf,
    pub message: String,
}

impl std::fmt::Display for LintError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.stack.display(), self.message)
    }
}

#[cfg_attr(not(test), allow(dead_code))]
pub fn lint_repo(repo_root: &Path) -> Result<(), Vec<LintError>> {
    let mut errors = Vec::new();
    lint_stack_dir(repo_root, &mut errors);
    let examples = repo_root.join("examples");
    if examples.is_dir() {
        for entry in std::fs::read_dir(&examples)
            .unwrap_or_else(|_| panic!("could not read {}", examples.display()))
        {
            let entry = entry.unwrap();
            if entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false) {
                lint_stack_dir(&entry.path(), &mut errors);
            }
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

fn lint_stack_dir(stack_dir: &Path, errors: &mut Vec<LintError>) {
    if !stack_dir.join(manifest::MANIFEST_FILE).is_file() {
        return;
    }
    let manifest = match manifest::load(stack_dir) {
        Ok(manifest) => manifest,
        Err(err) => {
            errors.push(LintError {
                stack: stack_dir.to_path_buf(),
                message: err.to_string(),
            });
            return;
        }
    };
    let compose_path = stack_dir.join(&manifest.compose_file);
    let compose_text = match std::fs::read_to_string(&compose_path) {
        Ok(text) => text,
        Err(err) => {
            errors.push(LintError {
                stack: stack_dir.to_path_buf(),
                message: format!("could not read {}: {err}", manifest.compose_file),
            });
            return;
        }
    };
    let compose = match serde_yaml::from_str::<serde_yaml::Value>(&compose_text) {
        Ok(value) => value,
        Err(err) => {
            errors.push(LintError {
                stack: stack_dir.to_path_buf(),
                message: format!("{} is invalid YAML: {err}", manifest.compose_file),
            });
            return;
        }
    };
    lint_services(stack_dir, &compose, errors);
    lint_secret_keys(stack_dir, &manifest, errors);
}

fn lint_services(stack_dir: &Path, compose: &serde_yaml::Value, errors: &mut Vec<LintError>) {
    let services = compose
        .get("services")
        .and_then(|value| value.as_mapping())
        .cloned()
        .unwrap_or_default();
    for (name, service) in services {
        let service_name = name.as_str().unwrap_or("<service>");
        let mapping = service.as_mapping().cloned().unwrap_or_default();
        lint_ports(stack_dir, service_name, &mapping, errors);
        lint_environment(stack_dir, service_name, &mapping, errors);
        if is_database_service(&mapping) {
            lint_database_service(stack_dir, service_name, &mapping, errors);
        }
    }
}

fn lint_ports(
    stack_dir: &Path,
    service_name: &str,
    service: &serde_yaml::Mapping,
    errors: &mut Vec<LintError>,
) {
    let ports = service.get(serde_yaml::Value::from("ports"));
    if let Some(ports) = ports.and_then(|value| value.as_sequence()) {
        for port in ports {
            let text = port_to_string(port);
            if !text.is_empty() && !port_binds_loopback(&text) {
                errors.push(LintError {
                    stack: stack_dir.to_path_buf(),
                    message: format!(
                        "service '{service_name}' publishes {text} without a 127.0.0.1 bind address"
                    ),
                });
            }
        }
    }
}

fn lint_database_service(
    stack_dir: &Path,
    service_name: &str,
    service: &serde_yaml::Mapping,
    errors: &mut Vec<LintError>,
) {
    if service.contains_key(serde_yaml::Value::from("ports")) {
        errors.push(LintError {
            stack: stack_dir.to_path_buf(),
            message: format!("database service '{service_name}' must not publish host ports"),
        });
    }
    if !service_has_named_volume(service) {
        errors.push(LintError {
            stack: stack_dir.to_path_buf(),
            message: format!(
                "database service '{service_name}' must declare at least one named volume"
            ),
        });
    }
}

fn lint_environment(
    stack_dir: &Path,
    service_name: &str,
    service: &serde_yaml::Mapping,
    errors: &mut Vec<LintError>,
) {
    let environment = service.get(serde_yaml::Value::from("environment"));
    if let Some(environment) = environment {
        if let Some(mapping) = environment.as_mapping() {
            for (key, value) in mapping {
                let key = yaml_string(key);
                if is_sensitive_key(&key) {
                    lint_sensitive_value(stack_dir, service_name, &key, value, errors);
                }
            }
        } else if let Some(sequence) = environment.as_sequence() {
            for entry in sequence {
                let text = yaml_string(entry);
                if let Some((key, value)) = text.split_once('=') {
                    if is_sensitive_key(key) {
                        lint_sensitive_value(
                            stack_dir,
                            service_name,
                            key,
                            &serde_yaml::Value::String(value.to_string()),
                            errors,
                        );
                    }
                }
            }
        }
    }
}

fn lint_sensitive_value(
    stack_dir: &Path,
    service_name: &str,
    key: &str,
    value: &serde_yaml::Value,
    errors: &mut Vec<LintError>,
) {
    let text = yaml_string(value);
    if !is_env_substitution(&text) {
        errors.push(LintError {
            stack: stack_dir.to_path_buf(),
            message: format!(
                "service '{service_name}' sets sensitive key '{key}' to a literal value; use ${{VAR}} instead"
            ),
        });
        return;
    }
    if text.contains(":-") || text.contains(":?") {
        errors.push(LintError {
            stack: stack_dir.to_path_buf(),
            message: format!(
                "service '{service_name}' sensitive key '{key}' must not use a default in the substitution"
            ),
        });
    }
}

fn lint_secret_keys(stack_dir: &Path, manifest: &ShellManifest, errors: &mut Vec<LintError>) {
    let example_path = stack_dir.join(".env.example");
    let example_text = std::fs::read_to_string(&example_path).unwrap_or_default();
    let example_vars = parse_env_example(&example_text);
    for key in &manifest.secret_keys {
        match example_vars.get(key) {
            None => errors.push(LintError {
                stack: stack_dir.to_path_buf(),
                message: format!("secret key '{key}' is missing from .env.example"),
            }),
            Some(value) if !value.is_empty() => errors.push(LintError {
                stack: stack_dir.to_path_buf(),
                message: format!("secret key '{key}' must be empty in .env.example"),
            }),
            _ => {}
        }
    }
}

fn parse_env_example(text: &str) -> HashMap<String, String> {
    let mut vars = HashMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);
        if let Some((key, value)) = line.split_once('=') {
            vars.insert(key.trim().to_string(), value.trim().to_string());
        }
    }
    vars
}

fn is_database_service(service: &serde_yaml::Mapping) -> bool {
    if service
        .get(serde_yaml::Value::from("labels"))
        .and_then(|labels| labels.as_mapping())
        .and_then(|labels| labels.get(serde_yaml::Value::from("shell.role")))
        .map(yaml_string)
        .as_deref()
        == Some("database")
    {
        return true;
    }
    let image = service
        .get(serde_yaml::Value::from("image"))
        .map(yaml_string)
        .unwrap_or_default()
        .to_ascii_lowercase();
    [
        "postgres", "neo4j", "mysql", "mariadb", "mongo", "redis", "valkey",
    ]
    .iter()
    .any(|name| image.contains(name))
}

fn service_has_named_volume(service: &serde_yaml::Mapping) -> bool {
    let volumes = service
        .get(serde_yaml::Value::from("volumes"))
        .and_then(|value| value.as_sequence());
    if let Some(volumes) = volumes {
        for volume in volumes {
            let text = yaml_string(volume);
            if text.contains(':') {
                let source = text.split(':').next().unwrap_or("").trim();
                if !source.is_empty() && !source.starts_with('/') && !source.starts_with('.') {
                    return true;
                }
            }
        }
    }
    false
}

fn port_to_string(port: &serde_yaml::Value) -> String {
    match port {
        serde_yaml::Value::String(text) => text.clone(),
        serde_yaml::Value::Number(number) => number.to_string(),
        serde_yaml::Value::Mapping(mapping) => mapping
            .get(serde_yaml::Value::from("published"))
            .map(yaml_string)
            .unwrap_or_default(),
        _ => String::new(),
    }
}

fn port_binds_loopback(text: &str) -> bool {
    if text.starts_with("127.0.0.1:") {
        return true;
    }
    if text.contains(':') {
        return false;
    }
    false
}

fn is_sensitive_key(key: &str) -> bool {
    let upper = key.to_ascii_uppercase();
    upper.contains("PASSWORD")
        || upper.contains("SECRET")
        || upper.contains("AUTH")
        || upper.contains("TOKEN")
}

fn is_env_substitution(text: &str) -> bool {
    if text.starts_with("${") && text.ends_with('}') {
        return true;
    }
    text.contains("${") && text.contains('}')
}

fn yaml_string(value: &serde_yaml::Value) -> String {
    match value {
        serde_yaml::Value::String(text) => text.clone(),
        serde_yaml::Value::Number(number) => number.to_string(),
        serde_yaml::Value::Bool(value) => value.to_string(),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stack;

    #[test]
    fn repo_stacks_pass_lint() {
        let root = stack::repo_root();
        lint_repo(&root).unwrap_or_else(|errors| {
            panic!("stack lint failed:\n{}", format_errors(&errors));
        });
    }

    fn format_errors(errors: &[LintError]) -> String {
        errors
            .iter()
            .map(|error| error.to_string())
            .collect::<Vec<_>>()
            .join("\n")
    }
}
