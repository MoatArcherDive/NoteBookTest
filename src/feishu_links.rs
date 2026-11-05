//! Local, editable directory of Feishu links. It never modifies notes or cloud documents.

use crate::input::{TextField, TextFieldEvent};
use crate::settings::tr;
use crate::store::Store;
use crate::theme::{self, Theme, craft};
use crate::ui::{TipExt, icon};
use gpui::{prelude::FluentBuilder, *};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::PathBuf;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum LinkOpenMode {
    #[default]
    Local,
    Online,
}

impl LinkOpenMode {
    fn is_local(&self) -> bool { *self == Self::Local }
}

enum LinkOpenAction {
    Default,
    Online,
    Download,
}

pub struct FeishuLinkOpen {
    pub url: String,
    pub name: String,
    pub force_download: bool,
    pub(crate) mode: LinkOpenMode,
}

pub(crate) struct FeishuLinkFormOpen;

pub(crate) struct FeishuIconOpen {
    pub id: String,
    pub position: Point<Pixels>,
}

pub(crate) struct FeishuSearchResult {
    pub id: String,
    pub name: String,
    pub url: String,
    pub icon: Option<String>,
}

#[derive(Clone, Serialize, Deserialize)]
struct LinkEntry {
    id: String,
    parent: Option<String>,
    name: String,
    // None is a folder; Some is a link.
    url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    icon: Option<String>,
    #[serde(default, skip_serializing_if = "LinkOpenMode::is_local")]
    open_mode: LinkOpenMode,
    expanded: bool,
}

#[derive(Clone, Default, Serialize, Deserialize)]
struct Directory {
    entries: Vec<LinkEntry>,
}


impl Directory {
    fn search_links(&self, query: &str) -> Vec<FeishuSearchResult> {
        let query = query.trim().to_lowercase();
        if query.is_empty() { return Vec::new(); }
        self.entries.iter().filter_map(|entry| {
            let url = entry.url.as_ref()?;
            entry.name.to_lowercase().contains(&query).then(|| FeishuSearchResult {
                id: entry.id.clone(), name: entry.name.clone(), url: url.clone(), icon: entry.icon.clone(),
            })
        }).collect()
    }

    fn expand_path(&mut self, parent: Option<&str>) {
        let mut next = parent.map(str::to_owned);
        for _ in 0..self.entries.len() {
            let Some(id) = next else { break; };
            let Some(folder) = self.entries.iter_mut().find(|entry| entry.id == id && entry.url.is_none()) else { break; };
            folder.expanded = true;
            next = folder.parent.clone();
        }
    }

    fn folder_choices(&self) -> Vec<(String, String)> {
        let mut choices: Vec<_> = self.entries.iter().filter(|entry| entry.url.is_none()).map(|folder| {
            let mut path = vec![folder.name.as_str()];
            let mut ancestors = vec![folder.id.as_str()];
            let mut parent = folder.parent.as_deref();
            while let Some(id) = parent {
                if ancestors.contains(&id) { break; }
                let Some(entry) = self.entries.iter().find(|entry| entry.id == id && entry.url.is_none()) else { break; };
                ancestors.push(id);
                path.push(&entry.name);
                parent = entry.parent.as_deref();
            }
            path.reverse();
            (folder.id.clone(), format!("{} / {}", tr("根目录", "Root folder"), path.join(" / ")))
        }).collect();
        let labels: Vec<_> = choices.iter().map(|(_, label)| label.clone()).collect();
        for (id, label) in &mut choices {
            if labels.iter().filter(|other| other.as_str() == label.as_str()).count() > 1 {
                label.push_str(&format!(" ({id})"));
            }
        }
        choices
    }
}

#[derive(Clone)]
enum Mode {
    NewFolder(Option<String>),
    NewLink(Option<String>, LinkOpenMode),
    ContextMenu(String, LinkOpenMode, Point<Pixels>),
    Rename(String, LinkOpenMode),
    Move(String),
    Delete(String),
}

pub struct FeishuLinks {
    path: PathBuf,
    data: Directory,
    // A failed read must never be followed by a write over the original file.
    read_error: bool,
    error: Option<String>,
    selected: Option<String>,
    mode: Option<Mode>,
    previous_focus: Option<FocusHandle>,
    name: Entity<TextField>,
    url: Entity<TextField>,
    _subs: Vec<Subscription>,
}

impl EventEmitter<FeishuLinkOpen> for FeishuLinks {}
impl EventEmitter<FeishuLinkFormOpen> for FeishuLinks {}
impl EventEmitter<FeishuIconOpen> for FeishuLinks {}

impl FeishuLinks {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let path = Store::root_dir().join("feishu-links.json");
        let (data, read_error, error) = match std::fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice(&bytes) {
                Ok(data) => (data, false, None),
                Err(e) => (Directory::default(), true, Some(format!("{} {e} {}", tr("无法读取飞书链接目录：", "Could not read the Feishu link directory:"), tr("原文件未修改。", "The original file was not modified.")))),
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (Directory::default(), false, None),
            Err(e) => (Directory::default(), true, Some(format!("{} {e} {}", tr("无法读取飞书链接目录：", "Could not read the Feishu link directory:"), tr("原文件未修改。", "The original file was not modified.")))),
        };
        let name = cx.new(|cx| { let mut f = TextField::new(tr("名称", "Name"), cx); f.tab_events = true; f });
        let url = cx.new(|cx| { let mut f = TextField::new(tr("飞书文档链接", "Feishu document URL"), cx); f.tab_events = true; f });
        let subs = vec![
            cx.subscribe_in(&name, window, |this, _, ev: &TextFieldEvent, window, cx| match ev {
                TextFieldEvent::Submit if matches!(this.mode, Some(Mode::NewLink(..))) => window.focus(&this.url.focus_handle(cx)),
                TextFieldEvent::Submit => this.submit(window, cx),
                TextFieldEvent::Tab | TextFieldEvent::ShiftTab if matches!(this.mode, Some(Mode::NewLink(..))) => window.focus(&this.url.focus_handle(cx)),
                TextFieldEvent::Cancel => this.close_form(window, cx),
                _ => {}
            }),
            cx.subscribe_in(&url, window, |this, _, ev: &TextFieldEvent, window, cx| match ev {
                TextFieldEvent::Submit => this.submit(window, cx),
                TextFieldEvent::Tab | TextFieldEvent::ShiftTab if matches!(this.mode, Some(Mode::NewLink(..))) => window.focus(&this.name.focus_handle(cx)),
                TextFieldEvent::Cancel => this.close_form(window, cx),
                _ => {}
            }),
        ];
        Self { path, data, read_error, error, selected: None, mode: None, previous_focus: None, name, url, _subs: subs }
    }

    pub(crate) fn has_open_panel(&self) -> bool {
        self.mode.is_some()
    }

    pub(crate) fn render_add_link_dialog(&self, window: &Window, cx: &mut Context<Self>) -> Option<AnyElement> {
        let Some(Mode::NewLink(parent, selected_mode)) = &self.mode else { return None };
        let parent = parent.clone();
        let compact = window.viewport_size().height < px(540.);
        let mut choices = div().id("feishu-new-link-folders").max_h(px(if compact { 56. } else { 150. })).overflow_y_scroll().flex().flex_col().gap(px(2.))
            .p(px(4.)).rounded(px(8.)).border_1().border_color(Theme::line_border()).bg(Theme::bg_body())
            .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
            .child(div().id("feishu-new-link-root").h(px(32.)).px(px(8.)).flex().items_center().gap(px(8.)).cursor_pointer().rounded(px(6.))
                .when(parent.is_none(), |d| d.bg(Theme::primary_fill_1()))
                .hover(|s| s.bg(Theme::fill_hover())).child(icon("folder", 16., Theme::text_caption()))
                .child(div().flex_1().min_w_0().child(tr("根目录", "Root folder")))
                .when(parent.is_none(), |d| d.child(icon("check", 14., Theme::primary())))
                .on_click(cx.listener(|this, _, _, cx| {
                    if let Some(Mode::NewLink(parent, _)) = &mut this.mode { *parent = None; }
                    cx.notify();
                })));
        for (id, label) in self.data.folder_choices() {
            let selected = parent.as_ref() == Some(&id);
            let tooltip = label.clone();
            choices = choices.child(div().id(SharedString::from(format!("feishu-new-link-folder-{id}"))).h(px(32.)).px(px(8.))
                .flex().items_center().gap(px(8.)).cursor_pointer().rounded(px(6.))
                .when(selected, |d| d.bg(Theme::primary_fill_1()))
                .hover(|s| s.bg(Theme::fill_hover())).tip_label(tooltip, crate::ui::TipSide::Below)
                .child(icon("folder", 16., Theme::text_caption()))
                .child(div().flex_1().min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().child(label))
                .when(selected, |d| d.child(icon("check", 14., Theme::primary())))
                .on_click(cx.listener(move |this, _, _, cx| {
                    if let Some(Mode::NewLink(parent, _)) = &mut this.mode { *parent = Some(id.clone()); }
                    cx.notify();
                })));
        }
        let name_focused = self.name.read(cx).is_focused(window);
        let url_focused = self.url.read(cx).is_focused(window);
        let field = |label: &'static str, field: Entity<TextField>, focused: bool| div().flex().flex_col().gap(px(6.))
            .child(div().text_size(px(13.)).line_height(px(20.)).font_weight(FontWeight::MEDIUM).child(label))
            .child(div().h(px(36.)).px(px(10.)).flex().items_center().rounded(px(8.)).bg(Theme::bg_body())
                .border_1().border_color(if focused { Theme::primary() } else { Theme::line_border() }).child(field));
        let dialog = crate::ui::dialog("feishu-add-dialog").relative().w(px(480.)).rounded(px(12.)).border_1().border_color(Theme::line_border())
            .max_w(window.viewport_size().width - px(32.)).max_h(window.viewport_size().height - px(48.))
            .flex().flex_col().on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(div().px(px(24.)).pt(px(if compact { 20. } else { 24. })).pb(px(if compact { 14. } else { 18. })).text_size(px(16.)).font_weight(FontWeight::SEMIBOLD)
                .child(tr("添加链接", "Add link")))
            .child(div().id("feishu-add-close").absolute().top(px(20.)).right(px(20.)).size(px(28.)).flex().items_center().justify_center()
                .rounded(px(6.)).cursor_pointer().hover(|s| s.bg(Theme::fill_hover())).child(icon("x", 16., Theme::text_caption()))
                .on_click(cx.listener(|this, _, window, cx| this.close_form(window, cx))))
            .child(div().id("feishu-add-content").min_h_0().overflow_y_scroll().px(px(24.)).pb(px(if compact { 12. } else { 20. })).flex().flex_col().gap(px(if compact { 8. } else { 16. }))
                .child(field(tr("名称", "Name"), self.name.clone(), name_focused))
                .child(field(tr("飞书文档链接", "Feishu document URL"), self.url.clone(), url_focused))
                .child(open_mode_controls(*selected_mode, true, cx))
                .child(div().flex().flex_col().gap(px(6.)).text_size(px(13.))
                    .child(div().line_height(px(20.)).font_weight(FontWeight::MEDIUM).child(tr("保存到飞书链接目录：", "Save in Feishu link folder:")))
                    .child(choices))
                .children(self.error.as_ref().map(|error| div().text_size(px(13.)).text_color(Theme::danger()).child(error.clone()))))
            .child(div().px(px(24.)).pb(px(if compact { 20. } else { 24. })).flex().justify_end().gap(px(8.))
                .child(crate::ui::secondary_button("feishu-add-cancel", tr("取消", "Cancel")).h(px(34.)).rounded(px(8.))
                    .on_click(cx.listener(|this, _, window, cx| this.close_form(window, cx))))
                .child(crate::ui::primary_button("feishu-add-save", tr("保存", "Save")).h(px(34.)).rounded(px(8.))
                    .on_click(cx.listener(|this, _, window, cx| this.submit(window, cx)))));
        Some(deferred(div().id("feishu-add-mask").occlude().absolute().inset_0().flex().items_center().justify_center().bg(crate::ui::scrim())
            .on_mouse_down(MouseButton::Left, cx.listener(|this, _, window, cx| this.close_form(window, cx)))
            .child(dialog)).with_priority(3).into_any_element())
    }

    pub(crate) fn search_links(&self, query: &str) -> Vec<FeishuSearchResult> {
        self.data.search_links(query)
    }

    pub(crate) fn open_link(&mut self, id: &str, cx: &mut Context<Self>) {
        self.open_link_as(id, LinkOpenAction::Default, cx);
    }

    fn open_link_as(&mut self, id: &str, action: LinkOpenAction, cx: &mut Context<Self>) {
        let Some(entry) = self.data.entries.iter().find(|entry| entry.id == id) else { return };
        let Some(url) = &entry.url else { return };
        let (mode, force_download) = match action {
            LinkOpenAction::Default => (entry.open_mode, false),
            LinkOpenAction::Online => (LinkOpenMode::Online, false),
            LinkOpenAction::Download => (LinkOpenMode::Local, true),
        };
        cx.emit(FeishuLinkOpen { url: url.clone(), name: entry.name.clone(), force_download, mode });
    }

    fn save(&mut self, previous: Directory, cx: &mut Context<Self>) -> bool {
        if self.read_error {
            self.data = previous;
            cx.notify();
            return false;
        }
        let result = (|| -> anyhow::Result<()> {
            std::fs::create_dir_all(self.path.parent().unwrap())?;
            let bytes = serde_json::to_vec_pretty(&self.data)?;
            let tmp = self.path.with_extension("json.tmp");
            let mut file = std::fs::File::create(&tmp)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            std::fs::rename(tmp, &self.path)?;
            Ok(())
        })();
        match result {
            Ok(()) => { self.error = None; cx.notify(); true }
            Err(e) => {
                self.data = previous;
                self.error = Some(format!("{} {e} {}", tr("保存飞书链接目录失败：", "Could not save the Feishu link directory:"), tr("更改未保存。", "Changes were not saved.")));
                cx.notify();
                false
            }
        }
    }

    fn close_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let modal = matches!(self.mode, Some(Mode::NewLink(..)));
        self.mode = None;
        if modal {
            if !self.read_error { self.error = None; }
            if let Some(focus) = self.previous_focus.take() { window.focus(&focus); }
        }
        cx.notify();
    }

    fn begin(&mut self, mode: Mode, window: &mut Window, cx: &mut Context<Self>) {
        let initial = match &mode {
            Mode::Rename(id, _) => self.data.entries.iter().find(|e| &e.id == id).map(|e| e.name.clone()).unwrap_or_default(),
            _ => String::new(),
        };
        let modal = matches!(mode, Mode::NewLink(..));
        if modal {
            self.previous_focus = window.focused(cx);
            if !self.read_error { self.error = None; }
        }
        let size = if modal { 14. } else { crate::settings::sidebar_text_size(13.) };
        self.name.update(cx, |f, cx| { f.size = size; f.line_height = size + 6.; f.set_placeholder(if modal { "" } else { tr("名称", "Name") }, cx); f.set_text(&initial, cx); f.select_all_text(cx); });
        self.url.update(cx, |f, cx| { f.size = size; f.line_height = size + 6.; f.set_placeholder(if modal { "https://..." } else { tr("飞书文档链接", "Feishu document URL") }, cx); f.set_text("", cx); });
        let needs_field = matches!(mode, Mode::NewFolder(_) | Mode::NewLink(..) | Mode::Rename(..));
        self.mode = Some(mode);
        if needs_field { window.focus(&self.name.focus_handle(cx)); }
        if !matches!(self.mode, Some(Mode::Rename(..) | Mode::NewLink(..))) { cx.emit(FeishuLinkFormOpen); }
        cx.notify();
    }

    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(mode) = self.mode.clone() else { return };
        let name = self.name.read(cx).text.trim().to_string();
        let url = self.url.read(cx).text.trim().to_string();
        if name.is_empty() {
            self.error = Some(tr("名称不能为空。", "Name cannot be empty.").into()); cx.notify(); return;
        }
        if matches!(mode, Mode::NewLink(..)) && url.is_empty() {
            self.error = Some(tr("链接不能为空。", "URL cannot be empty.").into()); cx.notify(); return;
        }
        let previous = self.data.clone();
        match mode {
            Mode::NewFolder(parent) | Mode::NewLink(parent, _) => {
                let (is_link, open_mode) = match &self.mode {
                    Some(Mode::NewLink(_, open_mode)) => (true, *open_mode),
                    _ => (false, LinkOpenMode::Local),
                };
                self.data.expand_path(parent.as_deref());
                self.data.entries.push(LinkEntry { id: uuid::Uuid::new_v4().to_string(), parent, name, url: is_link.then_some(url), icon: None, open_mode, expanded: true });
            }
            Mode::Rename(id, open_mode) => {
                if let Some(entry) = self.data.entries.iter_mut().find(|e| e.id == id) {
                    entry.name = name;
                    if entry.url.is_some() { entry.open_mode = open_mode; }
                }
            }
            _ => return,
        }
        if self.save(previous, cx) { self.close_form(window, cx); }
    }

    fn descendants(&self, id: &str) -> Vec<String> {
        let mut ids = vec![id.to_string()];
        let mut i = 0;
        while i < ids.len() {
            for entry in &self.data.entries {
                if entry.parent.as_ref() == Some(&ids[i]) && !ids.contains(&entry.id) { ids.push(entry.id.clone()); }
            }
            i += 1;
        }
        ids
    }

    fn move_to(&mut self, id: &str, parent: Option<String>, cx: &mut Context<Self>) {
        if parent.as_ref().is_some_and(|p| self.descendants(id).contains(p)) { return; }
        let previous = self.data.clone();
        if let Some(entry) = self.data.entries.iter_mut().find(|e| e.id == id) { entry.parent = parent.clone(); }
        self.data.expand_path(parent.as_deref());
        if self.save(previous, cx) { self.mode = None; cx.notify(); }
    }

    pub(crate) fn set_icon(&mut self, id: &str, icon: Option<&str>, cx: &mut Context<Self>) -> bool {
        let previous = self.data.clone();
        if let Some(entry) = self.data.entries.iter_mut().find(|e| e.id == id) {
            entry.icon = icon.map(str::to_string);
        }
        if self.save(previous, cx) { self.mode = None; cx.notify(); true } else { false }
    }

    fn delete(&mut self, id: &str, cx: &mut Context<Self>) {
        let ids = self.descendants(id);
        let previous = self.data.clone();
        self.data.entries.retain(|e| !ids.contains(&e.id));
        if self.save(previous, cx) {
            if self.selected.as_ref().is_some_and(|s| ids.contains(s)) { self.selected = None; }
            self.mode = None;
            cx.notify();
        }
    }

    fn rows(&self, parent: Option<&str>, depth: usize, out: &mut Vec<(LinkEntry, usize)>) {
        for entry in self.data.entries.iter().filter(|e| e.parent.as_deref() == parent) {
            out.push((entry.clone(), depth));
            if entry.url.is_none() && entry.expanded { self.rows(Some(&entry.id), depth + 1, out); }
        }
    }
}

fn open_mode_controls(selected: LinkOpenMode, modal: bool, cx: &mut Context<FeishuLinks>) -> Div {
    let mut controls = div().flex().flex_col().gap(px(4.)).when(modal, |d| d.text_size(px(13.)))
        .child(div().when(modal, |d| d.line_height(px(20.)).font_weight(FontWeight::MEDIUM))
            .when(!modal, |d| d.text_color(Theme::text_caption())).child(tr("默认打开方式：", "Default open mode:")));
    for (mode, id, label) in [
        (LinkOpenMode::Online, "feishu-mode-online", tr("在线打开", "Open online")),
        (LinkOpenMode::Local, "feishu-mode-local", tr("下载/打开本地副本", "Download/open local copy")),
    ] {
        let checked = selected == mode;
        controls = controls.child(div().id(id).cursor_pointer()
            .when(modal, |d| d.h(px(28.)).px(px(4.)).flex().items_center().gap(px(8.)).rounded(px(6.)).hover(|s| s.bg(Theme::fill_hover()))
                .child(div().size(px(16.)).flex_none().flex().items_center().justify_center().rounded_full().border_1()
                    .border_color(if checked { Theme::primary() } else { theme::c(craft::N400) })
                    .bg(if checked { Theme::primary() } else { Theme::bg_body() })
                    .when(checked, |d| d.child(div().size(px(6.)).rounded_full().bg(gpui::white()))))
                .child(label))
            .when(!modal, |d| d.rounded(px(4.)).px(px(4.)).when(checked, |d| d.bg(Theme::sidebar_selected()))
                .hover(|s| s.bg(Theme::sidebar_hover())).child(format!("{} {label}", if checked { "●" } else { "○" })))
            .on_click(cx.listener(move |this, _, _, cx| {
                match &mut this.mode {
                    Some(Mode::NewLink(_, current)) | Some(Mode::Rename(_, current)) => *current = mode,
                    _ => {}
                }
                cx.notify();
            })));
    }
    controls
}

impl Render for FeishuLinks {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let size = crate::settings::sidebar_text_size(13.);
        let mut root = div().flex().flex_col().gap(px(2.)).text_size(px(size)).text_color(Theme::sidebar_text());
        let selected = self.selected.clone();
        root = root.child(div().flex().items_center().gap(px(8.)).h(px((size + 12.).max(30.)))
            .child(div().id("feishu-root").flex_1().cursor_pointer().child(tr("飞书链接", "Feishu links")).on_click(cx.listener(|this, _, _, cx| { this.selected = None; this.mode = None; cx.notify(); })))
            .child(div().id("feishu-new-folder").cursor_pointer().child(tr("新建目录", "New folder")).on_click(cx.listener(move |this, _, window, cx| this.begin(Mode::NewFolder(selected.clone()), window, cx))))
            .child({ let selected = self.selected.clone(); div().id("feishu-new-link").cursor_pointer().child(tr("添加链接", "Add link")).on_click(cx.listener(move |this, _, window, cx| this.begin(Mode::NewLink(selected.clone(), LinkOpenMode::Local), window, cx))) }));
        if !matches!(self.mode, Some(Mode::NewLink(..))) && let Some(error) = &self.error { root = root.child(div().text_color(Theme::text_caption()).child(error.clone())); }
        let mut rows = Vec::new();
        self.rows(None, 0, &mut rows);
        for (entry, depth) in rows {
            let id = entry.id.clone();
            let folder = entry.url.is_none();
            let label: SharedString = entry.name.clone().into();
            let selected = self.selected.as_ref() == Some(&id);
            let glyph = crate::sidebar::sidebar_row_icon(entry.icon.as_deref(), folder, selected);
            let glyph = div().relative().size(px(crate::sidebar::ROW_ICON_SLOT)).flex_none()
                .child(glyph.when(folder, |d| d.group_hover("feishu-row", |s| s.opacity(0.))))
                .when(folder, |d| d.child(div().absolute().inset_0().flex().items_center().justify_center()
                    .opacity(0.).group_hover("feishu-row", |s| s.opacity(1.))
                    .child(icon("tree-triangle", 10., theme::c(craft::N500)).when(entry.expanded, |s| s.with_transformation(Transformation::rotate(radians(std::f32::consts::FRAC_PI_2)))))));
            let renaming = matches!(&self.mode, Some(Mode::Rename(editing, _)) if editing == &id);
            let title = if renaming {
                div().ml(px(crate::sidebar::ROW_LABEL_GAP)).flex_1().min_w_0().h(px((size + 10.).max(24.))).px(px(6.))
                    .flex().items_center().rounded(px(6.)).bg(Theme::bg_body()).border_1().border_color(Theme::primary_500())
                    .child(self.name.clone()).into_any_element()
            } else {
                div().relative().ml(px(crate::sidebar::ROW_LABEL_GAP)).flex_1().min_w_0().overflow_hidden()
                    .child(div().w_full().min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().group_hover("feishu-row", |s| s.opacity(0.)).child(crate::ui::ui_text(label.clone())))
                    .child(div().absolute().left_0().top_0().right(px(if folder { 88. } else { 132. })).overflow_hidden().whitespace_nowrap().text_ellipsis().opacity(0.).group_hover("feishu-row", |s| s.opacity(1.)).child(crate::ui::ui_text(label)))
                    .into_any_element()
            };
            let open_id = id.clone();
            let tooltip = entry.name.clone();
            let menu_id = id.clone();
            let open_mode = entry.open_mode;
            let show_tip = !matches!(self.mode, Some(Mode::ContextMenu(..)));
            let content = div().id(SharedString::from(format!("feishu-entry-{id}"))).flex_1().min_w_0().flex().items_center().overflow_hidden()
                .child(glyph).child(title)
                .when(!renaming, |d| d.cursor_pointer().when(show_tip, |d| d.tip_label(tooltip, crate::ui::TipSide::Below))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if !folder { this.open_link(&open_id, cx); }
                        else {
                            let previous = this.data.clone();
                            if let Some(entry) = this.data.entries.iter_mut().find(|e| e.id == open_id) { entry.expanded = !entry.expanded; }
                            this.selected = Some(open_id.clone());
                            this.save(previous, cx);
                        }
                    })));
            let mut row = div().id(SharedString::from(format!("feishu-row-{id}"))).group("feishu-row").relative().flex().items_center().pl(px(crate::sidebar::ROW_INSET + (depth.min(12) as f32) * crate::sidebar::ROW_INDENT)).h(px((size + 12.).max(crate::sidebar::ROW_H))).rounded(px(9.))
                .when(selected, |d| d.bg(Theme::sidebar_selected()))
                .hover(|s| s.bg(Theme::sidebar_hover()))
                .child(content)
                .when(!renaming, |row| row.on_mouse_down(MouseButton::Right, cx.listener(move |this, ev: &MouseDownEvent, _, cx| {
                    cx.stop_propagation();
                    this.mode = Some(Mode::ContextMenu(menu_id.clone(), open_mode, ev.position));
                    cx.notify();
                })));
            if !renaming {
                let icon_id = id.clone();
                let mut actions = div().absolute().right_0().top(px(((size + 12.).max(crate::sidebar::ROW_H) - 22.) / 2.)).flex().bg(Theme::sidebar_hover())
                    .opacity(0.).group_hover("feishu-row", |s| s.opacity(1.))
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation());
                actions = actions.child(div().id(SharedString::from(format!("feishu-icon-{id}"))).size(px(22.)).flex_none().flex().items_center().justify_center().rounded(px(6.))
                    .cursor_pointer().hover(|s| s.bg(Theme::sidebar_selected()))
                    .tip(tr("设置图标", "Set icon"), None).child(crate::editor_ui::icon("smile-plus", 14., Theme::text_caption()))
                    .on_click(cx.listener(move |this, ev: &ClickEvent, _, cx| {
                        cx.stop_propagation();
                        this.mode = None;
                        cx.emit(FeishuIconOpen { id: icon_id.clone(), position: ev.position() });
                        cx.notify();
                    })));
                if !folder {
                    let online_id = id.clone();
                    actions = actions.child(div().id(SharedString::from(format!("feishu-online-{id}"))).size(px(22.)).flex_none().flex().items_center().justify_center().rounded(px(6.))
                        .cursor_pointer().hover(|s| s.bg(Theme::sidebar_selected()))
                        .tip(tr("在线打开", "Open online"), None).child(crate::editor_ui::icon("globe", 14., Theme::text_caption()))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            cx.stop_propagation();
                            this.open_link_as(&online_id, LinkOpenAction::Online, cx);
                        })));
                    let download_id = id.clone();
                    actions = actions.child(div().id(SharedString::from(format!("feishu-download-{id}"))).size(px(22.)).flex_none().flex().items_center().justify_center().rounded(px(6.))
                        .cursor_pointer().hover(|s| s.bg(Theme::sidebar_selected()))
                        .tip(tr("下载为新副本", "Download another copy"), None).child(crate::editor_ui::icon("download", 14., Theme::text_caption()))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            cx.stop_propagation();
                            this.open_link_as(&download_id, LinkOpenAction::Download, cx);
                        })));
                }
                let rename_id = id.clone();
                actions = actions.child(div().id(SharedString::from(format!("feishu-rename-{id}"))).size(px(22.)).flex_none().flex().items_center().justify_center().rounded(px(6.))
                    .cursor_pointer().hover(|s| s.bg(Theme::sidebar_selected()))
                    .tip(if folder { tr("重命名", "Rename") } else { tr("编辑名称和打开方式", "Edit name and open mode") }, None).child(icon("pen-line", 14., Theme::text_caption()))
                    .on_click(cx.listener(move |this, _, window, cx| { cx.stop_propagation(); this.begin(Mode::Rename(rename_id.clone(), open_mode), window, cx); })));
                let move_id = id.clone();
                actions = actions.child(div().id(SharedString::from(format!("feishu-move-{id}"))).size(px(22.)).flex_none().flex().items_center().justify_center().rounded(px(6.))
                    .cursor_pointer().hover(|s| s.bg(Theme::sidebar_selected()))
                    .tip(tr("移动", "Move"), None).child(icon("folder-input", 14., Theme::text_caption()))
                    .on_click(cx.listener(move |this, _, window, cx| { cx.stop_propagation(); this.begin(Mode::Move(move_id.clone()), window, cx); })));
                let delete_id = id.clone();
                actions = actions.child(div().id(SharedString::from(format!("feishu-delete-{id}"))).size(px(22.)).flex_none().flex().items_center().justify_center().rounded(px(6.))
                    .cursor_pointer().hover(|s| s.bg(Theme::sidebar_selected()))
                    .tip(tr("删除", "Delete"), None).child(icon("bin", 14., Theme::text_caption()))
                    .on_click(cx.listener(move |this, _, window, cx| { cx.stop_propagation(); this.begin(Mode::Delete(delete_id.clone()), window, cx); })));
                row = row.child(actions);
            }
            root = root.child(row);
            if let Some(Mode::Rename(editing, selected_mode)) = &self.mode
                && editing == &id {
                let mut controls = div().ml(px(crate::sidebar::ROW_INSET + (depth.min(12) as f32) * crate::sidebar::ROW_INDENT + crate::sidebar::ROW_ICON_SLOT + crate::sidebar::ROW_LABEL_GAP))
                    .mr(px(6.)).py(px(4.)).flex().flex_col().gap(px(4.));
                if !folder { controls = controls.child(open_mode_controls(*selected_mode, false, cx)); }
                root = root.child(controls.child(div().flex().gap(px(12.))
                    .child(div().id("feishu-save").cursor_pointer().child(tr("保存", "Save")).on_click(cx.listener(|this, _, window, cx| this.submit(window, cx))))
                    .child(div().id("feishu-cancel").cursor_pointer().child(tr("取消", "Cancel")).on_click(cx.listener(|this, _, _, cx| { this.mode = None; cx.notify(); })))));
            }
        }
        match self.mode.clone() {
            Some(Mode::NewFolder(_)) => {
                let form = div().flex().flex_col().gap(px(4.)).p(px(6.)).bg(Theme::bg_body())
                    .child(div().h(px((size + 10.).max(26.))).child(self.name.clone()));
                root = root.child(form.child(div().flex().gap(px(12.))
                    .child(div().id("feishu-save").cursor_pointer().child(tr("保存", "Save")).on_click(cx.listener(|this, _, window, cx| this.submit(window, cx))))
                    .child(div().id("feishu-cancel").cursor_pointer().child(tr("取消", "Cancel")).on_click(cx.listener(|this, _, window, cx| this.close_form(window, cx))))));
            }
            Some(Mode::Move(id)) => {
                let excluded = self.descendants(&id);
                let mut choices = div().flex().flex_col().gap(px(4.)).p(px(6.)).bg(Theme::bg_body()).child(tr("移动到：", "Move to:"));
                let root_id = id.clone();
                choices = choices.child(div().id("feishu-move-root").cursor_pointer().child(tr("根目录", "Root folder")).on_click(cx.listener(move |this, _, _, cx| this.move_to(&root_id, None, cx))));
                for (target, label) in self.data.folder_choices().into_iter().filter(|(id, _)| !excluded.contains(id)) {
                    let moving = id.clone();
                    choices = choices.child(div().id(SharedString::from(format!("feishu-destination-{target}"))).cursor_pointer().child(label)
                        .on_click(cx.listener(move |this, _, _, cx| this.move_to(&moving, Some(target.clone()), cx))));
                }
                root = root.child(choices.child(div().id("feishu-cancel-move").cursor_pointer().child(tr("取消", "Cancel")).on_click(cx.listener(|this, _, _, cx| { this.mode = None; cx.notify(); }))));
            }
            Some(Mode::Delete(id)) => {
                let name = self.data.entries.iter().find(|e| e.id == id).map(|e| e.name.clone()).unwrap_or_default();
                root = root.child(div().flex().flex_col().gap(px(4.)).p(px(6.)).bg(Theme::bg_body())
                    .child(if crate::settings::english() { format!("Delete “{name}” and all links inside it? This only removes local links, not Feishu documents or local notes.") } else { format!("删除「{name}」及其目录内所有链接？仅删除本机链接，不会删除飞书文档或本地笔记。") })
                    .child(div().flex().gap(px(12.))
                        .child(div().id("feishu-confirm-delete").cursor_pointer().child(tr("确认删除", "Delete")).on_click(cx.listener(move |this, _, _, cx| this.delete(&id, cx))))
                        .child(div().id("feishu-cancel-delete").cursor_pointer().child(tr("取消", "Cancel")).on_click(cx.listener(|this, _, _, cx| { this.mode = None; cx.notify(); })))));
            }
            Some(Mode::ContextMenu(id, open_mode, position)) => {
                let group: SharedString = "feishu-context-rename".into();
                let menu = crate::ui::popover().id("feishu-context-menu").occlude().w(px(180.))
                    .px(px(5.)).py(px(crate::ui::MENU_PAD_Y))
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                        if matches!(this.mode, Some(Mode::ContextMenu(..))) { this.mode = None; cx.notify(); }
                    }))
                    .child(crate::ui::menu_row("feishu-context-rename", group.clone()).mx(px(0.))
                        .child(crate::ui::menu_glyph("pen-line", 16., group.clone()))
                        .child(crate::ui::menu_label(tr("重命名", "Rename"), group, gpui::white()))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            cx.stop_propagation();
                            this.begin(Mode::Rename(id.clone(), open_mode), window, cx);
                        })));
                root = root.child(deferred(anchored().anchor(Corner::TopLeft).position(position).snap_to_window_with_margin(px(8.)).child(menu)).with_priority(2));
            }
            Some(Mode::Rename(..) | Mode::NewLink(..)) | None => {}
        }
        root
    }
}

#[cfg(test)]
mod tests {
    use super::{Directory, LinkOpenMode};

    #[test]
    fn old_links_accept_icons_and_clear_them_without_losing_the_link() {
        let mut data: Directory = serde_json::from_str(r#"{"entries":[{"id":"one","parent":null,"name":"Doc","url":"https://example.test/docx/one","expanded":true}]}"#).unwrap();
        assert_eq!(data.entries[0].icon, None);
        data.entries[0].icon = Some("📁".into());
        let mut loaded: Directory = serde_json::from_str(&serde_json::to_string(&data).unwrap()).unwrap();
        assert_eq!(loaded.entries[0].icon.as_deref(), Some("📁"));
        loaded.entries[0].icon = None;
        let cleared = serde_json::to_value(&loaded).unwrap();
        assert!(cleared["entries"][0].get("icon").is_none());
        assert_eq!(cleared["entries"][0]["url"], "https://example.test/docx/one");
    }

    #[test]
    fn searches_saved_link_names_case_insensitively_but_not_folders_or_urls() {
        let data: Directory = serde_json::from_str(r#"{"entries":[
            {"id":"folder","parent":null,"name":"Project","url":null,"expanded":false},
            {"id":"one","parent":"folder","name":"Project Plan","url":"https://example.test/docx/one","icon":"📄","expanded":true},
            {"id":"two","parent":null,"name":"Other","url":"https://project.test","expanded":true}
        ]}"#).unwrap();
        assert!(data.search_links("").is_empty());
        assert!(data.search_links("   ").is_empty());
        let results = data.search_links("pRoJeCt");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, "one");
        assert_eq!(results[0].name, "Project Plan");
        assert_eq!(results[0].url, "https://example.test/docx/one");
        assert_eq!(results[0].icon.as_deref(), Some("📄"));
        assert!(data.search_links("example.test").is_empty());
    }


    #[test]
    fn legacy_and_updated_open_modes_survive_round_trip() {
        let mut data: Directory = serde_json::from_str(r#"{"entries":[
            {"id":"folder","parent":null,"name":"Folder","url":null,"expanded":true},
            {"id":"doc","parent":"folder","name":"Doc","url":"https://example.test/doc","expanded":true}
        ]}"#).unwrap();
        assert_eq!(data.entries[1].open_mode, LinkOpenMode::Local);
        data.entries[1].open_mode = LinkOpenMode::Online;
        let saved = serde_json::to_value(&data).unwrap();
        let mut loaded: Directory = serde_json::from_value(saved).unwrap();
        assert_eq!(loaded.entries[1].open_mode, LinkOpenMode::Online);
        loaded.entries[1].open_mode = LinkOpenMode::Local;
        let saved = serde_json::to_value(&loaded).unwrap();
        let loaded: Directory = serde_json::from_value(saved).unwrap();
        assert_eq!(loaded.entries[1].open_mode, LinkOpenMode::Local);
    }


    #[test]
    fn folder_choices_disambiguate_nested_and_duplicate_names() {
        let data: Directory = serde_json::from_str(r#"{"entries":[
            {"id":"first","parent":null,"name":"Notes","url":null,"expanded":true},
            {"id":"nested","parent":"first","name":"Notes","url":null,"expanded":true},
            {"id":"second","parent":null,"name":"Notes","url":null,"expanded":false}
        ]}"#).unwrap();
        let choices = data.folder_choices();
        let first = &choices.iter().find(|(id, _)| id == "first").unwrap().1;
        let nested = &choices.iter().find(|(id, _)| id == "nested").unwrap().1;
        let second = &choices.iter().find(|(id, _)| id == "second").unwrap().1;
        assert_ne!(first, nested);
        assert_ne!(first, second);
    }

    #[test]
    fn adding_to_nested_folder_expands_its_ancestors() {
        let mut data: Directory = serde_json::from_str(r#"{"entries":[
            {"id":"parent","parent":null,"name":"Parent","url":null,"expanded":false},
            {"id":"child","parent":"parent","name":"Child","url":null,"expanded":false},
            {"id":"other","parent":null,"name":"Other","url":null,"expanded":false}
        ]}"#).unwrap();
        data.expand_path(Some("child"));
        assert!(data.entries[0].expanded && data.entries[1].expanded);
        assert!(!data.entries[2].expanded);
    }
}
