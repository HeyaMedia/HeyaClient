//! macOS libmpv render surface hosted directly beneath Tauri's WKWebView.
//!
//! AppKit creates and removes the view on the main thread. A dedicated render
//! worker owns the OpenGL context and every `mpv_render_*` call so decoder/GPU
//! startup and frame timing can never block Tauri's window event loop. The
//! libmpv event/command worker remains separate, matching libmpv's threading
//! contract. The remote page receives only the semantic surface kind.

use super::EngineError;
use crate::navigation;
use libmpv2_sys as mpv_sys;
use objc2::{rc::Retained, AnyThread, MainThreadMarker, MainThreadOnly};
#[allow(deprecated)]
use objc2_app_kit::{
    NSAutoresizingMaskOptions, NSOpenGLContext, NSOpenGLPFADoubleBuffer, NSOpenGLPixelFormat,
    NSOpenGLView, NSResponder, NSView, NSWindowOrderingMode,
};
use objc2_foundation::NSLocking;
use std::{
    ffi::{c_char, c_void},
    ptr::{self, NonNull},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc, Arc, Mutex, OnceLock, Weak,
    },
    thread::{self, JoinHandle, Thread},
    time::Duration,
};
use tauri::{AppHandle, Manager};

const MAIN_THREAD_TIMEOUT: Duration = Duration::from_secs(10);
const RTLD_DEFAULT: *mut c_void = -2_isize as *mut c_void;

static ACTIVE_SURFACE: OnceLock<Mutex<Weak<Inner>>> = OnceLock::new();

fn active_surface() -> &'static Mutex<Weak<Inner>> {
    ACTIVE_SURFACE.get_or_init(|| Mutex::new(Weak::new()))
}

unsafe extern "C" {
    fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
}

#[derive(Default)]
struct RenderState {
    gl_view: usize,
    gl_context: usize,
    width: i32,
    height: i32,
}

impl RenderState {
    fn attached(&self) -> bool {
        self.gl_view != 0 && self.gl_context != 0
    }
}

struct Inner {
    app: AppHandle,
    state: Mutex<RenderState>,
    context_use: Mutex<()>,
    active: AtomicBool,
    render_requested: AtomicBool,
    render_thread: Mutex<Option<Thread>>,
    render_worker: Mutex<Option<JoinHandle<()>>>,
    first_frame_presented: AtomicBool,
    focus_restore_generation: AtomicU64,
}

/// `NSOpenGLContext` implements `NSLocking` as the AppKit wrapper around the
/// underlying CGL context lock. Holding it is required while another thread
/// may resize the hosting `NSOpenGLView`.
#[allow(deprecated)]
struct OpenGlContextLock<'a>(&'a NSOpenGLContext);

#[allow(deprecated)]
impl<'a> OpenGlContextLock<'a> {
    unsafe fn acquire(context: &'a NSOpenGLContext) -> Self {
        unsafe { context.lock() };
        Self(context)
    }
}

impl Drop for OpenGlContextLock<'_> {
    fn drop(&mut self) {
        unsafe { self.0.unlock() };
    }
}

/// A single libmpv OpenGL renderer attached below the main WKWebView.
///
/// The wrapper is `Send` because AppKit ownership is held behind opaque raw
/// handles. The view hierarchy is mutated only on the main thread; the OpenGL
/// context is used exclusively by the dedicated render worker.
pub struct MacEmbeddedRenderer {
    inner: Arc<Inner>,
}

impl MacEmbeddedRenderer {
    pub fn attach(app: AppHandle, mpv: &libmpv2::Mpv) -> Result<Self, EngineError> {
        let inner = Arc::new(Inner {
            app: app.clone(),
            state: Mutex::new(RenderState::default()),
            context_use: Mutex::new(()),
            active: AtomicBool::new(true),
            render_requested: AtomicBool::new(false),
            render_thread: Mutex::new(None),
            render_worker: Mutex::new(None),
            first_frame_presented: AtomicBool::new(false),
            focus_restore_generation: AtomicU64::new(0),
        });
        let window = app
            .get_webview_window(navigation::MAIN_WINDOW_LABEL)
            .ok_or_else(|| EngineError::unavailable("the main Heya window is unavailable"))?;
        let (sender, receiver) = mpsc::sync_channel(1);
        let attach_inner = inner.clone();

        window
            .with_webview(move |webview| {
                let result = unsafe { attach_on_main(&attach_inner, webview.inner()) };
                let _ = sender.send(result);
            })
            .map_err(|error| {
                EngineError::unavailable(format!(
                    "could not schedule the embedded MPV surface: {error}"
                ))
            })?;

        receiver.recv_timeout(MAIN_THREAD_TIMEOUT).map_err(|_| {
            EngineError::unavailable("the embedded MPV surface did not initialize in time")
        })??;

        let renderer = Self { inner };
        let mpv_handle = mpv.ctx.as_ptr() as usize;
        let render_inner = renderer.inner.clone();
        let (ready_sender, ready_receiver) = mpsc::sync_channel(1);
        let worker = thread::Builder::new()
            .name("heya-mpv-render".to_string())
            .spawn(move || unsafe {
                render_worker(
                    render_inner,
                    mpv_handle as *mut mpv_sys::mpv_handle,
                    ready_sender,
                )
            })
            .map_err(|error| {
                EngineError::unavailable(format!(
                    "could not start the embedded MPV render worker: {error}"
                ))
            })?;
        if let Ok(mut slot) = renderer.inner.render_worker.lock() {
            *slot = Some(worker);
        } else {
            renderer.shutdown();
            return Err(EngineError::unavailable(
                "the embedded MPV render worker is unavailable",
            ));
        }

        match ready_receiver.recv_timeout(MAIN_THREAD_TIMEOUT) {
            Ok(Ok(())) => {
                if let Ok(mut active) = active_surface().lock() {
                    *active = Arc::downgrade(&renderer.inner);
                }
                // libmpv invokes the update callback immediately. This extra
                // request also covers a callback racing worker publication.
                renderer.inner.schedule_render();
                Ok(renderer)
            }
            Ok(Err(error)) => {
                renderer.shutdown();
                Err(error)
            }
            Err(_) => {
                renderer.shutdown();
                Err(EngineError::unavailable(
                    "the embedded MPV renderer did not initialize in time",
                ))
            }
        }
    }

    /// True only after libmpv supplied and the OpenGL surface swapped a real
    /// video frame. The immediate empty redraw requested during render-context
    /// creation deliberately does not count.
    pub fn video_surface_ready(&self) -> bool {
        self.inner.first_frame_presented.load(Ordering::Acquire)
    }

    pub fn set_fullscreen(&self, fullscreen: bool) -> Result<(), EngineError> {
        let window = self
            .inner
            .app
            .get_webview_window(navigation::MAIN_WINDOW_LABEL)
            .ok_or_else(|| EngineError::command("the main Heya window is unavailable"))?;
        window.set_fullscreen(fullscreen).map_err(|error| {
            EngineError::command(format!("could not change native fullscreen: {error}"))
        })?;
        self.inner.schedule_webview_focus_restore();
        Ok(())
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

        self.inner.wake_render_thread();
        if let Ok(mut worker) = self.inner.render_worker.lock() {
            if let Some(worker) = worker.take() {
                if worker.join().is_err() {
                    log::warn!("embedded MPV render worker stopped unexpectedly");
                }
            }
        }

        // The render worker is fully joined above, so the libmpv render
        // context is already gone before the owning MPV core can be dropped.
        // View removal is deliberately asynchronous: shutdown can be reached
        // from a main-window lifecycle callback, where waiting for the main
        // thread would deadlock. The closure keeps `Inner` alive until AppKit
        // releases the retained view/context on its own thread.
        let inner = self.inner.clone();
        if let Err(error) = self.inner.app.run_on_main_thread(move || unsafe {
            detach_on_main(&inner);
        }) {
            log::warn!("could not schedule embedded MPV surface removal: {error}");
        }
    }
}

/// Refreshes the active embedded drawable after Tauri reports a real content
/// resize. This is called from the macOS application event loop, so AppKit can
/// safely update the view/context without a synchronous worker-to-main hop.
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

    unsafe { resize_on_main(&inner, event_width, event_height) };
}

impl Drop for MacEmbeddedRenderer {
    fn drop(&mut self) {
        self.shutdown();
    }
}

impl Inner {
    fn schedule_render(self: &Arc<Self>) {
        if !self.active.load(Ordering::Acquire) {
            return;
        }
        self.render_requested.store(true, Ordering::Release);
        self.wake_render_thread();
    }

    fn wake_render_thread(&self) {
        if let Ok(thread) = self.render_thread.lock() {
            if let Some(thread) = thread.as_ref() {
                thread.unpark();
            }
        }
    }

    /// AppKit temporarily moves first-responder ownership while entering and
    /// leaving its fullscreen Space. Repairing it from a resize callback is
    /// unsafe because fullscreen and live-resize use nested AppKit event
    /// loops. Reassert only the WKWebView responder for a short, bounded
    /// interval around the animation; this does not activate a background app
    /// and ensures AppKit's final transition callback cannot overwrite us.
    fn schedule_webview_focus_restore(self: &Arc<Self>) {
        let generation = self.focus_restore_generation.fetch_add(1, Ordering::AcqRel) + 1;
        let inner = self.clone();
        if let Err(error) = thread::Builder::new()
            .name("heya-webview-focus".to_string())
            .spawn(move || {
                for _ in 0..7 {
                    thread::sleep(Duration::from_millis(300));
                    if !inner.active.load(Ordering::Acquire)
                        || inner.focus_restore_generation.load(Ordering::Acquire) != generation
                    {
                        return;
                    }
                    restore_webview_first_responder(&inner.app);
                }
            })
        {
            log::warn!("could not schedule WKWebView focus restoration: {error}");
        }
    }
}

fn restore_webview_first_responder(app: &AppHandle) {
    let Some(window) = app.get_webview_window(navigation::MAIN_WINDOW_LABEL) else {
        return;
    };
    if let Err(error) = window.with_webview(|webview| unsafe {
        let view = &*(webview.inner() as *const NSView);
        let responder = &*(webview.inner() as *const NSResponder);
        if let Some(window) = view.window() {
            window.makeFirstResponder(Some(responder));
        }
    }) {
        log::warn!("could not restore WKWebView keyboard focus after fullscreen: {error}");
    }
}

unsafe extern "C" fn resolve_opengl(_context: *mut c_void, name: *const c_char) -> *mut c_void {
    if name.is_null() {
        return ptr::null_mut();
    }
    unsafe { dlsym(RTLD_DEFAULT, name) }
}

unsafe extern "C" fn request_render(context: *mut c_void) {
    if context.is_null() {
        return;
    }
    let weak = unsafe { &*(context as *const std::sync::Weak<Inner>) };
    if let Some(inner) = weak.upgrade() {
        inner.schedule_render();
    }
}

#[allow(deprecated)]
unsafe fn attach_on_main(inner: &Arc<Inner>, webview: *mut c_void) -> Result<(), EngineError> {
    let mtm = MainThreadMarker::new()
        .ok_or_else(|| EngineError::unavailable("the MPV surface is not on the main thread"))?;
    if webview.is_null() {
        return Err(EngineError::unavailable(
            "the native video surface received an invalid handle",
        ));
    }

    let webview = unsafe { &*(webview as *const NSView) };
    let parent = unsafe { webview.superview() }
        .ok_or_else(|| EngineError::unavailable("the Heya WebView has no parent surface"))?;
    let mut attributes = [NSOpenGLPFADoubleBuffer, 0];
    let attributes = NonNull::new(attributes.as_mut_ptr())
        .ok_or_else(|| EngineError::unavailable("could not configure the OpenGL surface"))?;
    let pixel_format = unsafe {
        NSOpenGLPixelFormat::initWithAttributes(NSOpenGLPixelFormat::alloc(), attributes)
    }
    .ok_or_else(|| EngineError::unavailable("could not create an OpenGL pixel format"))?;
    let gl_view = NSOpenGLView::initWithFrame_pixelFormat(
        NSOpenGLView::alloc(mtm),
        webview.bounds(),
        Some(&pixel_format),
    )
    .ok_or_else(|| EngineError::unavailable("could not create the embedded OpenGL view"))?;
    gl_view.setAutoresizingMask(
        NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewHeightSizable,
    );
    gl_view.setWantsBestResolutionOpenGLSurface(true);
    let gl_context = gl_view
        .openGLContext()
        .ok_or_else(|| EngineError::unavailable("the embedded OpenGL context is unavailable"))?;
    parent.addSubview_positioned_relativeTo(&gl_view, NSWindowOrderingMode::Below, Some(webview));
    gl_context.makeCurrentContext();
    gl_context.update(mtm);
    let backing = gl_view.convertRectToBacking(gl_view.bounds());
    let width = backing.size.width.round().clamp(1.0, f64::from(i32::MAX)) as i32;
    let height = backing.size.height.round().clamp(1.0, f64::from(i32::MAX)) as i32;
    NSOpenGLContext::clearCurrentContext();

    let mut state = inner
        .state
        .lock()
        .map_err(|_| EngineError::unavailable("the embedded render state is unavailable"))?;
    state.gl_view = Retained::into_raw(gl_view) as usize;
    state.gl_context = Retained::into_raw(gl_context) as usize;
    state.width = width;
    state.height = height;
    Ok(())
}

#[allow(deprecated)]
unsafe fn render_worker(
    inner: Arc<Inner>,
    mpv: *mut mpv_sys::mpv_handle,
    ready: mpsc::SyncSender<Result<(), EngineError>>,
) {
    if let Ok(mut render_thread) = inner.render_thread.lock() {
        *render_thread = Some(thread::current());
    }

    let gl_context_ptr = inner
        .state
        .lock()
        .ok()
        .filter(|state| state.attached())
        .map(|state| state.gl_context as *mut NSOpenGLContext);
    let Some(gl_context_ptr) = gl_context_ptr else {
        let _ = ready.send(Err(EngineError::unavailable(
            "the embedded OpenGL state is unavailable",
        )));
        return;
    };
    let gl_context = unsafe { &*gl_context_ptr };
    let context_lock = unsafe { OpenGlContextLock::acquire(gl_context) };
    gl_context.makeCurrentContext();

    let mut init = mpv_sys::mpv_opengl_init_params {
        get_proc_address: Some(resolve_opengl),
        get_proc_address_ctx: ptr::null_mut(),
    };
    let mut params = [
        mpv_sys::mpv_render_param {
            type_: mpv_sys::mpv_render_param_type_MPV_RENDER_PARAM_API_TYPE,
            data: mpv_sys::MPV_RENDER_API_TYPE_OPENGL.as_ptr() as *mut c_void,
        },
        mpv_sys::mpv_render_param {
            type_: mpv_sys::mpv_render_param_type_MPV_RENDER_PARAM_OPENGL_INIT_PARAMS,
            data: (&mut init as *mut mpv_sys::mpv_opengl_init_params).cast(),
        },
        mpv_sys::mpv_render_param {
            type_: mpv_sys::mpv_render_param_type_MPV_RENDER_PARAM_INVALID,
            data: ptr::null_mut(),
        },
    ];
    let mut render_context = ptr::null_mut();
    let status = unsafe {
        mpv_sys::mpv_render_context_create(&mut render_context, mpv, params.as_mut_ptr())
    };
    if status < 0 || render_context.is_null() {
        NSOpenGLContext::clearCurrentContext();
        let _ = ready.send(Err(EngineError::unavailable(format!(
            "libmpv could not create the embedded renderer (error {status})"
        ))));
        return;
    }

    let callback_context = Box::into_raw(Box::new(Arc::downgrade(&inner)));
    unsafe {
        mpv_sys::mpv_render_context_set_update_callback(
            render_context,
            Some(request_render),
            callback_context.cast(),
        );
    }
    NSOpenGLContext::clearCurrentContext();
    drop(context_lock);
    let _ = ready.send(Ok(()));

    while inner.active.load(Ordering::Acquire) {
        if !inner.render_requested.swap(false, Ordering::AcqRel) {
            thread::park();
            continue;
        }
        unsafe { render_on_worker(&inner, render_context) };
    }

    unsafe {
        mpv_sys::mpv_render_context_set_update_callback(render_context, None, ptr::null_mut());
        drop(Box::from_raw(callback_context));
        let _context_lock = OpenGlContextLock::acquire(gl_context);
        gl_context.makeCurrentContext();
        mpv_sys::mpv_render_context_free(render_context);
        NSOpenGLContext::clearCurrentContext();
    }
    if let Ok(mut render_thread) = inner.render_thread.lock() {
        *render_thread = None;
    }
}

#[allow(deprecated)]
unsafe fn render_on_worker(inner: &Inner, render_context: *mut mpv_sys::mpv_render_context) {
    let Ok(_context_guard) = inner.context_use.lock() else {
        return;
    };
    let (gl_context_ptr, width, height) = {
        let Ok(state) = inner.state.lock() else {
            return;
        };
        if !state.attached() {
            return;
        }
        (state.gl_context, state.width, state.height)
    };
    let gl_context = unsafe { &*(gl_context_ptr as *const NSOpenGLContext) };
    let _context_lock = unsafe { OpenGlContextLock::acquire(gl_context) };
    gl_context.makeCurrentContext();

    let flags = unsafe { mpv_sys::mpv_render_context_update(render_context) };
    if flags & u64::from(mpv_sys::mpv_render_update_flag_MPV_RENDER_UPDATE_FRAME) == 0 {
        NSOpenGLContext::clearCurrentContext();
        return;
    }

    // Never query the Tauri window from this thread. Window getters can make
    // a synchronous round-trip to AppKit, while a bridge request may be
    // waiting for the playback worker. The cached physical dimensions are
    // populated on AppKit during attach/fullscreen/resize handling.
    let mut framebuffer = mpv_sys::mpv_opengl_fbo {
        fbo: 0,
        w: width,
        h: height,
        internal_format: 0,
    };
    let mut flip_y = 1_i32;
    let mut frame_info = mpv_sys::mpv_render_frame_info {
        flags: 0,
        target_time: 0,
    };
    let frame_info_status = unsafe {
        mpv_sys::mpv_render_context_get_info(
            render_context,
            mpv_sys::mpv_render_param {
                type_: mpv_sys::mpv_render_param_type_MPV_RENDER_PARAM_NEXT_FRAME_INFO,
                data: (&mut frame_info as *mut mpv_sys::mpv_render_frame_info).cast(),
            },
        )
    };
    let presents_video_frame = frame_info_status >= 0
        && frame_info.flags
            & u64::from(mpv_sys::mpv_render_frame_info_flag_MPV_RENDER_FRAME_INFO_PRESENT)
            != 0;
    let mut params = [
        mpv_sys::mpv_render_param {
            type_: mpv_sys::mpv_render_param_type_MPV_RENDER_PARAM_OPENGL_FBO,
            data: (&mut framebuffer as *mut mpv_sys::mpv_opengl_fbo).cast(),
        },
        mpv_sys::mpv_render_param {
            type_: mpv_sys::mpv_render_param_type_MPV_RENDER_PARAM_FLIP_Y,
            data: (&mut flip_y as *mut i32).cast(),
        },
        mpv_sys::mpv_render_param {
            type_: mpv_sys::mpv_render_param_type_MPV_RENDER_PARAM_INVALID,
            data: ptr::null_mut(),
        },
    ];
    let status = unsafe { mpv_sys::mpv_render_context_render(render_context, params.as_mut_ptr()) };
    if status < 0 {
        log::warn!("embedded MPV frame render failed with libmpv error {status}");
        NSOpenGLContext::clearCurrentContext();
        return;
    }
    if presents_video_frame && !inner.first_frame_presented.swap(true, Ordering::AcqRel) {
        log::info!("embedded MPV rendered its first frame at {width}x{height}");
    }
    gl_context.flushBuffer();
    unsafe { mpv_sys::mpv_render_context_report_swap(render_context) };
    NSOpenGLContext::clearCurrentContext();
}

#[allow(deprecated)]
unsafe fn resize_on_main(inner: &Arc<Inner>, event_width: u32, event_height: u32) {
    let Some(mtm) = MainThreadMarker::new() else {
        log::warn!("embedded MPV resize was not delivered on the main thread");
        return;
    };
    let Ok(_context_guard) = inner.context_use.lock() else {
        return;
    };
    let Ok(mut state) = inner.state.lock() else {
        return;
    };
    if !state.attached() {
        return;
    }

    let gl_view = unsafe { &*(state.gl_view as *const NSOpenGLView) };
    let gl_context = unsafe { &*(state.gl_context as *const NSOpenGLContext) };
    let _context_lock = unsafe { OpenGlContextLock::acquire(gl_context) };
    let backing = gl_view.convertRectToBacking(gl_view.bounds());
    let measured_width = backing.size.width.round();
    let measured_height = backing.size.height.round();
    state.width = if measured_width.is_finite() && measured_width >= 1.0 {
        measured_width.clamp(1.0, f64::from(i32::MAX)) as i32
    } else {
        i32::try_from(event_width).unwrap_or(i32::MAX).max(1)
    };
    state.height = if measured_height.is_finite() && measured_height >= 1.0 {
        measured_height.clamp(1.0, f64::from(i32::MAX)) as i32
    } else {
        i32::try_from(event_height).unwrap_or(i32::MAX).max(1)
    };
    let (width, height) = (state.width, state.height);
    drop(state);

    gl_context.makeCurrentContext();
    gl_context.update(mtm);
    NSOpenGLContext::clearCurrentContext();
    log::debug!("embedded MPV drawable resized to {width}x{height}");
    inner.schedule_render();
}

#[allow(deprecated)]
unsafe fn detach_on_main(inner: &Inner) {
    let Ok(mut state) = inner.state.lock() else {
        return;
    };
    if !state.attached() {
        return;
    }

    let gl_view_ptr = state.gl_view as *mut NSOpenGLView;
    let gl_context_ptr = state.gl_context as *mut NSOpenGLContext;
    *state = RenderState::default();
    drop(state);

    unsafe {
        if let Some(view) = gl_view_ptr.as_ref() {
            view.clearGLContext();
            view.removeFromSuperview();
        }
        drop(Retained::from_raw(gl_context_ptr));
        drop(Retained::from_raw(gl_view_ptr));
    }
}
