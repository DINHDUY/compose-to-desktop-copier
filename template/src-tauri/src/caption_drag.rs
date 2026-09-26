use std::sync::atomic::{AtomicIsize, Ordering};

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::ScreenToClient;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Shell::{DefSubclassProc, SetWindowSubclass};
use windows::Win32::UI::WindowsAndMessaging::{GetClientRect, HTCLIENT, WM_NCHITTEST};

use crate::{CHROME_INSET_X, CHROME_WIDTH};

const SUBCLASS_ID: usize = 0x4452_4147;
const DRAG_HEIGHT: f64 = 12.0;

static HOST: AtomicIsize = AtomicIsize::new(0);

pub fn install(hwnd: *mut core::ffi::c_void) -> Result<(), String> {
    let hwnd = HWND(hwnd);
    HOST.store(hwnd.0 as isize, Ordering::Relaxed);
    unsafe {
        if !SetWindowSubclass(hwnd, Some(subclass), SUBCLASS_ID, 0).as_bool() {
            return Err("Could not install the window drag handler.".to_string());
        }
    }
    Ok(())
}

unsafe extern "system" fn subclass(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _id: usize,
    _data: usize,
) -> LRESULT {
    let host = HWND(HOST.load(Ordering::Relaxed) as *mut core::ffi::c_void);
    if hwnd == host && msg == WM_NCHITTEST && in_drag_band(host, lparam) {
        return LRESULT(HTCLIENT as isize);
    }
    DefSubclassProc(hwnd, msg, wparam, lparam)
}

fn in_drag_band(host: HWND, lparam: LPARAM) -> bool {
    if host.0.is_null() {
        return false;
    }
    let mut point = POINT {
        x: (lparam.0 as u32 & 0xffff) as u16 as i16 as i32,
        y: ((lparam.0 as u32) >> 16) as u16 as i16 as i32,
    };
    unsafe {
        if !ScreenToClient(host, &mut point).as_bool() {
            return false;
        }
        let dpi = GetDpiForWindow(host).max(96) as f64;
        let scale = dpi / 96.0;
        let y = point.y as f64 / scale;
        if !(0.0..=DRAG_HEIGHT).contains(&y) {
            return false;
        }
        let mut rect = RECT::default();
        if GetClientRect(host, &mut rect).is_err() {
            return false;
        }
        let width = rect.right as f64 / scale;
        let x = point.x as f64 / scale;
        x >= 0.0 && x < width - CHROME_WIDTH - CHROME_INSET_X
    }
}
