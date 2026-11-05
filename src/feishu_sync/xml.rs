//! Encode one inserted block without leaking local identities or uploading from arbitrary paths.
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{anyhow, bail, Context, Result};
use quick_xml::{events::{BytesStart, Event}, Reader, Writer};
use crate::model::{Block, BlockKind, Document, InlineKind};

static ASSET_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[cfg(test)]
pub(super) fn encode(block: &Block, root: &Path) -> Result<String> {
    encode_inner(block, root, None).map(|(xml, _)| xml)
}

pub(super) fn encode_with_assets(block: &Block, root: &Path, upload_root: &Path) -> Result<(String, Vec<(PathBuf, PathBuf)>)> {
    encode_inner(block, root, Some(upload_root))
}

fn encode_inner(block: &Block, root: &Path, upload_root: Option<&Path>) -> Result<(String, Vec<(PathBuf, PathBuf)>)> {
    let mut block = block.clone();
    // Planner emits ordinary children as separate insert operations. Cell content,
    // in contrast, is part of the table insertion and must remain intact.
    block.children.clear();
    let mut mappings = Vec::new();
    prepare_block(&mut block, root, upload_root, &mut mappings)?;
    let doc = Document { blocks: vec![block], ..Document::default() };
    let xml = crate::docxml::serialize(&doc);
    let (_, body) = xml.split_once('\n').ok_or_else(|| anyhow!("生成块 XML 失败"))?;
    Ok((body.trim_end().to_string(), mappings))
}

fn prepare_block(block: &mut Block, root: &Path, upload_root: Option<&Path>, mappings: &mut Vec<(PathBuf, PathBuf)>) -> Result<()> {
    block.source_id = None;
    block.sync_id = None;
    match &mut block.kind {
        BlockKind::Image { src_kind, src, crop, .. } => {
            if crop.is_some() { bail!("飞书图片写入暂不支持裁剪，无法安全同步裁剪后的图片"); }
            if matches!(src_kind.as_str(), "path" | "src" | "href") && (src.starts_with("https://") || src.starts_with("http://")) {
                *src_kind = "href".into();
            } else if src_kind == "src" && valid_token(src) {
                // DocxXML's src is the existing image token, not a local path.
            } else {
                *src = resolve_asset(src_kind, src, root, upload_root, mappings)?;
                *src_kind = "path".into();
            }
        }
        BlockKind::File(file) => {
            if file.src_kind == "token" {
                if !valid_token(&file.src) { bail!("飞书附件 token 无效"); }
            } else {
                file.src = resolve_asset(&file.src_kind, &file.src, root, upload_root, mappings)?;
                file.src_kind = "path".into();
            }
        }
        BlockKind::Table(table) => for row in &mut table.rows {
            for cell in row {
                cell.source_id = None;
                cell.sync_id = None;
                for child in &mut cell.blocks { prepare_block(child, root, upload_root, mappings)?; }
            }
        },
        BlockKind::Raw { .. } => bail!("未知块无法安全创建，未发送任何内容"),
        _ => {}
    }
    for run in &mut block.text.runs {
        if let Some(inline) = &mut run.marks.inline {
            if let InlineKind::File(file) = &mut inline.kind {
                let (kind, source) = if file.src_kind == "token" {
                    if !valid_token(&file.src) { bail!("飞书附件 token 无效"); }
                    ("token", file.src.clone())
                } else {
                    ("path", resolve_asset(&file.src_kind, &file.src, root, upload_root, mappings)?)
                };
                inline.xml = rewrite_inline(&inline.xml, Some((kind, &source)))?;
                file.src_kind = kind.into();
                file.src = source;
            } else {
                inline.xml = rewrite_inline(&inline.xml, None)?;
            }
        }
    }
    for child in &mut block.children { prepare_block(child, root, upload_root, mappings)?; }
    Ok(())
}

fn valid_token(token: &str) -> bool {
    !token.is_empty() && token.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
}

// Preserve unmodelled attachment attributes rather than reconstructing the inline element.
fn rewrite_inline(xml: &str, source: Option<(&str, &str)>) -> Result<String> {
    let mut reader = Reader::from_str(xml);
    let mut writer = Writer::new(Vec::new());
    let mut sources = 0;
    loop {
        match reader.read_event().context("行内附件 XML 无效")? {
            event @ (Event::Start(_) | Event::Empty(_)) => {
                let empty = matches!(&event, Event::Empty(_));
                let element = match event { Event::Start(e) | Event::Empty(e) => e, _ => unreachable!() };
                let name = std::str::from_utf8(element.name().as_ref())?.to_owned();
                let mut attrs = Vec::new();
                for attr in element.attributes() {
                    let attr = attr.context("行内附件属性无效")?;
                    let key = std::str::from_utf8(attr.key.as_ref())?.to_owned();
                    if key == "data-quiet-sync-id" || (name == "source" && matches!(key.as_str(), "id" | "path" | "src" | "href" | "token")) { continue; }
                    attrs.push((key, attr.unescape_value()?.into_owned()));
                }
                if name == "source" {
                    sources += 1;
                    let (kind, value) = source.ok_or_else(|| anyhow!("未知行内对象包含附件，无法安全同步"))?;
                    attrs.push((kind.to_string(), value.to_string()));
                }
                let mut start = BytesStart::new(name.as_str());
                for (key, value) in &attrs { start.push_attribute((key.as_str(), value.as_str())); }
                writer.write_event(if empty { Event::Empty(start) } else { Event::Start(start) })?;
            }
            Event::Eof => break,
            other => writer.write_event(other)?,
        }
    }
    if source.is_some() && sources != 1 { bail!("行内附件 XML 必须包含一个 source，无法安全同步"); }
    String::from_utf8(writer.into_inner()).context("生成行内附件 XML 失败")
}

fn resolve_asset(src_kind: &str, src: &str, root: &Path, upload_root: Option<&Path>, mappings: &mut Vec<(PathBuf, PathBuf)>) -> Result<String> {
    if src_kind == "token" || src.is_empty() || (src.contains("://") && !src.starts_with("file://")) || src.starts_with("data:") {
        bail!("素材不是可上传的本地文件：{src}");
    }
    let raw = src.strip_prefix("file://").unwrap_or(src);
    // A foreign-platform drive or UNC path must never become a relative path.
    if cfg!(not(windows)) && (raw.contains('\\') || raw.as_bytes().get(1) == Some(&b':')) {
        bail!("素材路径不属于当前平台：{src}");
    }
    let path = Path::new(raw);
    if !path.is_absolute() && (path.has_root() || path.components().any(|c| matches!(c, Component::Prefix(_)))) {
        bail!("素材路径不是完整的本地路径：{src}");
    }
    let candidate = if path.is_absolute() { path.to_path_buf() } else { root.join(path) };
    let source = candidate.canonicalize().with_context(|| format!("素材文件不存在或无法访问：{}", candidate.display()))?;
    if !source.is_file() { bail!("素材不是普通文件：{}", source.display()); }
    if upload_root.is_some() {
        let extension = source.extension().and_then(|x| x.to_str()).filter(|s| s.chars().all(|c| c.is_ascii_alphanumeric())).unwrap_or("");
        let number = ASSET_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let name = if extension.is_empty() { format!("{number}") } else { format!("{number}.{extension}") };
        let target = PathBuf::from("assets").join(name);
        let result = format!("@./{}", target.to_string_lossy().replace('\\', "/"));
        mappings.push((source, target));
        return Ok(result);
    }
    let root = root.canonicalize().with_context(|| format!("无法访问文档库：{}", root.display()))?;
    let relative = source.strip_prefix(&root).map_err(|_| anyhow!("素材位于文档库外，需要受控临时上传目录：{}", source.display()))?;
    if !relative.components().all(|c| matches!(c, Component::Normal(_))) { bail!("素材相对路径无效：{}", relative.display()); }
    let relative = relative.to_str().ok_or_else(|| anyhow!("素材路径不是有效 UTF-8：{}", relative.display()))?;
    if cfg!(not(windows)) && relative.contains('\\') { bail!("素材路径含不支持的反斜杠：{relative}"); }
    Ok(format!("@./{}", relative.replace('\\', "/")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{FileBlock, FileView, Marks, RichText, Table, TableCell};

    struct Files(PathBuf);
    impl Files {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("quiet-sync-xml-{}-{}", std::process::id(), ASSET_SEQUENCE.fetch_add(1, Ordering::Relaxed)));
            std::fs::create_dir_all(path.join("library/assets")).unwrap();
            std::fs::write(path.join("library/assets/pic.png"), b"image").unwrap();
            std::fs::write(path.join("library/assets/a.pdf"), b"file").unwrap();
            std::fs::write(path.join("outside.pdf"), b"outside").unwrap();
            Self(path)
        }
        fn root(&self) -> PathBuf { self.0.join("library") }
    }
    impl Drop for Files {
        fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.0); }
    }

    #[test]
    fn paragraph_link_and_identity() {
        let files = Files::new();
        let mut text = RichText::default();
        text.push("go", Marks { bold: true, link: Some("https://example.com/?a=1&b=2".into()), ..Marks::default() });
        let mut block = Block::new(BlockKind::Text, text);
        block.source_id = Some("old-cloud-id".into());
        block.sync_id = Some("private-id".into());
        let xml = encode(&block, &files.root()).unwrap();
        assert!(xml.contains("<strong>") || xml.contains("<b>"));
        assert!(xml.contains("https://example.com/?a=1&amp;b=2"));
        assert!(!xml.contains("old-cloud-id") && !xml.contains("private-id") && !xml.contains("data-quiet-sync-id"));
    }

    #[test]
    fn nested_lists_are_individual_insertions() {
        let files = Files::new();
        let mut parent = Block::new(BlockKind::Bullet, RichText::plain("parent"));
        parent.children.push(Block::new(BlockKind::Ordered { seq: Some(2) }, RichText::plain("child")));
        let parent_xml = encode(&parent, &files.root()).unwrap();
        let child_xml = encode(&parent.children[0], &files.root()).unwrap();
        assert!(parent_xml.contains("<ul>") && parent_xml.contains("parent") && !parent_xml.contains("child"));
        assert!(child_xml.contains("<ol>") && child_xml.contains("child") && child_xml.contains("seq=\"2\""));
    }

    #[test]
    fn table_preserves_merged_cells_and_media_without_ids() {
        let files = Files::new();
        let image = Block::new(BlockKind::Image { src_kind: "path".into(), src: "assets/pic.png".into(), width: Some(120.), height: Some(80.), caption: None, crop: None }, RichText::default());
        let file = Block::new(BlockKind::File(FileBlock { src_kind: "path".into(), src: "assets/a.pdf".into(), name: "report.pdf".into(), view: FileView::Preview }), RichText::default());
        let mut cell = TableCell::empty();
        cell.colspan = 2;
        cell.source_id = Some("old-cell".into());
        cell.sync_id = Some("private-cell".into());
        cell.blocks = vec![image, file];
        let table = Table { col_widths: vec![100., 100.], header_rows: 0, header_col: false, rows: vec![vec![cell, TableCell::covered()]] };
        let mut block = Block::new(BlockKind::Table(table), RichText::default());
        block.source_id = Some("old-table".into());
        let xml = encode(&block, &files.root()).unwrap();
        assert!(xml.contains("colspan=\"2\"") && xml.contains("width=\"120\""));
        assert!(xml.contains("path=\"@./assets/pic.png\"") && xml.contains("path=\"@./assets/a.pdf\""));
        assert!(xml.contains("name=\"report.pdf\"") && xml.contains("view-type=\"Preview\""));
        assert!(!xml.contains("old-cell") && !xml.contains("private-cell") && !xml.contains("old-table"));
    }

    #[test]
    fn remote_images_use_href_but_remote_attachments_are_rejected() {
        let files = Files::new();
        for url in ["https://example.com/a.png?x=1&y=2", "http://example.com/a.png"] {
            let image = Block::new(BlockKind::Image { src_kind: "src".into(), src: url.into(), width: Some(40.), height: None, caption: None, crop: None }, RichText::default());
            let (xml, mappings) = encode_with_assets(&image, &files.root(), &files.0.join("upload")).unwrap();
            assert!(xml.contains(&format!("href=\"{}\"", url.replace('&', "&amp;"))));
            assert!(xml.contains("width=\"40\"") && mappings.is_empty());
            let file = Block::new(BlockKind::File(FileBlock { src_kind: "path".into(), src: url.into(), name: "a.png".into(), view: FileView::Card }), RichText::default());
            assert!(encode_with_assets(&file, &files.root(), &files.0.join("upload")).is_err());
        }
    }

    #[test]
    fn existing_media_tokens_are_not_treated_as_paths() {
        let files = Files::new();
        let image = Block::new(BlockKind::Image { src_kind: "src".into(), src: "image_token".into(), width: None, height: None, caption: None, crop: None }, RichText::default());
        let file = Block::new(BlockKind::File(FileBlock { src_kind: "token".into(), src: "file_token".into(), name: "报告.pdf".into(), view: FileView::Preview }), RichText::default());
        let (image_xml, image_files) = encode_with_assets(&image, &files.root(), &files.0).unwrap();
        let (file_xml, file_files) = encode_with_assets(&file, &files.root(), &files.0).unwrap();
        assert!(image_xml.contains("src=\"image_token\"") && image_files.is_empty());
        assert!(file_xml.contains("token=\"file_token\"") && file_xml.contains("name=\"报告.pdf\"") && file_files.is_empty());
        let inline = crate::model::Inline { kind: InlineKind::File(FileBlock { src_kind: "token".into(), src: "file_token".into(), name: "报告.pdf".into(), view: FileView::Card }), xml: "<source token=\"file_token\" name=\"报告.pdf\"/>".into() };
        let mut text = RichText::default();
        text.push("\u{fffc}", Marks { inline: Some(inline), ..Marks::default() });
        assert!(encode(&Block::new(BlockKind::Text, text), &files.root()).unwrap().contains("token=\"file_token\""));
    }

    #[test]
    fn cropped_image_is_rejected_without_altering_the_source() {
        let files = Files::new();
        let image = Block::new(BlockKind::Image { src_kind: "path".into(), src: "assets/pic.png".into(), width: None, height: None, caption: None, crop: Some([0.1, 0.1, 0.9, 0.9]) }, RichText::default());
        assert!(encode(&image, &files.root()).unwrap_err().to_string().contains("裁剪"));
        assert_eq!(std::fs::read(files.root().join("assets/pic.png")).unwrap(), b"image");
    }

    #[test]
    fn path_boundaries_and_staging() {
        let files = Files::new();
        let outside = files.0.join("outside.pdf");
        let src = outside.to_str().unwrap();
        let block = Block::new(BlockKind::File(FileBlock { src_kind: "path".into(), src: src.into(), name: "outside.pdf".into(), view: FileView::Card }), RichText::default());
        assert!(encode(&block, &files.root()).unwrap_err().to_string().contains("文档库外"));
        let (xml, mappings) = encode_with_assets(&block, &files.root(), &files.0.join("upload")).unwrap();
        assert_eq!(mappings.len(), 1);
        assert_eq!(mappings[0].0, outside.canonicalize().unwrap());
        assert!(mappings[0].1.starts_with("assets"));
        assert!(xml.contains("@./assets/") && !xml.contains(src));
        assert!(resolve_asset("path", "../outside.pdf", &files.root(), Some(&files.0), &mut vec![]).is_ok());
        assert!(resolve_asset("path", "../outside.pdf", &files.root(), None, &mut vec![]).is_err());
        assert!(resolve_asset("path", "assets/missing.pdf", &files.root(), Some(&files.0), &mut vec![]).is_err());
        assert!(resolve_asset("token", "cloud-token", &files.root(), Some(&files.0), &mut vec![]).is_err());
        #[cfg(not(windows))]
        assert!(resolve_asset("path", r"C:\other\file.pdf", &files.root(), Some(&files.0), &mut vec![]).is_err());
    }

    #[test]
    fn inline_attachment_preserves_attributes_and_rewrites_path() {
        let files = Files::new();
        let file = FileBlock { src_kind: "path".into(), src: "assets/a.pdf".into(), name: "a.pdf".into(), view: FileView::Card };
        let inline = crate::model::Inline { kind: InlineKind::File(file), xml: "<source id=\"old\" data-quiet-sync-id=\"private\" path=\"assets/a.pdf\" name=\"a.pdf\" extra=\"keep\"/>".into() };
        let mut text = RichText::default();
        text.push("\u{fffc}", Marks { inline: Some(inline), ..Marks::default() });
        let xml = encode(&Block::new(BlockKind::Text, text), &files.root()).unwrap();
        assert!(xml.contains("path=\"@./assets/a.pdf\"") && xml.contains("extra=\"keep\""));
        assert!(!xml.contains("id=\"old\"") && !xml.contains("data-quiet-sync-id"));
    }
}
