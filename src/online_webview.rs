use gpui::{Bounds, Pixels, Window};
use raw_window_handle::{HandleError, HasWindowHandle, RawWindowHandle, WindowHandle};

pub(crate) struct OnlineParent {
    raw: RawWindowHandle,
    #[cfg(target_os = "macos")]
    _view: objc::rc::StrongPtr,
}

impl OnlineParent {
    pub(crate) fn capture(window: &Window) -> anyhow::Result<Self> {
        let raw = HasWindowHandle::window_handle(window)?.as_raw();
        #[cfg(target_os = "macos")]
        let _view = match raw {
            RawWindowHandle::AppKit(handle) => unsafe { objc::rc::StrongPtr::retain(handle.ns_view.as_ptr().cast()) },
            _ => anyhow::bail!("无法获取 macOS 窗口视图"),
        };
        Ok(Self { raw, #[cfg(target_os = "macos")] _view })
    }
}

impl HasWindowHandle for OnlineParent {
    fn window_handle(&self) -> Result<WindowHandle<'_>, HandleError> {
        // The parent was captured on the UI thread immediately before use;
        // on macOS its NSView is retained for the lifetime of this handle.
        Ok(unsafe { WindowHandle::borrow_raw(self.raw) })
    }
}

#[cfg(any(target_os = "windows", target_os = "macos"))]
use std::{cell::Cell, rc::Rc};
#[cfg(any(target_os = "windows", target_os = "macos"))]
use wry::{WebContext, WebView, WebViewBuilder, NewWindowResponse, Rect, dpi::{LogicalPosition, LogicalSize}};

#[cfg(any(target_os = "windows", target_os = "macos"))]
struct OnlineWebViewInner {
    // The WebView must be dropped before the context it uses.
    webview: WebView,
    visible: Cell<Option<bool>>,
    #[cfg(target_os = "windows")]
    bounds: Cell<Option<([f32; 4], [f32; 2], f32)>>,
    _context: WebContext,
}

#[cfg(any(target_os = "windows", target_os = "macos"))]
#[derive(Clone)]
pub(crate) struct OnlineWebView(Rc<OnlineWebViewInner>);

#[cfg(any(target_os = "windows", target_os = "macos"))]
impl OnlineWebView {
    pub(crate) fn new(parent: &OnlineParent, url: &str) -> anyhow::Result<Self> {
        // Wry uses this directory on Windows; WKWebView uses its default
        // persistent website data store on macOS.
        let mut context = WebContext::new(Some(fdl_core::store::Store::root_dir().join(".webview")));
        let webview = WebViewBuilder::new_with_web_context(&mut context)
            .with_visible(false)
            .with_focused(false)
            .with_clipboard(true)
            .with_new_window_req_handler(|_, _| NewWindowResponse::Allow)
            .build_as_child(parent)?;
        let inner = Rc::new(OnlineWebViewInner {
            webview, visible: Cell::new(Some(false)),
            #[cfg(target_os = "windows")]
            bounds: Cell::new(None),
            _context: context,
        });
        let view = Self(inner);
        view.load_url(url)?;
        Ok(view)
    }

    pub(crate) fn load_url(&self, url: &str) -> anyhow::Result<()> {
        self.0.webview.load_url(url)?;
        Ok(())
    }

    pub(crate) fn set_bounds(&self, bounds: Bounds<Pixels>, _window: &Window) -> anyhow::Result<()> {
        #[cfg(target_os = "windows")]
        let geometry = (
            [bounds.origin.x.into(), bounds.origin.y.into(), bounds.size.width.into(), bounds.size.height.into()],
            [_window.viewport_size().width.into(), _window.viewport_size().height.into()],
            _window.scale_factor(),
        );
        #[cfg(target_os = "windows")]
        if self.0.bounds.get() == Some(geometry) { return Ok(()); }
        #[cfg(target_os = "windows")]
        self.0.bounds.set(None);
        self.0.webview.set_bounds(Rect {
            position: LogicalPosition::new(f64::from(bounds.origin.x), f64::from(bounds.origin.y)).into(),
            size: LogicalSize::new(f64::from(bounds.size.width), f64::from(bounds.size.height)).into(),
        })?;
        #[cfg(target_os = "windows")]
        self.0.bounds.set(Some(geometry));
        Ok(())
    }

    pub(crate) fn set_visible(&self, visible: bool) -> anyhow::Result<()> {
        if self.0.visible.get() == Some(visible) {
            return Ok(());
        }
        // A failed native call may have changed only part of the visibility state.
        self.0.visible.set(None);
        self.0.webview.set_visible(visible)?;
        self.0.visible.set(Some(visible));
        Ok(())
    }
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
#[derive(Clone)]
pub(crate) struct OnlineWebView;

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
impl OnlineWebView {
    pub(crate) fn new(_parent: &OnlineParent, _url: &str) -> anyhow::Result<Self> {
        anyhow::bail!("在线文档内嵌浏览仅支持 Windows 和 macOS")
    }

    pub(crate) fn load_url(&self, _url: &str) -> anyhow::Result<()> {
        anyhow::bail!("在线文档内嵌浏览仅支持 Windows 和 macOS")
    }

    pub(crate) fn set_bounds(&self, _bounds: Bounds<Pixels>, _window: &Window) -> anyhow::Result<()> {
        anyhow::bail!("在线文档内嵌浏览仅支持 Windows 和 macOS")
    }

    pub(crate) fn set_visible(&self, _visible: bool) -> anyhow::Result<()> {
        anyhow::bail!("在线文档内嵌浏览仅支持 Windows 和 macOS")
    }
}
