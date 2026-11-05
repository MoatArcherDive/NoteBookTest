//! Targeted media writes. All preparation and preflight must precede any remote write.
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use quick_xml::{events::Event, Reader};
use serde_json::{json, Value};

use super::{check_update_result, cloud_id, read_block, remote_children, run, save, Source};
use crate::model::{Align, Block, BlockKind, FileView};

pub(super) struct Prepared {
    xml: String,
    cwd: PathBuf,
    expected: Block,
}

pub(super) struct Receipt {
    token: String,
}

/// The upload directory belongs to the caller and must remain alive through execution.
/// Copies are made during preparation; the originals are never modified.
pub(super) fn prepare(block: &Block, root: &Path, upload_root: &Path) -> Result<Prepared> {
    validate_block(block)?;
    let (xml, files) = super::xml::encode_with_assets(block, root, upload_root)?;
    for (source, relative) in files {
        let destination = upload_root.join(relative);
        std::fs::create_dir_all(destination.parent().ok_or_else(|| anyhow!("素材临时目录无效"))?)?;
        std::fs::copy(&source, &destination).with_context(|| format!("无法准备上传素材：{}", source.display()))?;
    }
    Ok(Prepared { xml, cwd: upload_root.to_owned(), expected: block.clone() })
}
pub(super) fn prepare_image_style(block: &Block) -> Prepared {
    Prepared { xml: String::new(), cwd: PathBuf::new(), expected: block.clone() }
}


/// Validate staged resources and probe public image URLs without writing a
/// document. The CLI uses a guarded range request for network images.
pub(super) fn check_prepared(source: &Source, prepared: &Prepared) -> Result<()> {
    let response = run(&["docs", "+script", "--as", "user", "--command", "parse", "--content", "-",
        "--presentation-decision", "{}"], Some(&prepared.xml), Some(&prepared.cwd))?;
    if response.pointer("/data/assessment/status").and_then(Value::as_str) != Some("passed") {
        bail!("素材无法通过飞书 CLI 资源预检");
    }
    let response = run(&["docs", "+update", "--doc", &source.document_id, "--as", "user", "--command", "block_replace",
        "--block-id", "0", "--content", "-", "--dry-run"], Some(&prepared.xml), Some(&prepared.cwd))?;
    if response.pointer("/data/error").is_some() || !response.pointer("/data/api").and_then(Value::as_array).is_some_and(|a| !a.is_empty()) {
        bail!("素材无法通过飞书 CLI 本地预检");
    }
    Ok(())
}

fn validate_block(block: &Block) -> Result<()> {
    if block.bg.is_some() || block.folded || !block.text.is_empty() || !block.children.is_empty() {
        bail!("素材块含无法安全写入的内容、子块或布局");
    }
    match &block.kind {
        BlockKind::Image { width, height, crop, .. } => {
            if crop.is_some() { bail!("飞书图片写入不支持裁剪"); }
            for size in [width, height].into_iter().flatten() {
                if !size.is_finite() || *size <= 0. || size.fract() != 0. || *size > i32::MAX as f32 {
                    bail!("飞书图片尺寸必须为正整数像素");
                }
            }
            // CLI normalizes larger widths to the document width, which would
            // otherwise silently change the requested presentation.
            if width.is_some_and(|w| w > 1020.) { bail!("飞书图片宽度超过 CLI 可保真的范围"); }
        }
        BlockKind::File(file) => {
            if block.align != Align::default() { bail!("附件对齐方式无法安全写入"); }
            if file.name.trim() != file.name || file.name.is_empty() || matches!(file.name.as_str(), "." | "..")
                || file.name.contains('/') || file.name.contains('\\') || file.name.chars().count() > 250 {
                bail!("附件文件名无效");
            }
        }
        _ => bail!("不是图片或附件块"),
    }
    Ok(())
}

// Replacement destroys the old block, so do not discard comments or fields
// which the local model cannot represent. This applies to every descendant.
fn check_original(xml: &str, id: &str) -> Result<()> {
    let mut reader = Reader::from_str(xml);
    let mut depth = 0usize;
    let mut target = None;
    let mut found = false;
    loop {
        match reader.read_event().context("原始素材 XML 无效")? {
            event @ (Event::Start(_) | Event::Empty(_)) => {
                let empty = matches!(&event, Event::Empty(_));
                let element = match event { Event::Start(e) | Event::Empty(e) => e, _ => unreachable!() };
                let tag = std::str::from_utf8(element.name().as_ref())?.to_owned();
                let attrs = element.attributes().collect::<std::result::Result<Vec<_>, _>>()?;
                if attrs.iter().any(|a| a.key.as_ref() == b"id" && a.unescape_value().is_ok_and(|v| v == id)) {
                    if found { bail!("原始文档含重复素材块 ID"); }
                    found = true;
                    target = Some(depth);
                }
                if target.is_some() {
                    if !matches!(tag.as_str(), "img" | "figure" | "source") { bail!("原始素材含无法保留的结构或评论"); }
                    for attr in &attrs {
                        if tag == "img" && attr.key.as_ref() == b"name" {
                            bail!("原始图片名称未被本地模型保存，无法安全替换");
                        }
                        if !matches!(attr.key.as_ref(), b"id" | b"src" | b"path" | b"href" | b"token" | b"data-ref" | b"name" | b"width" | b"height" | b"caption" | b"align" | b"view-type") {
                            bail!("原始素材含无法保留的属性或评论");
                        }
                        attr.unescape_value()?;
                    }
                }
                if !empty { depth += 1; } else if target == Some(depth) { target = None; }
            }
            Event::End(_) => {
                depth = depth.saturating_sub(1);
                if target == Some(depth) { target = None; }
            }
            Event::Eof => break,
            Event::Text(t) if target.is_some() && !t.unescape()?.trim().is_empty() => bail!("原始素材含无法保留的文字"),
            Event::Comment(_) | Event::CData(_) | Event::PI(_) | Event::DocType(_) if target.is_some() => bail!("原始素材含无法保留的结构或评论"),
            _ => {}
        }
    }
    if !found { bail!("原始文档找不到素材块"); }
    Ok(())
}

fn no_comments(block: &Value) -> Result<()> {
    if let Some(comments) = block.get("comment_ids")
        && !comments.as_array().is_some_and(Vec::is_empty) { bail!("素材块含评论，不能替换"); }
    Ok(())
}

fn id(value: &Value, field: &str) -> Result<String> {
    value.get(field).and_then(Value::as_str)
        .filter(|s| !s.is_empty() && s.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-'))
        .map(str::to_owned).ok_or_else(|| anyhow!("飞书返回的素材块身份无效"))
}

fn body(response: &Value) -> Result<&Value> {
    response.pointer("/data/block").filter(|v| v.is_object()).ok_or_else(|| anyhow!("飞书响应缺少素材块"))
}

/// Read-only check of the old block and its comments. The caller also checks
/// document revision and parent order before the first write.
pub(super) fn preflight(source: &Source, key: &str, old: &Block) -> Result<()> {
    validate_block(old)?;
    let cloud = cloud_id(key, source)?;
    check_original(&source.original_xml, &cloud)?;
    let response = read_block(&source.document_id, &cloud)?;
    let block = body(&response)?;
    no_comments(block)?;
    match &old.kind {
        BlockKind::Image { .. } => compare_image(block, old, None),
        BlockKind::File(_) => {
            let file = file_child(source, block, &cloud)?;
            no_comments(&file)?;
            compare_file(block, &file, old, None)
        }
        _ => bail!("不是图片或附件块"),
    }
}

/// True when the image keeps its source and can retain its cloud block ID.
pub(super) fn image_style_candidate(old: &Block, new: &Block) -> bool {
    match (&old.kind, &new.kind) {
        (BlockKind::Image { src_kind: a_kind, src: a, width: aw, height: ah, .. },
         BlockKind::Image { src_kind: b_kind, src: b, width: bw, height: bh, .. }) =>
            a_kind == b_kind && a == b && !(aw.is_some() && bw.is_none()) && !(ah.is_some() && bh.is_none()),
        _ => false,
    }
}

/// Run during the read-only preflight stage. The token comes from the actual
/// cloud block, not the local path.
pub(super) fn preflight_image_style(source: &Source, key: &str, old: &Block, new: &Block) -> Result<String> {
    if !image_style_candidate(old, new) { bail!("图片素材或尺寸重置不能原位更新"); }
    validate_block(old)?;
    validate_block(new)?;
    let cloud = cloud_id(key, source)?;
    check_original(&source.original_xml, &cloud)?;
    let response = read_block(&source.document_id, &cloud)?;
    let block = body(&response)?;
    no_comments(block)?;
    compare_image(block, old, None)?;
    block.pointer("/image/token").and_then(Value::as_str).filter(|s| !s.is_empty())
        .map(str::to_owned).ok_or_else(|| anyhow!("云端图片缺少素材 token"))
}

/// Update image presentation using replace_image; the block identity remains
/// unchanged. Caller must have preflighted and persisted interrupted first.
pub(super) fn update_image_style(source: &Source, key: &str, prepared: &Prepared, token: &str) -> Result<Receipt> {
    if !source.interrupted { bail!("写入素材前必须记录 interrupted"); }
    let BlockKind::Image { width, height, caption, .. } = &prepared.expected.kind else { bail!("不是图片"); };
    let align = match prepared.expected.align { Align::Left => 1, Align::Center => 2, Align::Right => 3 };
    let mut image = json!({"token":token, "align":align, "caption":{"content":caption.as_deref().unwrap_or("")}});
    if let Some(width) = width { image["width"] = json!(*width as u64); }
    if let Some(height) = height { image["height"] = json!(*height as u64); }
    let payload = json!({"replace_image":image}).to_string();
    let cloud = cloud_id(key, source)?;
    let path = format!("/open-apis/docx/v1/documents/{}/blocks/{cloud}", source.document_id);
    run(&["api", "PATCH", &path, "--as", "user", "--data", "-"], Some(&payload), None)?;
    verify_token(source, key, prepared, Some(token))?;
    Ok(Receipt { token: token.to_owned() })
}

fn file_child(source: &Source, view: &Value, view_id: &str) -> Result<Value> {
    if view.get("block_type").and_then(Value::as_u64) != Some(33) { bail!("附件缺少视图块"); }
    let children = view.get("children").and_then(Value::as_array).ok_or_else(|| anyhow!("附件视图缺少文件块"))?;
    if children.len() != 1 { bail!("附件视图含不支持的子块"); }
    let file_id = children[0].as_str().ok_or_else(|| anyhow!("附件文件块 ID 无效"))?;
    let response = read_block(&source.document_id, file_id)?;
    let file = body(&response)?;
    if file.get("block_type").and_then(Value::as_u64) != Some(23) || file.get("parent_id").and_then(Value::as_str) != Some(view_id) {
        bail!("附件文件块与视图关系不符");
    }
    Ok(file.clone())
}

fn compare_image(block: &Value, expected: &Block, token: Option<&str>) -> Result<()> {
    let BlockKind::Image { width, height, caption, .. } = &expected.kind else { bail!("不是图片"); };
    if block.get("block_type").and_then(Value::as_u64) != Some(27) { bail!("云端块不是图片"); }
    let image = block.get("image").and_then(Value::as_object).ok_or_else(|| anyhow!("云端图片内容缺失"))?;
    let actual_token = image.get("token").and_then(Value::as_str).filter(|s| !s.is_empty()).ok_or_else(|| anyhow!("云端图片缺少素材"))?;
    if token.is_some_and(|t| t != actual_token) { bail!("云端图片素材与返回值不符"); }
    for (name, value) in [("width", width), ("height", height)] {
        if let Some(value) = value
            && image.get(name).and_then(Value::as_u64) != Some(*value as u64) { bail!("云端图片尺寸不符"); }
    }
    let align = match expected.align { Align::Left => 1, Align::Center => 2, Align::Right => 3 };
    if image.get("align").and_then(Value::as_u64).unwrap_or(2) != align { bail!("云端图片对齐方式不符"); }
    let actual_caption = image.get("caption").and_then(|v| v.get("content")).and_then(Value::as_str).unwrap_or("");
    if actual_caption != caption.as_deref().unwrap_or("") { bail!("云端图片说明不符"); }
    Ok(())
}

fn compare_file(view: &Value, file: &Value, expected: &Block, token: Option<&str>) -> Result<()> {
    let BlockKind::File(expected) = &expected.kind else { bail!("不是附件"); };
    let actual = file.get("file").and_then(Value::as_object).ok_or_else(|| anyhow!("云端附件内容缺失"))?;
    let actual_token = actual.get("token").and_then(Value::as_str).filter(|s| !s.is_empty()).ok_or_else(|| anyhow!("云端附件缺少素材"))?;
    if token.is_some_and(|t| t != actual_token) { bail!("云端附件素材与返回值不符"); }
    if actual.get("name").and_then(Value::as_str) != Some(expected.name.as_str()) { bail!("云端附件文件名不符"); }
    let expected_view = match expected.view { FileView::Card => 1, FileView::Preview => 2 };
    if view.pointer("/view/view_type").and_then(Value::as_u64).unwrap_or(1) != expected_view { bail!("云端附件视图不符"); }
    Ok(())
}

fn binding_key(key: &str) -> Result<&str> {
    if let Some(local) = key.strip_prefix("l:").filter(|s| !s.is_empty()) { return Ok(local); }
    if key.strip_prefix("s:").is_some_and(|s| !s.is_empty()) { return Ok(key); }
    bail!("素材块缺少持久身份")
}

fn bind(id: &str, source: &mut Source, key: &str, cloud: &str) -> Result<()> {
    let binding = binding_key(key)?;
    if source.bindings.iter().any(|(k, v)| k != binding && v == cloud) { bail!("素材块返回了重复的云端 ID"); }
    source.bindings.insert(binding.to_owned(), cloud.to_owned());
    save(id, source).context("素材块已写入，但无法保存身份绑定；请重新下载并核对")
}

fn result_resource(response: &Value, image: bool) -> Result<(String, Option<String>)> {
    let blocks = response.pointer("/data/document/new_blocks").and_then(Value::as_array).ok_or_else(|| anyhow!("素材操作未返回新块 ID"))?;
    let kind = if image { "image" } else { "file" };
    let number = if image { 27 } else { 23 };
    let mut found = blocks.iter().filter(|b| b.get("block_type").is_some_and(|t| t.as_str() == Some(kind) || t.as_u64() == Some(number)));
    let block = found.next().ok_or_else(|| anyhow!("素材操作未返回资源块"))?;
    if found.next().is_some() { bail!("素材操作返回的资源块数量不符"); }
    let block_id = id(block, "block_id")?;
    let token = block.get("block_token").and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_owned);
    Ok((block_id, token))
}

/// Insert at an arbitrary native parent/index, including table cells. The
/// placeholder is bound before replacement; interrupted must already be saved.
pub(super) fn insert(id_: &str, source: &mut Source, key: &str, parent: &str, index: usize, prepared: &Prepared) -> Result<Receipt> {
    if !source.interrupted { bail!("写入素材前必须记录 interrupted"); }
    if !key.starts_with("l:") || source.bindings.contains_key(binding_key(key)?) { bail!("新增素材身份无效或已绑定"); }
    let image = matches!(prepared.expected.kind, BlockKind::Image { .. });
    let child = if image { json!({"block_type":27,"image":{}}) } else { json!({"block_type":23,"file":{"token":""}}) };
    let path = format!("/open-apis/docx/v1/documents/{}/blocks/{parent}/children", source.document_id);
    let payload = json!({"index":index,"children":[child]}).to_string();
    let response = run(&["api", "POST", &path, "--as", "user", "--data", "-"], Some(&payload), None)?;
    let children = response.pointer("/data/children").and_then(Value::as_array).ok_or_else(|| anyhow!("创建素材占位块未返回 ID"))?;
    if children.len() != 1 { bail!("创建素材占位块返回数量不符"); }
    let expected_type = if image { 27 } else { 33 };
    if children[0].get("block_type").and_then(Value::as_u64) != Some(expected_type) { bail!("素材占位块类型不符"); }
    let placeholder = id(&children[0], "block_id")?;
    bind(id_, source, key, &placeholder)?;
    std::thread::sleep(std::time::Duration::from_millis(350));
    replace(id_, source, key, prepared)
}

/// Replacement may create a new block. On any failure the caller must leave
/// interrupted set; never infer identity by content or by position.
pub(super) fn replace(id_: &str, source: &mut Source, key: &str, prepared: &Prepared) -> Result<Receipt> {
    if !source.interrupted { bail!("写入素材前必须记录 interrupted"); }
    let target = cloud_id(key, source)?;
    let response = run(&["docs", "+update", "--doc", &source.document_id, "--as", "user", "--command", "block_replace", "--block-id", &target, "--content", "-"], Some(&prepared.xml), Some(&prepared.cwd))?;
    let image = matches!(prepared.expected.kind, BlockKind::Image { .. });
    let (resource, token) = result_resource(&response, image)?;
    // Persist the returned ID even before the additional GET for a file's view.
    bind(id_, source, key, &resource)?;
    if !image {
        let file_response = read_block(&source.document_id, &resource)?;
        let file = body(&file_response)?;
        if file.get("block_type").and_then(Value::as_u64) != Some(23) { bail!("新附件资源块类型不符"); }
        let parent = id(file, "parent_id")?;
        let view_response = read_block(&source.document_id, &parent)?;
        let view = body(&view_response)?;
        let children = remote_children(&view_response)?;
        if view.get("block_type").and_then(Value::as_u64) != Some(33) || children.len() != 1 || children[0] != resource {
            bail!("新附件视图与文件块关系不符");
        }
        bind(id_, source, key, &parent)?;
    }
    check_update_result(&response)?;
    let token = token.ok_or_else(|| anyhow!("素材操作未返回真实 token，无法核对素材"))?;
    verify_token(source, key, prepared, Some(&token))?;
    Ok(Receipt { token })
}

fn verify_token(source: &Source, key: &str, prepared: &Prepared, token: Option<&str>) -> Result<()> {
    let cloud = cloud_id(key, source)?;
    let response = read_block(&source.document_id, &cloud)?;
    let block = body(&response)?;
    match &prepared.expected.kind {
        BlockKind::Image { .. } => compare_image(block, &prepared.expected, token),
        BlockKind::File(_) => {
            let file = file_child(source, block, &cloud)?;
            compare_file(block, &file, &prepared.expected, token)
        }
        _ => bail!("不是图片或附件"),
    }
}

/// Called again after all writes; the caller separately verifies parent order.
pub(super) fn verify(source: &Source, key: &str, prepared: &Prepared, receipt: &Receipt) -> Result<()> {
    verify_token(source, key, prepared, Some(&receipt.token))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{FileBlock, RichText};

    fn image() -> Block {
        let mut block = Block::new(BlockKind::Image { src_kind: "src".into(), src: "cloud_token".into(), width: Some(200.), height: Some(100.), caption: Some("caption".into()), crop: None }, RichText::default());
        block.align = Align::Center;
        block
    }

    #[test]
    fn image_style_updates_keep_source_and_reject_size_reset() {
        let old = image();
        let mut new = old.clone();
        if let BlockKind::Image { caption, width, .. } = &mut new.kind {
            *caption = Some("new description".into());
            *width = Some(300.);
        }
        assert!(image_style_candidate(&old, &new));
        if let BlockKind::Image { width, .. } = &mut new.kind { *width = None; }
        assert!(!image_style_candidate(&old, &new));
        if let BlockKind::Image { width, src, .. } = &mut new.kind {
            *width = Some(300.);
            *src = "different".into();
        }
        assert!(!image_style_candidate(&old, &new));
    }

    #[test]
    fn local_assets_are_copied_without_touching_originals() {
        let root = std::env::temp_dir().join(format!("quiet-media-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join("library")).unwrap();
        std::fs::create_dir_all(root.join("upload")).unwrap();
        let original = root.join("library/report.pdf");
        std::fs::write(&original, b"original").unwrap();
        let file = Block::new(BlockKind::File(FileBlock { src_kind: "path".into(), src: "report.pdf".into(), name: "shown.pdf".into(), view: FileView::Card }), RichText::default());
        let first = prepare(&file, &root.join("library"), &root.join("upload")).unwrap();
        let second = prepare(&file, &root.join("library"), &root.join("upload")).unwrap();
        assert_ne!(first.xml, second.xml);
        assert!(first.xml.contains("name=\"shown.pdf\""));
        assert_eq!(std::fs::read(&original).unwrap(), b"original");
        assert_eq!(std::fs::read_dir(root.join("upload/assets")).unwrap().count(), 2);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn original_media_rejects_comments_and_unmodelled_crop() {
        assert!(check_original("<img id=\"a\" src=\"x\" crop=\"0,0,1,1\"/>", "a").is_err());
        assert!(check_original("<img id=\"a\" src=\"x\" name=\"not-modelled.png\"/>", "a").is_err());
        assert!(check_original("<figure id=\"a\"><source token=\"x\" comment-id=\"c\"/></figure>", "a").is_err());
        assert!(check_original("<img id=\"a\" src=\"x\" caption=\"ok\"/>", "a").is_ok());
        assert!(no_comments(&json!({"comment_ids":["c"]})).is_err());
    }

    #[test]
    fn result_requires_one_typed_resource_id() {
        let response = json!({"data":{"document":{"new_blocks":[{"block_id":"child","block_type":"file","block_token":"token"}]}}});
        assert_eq!(result_resource(&response, false).unwrap(), ("child".into(), Some("token".into())));
        assert!(result_resource(&response, true).is_err());
        let ambiguous = json!({"data":{"document":{"new_blocks":[{"block_id":"a","block_type":"image"},{"block_id":"b","block_type":"image"}]}}});
        assert!(result_resource(&ambiguous, true).is_err());
    }

    #[test]
    fn image_checks_dimensions_caption_alignment_and_token() {
        let expected = image();
        let mut actual = json!({"block_type":27,"image":{"token":"uploaded","width":200,"height":100,"align":2,"caption":{"content":"caption"}}});
        assert!(compare_image(&actual, &expected, Some("uploaded")).is_ok());
        assert!(compare_image(&actual, &expected, Some("other")).is_err());
        actual["image"]["width"] = json!(201);
        assert!(compare_image(&actual, &expected, Some("uploaded")).is_err());
    }

    #[test]
    fn file_checks_name_and_view() {
        let expected = Block::new(BlockKind::File(FileBlock { src_kind: "token".into(), src: "asset".into(), name: "report.pdf".into(), view: FileView::Preview }), RichText::default());
        let view = json!({"view":{"view_type":2}});
        let mut file = json!({"file":{"token":"asset","name":"report.pdf"}});
        assert!(compare_file(&view, &file, &expected, Some("asset")).is_ok());
        file["file"]["name"] = json!("other.pdf");
        assert!(compare_file(&view, &file, &expected, Some("asset")).is_err());
        assert!(compare_file(&json!({"view":{"view_type":1}}), &file, &expected, Some("asset")).is_err());
    }
}
