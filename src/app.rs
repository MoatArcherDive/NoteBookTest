//! Workspace window: two panes — the document list on the left
//! (see `sidebar.rs`) and the editor on the right, with the
//! 64px top bar (breadcrumb title + save status) above the page.

use crate::docinfo::DocInfo;
use crate::docmenu::DocSub;
use crate::editor::view::{Editor, EditorEvent};
use crate::input::{TextField, TextFieldEvent};
use crate::settings::display_title;
use crate::store::Entry;
use crate::tab_bar::{TabKey, TabLabel};
use crate::shared_store::SharedStore;
use crate::theme::{self, Theme, craft};
use crate::ui::{TipExt, TipSide, TitleBarExt, ToastKind, icon, ui_text};
use gpui::{prelude::FluentBuilder, *};
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::time::{Duration, Instant};

actions!(workspace, [NewDoc, ImportDoc, ToggleSidebar, IncreaseDocumentZoom, DecreaseDocumentZoom, FocusSearch, FindInDoc, ReplaceInDoc, OpenSettings, OpenAbout, GoHome, CloseWindow, Quit]);

/// The menu bar, in the interface language.
pub fn set_menus(cx: &mut App) {
    use crate::settings::tr;
    cx.set_menus(vec![
        Menu {
            name: "quiet.md".into(),
            items: vec![
                MenuItem::action(tr("关于 quiet.md", "About quiet.md"), OpenAbout),
                MenuItem::separator(),
                MenuItem::action(tr("设置…", "Settings…"), OpenSettings),
                MenuItem::separator(),
                MenuItem::action(tr("退出 quiet.md", "Quit quiet.md"), Quit),
            ],
        },
        Menu {
            name: tr("文件", "File").into(),
            items: vec![
                MenuItem::action(tr("新建文档", "New Document"), NewDoc),
                MenuItem::action(tr("新建笔记窗口", "New Note Window"), crate::quick_note::QuickNew),
                MenuItem::action(tr("导入为文档…", "Import as Document…"), ImportDoc),
                MenuItem::separator(),
                MenuItem::action(tr("关闭窗口", "Close Window"), CloseWindow),
            ],
        },
    ]);
}

pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("cmd-n", NewDoc, None),
        KeyBinding::new("cmd-shift-n", crate::quick_note::QuickNew, None),
        KeyBinding::new("cmd-,", OpenSettings, None),
        KeyBinding::new("cmd-shift-i", ImportDoc, None),
        KeyBinding::new("cmd-alt-home", GoHome, None),
        KeyBinding::new("cmd-w", CloseWindow, None),
        KeyBinding::new("cmd-]", ToggleSidebar, None),
        KeyBinding::new("cmd-\\", ToggleSidebar, None),
        KeyBinding::new("cmd-=", IncreaseDocumentZoom, None),
        KeyBinding::new("cmd-+", IncreaseDocumentZoom, None),
        KeyBinding::new("cmd--", DecreaseDocumentZoom, None),
        KeyBinding::new("cmd-j", FocusSearch, None),
        KeyBinding::new("cmd-f", FindInDoc, None),
        KeyBinding::new("cmd-shift-h", ReplaceInDoc, None),
        KeyBinding::new("cmd-q", Quit, None),
    ]);
    cx.on_action(|_: &CloseWindow, cx| {
        if let Some(handle) = cx.active_window().and_then(|w| w.downcast::<Workspace>()) {
            // Key dispatch already borrows this window; close after it returns.
            cx.defer(move |cx| {
                if let Err(e) = handle.update(cx, |ws, window, cx| {
                    crate::quick_note::persist(cx);
                    if ws.save_before(cx, |_, window, _| window.remove_window()) { window.remove_window(); }
                }) { eprintln!("无法关闭笔记窗口：{e}"); }
            });
        }
    });
    cx.on_action(|_: &Quit, cx| {
        // The open document is saved first; when that fails its dialog asks
        // (重试 / 放弃修改) and quits afterwards.
        let ready = cx
            .windows()
            .into_iter()
            .filter_map(|w| w.downcast::<Workspace>())
            .all(|w| w.update(cx, |ws, _, cx| ws.save_before(cx, |_, _, cx| cx.quit())).unwrap_or(true));
        if ready {
            cx.quit();
        }
    });
    crate::modes::bind_keys(cx);
}

#[derive(Clone, Debug, PartialEq)]
pub enum SaveStatus {
    /// Freshly opened, nothing saved this session: show the modification time.
    Idle,
    Saved,
    Saving,
    Failed(String),
}

struct LocalTabState {
    core: crate::editor::core::Core,
    scroll: ListOffset,
}

struct OnlineTab {
    id: u64,
    url: String,
    title: String,
    webview: Option<crate::online_webview::OnlineWebView>,
    loading: bool,
    error: Option<String>,
}

enum OpenTab {
    Local { id: String, state: Option<LocalTabState> },
    Online(OnlineTab),
}

impl OpenTab {
    fn matches(&self, key: &TabKey) -> bool {
        match (self, key) {
            (Self::Local { id, .. }, TabKey::Local(other)) => id == other,
            (Self::Online(tab), TabKey::Online(url)) => &tab.url == url,
            _ => false,
        }
    }

    fn key(&self) -> TabKey {
        match self {
            Self::Local { id, .. } => TabKey::Local(id.clone()),
            Self::Online(tab) => TabKey::Online(tab.url.clone()),
        }
    }
}

/// Right-click / "…" menu on a sidebar row.
#[derive(Clone, Debug)]
pub struct RowMenu {
    pub id: String,
    pub pos: Point<Pixels>,
    /// Which corner of the menu sits at `pos`.
    pub corner: Corner,
    /// 下载 submenu open.
    pub sub: bool,
    /// The 批量操作 menu for the checked rows.
    pub batch: bool,
}

/// What the main pane shows: the open document, 主页 or 回收站.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum View {
    Doc,
    Home,
    Trash,
    Settings,
}

/// A confirmation dialog and the pages it is about.
#[derive(Clone, Debug)]
pub enum Confirm {
    /// 删除 (to the 回收站); `batch` for the 批量操作 selection.
    Delete { ids: Vec<String>, batch: bool },
    /// 回收站 → 恢复.
    Restore(String),
    /// 回收站 → 彻底删除.
    Purge(String),
    /// 隐藏引用列表 (with 不再提示) / 显示引用列表.
    Refs { show: bool, no_ask: bool },
    /// The open document could not be written before it was to be replaced
    /// (another document, 新建, 退出): 重试 / 放弃修改, then carry on.
    SaveFailed,
}

/// What to do once the open document is safely on disk (or its changes given up).
pub type AfterSave = Box<dyn FnOnce(&mut Workspace, &mut Window, &mut Context<Workspace>)>;

/// Inline 重命名 of a sidebar row.
pub struct Rename {
    pub id: String,
    pub field: Entity<TextField>,
    _subs: Vec<Subscription>,
}

pub struct Workspace {
    pub quick_note: bool,
    pub shortcut_recording: Option<String>,
    pub shortcut_strokes: Vec<String>,
    /// The failed change: (action, message); an empty action is page-wide.
    pub shortcut_error: Option<(String, String)>,
    pub settings_page: crate::settings::Page,
    /// 外观's theme dropdown while open: the theme its preview shows.
    pub theme_menu: Option<String>,
    /// Appearance dialog opened from the title bar or settings.
    pub appearance_dialog: bool,
    pub(crate) appearance_target: crate::settings::AppearanceTarget,
    pub appearance_font_scroll: ScrollHandle,
    pub appearance_font_drag: Rc<Cell<Option<Pixels>>>,
    /// Where the theme dropdown's button was drawn last frame.
    pub theme_anchor: std::rc::Rc<std::cell::Cell<Option<Bounds<Pixels>>>>,
    /// 快捷键's filter field.
    pub settings_filter: Entity<TextField>,
    /// Custom document background color in the appearance settings.
    pub background_hex: Entity<TextField>,
    pub background_hex_error: bool,
    pub store: SharedStore,
    pub entries: Vec<Entry>,
    pub current: Option<String>,
    pub editor: Option<Entity<Editor>>,
    pub feishu_links: Entity<crate::feishu_links::FeishuLinks>,
    tabs: Vec<OpenTab>,
    active_tab: Option<usize>,
    next_online_tab: u64,
    tab_scroll: ScrollHandle,
    feishu_source: bool,
    feishu_error: Option<String>,
    feishu_syncing: HashSet<String>,
    feishu_downloads: Vec<(u64, String, Option<crate::feishu_fetch::FetchProgress>)>,
    next_feishu_download: u64,
    pub search: Entity<TextField>,
    /// Keyboard highlight inside search results.
    pub search_sel: Option<usize>,
    pub sidebar_open: bool,
    /// Dragged width (300 by default, 268 to min(800, window − 540)).
    pub sidebar_width: f32,
    /// The resize handle is being dragged.
    pub sidebar_drag: bool,
    /// The search dialog (command-center style) is open.
    pub search_open: bool,
    pub status: SaveStatus,
    pub row_menu: Option<RowMenu>,
    pub(crate) sidebar_icon: Option<crate::sidebar::SidebarIconPicker>,
    pub rename: Option<Rename>,
    /// Last-frame bounds of each row's "…" button, to hang its menu from.
    pub more_bounds: Rc<RefCell<HashMap<String, Bounds<Pixels>>>>,
    /// Header "…" menu and its open submenu.
    pub doc_menu: bool,
    pub doc_sub: Option<DocSub>,
    /// Last-frame bounds of the open submenu (clicks there keep the menu open).
    pub doc_sub_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    pub doc_info: Option<DocInfo>,
    pub list_scroll: ScrollHandle,
    pub search_scroll: ScrollHandle,
    /// Last-frame bounds of the row menu's 下载 submenu.
    pub row_sub_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    /// 批量操作 mode: the checked docs of 我的文档, in check order.
    pub batch: Option<Vec<String>>,
    /// The open confirmation dialog.
    pub confirm: Option<Confirm>,
    pub view: View,
    pub home: crate::home::Home,
    pub settings_scroll: ScrollHandle,
    /// 回收站 rows, refreshed when the page opens or changes.
    pub trash: Vec<crate::store::TrashEntry>,
    /// A 回收站 row's menu: the page and where it opens.
    pub trash_menu: Option<(String, Point<Pixels>, Corner)>,
    /// The toast and when it appeared.
    pub toast: Option<(ToastKind, SharedString, Instant)>,
    toast_task: Option<Task<()>>,
    /// Folded sidebar sections: 置顶, 我的文档.
    pub folded: [bool; 2],
    /// Expanded pages of the 我的文档 tree.
    pub expanded: HashSet<String>,
    pub move_dlg: Option<crate::tree::MoveDialog>,
    /// 导出 Markdown 设置.
    pub export_md: Option<crate::transfer::ExportMd>,
    /// 编辑 / 阅读 dropdown, and the mode button's last-frame bounds.
    pub mode_menu: bool,
    pub mode_btn_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    /// The header "…" button, which the doc menu hangs from.
    pub more_btn_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    /// The 快捷键 drawer, and the help button's menu (open while hovered).
    pub shortcuts: bool,
    pub(crate) focus: FocusHandle,
    save_task: Option<Task<()>>,
    /// The open document has changes not yet on disk (cleared only by a
    /// successful save or by 放弃修改).
    unsaved: bool,
    /// Held back by a failed save: runs after 重试 succeeds or 放弃修改.
    pub(crate) after_save: Option<AfterSave>,
    editor_sub: Option<Subscription>,
    _subs: Vec<Subscription>,
}


/// A macOS notification (only logged when scripted).
fn notify_system(title: &str, body: &str) {
    if std::env::var_os("FDL_SCRIPT").is_some() {
        return eprintln!("notification: {title} / {body}");
    }
    let q = |s: &str| s.replace('\\', "\\\\").replace('"', "\\\"");
    let script = format!("display notification \"{}\" with title \"{}\"", q(body), q(title));
    std::thread::spawn(move || std::process::Command::new("osascript").arg("-e").arg(script).status());
}

impl Workspace {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        Self::new_window(false, window, cx)
    }

    pub fn new_window(quick: bool, window: &mut Window, cx: &mut Context<Self>) -> Self {
        Self::build_window(quick, None, window, cx)
    }

    pub fn new_document_window(document: Option<String>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        Self::build_window(true, document, window, cx)
    }

    fn build_window(quick: bool, document: Option<String>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        crate::settings::load();
        if !cx.has_global::<SharedStore>() { cx.set_global(SharedStore::open().expect("open document library")); }
        let mut store = cx.global::<SharedStore>().clone();
        let entries = store.list();
        let search = cx.new(|cx| {
            let mut f = TextField::new("搜索", cx);
            f.placeholder_color = Theme::text_disabled();
            f
        });
        let settings_filter = cx.new(|cx| {
            let mut f = TextField::new(crate::settings::tr("搜索快捷键", "Search shortcuts"), cx);
            f.placeholder_color = Theme::text_disabled();
            f
        });
        let background_hex = cx.new(|cx| {
            let mut f = TextField::new("#FFFFFF", cx);
            f.mono = true;
            f.set_text(crate::settings::prefs().document_background.as_deref().unwrap_or(""), cx);
            f
        });
        let feishu_links = cx.new(|cx| crate::feishu_links::FeishuLinks::new(window, cx));
        let subs = vec![
            cx.subscribe_in(&feishu_links, window, |this, _, ev: &crate::feishu_links::FeishuLinkOpen, window, cx| {
                if ev.mode == crate::feishu_links::LinkOpenMode::Online {
                    let valid_scheme = ev.url.split_once(':').is_some_and(|(scheme, rest)| {
                        (scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https"))
                            && rest.strip_prefix("//").is_some_and(|rest| {
                                rest.split(['/', '?', '#']).next().is_some_and(|host| !host.is_empty() && !host.contains(char::is_whitespace))
                            })
                    });
                    if valid_scheme {
                        this.open_online(ev.url.clone(), ev.name.clone(), window, cx);
                    } else {
                        this.show_toast_kind(ToastKind::Error, crate::settings::tr("在线打开需要有效的 HTTP 或 HTTPS 链接。", "Online opening requires a valid HTTP or HTTPS URL."), cx);
                    }
                } else {
                    let existing = if ev.force_download { None } else {
                        crate::feishu_sync::find_local_copy(&crate::store::Store::root_dir(), &ev.url, &this.entries).map(str::to_owned)
                    };
                    if let Some(id) = existing {
                        this.open(&id, window, cx);
                    } else {
                        this.choose_feishu_destination(ev.url.clone(), ev.name.clone(), ev.force_download, window, cx);
                    }
                }
            }),
            cx.subscribe_in(&feishu_links, window, |this, _, ev: &crate::feishu_links::FeishuIconOpen, window, cx| {
                let bounds = Bounds::new(ev.position, size(px(20.), px(20.)));
                this.sidebar_icon_picker(crate::sidebar::SidebarIconTarget::Feishu(ev.id.clone()), bounds, window, cx);
            }),
            cx.subscribe(&feishu_links, |this, _, _: &crate::feishu_links::FeishuLinkFormOpen, cx| {
                this.list_scroll.scroll_to_bottom();
                cx.notify();
            }),
            cx.observe(&feishu_links, |_, _, cx| cx.notify()),
            cx.subscribe(&settings_filter, |_, _, ev: &TextFieldEvent, cx| if matches!(ev, TextFieldEvent::Changed) { cx.notify(); }),
            cx.subscribe_in(&background_hex, window, |this, _, ev: &TextFieldEvent, window, cx| match ev {
                TextFieldEvent::Submit => this.apply_background_hex(cx),
                TextFieldEvent::Cancel if this.appearance_dialog => this.close_appearance_dialog(window, cx),
                TextFieldEvent::Changed if this.background_hex_error => { this.background_hex_error = false; cx.notify(); }
                _ => {}
            }),
            cx.observe_window_bounds(window, |this, window, cx| crate::quick_note::remember(window, this.quick_note, cx)),
            cx.observe_window_activation(window, |this, window, cx| {
                if window.is_window_active() { if let Some(id) = &this.current { crate::quick_note::used(id, cx); } this.entries = this.store.list(); this.sort(); cx.notify(); }
                else { this.cancel_shortcut_recording(cx); this.flush(cx); }
            }),
            cx.subscribe_in(&search, window, |this, _, ev: &TextFieldEvent, window, cx| {
                match ev {
                    TextFieldEvent::Changed => {
                        this.search_sel = this.default_search_sel(cx);
                        this.search_scroll.set_offset(point(px(0.), px(0.)));
                    }
                    TextFieldEvent::Up => this.move_search_sel(-1, cx),
                    TextFieldEvent::Down => this.move_search_sel(1, cx),
                    TextFieldEvent::Submit => {
                        if let Some(ix) = this.search_sel {
                            this.pick_search(ix, window, cx)
                        }
                    }
                    TextFieldEvent::Cancel => this.close_search(window, cx),
                    _ => {}
                }
                cx.notify();
            }),
            // Last resort (the system logging out): ⌘Q and the close button
            // go through `save_before` and ask when the save fails.
            cx.on_app_quit(|this, cx| {
                crate::quick_note::persist(cx);
                this.flush(cx);
                async {}
            }),
        ];
        let this = cx.weak_entity();
        window.on_window_should_close(cx, move |_, cx| {
            crate::quick_note::persist(cx);
            this.update(cx, |w, cx| { w.cancel_shortcut_recording(cx); w.save_before(cx, |_, window, _| window.remove_window()) }).unwrap_or(true)
        });
        if !quick { crate::settings::watch_themes(cx); }
        // 日期提醒: post what fell due since the last look (not what was
        // missed while the app was closed).
        cx.spawn(async |this, cx| {
            let mut last = crate::time::now_ms() as i64;
            loop {
                cx.background_executor().timer(Duration::from_secs(15)).await;
                let now = crate::time::now_ms() as i64;
                if this.update(cx, |w, cx| w.check_reminders(last, now, cx)).is_err() {
                    break;
                }
                last = now;
            }
        })
        .detach();
        let sidebar_width = store.settings().sidebar_width.unwrap_or(theme::SIDEBAR_WIDTH);
        let mut ws = Workspace {
            quick_note: quick,
            shortcut_recording: None,
            shortcut_strokes: vec![],
            shortcut_error: None,
            settings_page: Default::default(),
            theme_menu: None,
            appearance_dialog: false,
            appearance_target: crate::settings::AppearanceTarget::Document,
            appearance_font_scroll: ScrollHandle::new(),
            appearance_font_drag: Rc::default(),
            theme_anchor: Default::default(),
            settings_filter,
            background_hex,
            background_hex_error: false,
            store,
            entries,
            current: None,
            editor: None,
            feishu_links,
            tabs: vec![],
            active_tab: None,
            next_online_tab: 0,
            tab_scroll: ScrollHandle::new(),
            feishu_source: false,
            feishu_error: None,
            feishu_syncing: HashSet::new(),
            feishu_downloads: Vec::new(),
            next_feishu_download: 0,
            search,
            search_sel: None,
            sidebar_open: !quick,
            sidebar_width,
            sidebar_drag: false,
            search_open: false,
            status: SaveStatus::Idle,
            row_menu: None,
            sidebar_icon: None,
            rename: None,
            more_bounds: Default::default(),
            doc_menu: false,
            doc_sub: None,
            doc_sub_bounds: Rc::new(Cell::new(None)),
            doc_info: None,
            list_scroll: ScrollHandle::new(),
            search_scroll: ScrollHandle::new(),
            row_sub_bounds: Rc::new(Cell::new(None)),
            batch: None,
            confirm: None,
            view: View::Doc,
            home: Default::default(),
            settings_scroll: ScrollHandle::new(),
            trash: vec![],
            trash_menu: None,
            toast: None,
            toast_task: None,
            mode_menu: false,
            mode_btn_bounds: Rc::new(Cell::new(None)),
            more_btn_bounds: Rc::new(Cell::new(None)),
            shortcuts: false,
            folded: [false; 2],
            expanded: HashSet::new(),
            move_dlg: None,
            export_md: None,
            focus: cx.focus_handle(),
            save_task: None,
            unsaved: false,
            after_save: None,
            editor_sub: None,
            _subs: subs,
        };
        ws.sort();
        ws.load_home();
        match document.or_else(|| (!quick).then(|| ws.entries.first().map(|e| e.id.clone())).flatten()) {
            Some(id) => ws.open(&id, window, cx),
            None => ws.new_doc(window, cx),
        }
        // Kept in the top bar (not just a toast): the user should notice.
        let warnings = ws.store.take_warnings();
        if !warnings.is_empty() {
            let msg = warnings.iter().map(|s| crate::settings::diagnostic(s)).collect::<Vec<_>>().join(crate::settings::tr("；", "; "));
            ws.show_toast_kind(ToastKind::Error, &msg, cx);
            ws.status = SaveStatus::Failed(msg);
        }
        ws
    }

    /// Most recently modified first.
    pub(crate) fn sort(&mut self) {
        self.entries.sort_by_key(|e| std::cmp::Reverse(e.meta.modified));
    }

    pub fn query(&self, cx: &App) -> String {
        self.search.read(cx).text.trim().to_lowercase()
    }

    pub fn open(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.try_open(id, window, cx);
    }

    fn active_online(&self) -> Option<&OnlineTab> {
        match self.tabs.get(self.active_tab?)? {
            OpenTab::Online(tab) => Some(tab),
            OpenTab::Local { .. } => None,
        }
    }

    fn online_by_id_mut(&mut self, id: u64) -> Option<&mut OnlineTab> {
        self.tabs.iter_mut().find_map(|tab| match tab {
            OpenTab::Online(tab) if tab.id == id => Some(tab),
            _ => None,
        })
    }

    pub(crate) fn has_online_document(&self) -> bool { self.active_online().is_some() }

    fn tab_labels(&self) -> Vec<TabLabel> {
        self.tabs.iter().enumerate().map(|(index, tab)| {
            let (title, icon) = match tab {
                OpenTab::Local { id, .. } => self.entries.iter().find(|entry| &entry.id == id)
                    .map(|entry| (display_title(&entry.title), entry.icon.clone()))
                    .unwrap_or_else(|| (display_title(""), None)),
                OpenTab::Online(tab) => (tab.title.clone(), None),
            };
            TabLabel { key: tab.key(), title, icon, active: self.view == View::Doc && self.active_tab == Some(index) }
        }).collect()
    }

    fn hide_active_online(&mut self) {
        let Some(OpenTab::Online(tab)) = self.active_tab.and_then(|index| self.tabs.get_mut(index)) else { return };
        if let Some(error) = tab.webview.as_ref().and_then(|view| view.set_visible(false).err()) {
            tab.webview = None;
            tab.error = Some(crate::localized_format!(
                "无法显示应用内网页：{:#}", "Could not display the embedded web page: {:#}", error
            ));
        }
    }

    fn stash_active_editor(&mut self, cx: &mut Context<Self>) {
        let Some(OpenTab::Local { state, .. }) = self.active_tab.and_then(|index| self.tabs.get_mut(index)) else { return };
        let Some(editor) = self.editor.take() else { return };
        *state = Some(editor.update(cx, |editor, _| {
            let scroll = editor.list.logical_scroll_top();
            let mut core = std::mem::replace(&mut editor.core, crate::editor::core::Core::new(crate::model::Document::default()));
            core.break_coalescing();
            LocalTabState { core, scroll }
        }));
        self.editor_sub = None;
    }

    fn online_open_error(error: &anyhow::Error) -> String {
        if cfg!(target_os = "windows") {
            crate::localized_format!(
                "无法在应用内打开在线文档：{:#}。请确认已安装 WebView2 运行时。",
                "Could not open the online document in the app: {:#}. Please ensure the WebView2 runtime is installed.",
                error
            )
        } else {
            crate::localized_format!(
                "无法在应用内打开在线文档：{:#}",
                "Could not open the online document in the app: {:#}", error
            )
        }
    }

    fn start_online_tab(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(OpenTab::Online(tab)) = self.tabs.get_mut(index) else { return };
        if tab.loading || tab.webview.is_some() { return; }
        tab.loading = true;
        tab.error = None;
        let (id, url) = (tab.id, tab.url.clone());
        // WebView2 pumps native messages during creation. Run it on the UI
        // thread after the workspace borrow has been released.
        cx.spawn_in(window, async move |this, cx| {
            let Ok(parent) = cx.update(|window, _| crate::online_webview::OnlineParent::capture(window)) else { return };
            let result = parent.and_then(|parent| crate::online_webview::OnlineWebView::new(&parent, &url));
            let _ = this.update_in(cx, |w, _, cx| {
                if let Some(tab) = w.online_by_id_mut(id) {
                    tab.loading = false;
                    match result {
                        Ok(webview) => tab.webview = Some(webview),
                        Err(error) => tab.error = Some(Self::online_open_error(&error)),
                    }
                    cx.notify();
                }
            });
        }).detach();
    }

    fn activate_online_tab(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if self.active_tab != Some(index) {
            self.hide_active_online();
            self.stash_active_editor(cx);
        }
        self.active_tab = Some(index);
        self.current = None;
        self.editor = None;
        self.editor_sub = None;
        self.feishu_source = false;
        self.feishu_error = None;
        self.status = SaveStatus::Idle;
        self.view = View::Doc;
        self.row_menu = None;
        self.doc_menu = false;
        self.doc_sub = None;
        self.doc_info = None;
        self.mode_menu = false;
        self.shortcuts = false;
        self.tab_scroll.scroll_to_item(index);
        self.start_online_tab(index, window, cx);
        window.focus(&self.focus);
        cx.notify();
    }

    fn open_online(&mut self, url: String, name: String, window: &mut Window, cx: &mut Context<Self>) {
        let key = TabKey::Online(url.clone());
        if let Some(index) = self.tabs.iter().position(|tab| tab.matches(&key)) {
            if let OpenTab::Online(tab) = &mut self.tabs[index] { tab.title = name; }
            self.select_tab(key, window, cx);
            return;
        }
        let (retry_url, retry_name) = (url.clone(), name.clone());
        if !self.save_before(cx, move |w, window, cx| w.open_online(retry_url, retry_name, window, cx)) {
            return;
        }
        let id = self.next_online_tab;
        self.next_online_tab += 1;
        self.tabs.push(OpenTab::Online(OnlineTab { id, url, title: name, webview: None, loading: false, error: None }));
        self.activate_online_tab(self.tabs.len() - 1, window, cx);
    }

    pub(crate) fn select_tab(&mut self, key: TabKey, window: &mut Window, cx: &mut Context<Self>) {
        let Some(index) = self.tabs.iter().position(|tab| tab.matches(&key)) else { return };
        if let TabKey::Local(id) = &key {
            self.try_open(id, window, cx);
            return;
        }
        if self.active_tab != Some(index) {
            let retry = key.clone();
            if !self.save_before(cx, move |w, window, cx| w.select_tab(retry, window, cx)) { return; }
        }
        self.activate_online_tab(index, window, cx);
    }

    pub(crate) fn close_tab(&mut self, key: TabKey, window: &mut Window, cx: &mut Context<Self>) {
        let Some(index) = self.tabs.iter().position(|tab| tab.matches(&key)) else { return };
        let active = self.active_tab == Some(index);
        if active {
            let retry = key.clone();
            if !self.save_before(cx, move |w, window, cx| w.close_tab(retry, window, cx)) { return; }
            self.hide_active_online();
        }
        let view = self.view;
        self.tabs.remove(index);
        if active {
            self.active_tab = None;
            self.current = None;
            self.editor = None;
            self.editor_sub = None;
            self.feishu_source = false;
            self.feishu_error = None;
            self.status = SaveStatus::Idle;
            if let Some(next) = self.tabs.get(index.min(self.tabs.len().saturating_sub(1))).map(OpenTab::key) {
                self.select_tab(next, window, cx);
                if self.active_tab.is_none() { self.view = if view == View::Doc { View::Home } else { view }; }
                else if view != View::Doc { self.view = view; }
            } else {
                self.view = if view == View::Doc { View::Home } else { view };
            }
        } else if let Some(current) = self.active_tab && current > index {
            self.active_tab = Some(current - 1);
        }
        cx.notify();
    }

    fn forget_deleted_tabs(&mut self, deleted: &[String]) {
        let active = self.active_tab.and_then(|index| self.tabs.get(index)).map(OpenTab::key);
        self.tabs.retain(|tab| !matches!(tab, OpenTab::Local { id, .. } if deleted.contains(id)));
        self.active_tab = active.and_then(|key| self.tabs.iter().position(|tab| tab.matches(&key)));
    }

    /// Show `id`; false when it could not be opened (or the open document
    /// could not be saved first, see [`Self::save_before`]).
    pub fn try_open(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.current.as_deref() == Some(id) {
            self.view = View::Doc;
            if let Some(index) = self.active_tab { self.tab_scroll.scroll_to_item(index); }
            if !self.search_open && let Some(editor) = &self.editor { editor.read(cx).focus(window); }
            cx.notify();
            return true;
        }
        let target = id.to_string();
        if !self.save_before(cx, move |w, window, cx| {
            w.open(&target, window, cx);
        }) {
            return false;
        }
        let peers = self.document_peers(id, window, cx);
        let index = self.tabs.iter().position(|tab| matches!(tab, OpenTab::Local { id: open, .. } if open == id));
        let cached = if peers.is_empty() {
            match index.and_then(|index| match &self.tabs[index] {
                OpenTab::Local { state: Some(state), .. } => Some(&state.core.doc),
                _ => None,
            }).map(|doc| self.store.matches_document(id, doc)) {
                Some(Ok(true)) => index.and_then(|index| match &mut self.tabs[index] {
                    OpenTab::Local { state, .. } => state.take(),
                    _ => None,
                }),
                Some(Err(e)) => {
                    self.show_toast_kind(ToastKind::Error, &crate::localized_format!("无法打开文档：{}", "Could not open document: {}", crate::settings::diagnostic(&e.to_string())), cx);
                    cx.notify();
                    return false;
                }
                _ => None,
            }
        } else { None };
        let (core, restore_scroll) = if let Some(state) = cached {
            (state.core, Some(state.scroll))
        } else {
            match self.store.load_for_edit(id, !peers.is_empty()) {
                Ok(doc) => (crate::editor::core::Core::new(doc), None),
                Err(e) => {
                    self.show_toast_kind(ToastKind::Error, &crate::localized_format!("无法打开文档：{}", "Could not open document: {}", crate::settings::diagnostic(&e.to_string())), cx);
                    cx.notify();
                    return false;
                }
            }
        };
        let meta = self.entries.iter().find(|e| e.id == id).map(|e| e.meta.clone());
        let modified = meta.as_ref().map_or_else(crate::time::now, |m| m.modified);
        let width = meta.as_ref().map(|m| m.width).unwrap_or_default();
        let pinned = meta.as_ref().is_some_and(|m| !m.catalogue_closed);
        let editor = cx.new(|cx| {
            let mut e = Editor::new(core, modified, width, window, cx);
            e.restore_scroll = restore_scroll;
            e.cat_pinned = pinned;
            if let Some(m) = &meta {
                e.created = m.created;
                e.info = m.info;
                e.set_read_only(m.reading, cx);
            }
            e
        });
        // Reuse the live state, including unsaved edits and undo history.
        if let Some(peer) = peers.first() {
            let core = peer.read(cx).core.clone();
            editor.update(cx, |e, cx| e.receive_document(&core, cx));
        }
        self.hide_active_online();
        self.stash_active_editor(cx);
        self.editor_sub = Some(cx.subscribe_in(&editor, window, |this, _, ev: &EditorEvent, window, cx| match ev {
            EditorEvent::Changed => {
                // 阅读模式 put the document back: nothing to save.
                let rejected = this.editor.clone().is_some_and(|ed| ed.update(cx, |e, cx| e.reject_read_only_edit(cx)));
                if !rejected {
                    this.sync_document(window, cx);
                    this.schedule_save(window, cx);
                }
            }
            EditorEvent::Mode(reading) => this.set_mode(*reading, window, cx),
            EditorEvent::Info(info) => this.set_info_row(*info),
            EditorEvent::Toast => {
                this.toast = None;
                cx.notify();
            }
            EditorEvent::Save => {
                this.flush(cx);
            }
            EditorEvent::Search(q) => {
                this.open_search(window, cx);
                this.search.update(cx, |s, cx| s.set_text(q, cx));
                this.search_sel = this.default_search_sel(cx);
            }
            EditorEvent::OpenDoc(id) => this.open(id, window, cx),
            EditorEvent::OpenDocAt(id, ordinal) => this.open_at(id, *ordinal, window, cx),
            EditorEvent::HideRefs => this.ask_refs(false, cx),
            EditorEvent::Duplicate => {
                if let Some(id) = this.current.clone() {
                    this.duplicate_doc(&id, window, cx);
                }
            }
            EditorEvent::Catalogue(pinned) => {
                if let Some(id) = this.current.clone() {
                    if let Some(e) = this.entries.iter_mut().find(|e| e.id == id) {
                        e.meta.catalogue_closed = !pinned;
                    }
                    let _ = this.store.set_catalogue(&id, *pinned);
                }
            }
        }));
        // Opening a document puts the caret in it, unless the user is typing a search.
        if !self.search_open {
            editor.read(cx).focus(window);
        }
        if let Some(m) = self.store.touch(id)
            && let Some(e) = self.entries.iter_mut().find(|e| e.id == id) {
                e.meta.opened = m.opened;
            }
        let index = match index {
            Some(index) => {
                if let OpenTab::Local { state, .. } = &mut self.tabs[index] { *state = None; }
                index
            }
            None => { self.tabs.push(OpenTab::Local { id: id.to_string(), state: None }); self.tabs.len() - 1 }
        };
        self.active_tab = Some(index);
        self.tab_scroll.scroll_to_item(index);
        self.view = View::Doc;
        self.editor = Some(editor);
        self.current = Some(id.to_string());
        self.feishu_source = crate::feishu_sync::has_source(id);
        self.feishu_error = None;
        crate::quick_note::used(id, cx);
        self.reveal_in_tree(id);
        self.sync_sub_pages(cx);
        self.status = SaveStatus::Idle;
        self.row_menu = None;
        self.doc_menu = false;
        self.doc_sub = None;
        self.mode_menu = false;
        cx.notify();
        true
    }
    pub(crate) fn fetch_feishu(&mut self, url: String, name: String, target: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        let download_id = self.next_feishu_download;
        self.next_feishu_download += 1;
        self.feishu_downloads.push((download_id, name, Some(crate::feishu_fetch::FetchProgress::FetchingDocument)));
        self.show_toast_kind(ToastKind::Info, crate::settings::tr("正在获取飞书文档…", "Fetching Feishu document…"), cx);
        let (sender, receiver) = async_channel::unbounded();
        let task = cx.background_executor().spawn(async move {
            crate::feishu_fetch::fetch(&url, |progress| { let _ = sender.try_send(progress); })
        });
        cx.spawn_in(window, async move |this, cx| {
            while let Ok(progress) = receiver.recv().await {
                if this.update(cx, |w, cx| {
                    if let Some((_, _, stage)) = w.feishu_downloads.iter_mut().find(|(id, _, _)| *id == download_id) {
                        *stage = Some(progress);
                        cx.notify();
                    }
                }).is_err() { return; }
            }
            let fetched = match task.await {
                Ok(fetched) => fetched,
                Err(e) => {
                    let _ = this.update(cx, |w, cx| {
                        w.feishu_downloads.retain(|(id, _, _)| *id != download_id);
                        w.show_toast_kind(ToastKind::Error, &crate::localized_format!("获取飞书文档失败：{:#}", "Could not fetch Feishu document: {:#}", e), cx);
                    });
                    return;
                }
            };
            let (id, location_error) = match this.update(cx, |w, cx| {
                if let Some((_, _, stage)) = w.feishu_downloads.iter_mut().find(|(id, _, _)| *id == download_id) {
                    *stage = None;
                    cx.notify();
                }
                match w.store.create_from(&fetched.doc) {
                    Ok(id) => {
                        let location = match target.as_deref() {
                            Some(parent) if !crate::store::Store::root_dir().join("docs").join(format!("{parent}.xml")).is_file() =>
                                Err(crate::settings::tr("所选目标文档已不存在。", "The selected destination document no longer exists.").to_string()),
                            Some(parent) => w.store.set_parent(&id, Some(parent)).map(|_| ()).map_err(|e| format!("{e:#}")),
                            None => Ok(()),
                        };
                        w.entries = w.store.list();
                        w.sort();
                        w.folded[1] = false;
                        w.reveal_in_tree(&id);
                        w.sync_sub_pages(cx);
                        cx.notify();
                        Some((id, location.err()))
                    }
                    Err(e) => {
                        w.feishu_downloads.retain(|(id, _, _)| *id != download_id);
                        w.show_toast_kind(ToastKind::Error, &crate::localized_format!("创建飞书文档本地副本失败：{:#}", "Could not create local Feishu copy: {:#}", e), cx);
                        None
                    }
                }
            }) {
                Ok(Some(created)) => created,
                _ => return,
            };
            let register_id = id.clone();
            let task = cx.background_executor().spawn(async move { crate::feishu_sync::register(&register_id, &fetched) });
            let result = task.await;
            let _ = this.update_in(cx, |w, window, cx| {
                w.feishu_downloads.retain(|(id, _, _)| *id != download_id);
                cx.notify();
                match (location_error, result) {
                    (None, Ok(())) => {
                        if w.try_open(&id, window, cx) {
                            w.show_toast(crate::settings::tr("已打开飞书文档的本地副本", "Opened a local copy of the Feishu document"), cx);
                        }
                    }
                    (Some(location), Ok(())) => w.show_toast_kind(ToastKind::Error, &crate::localized_format!("保存下载位置失败：{}。本地副本和飞书来源绑定已保留。", "Could not save the download location: {}. The local copy and Feishu source link were kept.", location), cx),
                    (None, Err(e)) => w.show_toast_kind(ToastKind::Error, &crate::localized_format!("绑定飞书来源失败：{:#}。本地副本已保留，但不能同步。", "Could not link the Feishu source: {:#}. The local copy was kept but cannot sync.", e), cx),
                    (Some(location), Err(e)) => w.show_toast_kind(ToastKind::Error, &crate::localized_format!("保存下载位置失败：{}；绑定飞书来源也失败：{:#}。本地副本已保留，但不能同步。", "Could not save the download location: {}; linking the Feishu source also failed: {:#}. The local copy was kept but cannot sync.", location, e), cx),
                }
            });
        }).detach();
    }

    fn sync_feishu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.current.clone() else { return };
        if self.feishu_syncing.contains(&id) || !self.feishu_source { return; }
        self.feishu_error = None;
        if let Some(editor) = self.editor.clone() {
            let assigned = editor.update(cx, |e, cx| {
                let dirty = e.core.dirty;
                let assigned = e.core.ensure_sync_ids();
                if assigned { e.core.dirty = dirty; cx.notify(); }
                assigned
            });
            if assigned {
                self.unsaved = true;
                self.sync_document(window, cx);
            }
        }
        if !self.flush(cx) {
            self.show_toast_kind(ToastKind::Error, crate::settings::tr("本地保存失败，未同步飞书文档。", "Local save failed; the Feishu document was not synced."), cx);
            return;
        }
        let Some(editor) = &self.editor else { return };
        let doc = editor.read(cx).core.doc.clone();
        self.feishu_syncing.insert(id.clone());
        self.show_toast_kind(ToastKind::Info, crate::settings::tr("正在同步飞书文档…", "Syncing Feishu document…"), cx);
        let sync_id = id.clone();
        let task = cx.background_executor().spawn(async move { crate::feishu_sync::sync(&sync_id, &doc) });
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |w, cx| {
                w.feishu_syncing.remove(&id);
                match result {
                    Ok(()) => w.show_toast(crate::settings::tr("飞书文档同步成功", "Feishu document synced"), cx),
                    Err(e) => {
                        let message = crate::localized_format!("飞书文档同步失败：{:#}。本地内容已保留。", "Could not sync Feishu document: {:#}. Local changes were kept.", e);
                        if w.current.as_deref() == Some(id.as_str()) { w.feishu_error = Some(message.clone()); }
                        w.show_toast_kind(ToastKind::Error, &message, cx);
                    }
                }
            });
        }).detach();
    }


    pub fn new_doc(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.save_before(cx, |w, window, cx| w.new_doc(window, cx)) {
            return;
        }
        match self.store.create() {
            Ok((id, _)) => {
                self.search_open = false;
                self.doc_info = None;
                self.home.sort_menu = false;
                self.entries = self.store.list();
                self.sort();
                self.search.update(cx, |s, cx| s.set_text("", cx));
                self.open(&id, window, cx);
                if let Some(ed) = &self.editor {
                    ed.read(cx).focus(window);
                }
            }
            Err(e) => self.status = SaveStatus::Failed(crate::localized_format!("新建失败：{}", "Could not create document: {}", crate::settings::diagnostic(&e.to_string()))),
        }
        cx.notify();
    }

    /// Ask before 删除: a confirmation, then the 回收站.
    pub fn ask_delete(&mut self, id: &str, cx: &mut Context<Self>) {
        self.row_menu = None;
        self.doc_menu = false;
        self.doc_sub = None;
        self.confirm = Some(Confirm::Delete { ids: vec![id.to_string()], batch: false });
        cx.notify();
    }

    /// Move documents to the trash; if the open one goes, open the next.
    pub fn delete_docs(&mut self, ids: &[String], window: &mut Window, cx: &mut Context<Self>) {
        // A page goes with its child pages: each topmost page is
        // trashed as one item that carries its subtree.
        let tree = crate::tree::Tree::new(&self.entries);
        let mut all: Vec<(String, Option<String>)> = vec![];
        for id in ids {
            let covered = self.ancestors(id).iter().any(|a| ids.contains(&a.id));
            if covered {
                continue;
            }
            for d in tree.subtree(id) {
                if !all.iter().any(|(x, _)| *x == d) {
                    let root = (d != *id).then(|| id.clone());
                    all.push((d, root));
                }
            }
        }
        let groups = all;
        let all: Vec<String> = groups.iter().map(|(d, _)| d.clone()).collect();
        for handle in cx.windows().into_iter().filter(|w| w.window_id() != window.window_handle().window_id()) {
            if let Some(handle) = handle.downcast::<Workspace>()
                && handle.update(cx, |ws, _, _| ws.current.as_ref().is_some_and(|id| all.contains(id))).unwrap_or(false)
            {
                self.show_toast_kind(ToastKind::Error, crate::settings::text("请先关闭正在编辑这些文档的其他窗口，再删除"), cx);
                return;
            }
        }
        if self.current.as_ref().is_some_and(|c| all.contains(c)) {
            let retry = ids.to_vec();
            if !self.save_before(cx, move |w, window, cx| w.delete_docs(&retry, window, cx)) {
                return;
            }
        }
        let mut deleted = vec![];
        let mut errors = vec![];
        for (id, root) in &groups {
            // Do not split a subtree when its root could not be deleted.
            if root.as_ref().is_some_and(|r| !deleted.contains(r)) { continue; }
            match self.store.delete(id, root.as_deref()) {
                Ok(()) => deleted.push(id.clone()),
                Err(e) => errors.push(crate::localized_format!("删除失败：{}", "Could not delete document: {}", crate::settings::diagnostic(&e.to_string()))),
            }
        }
        let active_deleted = self.current.as_ref().is_some_and(|c| deleted.contains(c));
        let previous_tab = self.active_tab.unwrap_or(0);
        if active_deleted {
            self.save_task = None;
            self.unsaved = false;
            self.current = None;
            self.editor = None;
            self.editor_sub = None;
        }
        self.entries.retain(|e| !deleted.contains(&e.id));
        self.forget_deleted_tabs(&deleted);
        for handle in cx.windows().into_iter().filter(|w| w.window_id() != window.window_handle().window_id()).filter_map(|w| w.downcast::<Workspace>()) {
            let _ = handle.update(cx, |ws, _, cx| {
                ws.entries.retain(|entry| !deleted.contains(&entry.id));
                ws.forget_deleted_tabs(&deleted);
                ws.sync_sub_pages(cx);
                cx.notify();
            });
        }
        if let Some(b) = self.batch.as_mut() {
            b.retain(|id| !deleted.contains(id));
        }
        self.row_menu = None;
        if active_deleted {
            // Stay on 主页 / 回收站 if that's where the delete came from.
            let view = self.view;
            if let Some(next) = self.tabs.get(previous_tab.min(self.tabs.len().saturating_sub(1))).map(OpenTab::key) {
                self.select_tab(next, window, cx);
            } else {
                match self.entries.first().map(|e| e.id.clone()) {
                    Some(next) => self.open(&next, window, cx),
                    None => self.new_doc(window, cx),
                }
            }
            self.view = view;
        }
        self.sync_sub_pages(cx);
        if self.view == View::Trash {
            self.trash = self.store.list_trash();
        }
        errors.extend(self.store.take_warnings().iter().map(|s| crate::settings::diagnostic(s)));
        if errors.is_empty() {
            self.show_toast(crate::settings::text("删除成功"), cx);
        } else {
            let msg = errors.join("；");
            self.show_toast_kind(ToastKind::Error, &msg, cx);
            self.status = SaveStatus::Failed(msg);
        }
        cx.notify();
    }

    pub fn toggle_pin(&mut self, id: &str, cx: &mut Context<Self>) {
        let mut on = false;
        if let Some(e) = self.entries.iter_mut().find(|e| e.id == id) {
            e.meta.pinned = !e.meta.pinned;
            on = e.meta.pinned;
            let _ = self.store.set_pinned(id, e.meta.pinned);
        }
        self.sort();
        self.row_menu = None;
        self.show_toast(if on { crate::settings::text("已添加到“置顶”") } else { crate::settings::text("已从“置顶”移除") }, cx);
        cx.notify();
    }

    /// 收藏 / 取消收藏 (listed under 主页 → 收藏).
    pub fn toggle_star(&mut self, id: &str, cx: &mut Context<Self>) {
        let Some(e) = self.entries.iter_mut().find(|e| e.id == id) else { return };
        let on = e.meta.starred == 0;
        if let Ok(t) = self.store.set_starred(id, on) {
            e.meta.starred = t;
        }
        self.row_menu = None;
        self.doc_menu = false;
        self.doc_sub = None;
        self.show_toast(if on { crate::settings::text("已添加到“收藏”") } else { crate::settings::text("已从“收藏”中移除") }, cx);
        cx.notify();
    }

    /// Switch the main pane to 主页 or 回收站 (or back to the document).
    pub fn show_view(&mut self, view: View, cx: &mut Context<Self>) {
        self.cancel_shortcut_recording(cx);
        if view == View::Trash {
            self.trash = self.store.list_trash();
        }
        self.view = view;
        self.row_menu = None;
        self.trash_menu = None;
        self.theme_menu = None;
        self.appearance_dialog = false;
        self.home.sort_menu = false;
        cx.notify();
    }

    /// A success toast (删除成功, 移动成功 …).
    pub fn show_toast(&mut self, msg: &str, cx: &mut Context<Self>) {
        self.show_toast_kind(ToastKind::Success, msg, cx);
    }

    pub fn show_toast_kind(&mut self, kind: ToastKind, msg: &str, cx: &mut Context<Self>) {
        self.toast = Some((kind, SharedString::from(msg.to_string()), Instant::now()));
        if let Some(ed) = &self.editor {
            ed.update(cx, |e, cx| e.clear_toast(cx));
        }
        self.toast_task = Some(cx.spawn(async |this, cx| {
            cx.background_executor().timer(Duration::from_millis(2700)).await;
            let _ = this.update(cx, |_, cx| cx.notify());
        }));
        cx.notify();
    }

    fn render_toast(&self, window: &mut Window) -> Option<AnyElement> {
        let (kind, msg, t0) = self.toast.as_ref()?;
        crate::ui::toast_el(*kind, msg, *t0, window)
    }

    /// 重命名: the row's title turns into an input; Enter or clicking away saves, Esc cancels.
    pub fn start_rename(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(title) = self.entries.iter().find(|e| e.id == id).map(|e| e.title.trim().to_string()) else { return };
        let field = cx.new(|cx| {
            let mut f = TextField::new(crate::model::UNTITLED, cx);
            f.set_text(&title, cx);
            f.select_all_text(cx);
            f
        });
        let focus = field.focus_handle(cx);
        window.focus(&focus);
        let subs = vec![
            cx.subscribe_in(&field, window, |w, _, ev: &TextFieldEvent, window, cx| match ev {
                TextFieldEvent::Submit => w.finish_rename(true, window, cx),
                TextFieldEvent::Cancel => w.finish_rename(false, window, cx),
                _ => {}
            }),
            cx.on_blur(&focus, window, |w, window, cx| w.finish_rename(true, window, cx)),
        ];
        self.rename = Some(Rename { id: id.to_string(), field, _subs: subs });
        // From 主页 the row may be hidden: show it in the sidebar.
        self.sidebar_open = true;
        self.reveal_in_tree(id);
        self.row_menu = None;
        cx.notify();
    }

    fn finish_rename(&mut self, commit: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(r) = self.rename.take() else { return };
        let refocus = r.field.read(cx).is_focused(window);
        let title = r.field.read(cx).text.trim().to_string();
        let old = self.entries.iter().find(|e| e.id == r.id).map(|e| e.title.trim().to_string());
        if commit && old.is_some_and(|o| o != title) {
            if self.current.as_deref() == Some(r.id.as_str()) {
                // The open editor owns the title; its Changed event saves it.
                if let Some(ed) = &self.editor {
                    ed.update(cx, |e, cx| e.set_title(&title, cx));
                }
            } else if let Some(peer) = self.document_peers(&r.id, window, cx).first() {
                peer.update(cx, |e, cx| e.set_title(&title, cx));
            } else {
                match self.store.rename(&r.id, &title) {
                    Ok(meta) => {
                        if let Some(e) = self.entries.iter_mut().find(|e| e.id == r.id) {
                            e.title = title;
                            e.meta = meta;
                        }
                        self.sort();
                    }
                    Err(e) => self.status = SaveStatus::Failed(crate::localized_format!("重命名失败：{}", "Could not rename document: {}", crate::settings::diagnostic(&e.to_string()))),
                }
            }
        }
        if refocus
            && let Some(ed) = &self.editor {
                ed.read(cx).focus(window);
            }
        self.sync_sub_pages(cx);
        cx.notify();
    }

    fn document_peers(&self, id: &str, window: &Window, cx: &mut Context<Self>) -> Vec<Entity<Editor>> {
        cx.windows().into_iter().filter(|w| w.window_id() != window.window_handle().window_id())
            .filter_map(|w| w.downcast::<Workspace>())
            .filter_map(|w| w.update(cx, |ws, _, _| {
                (ws.current.as_deref() == Some(id)).then(|| ws.editor.clone()).flatten()
            }).ok().flatten()).collect()
    }

    pub(crate) fn set_sidebar_document_icon(&mut self, id: &str, icon: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        let editor = if self.current.as_deref() == Some(id) {
            self.editor.clone()
        } else {
            self.document_peers(id, window, cx).into_iter().next()
        };
        if let Some(editor) = editor {
            editor.update(cx, |e, cx| e.set_doc_icon(icon.clone(), cx));
            if let Some(entry) = self.entries.iter_mut().find(|e| e.id == id) {
                entry.icon = icon;
            }
        } else {
            let result = self.store.load(id).and_then(|mut doc| {
                if !crate::sidebar::update_document_icon(&mut doc, icon.clone()) { return Ok(None); }
                self.store.save(id, &doc).map(Some)
            });
            match result {
                Ok(meta) => {
                    if let Some(entry) = self.entries.iter_mut().find(|e| e.id == id) {
                        entry.icon = icon;
                        if let Some(meta) = meta { entry.meta = meta; }
                    }
                    self.sort();
                }
                Err(e) => {
                    let message = crate::localized_format!("保存失败：{}", "Could not save: {}", crate::settings::diagnostic(&e.to_string()));
                    self.show_toast_kind(ToastKind::Error, &message, cx);
                }
            }
        }
        self.sync_links(cx);
        self.sync_sub_pages(cx);
        cx.notify();
    }

    fn sync_document(&mut self, window: &Window, cx: &mut Context<Self>) {
        let (Some(id), Some(editor)) = (self.current.clone(), self.editor.clone()) else { return };
        let peers = self.document_peers(&id, window, cx);
        if peers.is_empty() { return; }
        let core = editor.read(cx).core.clone();
        for peer in peers { peer.update(cx, |e, cx| e.receive_document(&core, cx)); }
        for handle in cx.windows().into_iter().filter(|w| w.window_id() != window.window_handle().window_id()).filter_map(|w| w.downcast::<Workspace>()) {
            let _ = handle.update(cx, |ws, _, cx| {
                if let Some(entry) = ws.entries.iter_mut().find(|e| e.id == id) {
                    entry.title = core.doc.title.text.clone();
                    entry.icon = core.doc.icon.clone();
                }
                // Every remaining view can save the latest state if its sibling closes.
                if ws.current.as_deref() == Some(&id) { ws.unsaved = true; }
                cx.notify();
            });
        }
    }

    fn schedule_save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.status = SaveStatus::Saving;
        self.unsaved = true;
        // Keep the list title live while typing; the body/snippet updates on save.
        if let (Some(id), Some(ed)) = (&self.current, &self.editor) {
            let title = ed.read(cx).core.doc.title.text.clone();
            if let Some(e) = self.entries.iter_mut().find(|e| &e.id == id) {
                e.title = title;
            }
            if let Some(e) = self.entries.iter_mut().find(|e| &e.id == id) {
                e.icon = ed.read(cx).core.doc.icon.clone();
            }
        }
        self.save_task = Some(cx.spawn_in(window, async move |this, cx| {
            cx.background_executor().timer(Duration::from_millis(500)).await;
            let _ = this.update(cx, |w, cx| w.flush(cx));
        }));
        cx.notify();
    }

    fn check_reminders(&mut self, from: i64, to: i64, cx: &mut Context<Self>) {
        // The open document's reminders are read live (unsaved edits too).
        let open = self.editor.as_ref().map(|e| e.read(cx).core.doc.reminders());
        for e in &self.entries {
            let rs = match &open {
                Some(rs) if self.current.as_deref() == Some(e.id.as_str()) => rs,
                _ => &e.reminders,
            };
            for r in rs.iter().filter(|r| r.should_notify && from < r.notify && r.notify <= to) {
                notify_system(&display_title(&e.title), &crate::editor::reminder::notification(r));
            }
        }
    }

    /// Write the open document to disk if it has unsaved changes. False when
    /// that failed: the changes stay pending (a later save retries them) and
    /// the top bar shows the error.
    pub fn flush(&mut self, cx: &mut Context<Self>) -> bool {
        self.save_task = None;
        if !self.unsaved {
            return true;
        }
        let (Some(id), Some(ed)) = (self.current.clone(), self.editor.clone()) else {
            self.unsaved = false;
            return true;
        };
        let doc = &ed.read(cx).core.doc;
        let (title, icon, body, reminders, links) = (doc.title.text.clone(), doc.icon.clone(), doc.plain_text(), doc.reminders(), doc.doc_links());
        match self.store.save(&id, doc) {
            Ok(meta) => {
                if let Some(e) = self.entries.iter_mut().find(|e| e.id == id) {
                    e.title = title;
                    e.icon = icon;
                    e.body = body;
                    e.reminders = reminders;
                    e.links = links;
                    e.meta = meta.clone();
                }
                self.sync_links(cx);
                ed.update(cx, |e, cx| {
                    e.modified = meta.modified;
                    cx.notify();
                });
                self.status = SaveStatus::Saved;
                self.unsaved = false;
                self.sort();
            }
            Err(e) => self.status = SaveStatus::Failed(crate::localized_format!("保存失败：{}", "Could not save: {}", crate::settings::diagnostic(&e.to_string()))),
        }
        cx.notify();
        !self.unsaved
    }

    /// Save the open document before something replaces it. When the save
    /// fails nothing is replaced: the 保存失败 dialog offers 重试 / 放弃修改
    /// and runs `then` after either.
    pub fn save_before(&mut self, cx: &mut Context<Self>, then: impl FnOnce(&mut Self, &mut Window, &mut Context<Self>) + 'static) -> bool {
        if self.flush(cx) {
            return true;
        }
        self.after_save = Some(Box::new(then));
        self.confirm = Some(Confirm::SaveFailed);
        cx.notify();
        false
    }

    /// The 保存失败 dialog's choices: `retry` saves again, otherwise the
    /// changes are given up. Either way the held-back action runs once the
    /// document no longer needs saving.
    pub(crate) fn resolve_save_failed(&mut self, retry: bool, window: &mut Window, cx: &mut Context<Self>) {
        if retry {
            if !self.flush(cx) {
                self.confirm = Some(Confirm::SaveFailed);
                return;
            }
        } else {
            self.unsaved = false;
            self.status = SaveStatus::Idle;
        }
        if let Some(then) = self.after_save.take() {
            then(self, window, cx);
        }
        cx.notify();
    }

    /// The error of the last failed save, for the 保存失败 dialog.
    pub(crate) fn save_error(&self) -> String {
        match &self.status {
            SaveStatus::Failed(e) => e.clone(),
            _ => crate::settings::text("保存失败").to_string(),
        }
    }

    /// Hand the open document the library's titles and the documents
    /// linking to it (双向链接).
    pub(crate) fn sync_links(&mut self, cx: &mut Context<Self>) {
        let (Some(id), Some(ed)) = (self.current.clone(), self.editor.clone()) else { return };
        let docs = self
            .entries
            .iter()
            .map(|e| crate::editor::links::DocRef { id: e.id.clone(), title: e.title.clone(), icon: e.icon.clone(), modified: e.meta.modified, opened: e.meta.opened })
            .collect();
        let backlinks = self
            .entries
            .iter()
            .filter(|e| e.id != id)
            .filter_map(|e| {
                let lines: Vec<_> = e.links.iter().filter(|l| l.target == id).cloned().collect();
                (!lines.is_empty()).then(|| crate::editor::links::Backlink { id: e.id.clone(), lines })
            })
            .collect();
        let hidden = self.entries.iter().find(|e| e.id == id).is_some_and(|e| e.meta.refs_hidden);
        ed.update(cx, |e, cx| e.set_links(id, docs, backlinks, hidden, cx));
    }

    /// A 反向引用 line: open that document at its `ordinal`th block.
    fn open_at(&mut self, id: &str, ordinal: usize, window: &mut Window, cx: &mut Context<Self>) {
        if !self.try_open(id, window, cx) {
            return;
        }
        let Some(ed) = self.editor.clone() else { return };
        ed.update(cx, |e, cx| {
            let mut n = 0;
            let mut target = None;
            e.core.doc.for_each_block(|b| {
                if n == ordinal {
                    target = Some(b.id);
                }
                n += 1;
            });
            if let Some(b) = target {
                e.scroll_to_block(b, cx);
            }
        });
    }

    /// 隐藏引用列表 / 显示引用列表: confirm first (hiding can skip it with 不再提示).
    pub(crate) fn ask_refs(&mut self, show: bool, cx: &mut Context<Self>) {
        if !show && self.store.settings().refs_hide_no_ask {
            return self.set_refs_hidden(true, cx);
        }
        self.confirm = Some(Confirm::Refs { show, no_ask: false });
        cx.notify();
    }

    pub(crate) fn set_refs_hidden(&mut self, hidden: bool, cx: &mut Context<Self>) {
        let Some(id) = self.current.clone() else { return };
        if self.store.update_meta(&id, |m| m.refs_hidden = hidden).is_ok()
            && let Some(e) = self.entries.iter_mut().find(|e| e.id == id)
        {
            e.meta.refs_hidden = hidden;
        }
        self.sync_links(cx);
        cx.notify();
    }

    /// 主页's search box, centred on the pane but never over what sits on
    /// either side of it: the title (and, with the sidebar hidden, the
    /// traffic lights and 展开侧栏) on the left, + on the right. Below a
    /// usable width it is None and the caller shows a search icon instead.
    pub(crate) fn header_search(&self, window: &Window, cx: &mut Context<Self>) -> Option<Stateful<Div>> {
        let main = f32::from(window.viewport_size().width) - if self.sidebar_open { self.sidebar_w(window) } else { 0. };
        let title = crate::settings::label(crate::settings::text("主页"));
        let title_w = f32::from(window.text_system().shape_line(title.clone().into(), px(17.), &[window.text_style().highlight(HighlightStyle { font_weight: Some(FontWeight::SEMIBOLD), ..Default::default() }).to_run(title.len())], None).width);
        // page_header's left inset (26 with the sidebar, else the traffic
        // lights plus the 4 + 28 + 12 of 展开侧栏), then 16 clear of the title.
        let left = if self.sidebar_open { 26. } else { theme::TRAFFIC_LIGHTS_W + 44. + if cfg!(target_os = "windows") { 32. } else { 0. } } + title_w + 16.;
        // + is 32 wide with an 8pt inset; 16 clear of it.
        let right = HEADER_ACTIONS_H + crate::ui::header_right_inset() + 16.;
        let half = (main / 2. - left).min(main / 2. - right);
        let width = (main - 320.).min(2. * half).min(360.);
        if width < 120. { return None }
        Some(div().id("header-search").absolute().left(px((main - width) / 2.)).top(px(8.))
            .w(px(width)).h(px(32.)).no_drag().flex().items_center().gap(px(8.)).px(px(10.))
            .rounded(px(9.)).bg(Theme::fill_hover())
            .cursor_pointer().hover(|s| s.bg(Theme::fill_hover_strong()))
            .child(icon("search", 15., Theme::text_caption()))
            .child(div().flex_1().min_w_0().truncate().text_size(px(13.)).text_color(Theme::text_caption()).child(crate::settings::tr("搜索文档…", "Search documents…")))
            .when(width >= 200., |d| d.child(div().flex_none().text_size(px(11.)).text_color(Theme::text_placeholder()).child(crate::shortcuts::hint("workspace::FocusSearch", "⌘ + J").replace(" + ", " "))))
            .tip(crate::settings::tr("搜索文档", "Search documents"), Some(crate::shortcuts::hint("workspace::FocusSearch", "⌘ + J")))
            .on_click(cx.listener(|w, _, window, cx| w.open_search(window, cx))))
    }

    fn top_bar(&self, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        let main_width = f32::from(window.viewport_size().width) - if self.sidebar_open { self.sidebar_w(window) } else { 0. };
        // At narrow Windows widths, put app controls below the native window buttons.
        let compact = cfg!(target_os = "windows") && main_width < crate::ui::header_right_inset() + 280.;
        let save_error = match &self.status {
            SaveStatus::Failed(error) => Some(error.clone()),
            _ => None,
        }.or_else(|| self.feishu_error.clone());
        // Independent icon controls, with equal top and right insets.
        let seg = crate::ui::header_icon;
        let divider = || div().flex_none().w(px(1.)).h(px(18.)).bg(Theme::line_divider());
        // Breadcrumbs: the parent pages, each linking to itself, with the
        // middle ones folded into "…" when the chain is long.
        let chain: Vec<(String, String, Option<String>)> = self
            .current
            .as_ref()
            .map(|id| self.ancestors(id).iter().map(|e| (e.id.clone(), display_title(&e.title), e.icon.clone())).collect())
            .unwrap_or_default();
        let mut crumbs: Vec<AnyElement> = vec![];
        let sep = || div().mx(px(3.)).flex_none().child(icon("chevron-right", 12., theme::c(craft::N500))).into_any_element();
        for (i, (id, name, em)) in chain.iter().enumerate() {
            if chain.len() > 2 && i > 0 && i < chain.len() - 1 {
                if i == 1 {
                    let hidden: Vec<&str> = chain[1..chain.len() - 1].iter().map(|c| c.1.as_str()).collect();
                    crumbs.push(
                        div()
                            .id("crumb-more")
                            .no_drag()
                            .flex_none()
                            .size(px(20.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(4.))
                            .text_color(Theme::text_caption())
                            .tip(&hidden.join(" / "), None)
                            .child("…")
                            .into_any_element(),
                    );
                    crumbs.push(sep());
                }
                continue;
            }
            let id = id.clone();
            crumbs.push(
                div()
                    .id(SharedString::from(format!("crumb-{id}")))
                    .no_drag()
                    .min_w(px(28.))
                    .max_w(px(200.))
                    .px(px(4.))
                    .flex()
                    .items_center()
                    .rounded(px(4.))
                    .cursor_pointer()
                    .text_color(Theme::text_caption())
                    .hover(|s| s.text_color(Theme::primary_500()))
                    .when_some(em.clone(), |d, em| {
                        d.child(div().size(px(20.)).mr(px(4.)).flex_none().flex().items_center().justify_center().text_size(px(16.)).child(em))
                    })
                    .child(div().min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().child(ui_text(name.clone())))
                    .on_click(cx.listener(move |w, _, window, cx| w.open(&id, window, cx)))
                    .into_any_element(),
            );
            crumbs.push(sep());
        }
        let record_more = self.more_btn_bounds.clone();
        div()
            .id("top-bar").relative()
            .min_h(px(HEADER_H))
            .flex_none()
            .flex()
            .items_center()
            .pt(if compact { px(HEADER_H) } else { px(8.) })
            .pb(px(8.))
            .pr(if compact { px(8.) } else { px(crate::ui::header_right_inset()) })
            // Clear of the traffic lights, which sit over the note when the sidebar is closed.
            .pl(if self.sidebar_open { px(18.) } else { px(theme::TRAFFIC_LIGHTS_W) })
            .title_bar()
            .when(!self.sidebar_open, |d| {
                d.child(
                    div()
                        .id("open-sidebar")
                        .no_drag()
                        .flex_none()
                        .ml(px(4.))
                        .size(px(28.))
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded(px(8.))
                        .cursor_pointer()
                        .hover(|s| s.bg(Theme::fill_hover_strong()))
                        .tip(crate::settings::tr("展开侧栏", "Show sidebar"), Some(crate::shortcuts::hint("workspace::ToggleSidebar", "⌘ + ]")))
                        .child(icon("panel-left-open", 17., Theme::icon()))
                        .on_click(cx.listener(|w, _, _, cx| {
                            w.sidebar_open = true;
                            cx.notify();
                        })),
                )
                .child(
                    div().id("header-home").no_drag().flex_none().ml(px(4.)).size(px(28.))
                        .flex().items_center().justify_center().rounded(px(8.)).cursor_pointer()
                        .hover(|s| s.bg(Theme::fill_hover_strong()))
                        .active(|s| s.bg(Theme::fill_pressed()))
                        .tip(crate::settings::text("返回主页"), None)
                        .child(icon("home", 18., Theme::icon()))
                        .on_click(cx.listener(|w, _, _, cx| w.show_view(View::Home, cx))),
                )
                .child(div().mx(px(12.)).child(divider()))
            })
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .mr(px(4.))
                    .overflow_hidden()
                    .flex()
                    .items_center()
                    .gap(px(12.))
                    .child(
                        div().flex_none().max_w(px(200.)).h(px(20.)).min_w_0().overflow_hidden()
                            .flex().items_center().text_size(px(13.)).line_height(px(20.)).children(crumbs),
                    )
                    .child(crate::tab_bar::render_tab_bar(self.tab_labels(), &self.tab_scroll, cx))
                    .when_some(save_error, |d, error| d.child(
                        div().id("status-error").min_w_0().max_w(px(220.)).text_size(px(12.)).line_height(px(18.))
                            .text_color(theme::c(craft::R400)).overflow_hidden().whitespace_nowrap().text_ellipsis().tip(&error, None).child(error),
                    )),
            )
            .child(
                div().gap(px(4.))
                    .min_h(px(HEADER_ACTIONS_H)).min_w_0().flex_shrink()
                    .flex()
                    .flex_wrap()
                    .justify_end()
                    .items_center()
                    .when(!self.has_online_document(), |d| d.child(
                        div().id("document-zoom").no_drag().flex_none().w(px(50.)).h(px(28.))
                            .flex().items_center().justify_center().rounded(px(8.)).cursor_pointer()
                            .text_size(px(13.)).text_color(Theme::text_caption())
                            .hover(|s| s.bg(Theme::fill_hover_strong()))
                            .active(|s| s.bg(Theme::fill_pressed()))
                            .tip(crate::settings::tr("恢复到 100%", "Reset to 100%"), None)
                            .child(format!("{:.0}%", crate::settings::document_zoom() * 100.))
                            .on_click(cx.listener(|w, _, _, cx| if crate::settings::document_zoom() != 1. { w.set_pref(|p| p.document_zoom = 100, cx); })),
                    ))
                    .when(self.feishu_source, |d| {
                        let syncing = self.current.as_ref().is_some_and(|id| self.feishu_syncing.contains(id));
                        d.child(div().id("feishu-sync").no_drag().flex_none().max_w_full().h(px(28.)).px(px(9.)).flex().items_center()
                            .rounded(px(8.)).text_size(px(13.)).text_color(Theme::text())
                            .when(!syncing, |d| d.cursor_pointer().hover(|s| s.bg(Theme::fill_hover_strong())))
                            .child(div().min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().child(if syncing { crate::settings::tr("同步中…", "Syncing…") } else { crate::settings::tr("同步", "Sync") }))
                            .on_click(cx.listener(|w, _, window, cx| w.sync_feishu(window, cx))))
                    })
                    .when(self.current.is_some(), |d| d.child(
                        div().id("doc-appearance").no_drag().flex_none().max_w_full().h(px(28.)).px(px(8.)).flex().items_center().gap(px(4.))
                            .rounded(px(8.)).cursor_pointer().text_size(px(13.)).text_color(Theme::text())
                            .hover(|s| s.bg(Theme::fill_hover_strong()))
                            .child(icon("palette", 16., Theme::icon()))
                            .child(div().min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().child(crate::settings::tr("外观", "Appearance")))
                            .on_click(cx.listener(|w, _, window, cx| w.open_appearance_dialog(window, cx))),
                    ))
                    .when(self.current.is_some(), |d| {
                        d.child(
                            // No tooltip while its dropdown is open; the menu hangs
                            // from this button's right edge.
                            seg("doc-more", "ellipsis")
                                .relative()
                                .when(self.doc_menu, |d| d.bg(Theme::fill_hover()))
                                .when(!self.doc_menu, |d| d.tip(crate::settings::tr("更多", "More"), None))
                                .child(canvas(move |b, _, _| record_more.set(Some(b)), |_, _, _, _| {}).absolute().inset_0())
                                .on_click(cx.listener(|w, _, _, cx| {
                                    w.doc_menu = !w.doc_menu;
                                    w.doc_sub = None;
                                    cx.notify();
                                })),
                        )
                    })
                    .child(
                        seg("doc-search", "search")
                            .tip_label(crate::settings::text("搜索(⌘ + J)").into(), TipSide::Below)
                            .on_click(cx.listener(|_, _, window, cx| window.dispatch_action(Box::new(FocusSearch), cx))),
                    )
                    .child(
                        // Straight to a new document: no 新建 menu, as 导入 is rare.
                        seg("doc-new", "plus").tip(crate::settings::tr("新建文档", "New document"), None).on_click(cx.listener(|w, _, window, cx| w.new_doc(window, cx))),
                    ).into_any_element()
            )
    }
}

/// Header and action group share an 8px top/right inset.
pub const HEADER_H: f32 = 48.;
pub(crate) const HEADER_ACTIONS_H: f32 = 32.;

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let sidebar = if self.sidebar_open { Some(if matches!(self.view, View::Settings | View::Trash) { self.render_settings_sidebar(window, cx) } else { self.render_sidebar(window, cx) }) } else { None };
        let row_menu = self.render_row_menu(window, cx);
        let sidebar_icon = self.render_sidebar_icon_picker(window, cx);
        let resize = self.sidebar_open.then(|| self.render_resize_handle(window, cx));
        let resize_overlay = self.render_resize_overlay(cx);
        let search = self.render_search(window, cx);
        let doc_menu = self.render_doc_menu(window, cx);
        let doc_info = self.render_doc_info(cx);
        let confirm = self.render_confirm(cx);
        let trash_menu = self.render_trash_menu(cx);
        let home_menus = self.render_home_menus(cx);
        let theme_menu = self.render_theme_menu(cx);
        let appearance_dialog = self.render_appearance_dialog(window, cx);
        let feishu_link_dialog = self.feishu_links.update(cx, |links, cx| links.render_add_link_dialog(window, cx));
        let toast = self.render_toast(window);
        let mode_menu = self.render_mode_menu(cx);
        // Native child windows are always above GPUI: hide the page while any
        // workspace or sidebar panel can overlap it, then restore it in layout.
        let online_visible = self.view == View::Doc && self.active_online().is_some_and(|tab| tab.error.is_none())
            && !self.search_open && !self.sidebar_drag && self.row_menu.is_none()
            && self.sidebar_icon.is_none() && self.confirm.is_none() && self.move_dlg.is_none()
            && self.export_md.is_none() && !self.appearance_dialog && !self.doc_menu
            && self.doc_info.is_none() && !self.mode_menu && toast.is_none()
            && feishu_link_dialog.is_none() && !(self.sidebar_open && self.feishu_links.read(cx).has_open_panel());
        if !online_visible { self.hide_active_online(); }
        let online_content = if let Some(tab) = self.active_online() {
            let id = tab.id;
            if let Some(error) = &tab.error {
                Some(div().flex_1().p(px(24.)).text_color(Theme::text_caption()).child(error.clone()).into_any_element())
            } else {
                match tab.webview.clone() {
                    Some(webview) => {
                        let this = cx.weak_entity();
                        Some(canvas(move |bounds, window, cx| {
                            if let Err(error) = webview.set_bounds(bounds, window).and_then(|()| webview.set_visible(online_visible)) {
                                let _ = webview.set_visible(false);
                                let message = crate::localized_format!(
                                    "无法显示应用内网页：{:#}", "Could not display the embedded web page: {:#}", error
                                );
                                window.defer(cx, move |window, cx| {
                                    let _ = this.update(cx, |w, cx| {
                                        if let Some(tab) = w.online_by_id_mut(id) {
                                            tab.webview = None;
                                            tab.error = Some(message);
                                        }
                                        window.refresh();
                                        cx.notify();
                                    });
                                });
                            }
                        }, |_, _, _, _| {}).absolute().inset_0().into_any_element())
                    }
                    None => Some(div().flex_1().p(px(24.)).text_color(Theme::text_caption())
                        .child(crate::settings::tr("正在打开在线文档…", "Opening online document…")).into_any_element()),
                }
            }
        } else {
            None
        };
        let inactive_tabs = (self.view != View::Doc && !self.tabs.is_empty()).then(|| {
            div().h(px(HEADER_H)).flex_none().flex().items_center()
                .pl(if self.sidebar_open { px(18.) } else { px(theme::TRAFFIC_LIGHTS_W) })
                .pr(px(crate::ui::header_right_inset())).title_bar()
                .child(crate::tab_bar::render_tab_bar(self.tab_labels(), &self.tab_scroll, cx))
        });
        let main = match self.view {
            View::Doc => div()
                .flex_1()
                .min_w_0()
                .h_full()
                .flex()
                .flex_col()
                .child(self.top_bar(window, cx))
                .child(
                    div()
                        .flex_1()
                        .min_h_0()
                        .flex()
                        .child(
                            div()
                                .relative()
                                .flex_1()
                                .min_w_0()
                                .h_full()
                                .children(self.editor.clone())
                                .children(online_content),
                        )
                        .children(self.render_shortcuts(cx)),
                )
                .into_any_element(),
            View::Home => self.render_home(window, cx),
            View::Trash => self.render_trash(cx),
            View::Settings => self.render_settings(cx),
        };
        let move_dlg = self.render_move_dialog(cx);
        let export_md = self.render_export_md(cx);
        // The note and sidebar are flat, adjoining surfaces without a gutter,
        // border or shadow. A document uses its theme's page colour.
        let page = if self.view == View::Doc { crate::editor_theme::style().color("page.bg").unwrap_or_else(crate::editor_theme::Theme::bg_body) } else { Theme::bg_body() };
        let downloads = (!self.feishu_downloads.is_empty()).then(|| {
            let rows = self.feishu_downloads.iter().map(|(id, name, stage)| {
                let stage = match stage {
                    Some(crate::feishu_fetch::FetchProgress::FetchingDocument) => crate::settings::tr("正在获取正文", "Fetching document").to_string(),
                    Some(crate::feishu_fetch::FetchProgress::DownloadingAssets { completed, total }) => format!("{} {completed}/{total}", crate::settings::tr("正在下载素材，已完成：", "Downloading assets, completed:")),
                    Some(crate::feishu_fetch::FetchProgress::ParsingDocument) => crate::settings::tr("正在解析文档", "Parsing document").to_string(),
                    None => crate::settings::tr("正在创建本地副本并关联来源", "Creating local copy and linking source").to_string(),
                };
                div().text_size(px(12.)).text_color(Theme::text_caption()).child(format!("{name} #{} — {stage}", id + 1))
            });
            div().id("feishu-download-progress").max_h(px(120.)).overflow_y_scroll().flex_none().px(px(16.)).py(px(6.)).bg(Theme::window()).children(rows)
        });
        let note = div().id("note").relative().flex_1().min_w_0().h_full().flex().flex_col().bg(page)
            .children(inactive_tabs)
            .child(div().flex_1().min_h_0().w_full().child(main))
            .children(downloads);
        div()
            .id("workspace")
            .key_context("Workspace")
            .track_focus(&self.focus)
            .relative()
            .size_full()
            .flex()
            .bg(Theme::window())
            .font_family(theme::UI_FONT)
            .text_color(Theme::text())
            .on_action(cx.listener(|w, _: &OpenSettings, _, cx| w.show_view(View::Settings, cx)))
            .on_action(cx.listener(|w, _: &OpenAbout, _, cx| w.open_settings(crate::settings::Page::About, cx)))
            .on_action(cx.listener(|w, _: &GoHome, _, cx| w.show_view(View::Home, cx)))
            .on_action(cx.listener(|w, _: &NewDoc, window, cx| w.new_doc(window, cx)))
            .on_action(cx.listener(|w, _: &ImportDoc, window, cx| w.open_import(window, cx)))
            .on_action(cx.listener(|w, _: &ToggleSidebar, _, cx| {
                w.sidebar_open = !w.sidebar_open;
                cx.notify();
            }))
            .on_action(cx.listener(|w, _: &IncreaseDocumentZoom, _, cx| if w.view == View::Doc && !w.has_online_document() { w.adjust_document_zoom(1, cx); }))
            .on_action(cx.listener(|w, _: &DecreaseDocumentZoom, _, cx| if w.view == View::Doc && !w.has_online_document() { w.adjust_document_zoom(-1, cx); }))
            .on_action(cx.listener(|w, _: &FindInDoc, window, cx| w.find_in_doc(false, window, cx)))
            .on_action(cx.listener(|w, _: &ReplaceInDoc, window, cx| {
                let replace = !w.reading();
                w.find_in_doc(replace, window, cx)
            }))
            .on_action(cx.listener(|w, _: &crate::modes::EditMode, window, cx| w.set_mode(false, window, cx)))
            .on_action(cx.listener(|w, _: &crate::modes::ReadMode, window, cx| w.set_mode(true, window, cx)))
            .on_action(cx.listener(|w, _: &crate::modes::ToggleShortcuts, _, cx| {
                if !w.has_online_document() {
                    w.shortcuts = !w.shortcuts;
                    cx.notify();
                }
            }))
            .on_key_down(cx.listener(|w, ev: &KeyDownEvent, window, cx| {
                // The theme dropdown: ↑ ↓ preview, ↩ applies, Esc closes it (not 设置).
                if w.theme_menu.is_some() && w.theme_menu_key(&ev.keystroke.key, cx) {
                    cx.stop_propagation();
                    return;
                }
                // Esc leaves 设置 (unless its filter field has the caret).
                if ev.keystroke.key == "escape" && w.view == View::Settings && !w.settings_filter.read(cx).is_focused(window) {
                    w.close_settings(cx);
                    cx.stop_propagation();
                    return;
                }
                if ev.keystroke.key == "escape" && (w.doc_info.is_some() || w.doc_menu || w.mode_menu) {
                    w.doc_info = None;
                    w.mode_menu = false;
                    w.doc_menu = false;
                    w.doc_sub = None;
                    cx.stop_propagation();
                    cx.notify();
                }
            }))
            // Esc leaves 批量操作 (closing its menu or confirmation first), even from the editor.
            .capture_key_down(cx.listener(|w, ev: &KeyDownEvent, window, cx| {
                if w.record_shortcut(ev, cx) { return; }
                // Esc cancels a confirmation dialog.
                if ev.keystroke.key == "escape" && (w.confirm.is_some() || w.export_md.is_some() || w.appearance_dialog) {
                    w.confirm = None;
                    w.after_save = None;
                    w.export_md = None;
                    if w.appearance_dialog { w.close_appearance_dialog(window, cx); }
                    cx.stop_propagation();
                    cx.notify();
                    return;
                }
                if ev.keystroke.key != "escape" || w.batch.is_none() || w.search_open || w.rename.is_some() {
                    return;
                }
                if w.row_menu.is_some() {
                    w.row_menu = None;
                } else {
                    w.batch = None;
                }
                cx.stop_propagation();
                cx.notify();
            }))
            .on_action(cx.listener(|w, _: &FocusSearch, window, cx| {
                if w.search_open { w.close_search(window, cx); } else { w.open_search(window, cx); }
                cx.stop_propagation();
            }))
            .children(sidebar)
            .child(note)
            .children(resize)
            .children(crate::ui::window_controls(window))
            .children(row_menu)
            .children(sidebar_icon)
            .children(doc_menu)
            .children(mode_menu)
            .children(doc_info)
            .children(trash_menu)
            .children(home_menus)
            .children(theme_menu)
            .children(appearance_dialog)
            .children(feishu_link_dialog)
            .children(confirm)
            .children(move_dlg)
            .children(export_md)
            .children(search)
            .children(resize_overlay)
            .children(toast)
    }
}
