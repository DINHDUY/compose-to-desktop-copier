mod boot_progress;
#[cfg(windows)]
mod caption_drag;
mod compose;
mod engine;
mod manifest;
mod stack;
#[allow(dead_code)]
mod stack_lint;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use tauri::webview::{WebviewBuilder, WebviewWindow, WebviewWindowBuilder};
use tauri::{AppHandle, LogicalPosition, LogicalSize, Manager, RunEvent, WebviewUrl, WindowEvent};

pub(crate) const CHROME_WIDTH: f64 = 108.0;
const CHROME_HEIGHT: f64 = 28.0;
pub(crate) const CHROME_INSET_X: f64 = 8.0;
const CHROME_INSET_Y: f64 = 4.0;
const CHROME_HTML: &str = include_str!("chrome.html");
const DRAG_HTML: &str = include_str!("drag.html");
const DRAG_HEIGHT: f64 = 12.0;
const BOOT_HTML: &str = include_str!("boot.html");
const DRAG_STRIP_SCRIPT: &str = include_str!("drag_strip.js");

static SHUTTING_DOWN: AtomicBool = AtomicBool::new(false);

#[derive(Clone)]
struct RunningStack {
    engine: compose::Engine,
    context: compose::StackContext,
}

static ACTIVE_STACK: Mutex<Option<RunningStack>> = Mutex::new(None);
static PRODUCT_NAME: Mutex<String> = Mutex::new(String::new());

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .register_uri_scheme_protocol("shell-chrome", |_ctx, request| {
            let body = if request.uri().path().contains("drag") {
                DRAG_HTML
            } else {
                CHROME_HTML
            };
            tauri::http::Response::builder()
                .header(
                    tauri::http::header::CONTENT_TYPE,
                    "text/html; charset=utf-8",
                )
                .body(body.as_bytes().to_vec())
                .unwrap()
        })
        .register_uri_scheme_protocol("shell-boot", |_ctx, _request| {
            tauri::http::Response::builder()
                .header(
                    tauri::http::header::CONTENT_TYPE,
                    "text/html; charset=utf-8",
                )
                .body(BOOT_HTML.as_bytes().to_vec())
                .unwrap()
        })
        .setup(|app| {
            if let Err(message) = begin_boot(app) {
                show_error(&product_name(), &message);
                std::process::exit(1);
            }
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|_app, event| match event {
            RunEvent::ExitRequested { .. } => {
                SHUTTING_DOWN.store(true, Ordering::SeqCst);
            }
            RunEvent::Exit => {
                if let Some(stack) = take_active_stack() {
                    if let Err(err) = compose::stop_stack(&stack.engine, &stack.context) {
                        eprintln!("{err}");
                    }
                }
            }
            _ => {}
        });
}

fn begin_boot(app: &mut tauri::App) -> Result<(), String> {
    let resource_dir = app.path().resource_dir().ok();
    let app_data_dir = app.path().app_data_dir().ok();
    let stack_dir = stack::resolve_stack_dir(
        cfg!(debug_assertions),
        resource_dir.as_deref(),
        app_data_dir.as_deref(),
    )
    .map_err(|err| err.to_string())?;
    let manifest = manifest::load(&stack_dir).map_err(|err| err.to_string())?;
    set_product_name(&manifest.product_name);
    open_boot_window(app, &manifest.product_name)?;
    let handle = app.handle().clone();
    std::thread::spawn(move || {
        let result = boot_worker(handle.clone(), resource_dir, app_data_dir);
        let app = handle.clone();
        let _ = handle.run_on_main_thread(move || complete_boot(&app, result));
    });
    Ok(())
}

fn open_boot_window(app: &mut tauri::App, product_name: &str) -> Result<(), String> {
    let url = WebviewUrl::CustomProtocol(
        "shell-boot://localhost/"
            .parse()
            .map_err(|err| format!("boot URL is invalid: {err}"))?,
    );
    let window = WebviewWindowBuilder::new(app, "boot", url)
        .title(product_name)
        .inner_size(480.0, 240.0)
        .resizable(false)
        .center()
        .decorations(true)
        .focused(true)
        .build()
        .map_err(|err| err.to_string())?;
    let script = format!("window.setProductName({});", js_string(product_name));
    let _ = window.eval(script);
    Ok(())
}

struct Started {
    engine: compose::Engine,
    context: compose::StackContext,
    settings: manifest::ResolvedSettings,
}

fn boot_worker(
    handle: AppHandle,
    resource_dir: Option<std::path::PathBuf>,
    app_data_dir: Option<std::path::PathBuf>,
) -> Result<Started, String> {
    let debug = cfg!(debug_assertions);
    let stack_dir =
        stack::resolve_stack_dir(debug, resource_dir.as_deref(), app_data_dir.as_deref())
            .map_err(|err| err.to_string())?;
    let manifest = manifest::load(&stack_dir).map_err(|err| err.to_string())?;
    set_product_name(&manifest.product_name);
    if debug {
        stack::prepare_stack_in_place(&stack_dir, &manifest).map_err(|err| err.to_string())?;
    }
    let context = compose::build_context(stack_dir, manifest);
    let engine = engine::resolve(
        resource_dir.as_deref(),
        Some(context.manifest.engine.as_str()),
    )
    .map_err(|err| err.to_string())?;
    remember_stack(RunningStack {
        engine: engine.clone(),
        context: context.clone(),
    });
    let reporter: Arc<dyn boot_progress::BootReport> = Arc::new(boot_progress::BootReporter::new(
        handle.clone(),
        context.manifest.wait_secs,
    ));
    let ticker_done = boot_progress::spawn_progress_ticker(reporter.clone());
    let settings =
        compose::start_and_wait(&engine, &context, reporter).map_err(|err| err.to_string())?;
    ticker_done.store(true, Ordering::Relaxed);
    Ok(Started {
        engine,
        context,
        settings,
    })
}

fn complete_boot(app: &AppHandle, result: Result<Started, String>) {
    if SHUTTING_DOWN.load(Ordering::SeqCst) {
        if let Ok(started) = result {
            let _ = compose::stop_stack(&started.engine, &started.context);
            clear_active_stack();
        }
        return;
    }
    match result {
        Ok(started) => {
            if let Err(message) = open_content(app, &started) {
                let _ = compose::stop_stack(&started.engine, &started.context);
                clear_active_stack();
                close_boot(app);
                show_error(&started.context.manifest.product_name, &message);
                app.exit(1);
            }
        }
        Err(message) => {
            clear_active_stack();
            close_boot(app);
            show_error(&product_name(), &message);
            app.exit(1);
        }
    }
}

fn open_content(app: &AppHandle, started: &Started) -> Result<(), String> {
    let window_url: url::Url = started
        .settings
        .window_url
        .parse()
        .map_err(|err| format!("UI URL is not a valid http address: {err}"))?;
    let allow_remote_ui = started.context.manifest.allow_remote_ui;
    let origin = window_url.clone();
    let title = started.context.manifest.product_name.clone();
    let window = WebviewWindowBuilder::new(app, "main", WebviewUrl::External(window_url))
        .title(title)
        .inner_size(1280.0, 800.0)
        .center()
        .decorations(false)
        .resizable(true)
        .shadow(true)
        .focused(true)
        .initialization_script(drag_strip_script())
        .on_navigation(move |url| navigation_allowed(url, allow_remote_ui, &origin))
        .build()
        .map_err(|err| err.to_string())?;
    attach_chrome(&window)?;
    close_boot(app);
    Ok(())
}

fn close_boot(app: &AppHandle) {
    if let Some(boot) = app.get_webview_window("boot") {
        let _ = boot.close();
    }
}

fn js_string(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "\"\"".to_string())
}

fn set_product_name(name: &str) {
    *product_name_store()
        .lock()
        .unwrap_or_else(|err| err.into_inner()) = name.to_string();
}

fn product_name() -> String {
    product_name_store()
        .lock()
        .unwrap_or_else(|err| err.into_inner())
        .clone()
}

fn product_name_store() -> &'static Mutex<String> {
    &PRODUCT_NAME
}

fn remember_stack(stack: RunningStack) {
    *active_stack().lock().unwrap_or_else(|err| err.into_inner()) = Some(stack);
}

fn clear_active_stack() {
    *active_stack().lock().unwrap_or_else(|err| err.into_inner()) = None;
}

fn take_active_stack() -> Option<RunningStack> {
    active_stack()
        .lock()
        .unwrap_or_else(|err| err.into_inner())
        .take()
}

fn active_stack() -> &'static Mutex<Option<RunningStack>> {
    &ACTIVE_STACK
}

fn drag_strip_script() -> String {
    let reserve = (CHROME_WIDTH + CHROME_INSET_X) as i32;
    DRAG_STRIP_SCRIPT.replace("__CHROME_RESERVE__", &reserve.to_string())
}

fn attach_chrome(shell: &WebviewWindow) -> Result<(), String> {
    let host = shell
        .get_window("main")
        .ok_or_else(|| "main window is missing".to_string())?;
    let drag_url = WebviewUrl::CustomProtocol(
        "shell-chrome://localhost/drag"
            .parse()
            .map_err(|err| format!("drag URL is invalid: {err}"))?,
    );
    let chrome_url = WebviewUrl::CustomProtocol(
        "shell-chrome://localhost/"
            .parse()
            .map_err(|err| format!("chrome URL is invalid: {err}"))?,
    );
    let drag_size = drag_size(shell)?;
    host.add_child(
        WebviewBuilder::new("drag", drag_url)
            .transparent(true)
            .focused(false)
            .zoom_hotkeys_enabled(false),
        LogicalPosition::new(0.0, 0.0),
        drag_size,
    )
    .map_err(|err| err.to_string())?;
    host.add_child(
        WebviewBuilder::new("chrome", chrome_url)
            .transparent(true)
            .focused(false)
            .zoom_hotkeys_enabled(false),
        chrome_origin(shell)?,
        LogicalSize::new(CHROME_WIDTH, CHROME_HEIGHT),
    )
    .map_err(|err| err.to_string())?;

    #[cfg(windows)]
    {
        let hwnd = shell.hwnd().map_err(|err| err.to_string())?;
        caption_drag::install(hwnd.0)?;
    }

    let tracked = shell.clone();
    shell.on_window_event(move |event| {
        let (width, scale) = match event {
            WindowEvent::Resized(size) => {
                let Ok(scale) = tracked.scale_factor() else {
                    return;
                };
                (size.width, scale)
            }
            WindowEvent::ScaleFactorChanged {
                scale_factor,
                new_inner_size,
                ..
            } => (new_inner_size.width, *scale_factor),
            _ => return,
        };
        place_overlays(&tracked, width, scale);
    });
    Ok(())
}

fn chrome_origin(shell: &WebviewWindow) -> Result<LogicalPosition<f64>, String> {
    let size = shell.inner_size().map_err(|err| err.to_string())?;
    let scale = shell.scale_factor().map_err(|err| err.to_string())?;
    Ok(chrome_position(size.width, scale))
}

fn chrome_position(physical_width: u32, scale: f64) -> LogicalPosition<f64> {
    let logical_width = physical_width as f64 / scale;
    let x = (logical_width - CHROME_WIDTH - CHROME_INSET_X).max(0.0);
    LogicalPosition::new(snap_logical(x, scale), snap_logical(CHROME_INSET_Y, scale))
}

fn snap_logical(value: f64, scale: f64) -> f64 {
    if scale <= 0.0 {
        return value;
    }
    (value * scale).round() / scale
}

fn drag_size(shell: &WebviewWindow) -> Result<LogicalSize<f64>, String> {
    let size = shell.inner_size().map_err(|err| err.to_string())?;
    let scale = shell.scale_factor().map_err(|err| err.to_string())?;
    Ok(drag_logical_size(size.width, scale))
}

fn drag_logical_size(physical_width: u32, scale: f64) -> LogicalSize<f64> {
    let logical_width = physical_width as f64 / scale;
    let width = (logical_width - CHROME_WIDTH - CHROME_INSET_X).max(0.0);
    LogicalSize::new(width, DRAG_HEIGHT)
}

fn place_overlays(shell: &WebviewWindow, physical_width: u32, scale: f64) {
    if scale <= 0.0 {
        return;
    }
    if let Some(drag) = shell.get_webview("drag") {
        let _ = drag.set_position(LogicalPosition::new(0.0, 0.0));
        let _ = drag.set_size(drag_logical_size(physical_width, scale));
    }
    if let Some(chrome) = shell.get_webview("chrome") {
        let _ = chrome.set_position(chrome_position(physical_width, scale));
    }
}

fn navigation_allowed(url: &url::Url, allow_remote_ui: bool, window_origin: &url::Url) -> bool {
    if url.scheme() != "http" && url.scheme() != "https" {
        return false;
    }
    if allow_remote_ui {
        return url.scheme() == window_origin.scheme()
            && url.host() == window_origin.host()
            && url.port_or_known_default() == window_origin.port_or_known_default();
    }
    matches!(url.host_str(), Some("127.0.0.1") | Some("localhost"))
}

#[cfg(test)]
mod tests {
    use super::navigation_allowed;

    fn url(value: &str) -> url::Url {
        value.parse().unwrap()
    }

    #[test]
    fn loopback_navigation_allows_other_local_ports() {
        let origin = url("http://127.0.0.1:3000");
        assert!(navigation_allowed(
            &url("http://127.0.0.1:3000/health"),
            false,
            &origin
        ));
        assert!(navigation_allowed(
            &url("http://localhost:8080/"),
            false,
            &origin
        ));
        assert!(!navigation_allowed(
            &url("https://example.com/"),
            false,
            &origin
        ));
        assert!(!navigation_allowed(
            &url("file:///etc/passwd"),
            false,
            &origin
        ));
    }

    #[test]
    fn remote_ui_navigation_stays_on_the_window_origin() {
        let origin = url("https://example.com");
        assert!(navigation_allowed(
            &url("https://example.com/app"),
            true,
            &origin
        ));
        assert!(!navigation_allowed(
            &url("https://evil.com/"),
            true,
            &origin
        ));
        assert!(!navigation_allowed(
            &url("http://127.0.0.1:3000/"),
            true,
            &origin
        ));
    }
}

fn show_error(title: &str, message: &str) {
    eprintln!("{message}");
    let _ = std::io::Write::flush(&mut std::io::stderr());
    rfd::MessageDialog::new()
        .set_title(title)
        .set_level(rfd::MessageLevel::Error)
        .set_description(message)
        .show();
}
