//! Conservative conversion between DocxXML rich text and Feishu text elements.
use anyhow::{anyhow, bail, Result};
use quick_xml::events::Event;
use quick_xml::Reader;
use serde_json::{json, Value};
use crate::model::{BlockKind, ColorName, Marks, RichText};

// FontColor and FontBackgroundColor enum order from Feishu's docx Block
// schema. RGB/RGBA strings are the corresponding full-detail DocxXML values.
const FONT_COLORS: [(&str, &str); 8] = [("", ""), ("red", "rgb(216,57,49)"), ("orange", "rgb(222,120,2)"),
    ("yellow", "rgb(220,155,4)"), ("green", "rgb(46,161,33)"), ("blue", "rgb(36,91,219)"),
    ("purple", "rgb(100,37,208)"), ("gray", "rgb(143,149,158)")];
const BG_COLORS: [(&str, &str); 16] = [("", ""), ("light-red", "rgb(251,191,188)"),
    ("light-orange", "rgba(254,212,164,0.8)"), ("light-yellow", "rgba(255,246,122,0.8)"),
    ("light-green", "rgba(183,237,177,0.8)"), ("light-blue", "rgba(186,206,253,0.7)"),
    ("light-purple", "rgba(205,178,250,0.7)"), ("medium-gray", "rgba(222,224,227,0.8)"),
    ("red", "rgb(247,105,100)"), ("orange", "rgb(255,165,61)"), ("yellow", "rgb(255,233,40)"),
    ("green", "rgb(98,210,86)"), ("blue", "rgba(78,131,253,0.55)"),
    ("purple", "rgba(147,90,246,0.55)"), ("gray", "rgb(187,191,196)"), ("light-gray", "rgb(242,243,245)")];

fn color_index(name: &str, background: bool) -> Result<usize> {
    let colors: &[(&str, &str)] = if background { &BG_COLORS } else { &FONT_COLORS };
    let name = if background { crate::palette::bg_canon(name) } else { name };
    colors.iter().position(|&(canonical, rgb)| !canonical.is_empty() && (canonical == name || rgb == name))
        .ok_or_else(|| anyhow!("颜色 {name} 无法安全映射到飞书"))
}

pub(crate) fn canonical_color(name: &str, background: bool) -> Option<&'static str> {
    let colors: &[(&str, &str)] = if background { &BG_COLORS } else { &FONT_COLORS };
    color_index(name, background).ok().map(|index| colors[index].0)
}

fn cloud_color(style: Option<&Value>, key: &str, background: bool) -> Result<Option<ColorName>> {
    let Some(value) = style.and_then(|s| s.get(key)) else { return Ok(None); };
    let colors: &[(&str, &str)] = if background { &BG_COLORS } else { &FONT_COLORS };
    let index = value.as_u64().and_then(|n| usize::try_from(n).ok()).filter(|&n| n > 0 && n < colors.len())
        .ok_or_else(|| anyhow!("云端颜色无法安全映射"))?;
    Ok(Some(ColorName(colors[index].0.into())))
}

// Encode the entire URL as one RFC 3986 component, not as form data. In
// particular, '+' is encoded and percent escapes in the URL are preserved.
fn encode_link(url: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut result = String::with_capacity(url.len());
    for b in url.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            result.push(char::from(b));
        } else {
            result.push('%');
            result.push(char::from(HEX[(b >> 4) as usize]));
            result.push(char::from(HEX[(b & 15) as usize]));
        }
    }
    result
}

fn decode_link(url: &str) -> Result<String> {
    let bytes = url.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = |b: u8| (b as char).to_digit(16);
            let value = bytes.get(i + 1).and_then(|b| hex(*b))
                .zip(bytes.get(i + 2).and_then(|b| hex(*b)))
                .ok_or_else(|| anyhow!("云端链接编码无效"))?;
            decoded.push((value.0 * 16 + value.1) as u8);
            i += 3;
        } else {
            decoded.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(decoded).map_err(|_| anyhow!("云端链接编码无效"))
}

pub(super) fn comparable_text(text: &RichText) -> Result<RichText> {
    let mut normalized = RichText::default();
    let mut pos = 0;
    for run in &text.runs {
        let end = pos + run.len;
        let content = text.text.get(pos..end).ok_or_else(|| anyhow!("本地文字样式范围无效"))?;
        let mut marks = run.marks.clone();
        if let Some(color) = &mut marks.text_color {
            color.0 = canonical_color(&color.0, false).ok_or_else(|| anyhow!("文字颜色无法安全映射"))?.into();
        }
        if let Some(color) = &mut marks.bg_color {
            color.0 = canonical_color(&color.0, true).ok_or_else(|| anyhow!("背景颜色无法安全映射"))?.into();
        }
        normalized.push(content, marks);
        pos = end;
    }
    if pos != text.text.len() { bail!("本地文字样式范围无效"); }
    Ok(normalized)
}

pub(super) fn elements(text: &RichText) -> Result<Vec<Value>> {
    let mut result = Vec::new();
    let mut pos = 0;
    for run in &text.runs {
        let m = &run.marks;
        if m.inline.is_some() { bail!("段落含行内对象，无法安全同步"); }
        let end = pos + run.len;
        let content = text.text.get(pos..end).ok_or_else(|| anyhow!("本地文字样式范围无效"))?;
        let mut style = json!({"bold": m.bold, "italic": m.italic, "underline": m.underline,
            "strikethrough": m.strike, "inline_code": m.code});
        if let Some(color) = &m.text_color { style["text_color"] = json!(color_index(&color.0, false)?); }
        if let Some(color) = &m.bg_color { style["background_color"] = json!(color_index(&color.0, true)?); }
        if let Some(link) = &m.link { style["link"] = json!({"url": encode_link(link)}); }
        result.push(json!({"text_run": {"content": content, "text_element_style": style}}));
        pos = end;
    }
    if pos != text.text.len() { bail!("本地文字样式范围无效"); }
    if result.is_empty() { result.push(json!({"text_run": {"content": ""}})); }
    Ok(result)
}

// Refuse to rewrite a target unless its original inline markup is understood.
pub(super) fn check_xml(xml: &str, id: &str) -> Result<()> {
    let mut reader = Reader::from_str(xml);
    let mut depth = 0usize;
    let mut target = None;
    let mut nested = None;
    let mut found = false;
    loop {
        match reader.read_event()? {
            Event::Start(e) | Event::Empty(e) => {
                let empty = reader.buffer_position() > 0 && xml.as_bytes().get(reader.buffer_position() as usize - 2) == Some(&b'/');
                let name = String::from_utf8_lossy(e.name().as_ref()).into_owned();
                let attrs = e.attributes().collect::<std::result::Result<Vec<_>, _>>()?;
                let is_target = attrs.iter().any(|a| a.key.as_ref() == b"id" && a.unescape_value().is_ok_and(|v| v == id));
                if is_target {
                    if found { bail!("原始文档含重复块 ID"); }
                    found = true; target = Some(depth);
                }
                // Nested lists are separate blocks, not part of the text update.
                let nested_list = nested.is_none() && target.is_some_and(|d| depth == d + 1)
                    && matches!(name.as_str(), "ul" | "ol");
                if nested_list && !empty { nested = Some(depth); }
                if target.is_some() && nested.is_none() && !nested_list {
                    let root = target == Some(depth);
                    let allowed = if root { matches!(name.as_str(), "p" | "li" | "checkbox") || (name.len() == 2 && name.starts_with('h') && matches!(name.as_bytes()[1], b'1'..=b'9')) }
                        else { matches!(name.as_str(), "b" | "strong" | "i" | "em" | "u" | "s" | "del" | "code" | "br" | "span" | "a") };
                    if !allowed { bail!("原始块含评论锚点或无法安全映射的结构"); }
                    if !root && name == "a" && !attrs.iter().any(|a| a.key.as_ref() == b"href") {
                        bail!("原始链接缺少地址");
                    }
                    for attr in &attrs {
                        let key = attr.key.as_ref();
                        if root && matches!(key, b"id" | b"align" | b"background-color" | b"folded" | b"seq" | b"done") { continue; }
                        if !root && name == "a" && key == b"href" { attr.unescape_value()?; continue; }
                        if !root && name == "span" && matches!(key, b"text-color" | b"background-color") {
                            color_index(&attr.unescape_value()?, key == b"background-color")?;
                        } else { bail!("原始块含评论锚点或无法安全映射的属性"); }
                    }
                }
                if !empty { depth += 1; } else if target == Some(depth) { target = None; }
            }
            Event::End(_) => {
                depth = depth.saturating_sub(1);
                if nested == Some(depth) { nested = None; }
                if target == Some(depth) { target = None; }
            }
            Event::Eof => break,
            Event::Comment(_) | Event::CData(_) | Event::PI(_) | Event::DocType(_) if target.is_some() && nested.is_none() => bail!("原始块含无法安全映射的结构"),
            _ => {}
        }
    }
    if !found { bail!("原始文档找不到对应块"); }
    Ok(())
}

pub(super) fn remote_text(response: &Value, kind: &BlockKind) -> Result<RichText> {
    let (expected_type, field) = match kind {
        BlockKind::Text => (2, "text".to_string()),
        BlockKind::Heading { level, .. } if (1..=9).contains(level) => (u64::from(*level) + 2, format!("heading{level}")),
        BlockKind::Bullet => (12, "bullet".to_string()),
        BlockKind::Ordered { .. } => (13, "ordered".to_string()),
        BlockKind::Todo { .. } => (17, "todo".to_string()),
        _ => bail!("本地块不是支持的文字块"),
    };
    parse_remote_text(response, expected_type, &field, false)
}

pub(super) fn remote_title(response: &Value) -> Result<RichText> {
    parse_remote_text(response, 1, "page", true)
}

fn parse_remote_text(response: &Value, expected_type: u64, field: &str, plain: bool) -> Result<RichText> {
    let block = response.pointer("/data/block").ok_or_else(|| anyhow!("飞书响应缺少块信息"))?;
    if let Some(ids) = block.get("comment_ids")
        && !ids.as_array().is_some_and(|ids| ids.is_empty())
    { bail!("云端块含评论或无效评论信息"); }
    let block_type = block.get("block_type").and_then(Value::as_u64).ok_or_else(|| anyhow!("飞书响应缺少块类型"))?;
    if block_type != expected_type { bail!("云端块类型与本地来源不符"); }
    let elements = block.get(field).and_then(|v| v.get("elements")).and_then(Value::as_array).ok_or_else(|| anyhow!("飞书响应缺少文字元素"))?;
    let mut cloud_text = RichText::default();
    for element in elements {
        let object = element.as_object().ok_or_else(|| anyhow!("云端文字元素无效"))?;
        if object.len() != 1 { bail!("云端块包含无法安全同步的行内对象"); }
        let run = element.get("text_run").and_then(Value::as_object).ok_or_else(|| anyhow!("云端块包含无法安全同步的行内对象"))?;
        if run.keys().any(|k| !matches!(k.as_str(), "content" | "text_element_style")) { bail!("云端文字包含未知属性"); }
        let style = run.get("text_element_style");
        let mut link = None;
        if let Some(style) = style {
            let style = style.as_object().ok_or_else(|| anyhow!("云端样式无效"))?;
            if let Some(ids) = style.get("comment_ids")
                && !ids.as_array().is_some_and(|ids| ids.is_empty())
            { bail!("云端块含评论锚点或无效评论信息"); }
            if style.keys().any(|k| !matches!(k.as_str(), "bold" | "italic" | "underline" | "strikethrough" | "inline_code" | "comment_ids" | "text_color" | "background_color" | "link")) {
                bail!("云端块包含无法安全映射的样式");
            }
            for key in ["bold", "italic", "underline", "strikethrough", "inline_code"] {
                if style.get(key).is_some_and(|v| !v.is_boolean()) { bail!("云端文字样式无效"); }
            }
            if let Some(value) = style.get("link") {
                let object = value.as_object().ok_or_else(|| anyhow!("云端链接无效"))?;
                if object.len() != 1 { bail!("云端链接含无法安全映射的属性"); }
                let url = object.get("url").and_then(Value::as_str).ok_or_else(|| anyhow!("云端链接缺少地址"))?;
                link = Some(decode_link(url)?);
            }
        }
        let text = run.get("content").and_then(Value::as_str).ok_or_else(|| anyhow!("飞书响应缺少文字内容"))?;
        let flag = |name: &str| style.and_then(|s| s.get(name)).and_then(Value::as_bool).unwrap_or(false);
        let marks = Marks { bold: flag("bold"), italic: flag("italic"), underline: flag("underline"),
            strike: flag("strikethrough"), code: flag("inline_code"), text_color: cloud_color(style, "text_color", false)?,
            bg_color: cloud_color(style, "background_color", true)?, link, ..Marks::default() };
        if plain && marks != Marks::default() { bail!("云端标题包含无法安全同步的样式"); }
        cloud_text.push(text, marks);
    }
    Ok(cloud_text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_plain_page_title_and_rejects_unsafe_content() {
        let response = json!({"data": {"block": {"block_type": 1, "page": {"elements": [
            {"text_run": {"content": "标题 ", "text_element_style": {"bold": false, "comment_ids": []}}},
            {"text_run": {"content": "100% + 中文"}}
        ]}}}});
        assert_eq!(remote_title(&response).unwrap(), RichText::plain("标题 100% + 中文"));

        let mut wrong_type = response.clone();
        wrong_type["data"]["block"]["block_type"] = json!(2);
        assert!(remote_title(&wrong_type).is_err());
        let mut missing_elements = response.clone();
        missing_elements["data"]["block"]["page"] = json!({});
        assert!(remote_title(&missing_elements).is_err());
        let mut commented = response.clone();
        commented["data"]["block"]["comment_ids"] = json!(["c"]);
        assert!(remote_title(&commented).is_err());
        commented["data"]["block"]["comment_ids"] = json!([]);
        commented["data"]["block"]["page"]["elements"][0]["text_run"]["text_element_style"]["comment_ids"] = json!(["c"]);
        assert!(remote_title(&commented).is_err());
        let mut styled = response.clone();
        styled["data"]["block"]["page"]["elements"][0]["text_run"]["text_element_style"]["bold"] = json!(true);
        assert!(remote_title(&styled).is_err());
        let mut unknown = response.clone();
        unknown["data"]["block"]["page"]["elements"][0] = json!({"mention_user": {"user_id": "u"}});
        assert!(remote_title(&unknown).is_err());
    }

    #[test]
    fn links_round_trip_as_encoded_components_without_losing_styles() {
        let url = "https://example.com/中文/%20?x=a+b&y=100%#片段";
        let mut text = RichText::plain("before ");
        text.push("链接", Marks { link: Some(url.into()), bold: true, underline: true,
            text_color: Some(ColorName("red".into())), bg_color: Some(ColorName("light-yellow".into())), ..Marks::default() });
        text.push(" after", Marks { italic: true, ..Marks::default() });
        let payload = elements(&text).unwrap();
        assert_eq!(payload[1]["text_run"]["text_element_style"]["link"]["url"],
            "https%3A%2F%2Fexample.com%2F%E4%B8%AD%E6%96%87%2F%2520%3Fx%3Da%2Bb%26y%3D100%25%23%E7%89%87%E6%AE%B5");
        let response = json!({"data": {"block": {"block_type": 2, "text": {"elements": payload}}}});
        assert_eq!(remote_text(&response, &BlockKind::Text).unwrap(), comparable_text(&text).unwrap());
        assert_eq!(decode_link("https%3A%2F%2Fx%2F%2520%3Fq%3Da+b").unwrap(), "https://x/%20?q=a+b");
    }

    #[test]
    fn reads_list_item_text_and_retains_styles() {
        for (kind, number, field) in [(BlockKind::Bullet, 12, "bullet"),
            (BlockKind::Ordered { seq: Some(3) }, 13, "ordered"), (BlockKind::Todo { done: true }, 17, "todo")] {
            let mut text = RichText::plain("hello ");
            text.push("world", Marks { bold: true, link: Some("https://example.com/?a=1+2".into()), ..Marks::default() });
            let payload = elements(&text).unwrap();
            let mut block = json!({"block_type": number});
            block[field] = json!({"elements": payload, "style": {"done": true, "align": 2}});
            let response = json!({"data": {"block": block}});
            assert_eq!(remote_text(&response, &kind).unwrap(), comparable_text(&text).unwrap());
        }
        assert!(check_xml("<ul><li id=\"b\"><a href=\"https://example.com/?a=1&amp;b=2\"><b>link</b></a></li></ul>", "b").is_ok());
        assert!(check_xml("<checkbox id=\"t\" done=\"true\">todo</checkbox>", "t").is_ok());
    }

    #[test]
    fn refuses_comments_unknown_inline_and_unmappable_colors() {
        assert!(check_xml("<p id=\"b\"><a href=\"x\" comment-id=\"c\">x</a></p>", "b").is_err());
        assert!(check_xml("<p id=\"b\"><span text-color=\"rgb(1,2,3)\">x</span></p>", "b").is_err());
        assert!(check_xml("<p id=\"b\"><mystery>x</mystery></p>", "b").is_err());
        for element in [json!({"mention_user": {"user_id": "x"}}),
            json!({"text_run": {"content": "x", "text_element_style": {"comment_ids": ["c"]}}}),
            json!({"text_run": {"content": "x", "text_element_style": {"link": {"url": "%FF"}}}}),
            json!({"text_run": {"content": "x", "text_element_style": {"text_color": 8}}})] {
            let response = json!({"data": {"block": {"block_type": 2, "text": {"elements": [element]}}}});
            assert!(remote_text(&response, &BlockKind::Text).is_err());
        }
    }
}
