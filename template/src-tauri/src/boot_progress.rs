use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Serialize;
use tauri::{AppHandle, Manager};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BootPhase {
    Prepare,
    Engine,
    Pull,
    Health,
    Ui,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct BootUpdate {
    pub title: String,
    pub detail: String,
    pub percent: u8,
    #[serde(rename = "elapsedSecs")]
    pub elapsed_secs: u64,
}

pub trait BootReport: Send + Sync {
    fn set_wait_secs(&self, wait_secs: u64);
    fn report_phase(&self, phase: BootPhase, title: &str, detail: &str);
    fn report_detail(&self, detail: &str);
    fn note_compose_line(&self, line: &str);
    fn tick(&self);
}

struct BootReporterInner {
    wait_secs: u64,
    started: Instant,
    phase: BootPhase,
    phase_started: Instant,
    title: String,
    detail: String,
    percent: u8,
    last_elapsed_secs: u64,
}

pub struct BootReporter {
    app: AppHandle,
    inner: Mutex<BootReporterInner>,
}

impl BootReporter {
    pub fn new(app: AppHandle, wait_secs: u64) -> Self {
        let reporter = Self {
            app,
            inner: Mutex::new(BootReporterInner {
                wait_secs,
                started: Instant::now(),
                phase: BootPhase::Prepare,
                phase_started: Instant::now(),
                title: String::new(),
                detail: String::new(),
                percent: 0,
                last_elapsed_secs: 0,
            }),
        };
        reporter.report_phase(
            BootPhase::Prepare,
            "Preparing the stack",
            "Loading configuration",
        );
        reporter
    }

    fn push_update(&self, update: BootUpdate) {
        let json = serde_json::to_string(&update).unwrap_or_else(|_| "{}".to_string());
        let app = self.app.clone();
        let _ = app.clone().run_on_main_thread(move || {
            if let Some(window) = app.get_webview_window("boot") {
                let script = format!("window.updateBoot({json});");
                let _ = window.eval(script);
            }
        });
    }

    fn compute_percent(inner: &BootReporterInner) -> u8 {
        let (floor, ceiling, creep_secs) = phase_spec(inner.phase, inner.wait_secs);
        let span = ceiling.saturating_sub(floor);
        if span == 0 {
            return inner.percent.max(floor);
        }
        let elapsed = inner.phase_started.elapsed().as_secs_f64();
        let progress = if creep_secs <= 0.0 {
            0.0
        } else {
            (elapsed / creep_secs).min(1.0)
        };
        let creep_cap = ceiling.saturating_sub(1).max(floor);
        let computed = floor + ((span as f64) * progress * 0.95).round() as u8;
        inner.percent.max(floor).max(computed).min(creep_cap)
    }
}

pub fn spawn_progress_ticker(reporter: Arc<dyn BootReport>) -> Arc<AtomicBool> {
    let done = Arc::new(AtomicBool::new(false));
    let done_flag = done.clone();
    std::thread::spawn(move || {
        while !done_flag.load(Ordering::Relaxed) {
            reporter.tick();
            std::thread::sleep(Duration::from_millis(500));
        }
    });
    done
}

impl BootReport for BootReporter {
    fn set_wait_secs(&self, wait_secs: u64) {
        let mut inner = self.inner.lock().unwrap();
        inner.wait_secs = wait_secs;
    }

    fn report_phase(&self, phase: BootPhase, title: &str, detail: &str) {
        let update = {
            let mut inner = self.inner.lock().unwrap();
            let floor = phase_floor(phase);
            inner.phase = phase;
            inner.phase_started = Instant::now();
            inner.title = title.to_string();
            inner.detail = detail.to_string();
            inner.percent = inner.percent.max(floor);
            BootUpdate {
                title: inner.title.clone(),
                detail: inner.detail.clone(),
                percent: inner.percent,
                elapsed_secs: inner.started.elapsed().as_secs(),
            }
        };
        self.push_update(update);
    }

    fn report_detail(&self, detail: &str) {
        let update = {
            let mut inner = self.inner.lock().unwrap();
            inner.detail = detail.to_string();
            BootUpdate {
                title: inner.title.clone(),
                detail: inner.detail.clone(),
                percent: inner.percent,
                elapsed_secs: inner.started.elapsed().as_secs(),
            }
        };
        self.push_update(update);
    }

    fn note_compose_line(&self, line: &str) {
        if let Some(detail) = compose_line_detail(line) {
            self.report_detail(&detail);
        }
    }

    fn tick(&self) {
        let update = {
            let mut inner = self.inner.lock().unwrap();
            let percent = Self::compute_percent(&inner);
            let elapsed_secs = inner.started.elapsed().as_secs();
            let percent_changed = percent != inner.percent;
            let elapsed_changed = elapsed_secs != inner.last_elapsed_secs;
            if !percent_changed && !elapsed_changed {
                return;
            }
            if percent_changed {
                inner.percent = percent;
            }
            inner.last_elapsed_secs = elapsed_secs;
            BootUpdate {
                title: inner.title.clone(),
                detail: inner.detail.clone(),
                percent: inner.percent,
                elapsed_secs,
            }
        };
        self.push_update(update);
    }
}

#[cfg(test)]
pub struct QuietBootReport;

#[cfg(test)]
impl BootReport for QuietBootReport {
    fn set_wait_secs(&self, _wait_secs: u64) {}

    fn report_phase(&self, _phase: BootPhase, _title: &str, _detail: &str) {}

    fn report_detail(&self, _detail: &str) {}

    fn note_compose_line(&self, _line: &str) {}

    fn tick(&self) {}
}

pub fn phase_floor(phase: BootPhase) -> u8 {
    phase_spec(phase, 0).0
}

pub fn phase_spec(phase: BootPhase, wait_secs: u64) -> (u8, u8, f64) {
    match phase {
        BootPhase::Prepare => (0, 5, 5.0),
        BootPhase::Engine => (5, 20, 15.0),
        BootPhase::Pull => (20, 75, wait_secs as f64 * 0.6),
        BootPhase::Health => (75, 90, wait_secs as f64 * 0.25),
        BootPhase::Ui => (90, 100, wait_secs as f64 * 0.15),
    }
}

fn is_layer_hash_line(line: &str) -> bool {
    let first = line.split_whitespace().next().unwrap_or("");
    first.len() >= 12 && first.chars().all(|ch| ch.is_ascii_hexdigit())
}

pub fn compose_line_detail(line: &str) -> Option<String> {
    let trimmed = line.trim();
    if trimmed.is_empty() || is_layer_hash_line(trimmed) {
        return None;
    }
    let lower = trimmed.to_ascii_lowercase();
    if lower.contains("pulling")
        || lower.contains("pull complete")
        || lower.starts_with("creating")
        || lower.contains(" created")
        || lower.contains(" started")
        || lower.contains("container")
        || lower.contains("network")
    {
        Some(redact_sensitive(trimmed))
    } else {
        None
    }
}

pub fn redact_sensitive(text: &str) -> String {
    redact_assignments(&redact_url_userinfo(text))
}

fn redact_url_userinfo(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(index) = rest.find("://") {
        result.push_str(&rest[..index + 3]);
        rest = &rest[index + 3..];
        let authority_end = rest
            .find(['/', '?', '#', ' ', '\t', '\r', '\n'])
            .unwrap_or(rest.len());
        let authority = &rest[..authority_end];
        if let Some(at) = authority.rfind('@') {
            if !authority[..at].is_empty() {
                result.push_str("***");
                result.push_str(&authority[at..]);
                rest = &rest[authority_end..];
                continue;
            }
        }
        result.push_str(authority);
        rest = &rest[authority_end..];
    }
    result.push_str(rest);
    result
}

fn redact_assignments(text: &str) -> String {
    const KEYS: [&str; 4] = ["password", "secret", "token", "authorization"];
    let lower = text.to_ascii_lowercase();
    let mut ranges = Vec::new();
    let mut index = 0;
    while index < lower.len() {
        let key_len = KEYS
            .iter()
            .find_map(|key| lower[index..].starts_with(key).then_some(key.len()));
        let Some(key_len) = key_len else {
            index += lower[index..]
                .chars()
                .next()
                .map(|ch| ch.len_utf8())
                .unwrap_or(1);
            continue;
        };
        let mut cursor = index + key_len;
        while cursor < text.len() && text.as_bytes()[cursor].is_ascii_whitespace() {
            cursor += 1;
        }
        if cursor >= text.len()
            || (text.as_bytes()[cursor] != b'=' && text.as_bytes()[cursor] != b':')
        {
            index += key_len;
            continue;
        }
        cursor += 1;
        while cursor < text.len() && text.as_bytes()[cursor].is_ascii_whitespace() {
            cursor += 1;
        }
        let value_start = cursor;
        if cursor < text.len()
            && (text.as_bytes()[cursor] == b'"' || text.as_bytes()[cursor] == b'\'')
        {
            let quote = text.as_bytes()[cursor];
            cursor += 1;
            if let Some(end) = text[cursor..].bytes().position(|byte| byte == quote) {
                cursor += end + 1;
            } else {
                cursor = text.len();
            }
        } else if lower[cursor..].starts_with("bearer")
            && text
                .as_bytes()
                .get(cursor + "bearer".len())
                .is_some_and(|byte| byte.is_ascii_whitespace())
        {
            cursor += "bearer".len();
            while cursor < text.len() && text.as_bytes()[cursor].is_ascii_whitespace() {
                cursor += 1;
            }
            while cursor < text.len() && !text.as_bytes()[cursor].is_ascii_whitespace() {
                cursor += 1;
            }
        } else {
            while cursor < text.len() && !text.as_bytes()[cursor].is_ascii_whitespace() {
                cursor += 1;
            }
        }
        if cursor > value_start {
            ranges.push((value_start, cursor));
        }
        index = cursor.max(index + key_len);
    }
    apply_masks(text, ranges)
}

fn apply_masks(text: &str, mut ranges: Vec<(usize, usize)>) -> String {
    if ranges.is_empty() {
        return text.to_string();
    }
    ranges.sort_by_key(|range| range.0);
    let mut merged = Vec::new();
    for range in ranges {
        if let Some((_, end)) = merged.last_mut() {
            if range.0 <= *end {
                *end = (*end).max(range.1);
                continue;
            }
        }
        merged.push(range);
    }
    let mut output = String::with_capacity(text.len());
    let mut cursor = 0;
    for (start, end) in merged {
        output.push_str(&text[cursor..start]);
        output.push_str("***");
        cursor = end;
    }
    output.push_str(&text[cursor..]);
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phase_transitions_never_lower_percent() {
        let mut percent = 0_u8;
        for phase in [
            BootPhase::Prepare,
            BootPhase::Engine,
            BootPhase::Pull,
            BootPhase::Health,
            BootPhase::Ui,
        ] {
            let floor = phase_floor(phase);
            percent = percent.max(floor);
        }
        assert_eq!(percent, 90);
    }

    #[test]
    fn creep_stays_below_phase_ceiling() {
        let (floor, ceiling, creep_secs) = phase_spec(BootPhase::Pull, 120);
        assert_eq!(floor, 20);
        assert_eq!(ceiling, 75);
        assert_eq!(creep_secs, 72.0);
        let creep_cap = ceiling.saturating_sub(1).max(floor);
        assert!(creep_cap < ceiling);
    }

    #[test]
    fn boot_update_serializes_elapsed_secs_as_camel_case() {
        let update = BootUpdate {
            title: "Pulling images".to_string(),
            detail: "ui Pulling".to_string(),
            percent: 25,
            elapsed_secs: 84,
        };
        let json = serde_json::to_string(&update).unwrap();
        assert!(json.contains("\"elapsedSecs\":84"));
        assert!(!json.contains("elapsed_secs"));
    }

    #[test]
    fn compose_lines_pick_useful_details() {
        assert_eq!(
            compose_line_detail(" ui Pulling"),
            Some("ui Pulling".to_string())
        );
        assert_eq!(
            compose_line_detail(" Container superset-desktop-ui-1 Started"),
            Some("Container superset-desktop-ui-1 Started".to_string())
        );
        assert!(compose_line_detail(" 5f70bf18a086 Pull complete").is_none());
    }

    #[test]
    fn redact_masks_secrets_and_leaves_pull_errors() {
        assert_eq!(
            redact_sensitive("pull access denied for ghcr.io/company/example-ui:1.0"),
            "pull access denied for ghcr.io/company/example-ui:1.0"
        );
        assert_eq!(
            redact_sensitive("POSTGRES_PASSWORD=hunter2"),
            "POSTGRES_PASSWORD=***"
        );
        assert_eq!(redact_sensitive("token = \"abc def\""), "token = ***");
        assert_eq!(
            redact_sensitive("Authorization: Bearer abc.def"),
            "Authorization: ***"
        );
        assert_eq!(
            redact_sensitive("https://user:secret@ghcr.io/v2/"),
            "https://***@ghcr.io/v2/"
        );
        assert_eq!(
            compose_line_detail(" Container app-db-1 Started password=hunter2"),
            Some("Container app-db-1 Started password=***".to_string())
        );
    }
}
