//! Tabs shared by local documents and in-app online pages.

use crate::app::Workspace;
use crate::theme::{self, Theme};
use crate::ui::{TipExt, TipSide, TitleBarExt, icon, ui_text};
use gpui::{prelude::FluentBuilder, *};
use unicode_segmentation::UnicodeSegmentation;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum TabKey {
    Local(String),
    Online(String),
}

pub(crate) struct TabLabel {
    pub key: TabKey,
    pub title: String,
    pub icon: Option<String>,
    pub active: bool,
}

pub(crate) fn render_tab_bar(labels: Vec<TabLabel>, scroll: &ScrollHandle, cx: &mut Context<Workspace>) -> AnyElement {
    let mut tabs = div().id("tab-scroll").flex_1().min_w_0().h_full().flex().items_center()
        .overflow_x_scroll().track_scroll(scroll);
    for label in labels {
        let (kind, id, online) = match &label.key {
            TabKey::Local(id) => ("local", id, false),
            TabKey::Online(id) => ("online", id, true),
        };
        let tab_id = SharedString::from(format!("tab-{kind}-{id}"));
        let title_id = SharedString::from(format!("tab-title-{kind}-{id}"));
        let close_id = SharedString::from(format!("tab-close-{kind}-{id}"));
        let select_key = label.key.clone();
        let close_key = label.key;
        // The shared tooltip is single-line per entry; wrap long titles so they
        // remain readable even at the minimum window width.
        let mut tip = String::new();
        let mut column = 0;
        for grapheme in label.title.graphemes(true) {
            if grapheme == "\n" {
                column = 0;
            } else if column == 28 {
                tip.push('\n');
                column = 0;
            }
            tip.push_str(grapheme);
            if grapheme != "\n" { column += 1; }
        }
        let glyph = div().relative().size(px(18.)).flex_none().flex().items_center().justify_center()
            .map(|d| match label.icon {
                Some(emoji) => d.text_size(px(14.)).line_height(px(18.)).child(emoji)
                    .when(online, |d| d.child(div().absolute().right_0().bottom_0().size(px(10.)).rounded_full()
                        .bg(Theme::bg_body()).child(crate::editor_ui::icon("globe", 10., Theme::link())))),
                None if online => d.child(crate::editor_ui::icon("globe", 14., Theme::icon())),
                None => d.child(icon("doc", 14., Theme::icon())),
            });
        let title = div().id(title_id).relative().flex_1().min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis()
            .tip_label(tip, TipSide::Right).child(ui_text(label.title));
        let close = div().id(close_id).no_drag().size(px(20.)).flex_none().flex().items_center().justify_center()
            .rounded(px(5.)).cursor_pointer().hover(|s| s.bg(Theme::fill_pressed()))
            .tip_label(crate::settings::tr("关闭标签", "Close tab").into(), TipSide::Right).child(icon("x", 12., Theme::icon()))
            .on_click(cx.listener(move |w, _, window, cx| {
                cx.stop_propagation();
                w.close_tab(close_key.clone(), window, cx);
            }));
        tabs = tabs.child(div().id(tab_id).no_drag().w(px(168.)).max_w_full().min_w_0().h(px(32.)).flex_none()
            .px(px(6.)).flex().items_center().gap(px(4.)).rounded(px(8.)).cursor_pointer()
            .text_size(px(13.)).line_height(px(20.))
            .when(label.active, |d| d.bg(Theme::fill_hover_strong()).text_color(Theme::text()).font_weight(FontWeight::SEMIBOLD))
            .when(!label.active, |d| d.text_color(Theme::text_caption()).hover(|s| s.bg(Theme::fill_hover())))
            .child(glyph).child(title).child(close)
            .on_click(cx.listener(move |w, _, window, cx| w.select_tab(select_key.clone(), window, cx))));
    }
    div().id("tab-bar").h(px(32.)).flex_1().min_w_0().flex().items_center()
        .font_family(theme::UI_FONT).child(tabs).into_any_element()
}
