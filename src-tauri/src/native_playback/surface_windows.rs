//! Windows MPV surface hosted underneath the transparent WebView2 layer.
//!
//! MPV's documented Win32 `wid` integration creates its own rendering child
//! inside the HWND supplied here. Keeping that platform detail behind this
//! type preserves the narrow Heya playback bridge and lets MPV retain its
//! normal gpu-next renderer, hardware decoding, subtitles, and shader path.

use super::EngineError;
use crate::navigation;
use std::{
    ffi::c_void,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc, Mutex, OnceLock, Weak,
    },
    time::Duration,
};
use tauri::{AppHandle, Manager};
use windows::{
    core::w,
    Win32::{
        Foundation::HWND,
        UI::WindowsAndMessaging::{
            CreateWindowExW, DestroyWindow, SetWindowPos, HWND_BOTTOM, SWP_NOACTIVATE,
            SWP_NOOWNERZORDER, SWP_NOSENDCHANGING, SWP_SHOWWINDOW, WINDOW_STYLE, WS_CHILD,
            WS_CLIPCHILDREN, WS_CLIPSIBLINGS, WS_EX_NOACTIVATE, WS_EX_NOPARENTNOTIFY, WS_VISIBLE,
        },
    },
};

const ATTACH_TIMEOUT: Duration = Duration::from_secs(5);
// Win32's built-in STATIC class uses style 0x0004 (SS_BLACKRECT) to paint a
// neutral black surface before MPV presents its first frame.
const SS_BLACKRECT_STYLE: WINDOW_STYLE = WINDOW_STYLE(0x0004);

fn active_surface() -> &'static Mutex<Weak<Inner>> {
    static ACTIVE: OnceLock<Mutex<Weak<Inner>>> = OnceLock::new();
    ACTIVE.get_or_init(|| Mutex::new(Weak::new()))
}

struct Inner {
    app: AppHandle,
    host_hwnd: Mutex<Option<isize>>,
    active: AtomicBool,
}

impl Drop for Inner {
    fn drop(&mut self) {
        // This is a last-resort cleanup for an attach timeout or an early
        // initialization error before WindowsEmbeddedRenderer takes
        // ownership. Normal teardown takes the handle first, so this cannot
        // race a second DestroyWindow call.
        let raw = self.host_hwnd.get_mut().ok().and_then(|host| host.take());
        let Some(raw) = raw else {
            return;
        };
        if let Err(error) = self.app.run_on_main_thread(move || unsafe {
            if let Err(error) = DestroyWindow(hwnd_from_raw(raw)) {
                log::warn!("could not remove an incomplete Windows video surface: {error}");
            }
        }) {
            log::warn!("could not schedule incomplete Windows surface removal: {error}");
        }
    }
}

pub struct WindowsEmbeddedRenderer {
    inner: Arc<Inner>,
    mpv_window_id: u32,
}

impl WindowsEmbeddedRenderer {
    pub fn attach(app: AppHandle) -> Result<Self, EngineError> {
        let inner = Arc::new(Inner {
            app: app.clone(),
            host_hwnd: Mutex::new(None),
            active: AtomicBool::new(true),
        });
        let main_inner = inner.clone();
        let (sender, receiver) = mpsc::sync_channel(1);
        app.run_on_main_thread(move || {
            let result = unsafe { attach_on_main(&main_inner) };
            let _ = sender.send(result);
        })
        .map_err(|error| {
            EngineError::unavailable(format!(
                "could not schedule the embedded Windows video surface: {error}"
            ))
        })?;

        let mpv_window_id = match receiver.recv_timeout(ATTACH_TIMEOUT) {
            Ok(result) => result?,
            Err(_) => {
                inner.active.store(false, Ordering::Release);
                return Err(EngineError::unavailable(
                    "the embedded Windows video surface did not initialize in time",
                ));
            }
        };

        if let Ok(mut active) = active_surface().lock() {
            *active = Arc::downgrade(&inner);
        }
        Ok(Self {
            inner,
            mpv_window_id,
        })
    }

    pub fn mpv_window_id(&self) -> u32 {
        self.mpv_window_id
    }

    pub fn set_fullscreen(&self, fullscreen: bool) -> Result<(), EngineError> {
        let window = self
            .inner
            .app
            .get_webview_window(navigation::MAIN_WINDOW_LABEL)
            .ok_or_else(|| EngineError::command("the main Heya window is unavailable"))?;
        window.set_fullscreen(fullscreen).map_err(|error| {
            EngineError::command(format!("could not change native fullscreen: {error}"))
        })
    }

    fn shutdown(&self) {
        if let Ok(mut active) = active_surface().lock() {
            let owns_registration = active
                .upgrade()
                .is_some_and(|registered| Arc::ptr_eq(&registered, &self.inner));
            if owns_registration {
                *active = Weak::new();
            }
        }
        if !self.inner.active.swap(false, Ordering::AcqRel) {
            return;
        }

        let raw = self
            .inner
            .host_hwnd
            .lock()
            .ok()
            .and_then(|mut hwnd| hwnd.take());
        let Some(raw) = raw else {
            return;
        };
        if let Err(error) = self.inner.app.run_on_main_thread(move || unsafe {
            if let Err(error) = DestroyWindow(hwnd_from_raw(raw)) {
                log::warn!("could not remove the embedded Windows video surface: {error}");
            }
        }) {
            log::warn!("could not schedule embedded Windows surface removal: {error}");
        }
    }
}

impl Drop for WindowsEmbeddedRenderer {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Resize and reassert the bottom-most sibling order from Tauri's main event
/// loop. The WebView2 controller stays above this host and therefore retains
/// all pointer, keyboard, titlebar, and HTML control input.
pub(crate) fn resize_active_surface(event_width: u32, event_height: u32) {
    let inner = active_surface()
        .lock()
        .ok()
        .and_then(|surface| surface.upgrade());
    let Some(inner) = inner else {
        return;
    };
    if !inner.active.load(Ordering::Acquire) {
        return;
    }
    let raw = inner.host_hwnd.lock().ok().and_then(|hwnd| *hwnd);
    let Some(raw) = raw else {
        return;
    };

    if let Err(error) = unsafe { resize_on_main(hwnd_from_raw(raw), event_width, event_height) } {
        log::warn!("could not resize the embedded Windows video surface: {error}");
    }
}

unsafe fn attach_on_main(inner: &Arc<Inner>) -> Result<u32, EngineError> {
    if !inner.active.load(Ordering::Acquire) {
        return Err(EngineError::unavailable(
            "the embedded Windows video surface was cancelled",
        ));
    }
    let window = inner
        .app
        .get_webview_window(navigation::MAIN_WINDOW_LABEL)
        .ok_or_else(|| EngineError::unavailable("the main Heya window is unavailable"))?;
    let parent = window.hwnd().map_err(|error| {
        EngineError::unavailable(format!("could not read the Heya window handle: {error}"))
    })?;
    let size = window.inner_size().map_err(|error| {
        EngineError::unavailable(format!("could not read the Heya window size: {error}"))
    })?;

    let host = unsafe {
        CreateWindowExW(
            WS_EX_NOACTIVATE | WS_EX_NOPARENTNOTIFY,
            w!("STATIC"),
            w!(""),
            WS_CHILD | WS_VISIBLE | WS_CLIPSIBLINGS | WS_CLIPCHILDREN | SS_BLACKRECT_STYLE,
            0,
            0,
            dimension(size.width),
            dimension(size.height),
            Some(parent),
            None,
            None,
            None,
        )
    }
    .map_err(|error| {
        EngineError::unavailable(format!(
            "could not create the embedded Windows video surface: {error}"
        ))
    })?;

    if let Err(error) = unsafe { resize_on_main(host, size.width, size.height) } {
        let _ = unsafe { DestroyWindow(host) };
        return Err(error);
    }

    let raw = host.0 as isize;
    let mpv_window_id = mpv_window_id(raw as usize)?;
    if let Ok(mut stored) = inner.host_hwnd.lock() {
        *stored = Some(raw);
    } else {
        let _ = unsafe { DestroyWindow(host) };
        return Err(EngineError::unavailable(
            "the embedded Windows video surface lock is unavailable",
        ));
    }
    Ok(mpv_window_id)
}

unsafe fn resize_on_main(hwnd: HWND, width: u32, height: u32) -> Result<(), EngineError> {
    unsafe {
        SetWindowPos(
            hwnd,
            Some(HWND_BOTTOM),
            0,
            0,
            dimension(width),
            dimension(height),
            SWP_NOACTIVATE | SWP_NOOWNERZORDER | SWP_NOSENDCHANGING | SWP_SHOWWINDOW,
        )
    }
    .map_err(|error| {
        EngineError::command(format!(
            "could not position the embedded Windows video surface: {error}"
        ))
    })
}

fn dimension(value: u32) -> i32 {
    value.min(i32::MAX as u32) as i32
}

fn mpv_window_id(raw: usize) -> Result<u32, EngineError> {
    let id = raw as u32;
    if id == 0 {
        Err(EngineError::unavailable(
            "the embedded Windows video surface has an invalid handle",
        ))
    } else {
        // MPV explicitly requires Win32 HWND values to be cast to uint32_t,
        // including handles that Windows sign-extends on 64-bit processes.
        Ok(id)
    }
}

fn hwnd_from_raw(raw: isize) -> HWND {
    HWND(raw as *mut c_void)
}

#[cfg(test)]
mod tests {
    use super::mpv_window_id;

    #[test]
    fn converts_win32_handles_using_mpvs_documented_uint32_cast() {
        assert_eq!(mpv_window_id(0x1234_5678).unwrap(), 0x1234_5678);
        if usize::BITS == 64 {
            assert_eq!(mpv_window_id(0xffff_ffff_f123_4567).unwrap(), 0xf123_4567);
        }
        assert!(mpv_window_id(0).is_err());
    }
}
