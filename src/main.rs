#![recursion_limit = "512"]
#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

mod app;
mod shared_store;
mod quick_note;
mod assets;
mod guides;
mod clipboard;
mod devtools;
mod docinfo;
mod docmenu;
mod editor;
mod highlight;
mod feishu_links;
mod feishu_fetch;
mod feishu_sync;
mod home;
mod html;
mod input;
mod modes;
mod online_webview;
mod printing;
mod search;
mod sidebar;
mod tab_bar;
mod settings;
mod themes;
mod shortcuts;
#[allow(dead_code, unused_imports)]
mod feishu_theme;
#[allow(dead_code, unused_imports)]
mod editor_theme;
#[allow(dead_code, unused_imports)]
mod editor_ui;
mod theme;
mod transfer;
mod trash;
mod tree;
mod ui;

use fdl_core::{docxml, lang, markdown, model, palette, paste, store, time};

use gpui::*;

#[cfg(target_os = "macos")]
fn set_app_icon() {
    use objc::{class, msg_send, sel, sel_impl};
    use objc::runtime::Object;
    let png = include_bytes!("../assets/branding/app-icon.png");
    unsafe {
        let data: *mut Object = msg_send![class!(NSData), dataWithBytes:png.as_ptr() length:png.len()];
        let image: *mut Object = msg_send![class!(NSImage), alloc];
        let image: *mut Object = msg_send![image, initWithData:data];
        if !image.is_null() {
            let app: *mut Object = msg_send![class!(NSApplication), sharedApplication];
            let _: () = msg_send![app, setApplicationIconImage:image];
            let _: () = msg_send![image, release];
        }
    }
}

fn main() {
    let scripted = std::env::var_os("FDL_SCRIPT").is_some();
    // No single-instance guard: copies may share a library (builds tested
    // side by side). `Store` rereads an index another copy rewrote before
    // writing its own, and `guides` seeds a new library only once.
    if !scripted && let Err(err) = guides::initialize(&store::Store::root_dir()) {
        eprintln!("无法初始化开始指南：{err:#}");
        return;
    }
    Application::new().with_assets(assets::Assets).run(move |cx: &mut App| {
        #[cfg(target_os = "macos")]
        set_app_icon();
        editor::math::load_fonts(cx);
        input::bind_keys(cx);
        editor::keymap::bind_keys(cx);
        editor::image::bind_keys(cx);
        app::bind_keys(cx);
        settings::load();
        themes::load(&settings::prefs().theme_id(), false, || cx.text_system().available_font_names());
        shortcuts::init(cx);
        cx.set_global(shared_store::SharedStore::open().expect("open document library"));
        let quick_enabled = !scripted;
        if quick_enabled { quick_note::install(cx); }
        app::set_menus(cx);
        // Scripted dev runs must not steal keyboard focus from whatever the
        // user is typing into.
        let bounds = Bounds::centered(None, quick_note::main_window_size(cx), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                window_background: WindowBackgroundAppearance::Opaque,
                titlebar: Some(TitlebarOptions {
                    title: Some("quiet.md".into()),
                    appears_transparent: cfg!(any(target_os = "macos", target_os = "windows")),
                    traffic_light_position: Some(point(px(20.), px(app::HEADER_H / 2. - 7.))),
                }),
                window_min_size: Some(size(px(480.), px(480.))),
                focus: !scripted,
                ..Default::default()
            },
            |window, cx| {
                devtools::start(window, cx);
                cx.new(|cx| app::Workspace::new(window, cx))
            },
        )
        .expect("open window");
        cx.on_window_closed(move |cx| {
            if (!quick_enabled || !cfg!(target_os = "macos")) && cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();
        if !scripted {
            cx.activate(true);
        }
    });
}
