//! 下载为 → PDF and 打印: the exported HTML page loads in an off-screen
//! WKWebView, then goes through an `NSPrintOperation` on A4 — saved straight
//! to a file for PDF, or behind the system print panel for 打印.

use gpui::*;
use std::path::PathBuf;
#[cfg(target_os = "macos")]
use std::time::Duration;

pub const SUPPORTED: bool = cfg!(target_os = "macos");

pub enum Job {
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))] // The native PDF backend is macOS-only.
    Pdf(PathBuf),
    Print,
}

/// Lays out `html` and runs the job; `Ok(false)` when the user cancelled
/// the print panel.
pub async fn run(html: String, title: String, job: Job, ns_view: usize, cx: &mut AsyncApp) -> Result<bool, String> {
    #[cfg(target_os = "macos")]
    {
        native::run(html, title, job, ns_view, cx).await
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (html, title, job, ns_view, cx);
        Err(crate::settings::text("当前系统不支持").into())
    }
}

/// The GPUI window's NSView (the print panel opens as its sheet).
pub fn ns_view_of(window: &Window) -> usize {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    match HasWindowHandle::window_handle(window).ok().map(|h| h.as_raw()) {
        Some(RawWindowHandle::AppKit(h)) => h.ns_view.as_ptr() as usize,
        _ => 0,
    }
}

#[cfg(target_os = "macos")]
mod native {
    use super::*;
    use core_graphics::geometry::{CGPoint, CGRect, CGSize};
    use objc::declare::ClassDecl;
    use objc::runtime::{BOOL, Class, NO, Object, Sel, YES};
    use objc::{class, msg_send, sel, sel_impl};
    use std::sync::atomic::{AtomicU8, Ordering};

    #[link(name = "WebKit", kind = "framework")]
    unsafe extern "C" {}

    #[link(name = "AppKit", kind = "framework")]
    unsafe extern "C" {
        static NSPrintJobSavingURL: *mut Object;
        static NSPrintSaveJob: *mut Object;
    }

    /// A4 in points, and the page margins.
    const PAPER: (f64, f64) = (595.28, 841.89);
    const MARGIN_X: f64 = 48.;
    const MARGIN_Y: f64 = 40.;
    /// CSS px per point when printing (96 dpi on 72 dpi paper).
    const PX_PER_PT: f64 = 96. / 72.;
    const LOAD_TIMEOUT: Duration = Duration::from_secs(20);

    /// 0 running, 1 done, 2 cancelled or failed.
    static STATE: AtomicU8 = AtomicU8::new(0);

    extern "C" fn did_run(_: &Object, _: Sel, _op: *mut Object, success: BOOL, _ctx: *mut std::ffi::c_void) {
        STATE.store(if success == YES { 1 } else { 2 }, Ordering::SeqCst);
    }

    fn delegate_class() -> &'static Class {
        if let Some(c) = Class::get("FDLPrintDelegate") {
            return c;
        }
        let mut decl = ClassDecl::new("FDLPrintDelegate", class!(NSObject)).expect("FDLPrintDelegate");
        unsafe {
            decl.add_method(
                sel!(printOperationDidRun:success:contextInfo:),
                did_run as extern "C" fn(&Object, Sel, *mut Object, BOOL, *mut std::ffi::c_void),
            );
        }
        decl.register()
    }

    unsafe fn ns_string(s: &str) -> *mut Object {
        unsafe {
            let o: *mut Object = msg_send![class!(NSString), alloc];
            let o: *mut Object = msg_send![o, initWithBytes: s.as_ptr() length: s.len() encoding: 4usize];
            msg_send![o, autorelease]
        }
    }

    fn rect(x: f64, y: f64, w: f64, h: f64) -> CGRect {
        CGRect::new(&CGPoint::new(x, y), &CGSize::new(w, h))
    }

    pub async fn run(html: String, title: String, job: Job, ns_view: usize, cx: &mut AsyncApp) -> Result<bool, String> {
        let html = cx.background_executor().spawn(async move { crate::html::prepare_print_html(html) }).await;
        let width_px = (PAPER.0 - 2. * MARGIN_X) * PX_PER_PT;
        let (holder, web) = unsafe {
            let holder: *mut Object = msg_send![class!(NSWindow), alloc];
            let holder: *mut Object = msg_send![holder,
                initWithContentRect: rect(-20000., -20000., width_px, 1000.)
                styleMask: 0usize
                backing: 2usize
                defer: NO];
            let _: () = msg_send![holder, setReleasedWhenClosed: NO];
            let conf: *mut Object = msg_send![class!(WKWebViewConfiguration), new];
            let web: *mut Object = msg_send![class!(WKWebView), alloc];
            let web: *mut Object = msg_send![web, initWithFrame: rect(0., 0., width_px, 1000.) configuration: conf];
            let _: () = msg_send![conf, release];
            let _: () = msg_send![holder, setContentView: web];
            let nil: *mut Object = std::ptr::null_mut();
            let _: *mut Object = msg_send![web, loadHTMLString: ns_string(&html) baseURL: nil];
            (holder, web)
        };
        let cleanup = move || unsafe {
            let _: () = msg_send![holder, setContentView: std::ptr::null_mut::<Object>()];
            let _: () = msg_send![web, release];
            let _: () = msg_send![holder, close];
            let _: () = msg_send![holder, release];
        };
        let started = std::time::Instant::now();
        loop {
            cx.background_executor().timer(Duration::from_millis(50)).await;
            let loading: BOOL = unsafe { msg_send![web, isLoading] };
            if loading == NO {
                break;
            }
            if started.elapsed() > LOAD_TIMEOUT {
                cleanup();
                return Err(crate::settings::text("打印超时，请稍后重试").into());
            }
        }
        // Let images decode and the page settle before it is paginated.
        cx.background_executor().timer(Duration::from_millis(300)).await;
        STATE.store(0, Ordering::SeqCst);
        let print = matches!(job, Job::Print);
        unsafe {
            let shared: *mut Object = msg_send![class!(NSPrintInfo), sharedPrintInfo];
            let info: *mut Object = msg_send![shared, copy];
            let _: () = msg_send![info, setPaperSize: CGSize::new(PAPER.0, PAPER.1)];
            let _: () = msg_send![info, setOrientation: 0isize];
            let _: () = msg_send![info, setLeftMargin: MARGIN_X];
            let _: () = msg_send![info, setRightMargin: MARGIN_X];
            let _: () = msg_send![info, setTopMargin: MARGIN_Y];
            let _: () = msg_send![info, setBottomMargin: MARGIN_Y];
            let _: () = msg_send![info, setHorizontalPagination: 1usize]; // NSPrintingPaginationModeFit
            let _: () = msg_send![info, setVerticalPagination: 0usize]; // automatic
            let _: () = msg_send![info, setHorizontallyCentered: NO];
            let _: () = msg_send![info, setVerticallyCentered: NO];
            if let Job::Pdf(path) = &job {
                let _: () = msg_send![info, setJobDisposition: NSPrintSaveJob];
                let url: *mut Object = msg_send![class!(NSURL), fileURLWithPath: ns_string(&path.to_string_lossy())];
                let dict: *mut Object = msg_send![info, dictionary];
                let _: () = msg_send![dict, setObject: url forKey: NSPrintJobSavingURL];
            }
            let op: *mut Object = msg_send![web, printOperationWithPrintInfo: info];
            let _: () = msg_send![info, release];
            let _: () = msg_send![op, setShowsPrintPanel: if print { YES } else { NO }];
            let _: () = msg_send![op, setShowsProgressPanel: NO];
            let _: () = msg_send![op, setJobTitle: ns_string(&title)];
            // WebKit paginates its print view at this frame's width.
            let view: *mut Object = msg_send![op, view];
            let _: () = msg_send![view, setFrame: rect(0., 0., width_px, 1000.)];
            let delegate: *mut Object = msg_send![delegate_class(), new];
            let parent: *mut Object = if ns_view != 0 { msg_send![ns_view as *mut Object, window] } else { holder };
            let _: () = msg_send![op,
                runOperationModalForWindow: parent
                delegate: delegate
                didRunSelector: sel!(printOperationDidRun:success:contextInfo:)
                contextInfo: std::ptr::null_mut::<std::ffi::c_void>()];
            // The operation keeps no strong reference to its delegate.
            let keep = delegate as usize;
            loop {
                cx.background_executor().timer(Duration::from_millis(100)).await;
                if STATE.load(Ordering::SeqCst) != 0 {
                    break;
                }
            }
            let _: () = msg_send![keep as *mut Object, release];
        }
        cleanup();
        Ok(STATE.load(Ordering::SeqCst) == 1)
    }
}
