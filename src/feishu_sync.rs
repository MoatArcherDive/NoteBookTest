//! Manual, conservative synchronization of Feishu documents.
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use anyhow::{anyhow, bail, Context, Result};
mod text;
mod plan;
mod media;
mod xml;
mod table;
mod table_exec;
pub(crate) use text::canonical_color;
use text::{check_xml, comparable_text, elements, remote_text, remote_title};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use crate::feishu_fetch::run;
use crate::model::{Block, BlockKind, Document, RichText};

#[derive(Serialize, Deserialize)]
struct Source {
    url: String,
    document_id: String,
    revision_id: i64,
    original_xml: String,
    baseline: String,
    interrupted: bool,
    #[serde(default)]
    bindings: HashMap<String, String>,
}

fn source_path(id: &str) -> Result<PathBuf> {
    if id.is_empty() || !id.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_') {
        bail!("本地文档 ID 无效");
    }
    Ok(crate::store::Store::root_dir().join("feishu-sources").join(format!("{id}.json")))
}

fn save(id: &str, source: &Source) -> Result<()> {
    let path = source_path(id)?;
    std::fs::create_dir_all(path.parent().unwrap()).context("无法创建飞书来源目录")?;
    let tmp = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
    let mut file = std::fs::File::create(&tmp).context("无法保存飞书来源")?;
    file.write_all(&serde_json::to_vec(source)?).context("无法保存飞书来源")?;
    file.sync_all().context("无法保存飞书来源")?;
    std::fs::rename(&tmp, &path).context("无法保存飞书来源")?;
    Ok(())
}

pub(crate) fn register(id: &str, fetched: &crate::feishu_fetch::Fetched) -> Result<()> {
    save(id, &Source { url: fetched.url.clone(), document_id: fetched.document_id.clone(),
        revision_id: fetched.revision_id, original_xml: fetched.original_xml.clone(),
        baseline: crate::docxml::serialize(&fetched.doc), interrupted: false, bindings: HashMap::new() })
}

pub(crate) fn has_source(id: &str) -> bool {
    source_path(id).is_ok_and(|p| p.is_file())
}

/// Find an active local copy without loading its synchronization baseline.
pub(crate) fn find_local_copy<'a>(root: &Path, url: &str, entries: &'a [crate::store::Entry]) -> Option<&'a str> {
    #[derive(Deserialize)]
    struct SourceUrl { url: String }

    // Keep paths byte-for-byte; in particular, percent-encoded characters are
    // not decoded. Only HTTP(S) links can identify a Feishu document.
    fn parts(url: &str) -> Option<(&str, &str, &str)> {
        let url = url.trim().split(['?', '#']).next()?;
        let (scheme, rest) = url.split_once("://")?;
        if !scheme.eq_ignore_ascii_case("https") && !scheme.eq_ignore_ascii_case("http") { return None; }
        let (host, path) = rest.split_once('/')?;
        let path = path.trim_end_matches('/');
        if host.is_empty() || path.is_empty() { return None; }
        Some((scheme, host, path))
    }

    let (scheme, host, path) = parts(url)?;
    let sources = root.join("feishu-sources");
    let docs = root.join("docs");
    entries.iter().filter(|entry| {
        let id = &entry.id;
        if id.is_empty() || !id.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_') { return false; }
        let Ok(file) = std::fs::File::open(sources.join(format!("{id}.json"))) else { return false };
        let Ok(source) = serde_json::from_reader::<_, SourceUrl>(std::io::BufReader::new(file)) else { return false };
        parts(&source.url).is_some_and(|(source_scheme, source_host, source_path)| {
            scheme.eq_ignore_ascii_case(source_scheme) && host.eq_ignore_ascii_case(source_host) && path == source_path
        }) && docs.join(format!("{id}.xml")).is_file()
    }).max_by(|a, b| a.meta.opened.max(a.meta.modified).cmp(&b.meta.opened.max(b.meta.modified))
        .then_with(|| b.id.cmp(&a.id)))
        .map(|entry| entry.id.as_str())
}


fn fetch_document(url: &str, document_id: &str) -> Result<(i64, String)> {
    let response = run(&["docs", "+fetch", "--doc", url, "--as", "user", "--doc-format", "xml", "--detail", "full", "--scope", "full"], None, None)?;
    let doc = response.pointer("/data/document").ok_or_else(|| anyhow!("飞书响应缺少文档信息"))?;
    if doc.get("document_id").and_then(Value::as_str) != Some(document_id) { bail!("来源链接指向的文档已改变"); }
    let revision = doc.get("revision_id").and_then(Value::as_i64).ok_or_else(|| anyhow!("飞书响应缺少版本号"))?;
    let xml = doc.get("content").and_then(Value::as_str).filter(|s| !s.trim().is_empty())
        .ok_or_else(|| anyhow!("飞书响应缺少文档内容"))?;
    Ok((revision, xml.to_owned()))
}

// Compare a single block, ignoring ephemeral BlockIds but retaining all data
// that the local DocxXML serializer can represent.
fn block_xml(block: &Block) -> String {
    let mut block = block.clone();
    block.children.clear();
    let doc = Document { blocks: vec![block], ..Document::default() };
    crate::docxml::serialize(&doc)
}



fn title_payload(title: &RichText) -> Result<Vec<Value>> {
    if comparable_text(title)? != RichText::plain(&title.text) {
        bail!("飞书文档标题不支持富文本样式");
    }
    Ok(vec![json!({"text_run": {"content": title.text}})])
}

// Keys are namespaced: a locally generated identity must never be mistaken for
// a cloud block ID. Bindings are durable across subsequent local edits.
fn block_key(block: &Block) -> Result<String> {
    if let Some(id) = &block.source_id { return Ok(format!("s:{id}")); }
    if let Some(id) = &block.sync_id { return Ok(format!("l:{id}")); }
    bail!("块缺少持久身份，无法安全同步")
}

fn cloud_id(key: &str, source: &Source) -> Result<String> {
    let id = if let Some(id) = key.strip_prefix("s:") { source.bindings.get(key).map(String::as_str).unwrap_or(id) }
        else if let Some(id) = key.strip_prefix("l:") {
            source.bindings.get(id).map(String::as_str).ok_or_else(|| anyhow!("本地块尚未绑定云端身份"))?
        } else { bail!("块身份无效") };
    if id.is_empty() || !id.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-') {
        bail!("云端块 ID 无效");
    }
    Ok(id.to_owned())
}

fn child_keys(blocks: &[Block]) -> Result<Vec<String>> {
    blocks.iter().map(block_key).collect()
}

fn old_parent<'a>(blocks: &'a [Block], id: crate::model::BlockId, parent: Option<&'a Block>) -> Option<Option<&'a Block>> {
    for block in blocks {
        if block.id == id { return Some(parent); }
        if let Some(found) = old_parent(&block.children, id, Some(block)) { return Some(found); }
    }
    None
}

fn find_block<'a>(blocks: &'a [Block], key: &str) -> Option<&'a Block> {
    for block in blocks {
        if block_key(block).ok().as_deref() == Some(key) { return Some(block); }
        if let Some(found) = find_block(&block.children, key) { return Some(found); }
    }
    None
}

fn read_block(document: &str, block: &str) -> Result<Value> {
    let path = format!("/open-apis/docx/v1/documents/{document}/blocks/{block}");
    let result = run(&["api", "GET", &path, "--as", "user"], None, None);
    std::thread::sleep(std::time::Duration::from_millis(210));
    result
}

fn remote_children(response: &Value) -> Result<Vec<String>> {
    let block = response.pointer("/data/block").ok_or_else(|| anyhow!("飞书响应缺少块信息"))?;
    match block.get("children") {
        None => Ok(Vec::new()),
        Some(value) => value.as_array().ok_or_else(|| anyhow!("云端子块列表无效"))?.iter().map(|v| {
            v.as_str().filter(|s| !s.is_empty()).map(str::to_owned).ok_or_else(|| anyhow!("云端子块 ID 无效"))
        }).collect(),
    }
}

fn native_new_block(block: &Block) -> Result<Value> {
    if block.align != crate::model::Align::default() || block.bg.is_some() || block.folded {
        bail!("新增块含不支持的对齐、背景或折叠属性");
    }
    let (kind, field) = match &block.kind {
        BlockKind::Text => (2, "text".to_owned()),
        BlockKind::Heading { level, seq_auto: false } if (1..=9).contains(level) => (u64::from(*level) + 2, format!("heading{level}")),
        BlockKind::Bullet => (12, "bullet".to_owned()),
        BlockKind::Ordered { seq: None } => (13, "ordered".to_owned()),
        BlockKind::Todo { .. } => (17, "todo".to_owned()),
        _ => bail!("新增块的类型或属性尚不能安全映射到飞书原生创建接口"),
    };
    let mut value = json!({"block_type": kind});
    value[field.as_str()] = json!({"elements": elements(&block.text)?});
    if let BlockKind::Todo { done } = &block.kind {
        value["todo"]["style"] = json!({"done": done});
    }
    Ok(value)
}

fn remote_checked(response: &Value, kind: &BlockKind) -> Result<RichText> {
    if let BlockKind::Todo { done } = kind {
        let actual = match response.pointer("/data/block/todo/style/done") {
            None => false,
            Some(value) => value.as_bool().ok_or_else(|| anyhow!("云端待办状态无效"))?,
        };
        if actual != *done { bail!("云端待办状态与本地不符"); }
    }
    remote_text(response, kind)
}

fn check_update_result(value: &Value) -> Result<()> {
    if value.pointer("/data/result").and_then(Value::as_str) != Some("success") {
        bail!("飞书块操作未报告完全成功");
    }
    if let Some(warnings) = value.pointer("/data/warnings")
        && !warnings.as_array().is_some_and(Vec::is_empty) { bail!("飞书块操作返回警告，结果需要人工核对"); }
    Ok(())
}

enum Operation<'a> {
    Text { key: String, old: RichText, new: RichText, old_kind: Option<BlockKind>, kind: Option<BlockKind>,
        payload: Option<Vec<Value>>, done: Option<bool> },
    Insert { parent: String, after: Option<String>, key: String, kind: BlockKind, new: RichText, payload: Value },
    MediaInsert { parent: String, after: Option<String>, key: String, prepared: media::Prepared, receipt: Option<media::Receipt> },
    MediaUpdate { key: String, old: &'a Block, new: &'a Block, prepared: media::Prepared,
        style_token: Option<String>, receipt: Option<media::Receipt> },
    TableInsert { parent: String, after: Option<String>, key: String, prepared: table_exec::Prepared },
    TableUpdate { prepared: table_exec::Prepared },
    Move { from: String, parent: String, after: Option<String>, key: String },
    Delete { parent: String, key: String },
}
struct UploadDir(PathBuf);

impl Drop for UploadDir {
    fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.0); }
}

fn upload_dir(dir: &mut Option<UploadDir>) -> Result<&Path> {
    if dir.is_none() {
        let path = std::env::temp_dir().join(format!("quiet-md-sync-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&path).context("无法创建素材临时目录")?;
        *dir = Some(UploadDir(path));
    }
    Ok(&dir.as_ref().unwrap().0)
}


fn insertion_index(children: &[String], after: Option<&str>) -> Result<usize> {
    match after {
        None => Ok(0),
        Some(key) => children.iter().position(|x| x == key).map(|i| i + 1)
            .ok_or_else(|| anyhow!("目标前置块不在同一父块内")),
    }
}

// Translate a first-position move into documented move-after operations.
// The destination must contain an anchor; neither 0 nor the parent ID is
// documented as a first-child anchor for arbitrary nested containers.
fn move_steps(orders: &HashMap<String, Vec<String>>, from: &str, parent: &str, after: Option<&str>, key: &str) -> Result<Vec<(String, String, String)>> {
    let source = orders.get(from).ok_or_else(|| anyhow!("缺少移动源父块状态"))?;
    let pos = source.iter().position(|x| x == key).ok_or_else(|| anyhow!("移动块不在源父块中"))?;
    let target = orders.get(parent).ok_or_else(|| anyhow!("缺少移动目标父块状态"))?;
    if let Some(anchor) = after {
        if anchor == key || !target.iter().any(|x| x == anchor) { bail!("移动锚点不在目标父块中"); }
        return Ok(vec![(from.to_owned(), anchor.to_owned(), key.to_owned())]);
    }
    if from == parent {
        let mut anchor = key.to_owned();
        return Ok(source[..pos].iter().map(|sibling| {
            let result = (parent.to_owned(), anchor.clone(), sibling.clone());
            anchor = sibling.clone();
            result
        }).collect());
    }
    let first = target.first().ok_or_else(|| anyhow!("目标父块为空，飞书未公开可用的首个子块移动锚点"))?;
    Ok(vec![(from.to_owned(), first.clone(), key.to_owned()),
        (parent.to_owned(), key.to_owned(), first.clone())])
}

fn apply_move(orders: &mut HashMap<String, Vec<String>>, from: &str, parent: &str, anchor: &str, key: &str) -> Result<()> {
    let old = orders.get(from).ok_or_else(|| anyhow!("缺少移动源父块状态"))?;
    let pos = old.iter().position(|x| x == key).ok_or_else(|| anyhow!("移动块不在源父块中"))?;
    if anchor == key || !orders.get(parent).is_some_and(|items| items.iter().any(|x| x == anchor)) {
        bail!("移动锚点不在目标父块中");
    }
    orders.get_mut(from).unwrap().remove(pos);
    let target = orders.get_mut(parent).unwrap();
    let index = insertion_index(target, Some(anchor))?;
    target.insert(index, key.to_owned());
    Ok(())
}
fn apply_empty_move(orders: &mut HashMap<String, Vec<String>>, from: &str, parent: &str, key: &str) -> Result<()> {
    if from == parent || !orders.get(parent).is_some_and(Vec::is_empty) { bail!("目标父块不是空容器"); }
    let source = orders.get_mut(from).ok_or_else(|| anyhow!("缺少移动源父块状态"))?;
    let pos = source.iter().position(|item| item == key).ok_or_else(|| anyhow!("移动块不在源父块中"))?;
    source.remove(pos);
    orders.get_mut(parent).unwrap().push(key.to_owned());
    Ok(())
}


pub(crate) fn sync(id: &str, doc: &Document) -> Result<()> {
    use plan::Edit;
    let mut source: Source = serde_json::from_slice(&std::fs::read(source_path(id)?).context("无法读取飞书来源")?).context("飞书来源记录损坏")?;
    if source.interrupted { bail!("此前同步已中断或结果不确定，请重新下载文档并核对云端内容"); }
    let baseline = crate::docxml::parse(&source.baseline).context("无法读取同步基线")?;
    let edits = plan::plan(&baseline, doc)?;
    fn contains_table(block: &Block) -> bool {
        matches!(block.kind, BlockKind::Table(_)) || block.children.iter().any(contains_table)
    }
    if edits.iter().any(|edit| match edit {
        Edit::Table { .. } => true,
        Edit::Delete { block } => contains_table(block),
        _ => false,
    }) && fetch_document(&source.url, &source.document_id)?.0 != source.revision_id {
        bail!("云端版本已变化，请重新下载并核对；未写入");
    }
    let root = format!("s:{}", source.document_id);
    cloud_id(&root, &source)?;
    let mut operations = Vec::new();
    let mut parents = HashSet::new();
    let mut new_parents = HashSet::new();
    let mut deleted = Vec::new();
    let mut removed_bindings = Vec::new();
    let mut deleted_media = Vec::new();
    let mut upload = None;
    for edit in edits {
        match edit {
            Edit::Title { old, new } => {
                title_payload(old)?;
                operations.push(Operation::Text { key: root.clone(), old: comparable_text(old)?, new: comparable_text(new)?,
                    old_kind: None, kind: None, payload: Some(title_payload(new)?), done: None });
            }
            Edit::Update { old, new } => {
                let key = block_key(old)?;
                if matches!(old.kind, BlockKind::Image { .. } | BlockKind::File(_))
                    && matches!(new.kind, BlockKind::Image { .. } | BlockKind::File(_)) {
                    let prepared = if media::image_style_candidate(old, new) {
                        media::prepare_image_style(new)
                    } else {
                        media::prepare(new, &crate::store::Store::root_dir(), upload_dir(&mut upload)?)?
                    };
                    operations.push(Operation::MediaUpdate { key, old, new, prepared, style_token: None, receipt: None });
                    continue;
                }
                let mut unchanged = old.clone();
                unchanged.text = new.text.clone();
                unchanged.source_id = new.source_id.clone();
                unchanged.sync_id = new.sync_id.clone();
                let done = match (&old.kind, &new.kind) {
                    (BlockKind::Todo { done: before }, BlockKind::Todo { done: after }) if before != after => Some(*after),
                    _ => None,
                };
                if done.is_some() { unchanged.kind = new.kind.clone(); }
                if block_xml(&unchanged) != block_xml(new) {
                    bail!("不支持原位修改块类型、布局、附件或其他复杂属性");
                }
                let cloud = cloud_id(&key, &source)?;
                check_xml(&source.original_xml, &cloud)?;
                operations.push(Operation::Text { key, old: comparable_text(&old.text)?, new: comparable_text(&new.text)?,
                    old_kind: Some(old.kind.clone()), kind: Some(new.kind.clone()),
                    payload: if old.text != new.text { Some(elements(&new.text)?) } else { None }, done });
            }
            Edit::Insert { parent, after, block } => {
                let parent = parent.map(block_key).transpose()?.unwrap_or_else(|| root.clone());
                if parent != root && find_block(&baseline.blocks, &parent).is_none() && !new_parents.contains(&parent) {
                    bail!("新增块的父块尚未在同步计划中创建");
                }
                let key = block_key(block)?;
                let local = key.strip_prefix("l:").ok_or_else(|| anyhow!("新增块缺少本地同步身份"))?;
                if source.bindings.contains_key(local) { bail!("新增块的本地身份已绑定云端块"); }
                new_parents.insert(key.clone());
                parents.insert(parent.clone());
                let after = after.map(block_key).transpose()?;
                if matches!(block.kind, BlockKind::Image { .. } | BlockKind::File(_)) {
                    let prepared = media::prepare(block, &crate::store::Store::root_dir(), upload_dir(&mut upload)?)?;
                    operations.push(Operation::MediaInsert { parent, after, key, prepared, receipt: None });
                } else if matches!(block.kind, BlockKind::Table(_)) {
                    let prepared = table_exec::prepare_new(block, &source, &crate::store::Store::root_dir(), upload_dir(&mut upload)?)?;
                    operations.push(Operation::TableInsert { parent, after, key, prepared });
                } else {
                    operations.push(Operation::Insert { parent, after, key,
                        kind: block.kind.clone(), new: comparable_text(&block.text)?, payload: native_new_block(block)? });
                }
            }
            Edit::Move { block, parent, after } => {
                let key = block_key(block)?;
                let old_parent = old_parent(&baseline.blocks, block.id, None).ok_or_else(|| anyhow!("找不到移动块的基线父块"))?
                    .map(block_key).transpose()?.unwrap_or_else(|| root.clone());
                let parent = parent.map(block_key).transpose()?.unwrap_or_else(|| root.clone());
                let after = after.map(block_key).transpose()?;
                cloud_id(&key, &source)?;
                parents.insert(parent.clone());
                parents.insert(old_parent.clone());
                operations.push(Operation::Move { from: old_parent, parent, after, key });
            }
            Edit::Delete { block } => {
                let key = block_key(block)?;
                let parent = old_parent(&baseline.blocks, block.id, None).ok_or_else(|| anyhow!("找不到删除块的基线父块"))?
                    .map(block_key).transpose()?.unwrap_or_else(|| root.clone());
                let mut pending = vec![block];
                while let Some(node) = pending.pop() {
                    if matches!(node.kind, BlockKind::Table(_)) {
                        table_exec::bind_existing(node, &mut source)?;
                        removed_bindings.extend(table_exec::preflight_delete(node, &source)?);
                        continue;
                    }
                    let node_key = block_key(node)?;
                    let cloud = cloud_id(&node_key, &source)?;
                    if matches!(node.kind, BlockKind::Image { .. } | BlockKind::File(_)) {
                        deleted_media.push((node_key.clone(), node));
                    } else {
                        check_xml(&source.original_xml, &cloud)?;
                        comparable_text(&node.text)?;
                        if !matches!(node.kind, BlockKind::Text | BlockKind::Heading { .. } | BlockKind::Bullet | BlockKind::Ordered { .. } | BlockKind::Todo { .. }) {
                            bail!("删除的块或子块含无法安全核对评论及内容的复杂类型");
                        }
                        deleted.push(node);
                    }
                    if let Some(local) = &node.sync_id { removed_bindings.push(local.clone()); }
                    if node.source_id.is_some() { removed_bindings.push(node_key); }
                    pending.extend(&node.children);
                }
                parents.insert(parent.clone());
                operations.push(Operation::Delete { parent, key });
            }
            Edit::Table { old, new } => {
                table_exec::bind_existing(old, &mut source)?;
                let prepared = table_exec::prepare_with_assets(old, new, &source, &crate::store::Store::root_dir(), upload_dir(&mut upload)?)?;
                operations.push(Operation::TableUpdate { prepared });
            }
        }
    }
    if fetch_document(&source.url, &source.document_id)?.0 != source.revision_id { bail!("云端版本已变化，请重新下载并核对；未写入"); }
    let mut initial = HashMap::new();
    for parent in &parents {
        if new_parents.contains(parent) {
            initial.insert(parent.clone(), Vec::new());
            continue;
        }
        let children = if parent == &root { &baseline.blocks } else {
            &find_block(&baseline.blocks, parent).ok_or_else(|| anyhow!("父块不在下载基线中"))?.children
        };
        let keys = child_keys(children)?;
        let expected = keys.iter().map(|key| cloud_id(key, &source)).collect::<Result<Vec<_>>>()?;
        let actual = remote_children(&read_block(&source.document_id, &cloud_id(parent, &source)?)?)?;
        if actual != expected { bail!("云端父块子项与下载基线不符，未写入"); }
        initial.insert(parent.clone(), keys);
    }
    for operation in &mut operations {
        match operation {
            Operation::Text { key, old, old_kind, .. } => {
                let response = read_block(&source.document_id, &cloud_id(key, &source)?)?;
                let actual = match old_kind { Some(kind) => remote_checked(&response, kind)?, None => remote_title(&response)? };
                if actual != *old { bail!("云端块内容与下载基线不符，未写入"); }
            }
            Operation::MediaInsert { prepared, .. } => media::check_prepared(&source, prepared)?,
            Operation::MediaUpdate { key, old, new, prepared, style_token, .. } => {
                if media::image_style_candidate(old, new) {
                    *style_token = Some(media::preflight_image_style(&source, key, old, new)?);
                } else {
                    media::check_prepared(&source, prepared)?;
                    media::preflight(&source, key, old)?;
                }
            }
            Operation::TableInsert { prepared, .. } | Operation::TableUpdate { prepared } => table_exec::preflight(prepared, &source)?,
            _ => {}
        }
    }
    for block in deleted {
        let response = read_block(&source.document_id, &cloud_id(&block_key(block)?, &source)?)?;
        if remote_checked(&response, &block.kind)? != comparable_text(&block.text)? {
            bail!("待删除块的云端内容与下载基线不符，未写入");
        }
        let expected = child_keys(&block.children)?.iter().map(|key| cloud_id(key, &source)).collect::<Result<Vec<_>>>()?;
        if remote_children(&response)? != expected { bail!("待删除块的云端子项与基线不符，未写入"); }
    }
    for (key, block) in deleted_media { media::preflight(&source, &key, block)?; }
    // Simulate every structural operation before the first write. The same
    // operations are then applied to a separate copy during execution.
    let mut final_order = initial.clone();
    for operation in &operations {
        match operation {
            Operation::Insert { parent, after, key, .. } | Operation::MediaInsert { parent, after, key, .. }
                | Operation::TableInsert { parent, after, key, .. } => {
                let list = final_order.get_mut(parent).ok_or_else(|| anyhow!("缺少父块基线"))?;
                let index = insertion_index(list, after.as_deref())?;
                list.insert(index, key.clone());
            }
            Operation::Move { from, parent, after, key } => {
                if from != parent && after.is_none() && final_order.get(parent).is_some_and(Vec::is_empty) {
                    apply_empty_move(&mut final_order, from, parent, key)?;
                } else {
                    for (source_parent, anchor, target) in move_steps(&final_order, from, parent, after.as_deref(), key)? {
                        apply_move(&mut final_order, &source_parent, parent, &anchor, &target)?;
                    }
                }
            }
            Operation::Delete { parent, key } => {
                let list = final_order.get_mut(parent).ok_or_else(|| anyhow!("缺少父块基线"))?;
                let index = list.iter().position(|x| x == key).ok_or_else(|| anyhow!("删除块不在父块中"))?;
                list.remove(index);
            }
            Operation::Text { .. } | Operation::MediaUpdate { .. } | Operation::TableUpdate { .. } => {}
        }
    }
    if operations.is_empty() { return Ok(()); }
    if fetch_document(&source.url, &source.document_id)?.0 != source.revision_id { bail!("云端版本已变化，请重新下载并核对；未写入"); }
    source.interrupted = true;
    save(id, &source).context("无法记录同步状态，未写入云端")?;
    let mut current = initial;
    let result: Result<()> = (|| {
    for operation in &mut operations {
        match operation {
            Operation::Text { key, payload, done, .. } => {
                let cloud = cloud_id(key, &source)?;
                let path = format!("/open-apis/docx/v1/documents/{}/blocks/{cloud}", source.document_id);
                if let Some(payload) = payload {
                    let body = json!({"update_text_elements": {"elements": payload}}).to_string();
                    run(&["api", "PATCH", &path, "--as", "user", "--data", "-"], Some(&body), None)?;
                    std::thread::sleep(std::time::Duration::from_millis(350));
                }
                if let Some(done) = done {
                    let body = json!({"update_text_style": {"style": {"done": done}, "fields": [2]}}).to_string();
                    run(&["api", "PATCH", &path, "--as", "user", "--data", "-"], Some(&body), None)?;
                }
            }
            Operation::Insert { parent, after, key, kind, new, payload } => {
                let parent_id = cloud_id(parent, &source)?;
                let list = current.get_mut(parent).ok_or_else(|| anyhow!("缺少父块状态"))?;
                let index = insertion_index(list, after.as_deref())?;
                let path = format!("/open-apis/docx/v1/documents/{}/blocks/{parent_id}/children", source.document_id);
                let body = json!({"index": index, "children": [payload]}).to_string();
                let response = run(&["api", "POST", &path, "--as", "user", "--data", "-"], Some(&body), None)?;
                let children = response.pointer("/data/children").and_then(Value::as_array).ok_or_else(|| anyhow!("创建块后未返回真实块 ID"))?;
                if children.len() != 1 { bail!("创建块后返回的块数量不符"); }
                let new_id = children[0].get("block_id").and_then(Value::as_str).filter(|s| !s.is_empty())
                    .ok_or_else(|| anyhow!("创建块后未返回真实块 ID"))?.to_owned();
                let local = key.strip_prefix("l:").ok_or_else(|| anyhow!("新增块缺少本地身份"))?;
                if source.bindings.values().any(|v| v == &new_id) { bail!("创建块返回重复的云端 ID"); }
                source.bindings.insert(local.to_owned(), new_id);
                save(id, &source).context("云端块已创建，但无法保存绑定；请重新下载并核对")?;
                let created = read_block(&source.document_id, &cloud_id(key, &source)?)?;
                if remote_checked(&created, kind)? != *new || !remote_children(&created)?.is_empty() {
                    bail!("创建块后的云端内容或子项不符，请重新下载并核对");
                }
                list.insert(index, key.clone());
            }
            Operation::MediaInsert { parent, after, key, prepared, receipt } => {
                let parent_id = cloud_id(parent, &source)?;
                let list = current.get_mut(parent).ok_or_else(|| anyhow!("缺少父块状态"))?;
                let index = insertion_index(list, after.as_deref())?;
                *receipt = Some(media::insert(id, &mut source, key, &parent_id, index, prepared)?);
                list.insert(index, key.clone());
            }
            Operation::MediaUpdate { key, prepared, style_token, receipt, .. } => {
                *receipt = Some(if let Some(token) = style_token {
                    media::update_image_style(&source, key, prepared, token)?
                } else {
                    media::replace(id, &mut source, key, prepared)?
                });
            }
            Operation::TableInsert { parent, after, key, prepared } => {
                let parent_id = cloud_id(parent, &source)?;
                let list = current.get_mut(parent).ok_or_else(|| anyhow!("缺少父块状态"))?;
                let index = insertion_index(list, after.as_deref())?;
                table_exec::execute_new(prepared, id, &mut source, &parent_id, index)?;
                list.insert(index, key.clone());
            }
            Operation::TableUpdate { prepared } => table_exec::execute(prepared, id, &mut source)?,
            Operation::Move { from, parent, after, key } => {
                if from != parent && after.is_none() && current.get(parent).is_some_and(Vec::is_empty) {
                    // A temporary empty child provides the documented move-after
                    // anchor; delete it only after confirming it stayed empty.
                    let parent_id = cloud_id(parent, &source)?;
                    let target_id = cloud_id(key, &source)?;
                    let path = format!("/open-apis/docx/v1/documents/{}/blocks/{parent_id}/children", source.document_id);
                    let body = json!({"index": 0, "children": [{"block_type": 2, "text": {"elements": [{"text_run": {"content": ""}}]}}]}).to_string();
                    let response = run(&["api", "POST", &path, "--as", "user", "--data", "-"], Some(&body), None)?;
                    let children = response.pointer("/data/children").and_then(Value::as_array).ok_or_else(|| anyhow!("临时锚点没有返回块 ID"))?;
                    if children.len() != 1 { bail!("临时锚点返回的块数量不符"); }
                    let temporary = children[0].get("block_id").and_then(Value::as_str).filter(|s| !s.is_empty())
                        .ok_or_else(|| anyhow!("临时锚点没有返回块 ID"))?;
                    if temporary == target_id || temporary == parent_id { bail!("临时锚点返回了重复的块 ID"); }
                    let anchor = format!("s:{temporary}");
                    cloud_id(&anchor, &source)?;
                    if remote_children(&read_block(&source.document_id, &parent_id)?)? != [temporary] {
                        bail!("临时锚点的云端位置不符，请重新下载并核对");
                    }
                    std::thread::sleep(std::time::Duration::from_millis(350));
                    let response = run(&["docs", "+update", "--doc", &source.url, "--as", "user", "--command", "block_move_after",
                        "--block-id", temporary, "--src-block-ids", &target_id], None, None)?;
                    check_update_result(&response)?;
                    if remote_children(&read_block(&source.document_id, &parent_id)?)? != [temporary, target_id.as_str()] {
                        bail!("移动后云端目标父块子项不符，请重新下载并核对");
                    }
                    let anchor_block = read_block(&source.document_id, temporary)?;
                    if !remote_checked(&anchor_block, &BlockKind::Text)?.is_empty() || !remote_children(&anchor_block)?.is_empty() {
                        bail!("临时锚点已被修改，不能删除，请重新下载并核对");
                    }
                    std::thread::sleep(std::time::Duration::from_millis(350));
                    let response = run(&["docs", "+update", "--doc", &source.url, "--as", "user", "--command", "block_delete",
                        "--block-id", temporary], None, None)?;
                    check_update_result(&response)?;
                    apply_empty_move(&mut current, from, parent, key)?;
                    for checked in [from, parent] {
                        let expected = current[checked].iter().map(|key| cloud_id(key, &source)).collect::<Result<Vec<_>>>()?;
                        if remote_children(&read_block(&source.document_id, &cloud_id(checked, &source)?)?)? != expected {
                            bail!("移动后云端父块子项不符，请重新下载并核对");
                        }
                    }
                } else {
                for (source_parent, anchor, target) in move_steps(&current, from, parent, after.as_deref(), key)? {
                    let anchor_id = cloud_id(&anchor, &source)?;
                    let target_id = cloud_id(&target, &source)?;
                    let response = run(&["docs", "+update", "--doc", &source.url, "--as", "user", "--command", "block_move_after",
                        "--block-id", &anchor_id, "--src-block-ids", &target_id], None, None)?;
                    check_update_result(&response)?;
                    apply_move(&mut current, &source_parent, parent, &anchor, &target)?;
                    // In particular, confirm the first cross-parent move before
                    // using the moved block as the anchor for another request.
                    for checked in [&source_parent, parent] {
                        let expected = current[checked].iter().map(|key| cloud_id(key, &source)).collect::<Result<Vec<_>>>()?;
                        if remote_children(&read_block(&source.document_id, &cloud_id(checked, &source)?)?)? != expected {
                            bail!("移动后云端父块子项不符，请重新下载并核对");
                        }
                        if source_parent.as_str() == parent.as_str() { break; }
                    }
                    std::thread::sleep(std::time::Duration::from_millis(350));
                }
                }
            }
            Operation::Delete { parent, key } => {
                let target = cloud_id(key, &source)?;
                let response = run(&["docs", "+update", "--doc", &source.url, "--as", "user", "--command", "block_delete",
                    "--block-id", &target], None, None)?;
                check_update_result(&response)?;
                let list = current.get_mut(parent).ok_or_else(|| anyhow!("缺少父块状态"))?;
                let pos = list.iter().position(|x| x == key).ok_or_else(|| anyhow!("删除块不在父块中"))?;
                list.remove(pos);
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(350));
    }
    for (parent, keys) in &final_order {
        if parent != &root && find_block(&doc.blocks, parent).is_none() { continue; }
        let expected = keys.iter().map(|key| cloud_id(key, &source)).collect::<Result<Vec<_>>>()?;
        if remote_children(&read_block(&source.document_id, &cloud_id(parent, &source)?)?)? != expected {
            bail!("写入后云端块顺序与发送快照不符，请重新下载并核对");
        }
    }
    for operation in &operations {
        match operation {
            Operation::MediaInsert { key, prepared, receipt, .. } | Operation::MediaUpdate { key, prepared, receipt, .. } => {
                media::verify(&source, key, prepared, receipt.as_ref().ok_or_else(|| anyhow!("缺少素材同步回执"))?)?;
            }
            Operation::TableInsert { prepared, .. } | Operation::TableUpdate { prepared } => table_exec::verify(prepared, &source)?,
            _ => {}
        }
    }
    for operation in &operations {
        let (key, expected, kind) = match operation {
            Operation::Text { key, new, kind, .. } => (key, new, kind.as_ref()),
            Operation::Insert { key, new, kind, .. } => (key, new, Some(kind)),
            _ => continue,
        };
        let response = read_block(&source.document_id, &cloud_id(key, &source)?)?;
        let actual = match kind { Some(kind) => remote_checked(&response, kind)?, None => remote_title(&response)? };
        if actual != *expected { bail!("写入后云端块内容与发送快照不符，请重新下载并核对"); }
    }
    Ok(())
    })();
    result.context("同步可能已部分写入云端，状态已标记为中断；请重新下载并核对")?;
    let (revision, original_xml) = fetch_document(&source.url, &source.document_id).context("写入已完成，但无法取得新版本；请重新下载并核对云端内容")?;
    source.revision_id = revision;
    source.original_xml = original_xml;
    for operation in &operations {
        if let Operation::TableInsert { prepared, .. } | Operation::TableUpdate { prepared } = operation {
            removed_bindings.extend(prepared.removed_bindings());
        }
    }
    for local in removed_bindings { source.bindings.remove(&local); }
    source.baseline = crate::docxml::serialize(doc);
    source.interrupted = false;
    save(id, &source).context("写入已完成，但无法保存新基线；请重新下载并核对云端内容")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct LookupLibrary(PathBuf);

    impl LookupLibrary {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!("quiet-md-source-lookup-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(root.join("docs")).unwrap();
            std::fs::create_dir_all(root.join("feishu-sources")).unwrap();
            Self(root)
        }

        fn document(&self, id: &str) {
            std::fs::write(self.0.join("docs").join(format!("{id}.xml")), b"local edits").unwrap();
        }

        fn source(&self, id: &str, url: &str) -> Vec<u8> {
            // The pre-bindings source format is still accepted.
            let source = serde_json::to_vec(&json!({
                "url": url, "document_id": "cloud-id", "revision_id": 3,
                "original_xml": "<p>original \"content\"</p>", "baseline": "<p>baseline</p>",
                "interrupted": false
            })).unwrap();
            std::fs::write(self.0.join("feishu-sources").join(format!("{id}.json")), &source).unwrap();
            source
        }
    }

    impl Drop for LookupLibrary {
        fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.0); }
    }

    fn lookup_entry(id: &str, opened: u64, modified: u64) -> crate::store::Entry {
        crate::store::Entry {
            id: id.into(), meta: crate::store::Meta { opened, modified, ..Default::default() },
            title: String::new(), icon: None, body: String::new(), reminders: vec![], links: vec![],
        }
    }

    #[test]
    fn local_copy_matches_share_links_without_changing_source_or_document() {
        let lib = LookupLibrary::new();
        let entries = vec![lookup_entry("one", 1, 2)];
        lib.document("one");
        assert_eq!(find_local_copy(&lib.0, "https://tenant.feishu.cn/docx/AbC", &entries), None);
        let source = lib.source("one", "https://tenant.feishu.cn/docx/AbC/?from=first#section");

        assert_eq!(find_local_copy(&lib.0, " https://TENANT.feishu.cn/docx/AbC///?from=share#part ", &entries), Some("one"));
        assert_eq!(find_local_copy(&lib.0, "https://other.feishu.cn/docx/AbC", &entries), None);
        assert_eq!(find_local_copy(&lib.0, "https://tenant.feishu.cn/docx/abc", &entries), None);
        assert_eq!(find_local_copy(&lib.0, "https://tenant.feishu.cn/wiki/AbC", &entries), None);
        assert_eq!(find_local_copy(&lib.0, "https://tenant.feishu.cn/docx/%41bC", &entries), None);
        assert_eq!(find_local_copy(&lib.0, "http://tenant.feishu.cn/docx/AbC", &entries), None);
        assert_eq!(find_local_copy(&lib.0, "not a URL", &entries), None);
        assert_eq!(std::fs::read(lib.0.join("feishu-sources/one.json")).unwrap(), source);
        assert_eq!(std::fs::read(lib.0.join("docs/one.xml")).unwrap().as_slice(), b"local edits");
    }

    #[test]
    fn local_copy_prefers_latest_open_or_modification_then_smallest_id() {
        let lib = LookupLibrary::new();
        let url = "https://tenant.feishu.cn/docx/shared";
        for id in ["modified", "opened", "tie-a", "tie-b"] {
            lib.document(id);
            lib.source(id, url);
        }
        let mut entries = vec![
            lookup_entry("modified", 1, 300),
            lookup_entry("opened", 400, 2),
            lookup_entry("tie-b", 2, 400),
            lookup_entry("tie-a", 400, 1),
        ];
        assert_eq!(find_local_copy(&lib.0, url, &entries), Some("opened"));
        entries.retain(|entry| entry.id != "opened");
        assert_eq!(find_local_copy(&lib.0, url, &entries), Some("tie-a"));
        entries.reverse();
        assert_eq!(find_local_copy(&lib.0, url, &entries), Some("tie-a"));
        entries.iter_mut().find(|entry| entry.id == "modified").unwrap().meta.modified = 500;
        assert_eq!(find_local_copy(&lib.0, url, &entries), Some("modified"));
    }

    #[test]
    fn local_copy_ignores_missing_deleted_and_trashed_documents() {
        let lib = LookupLibrary::new();
        let url = "https://tenant.feishu.cn/docx/shared";
        for id in ["live", "deleted", "trashed", "not-in-entries"] {
            lib.document(id);
            lib.source(id, url);
        }
        std::fs::remove_file(lib.0.join("docs/deleted.xml")).unwrap();
        std::fs::create_dir(lib.0.join("trash")).unwrap();
        std::fs::rename(lib.0.join("docs/trashed.xml"), lib.0.join("trash/trashed.xml")).unwrap();
        let entries = vec![
            lookup_entry("deleted", 100, 100),
            lookup_entry("trashed", 200, 200),
            lookup_entry("live", 1, 1),
            lookup_entry("no-source", 300, 300),
        ];
        lib.document("no-source");
        assert_eq!(find_local_copy(&lib.0, url, &entries), Some("live"));
        std::fs::remove_file(lib.0.join("docs/live.xml")).unwrap();
        assert_eq!(find_local_copy(&lib.0, url, &entries), None);
        assert_eq!(find_local_copy(&lib.0, url, &[]), None);
    }

    #[test]
    fn cross_parent_moves_use_real_anchors_and_preserve_order() {
        let original = HashMap::from([
            ("a".to_owned(), vec!["x".to_owned(), "y".to_owned()]),
            ("b".to_owned(), vec!["p".to_owned(), "q".to_owned()]),
            ("empty".to_owned(), Vec::new()),
        ]);
        let mut after = original.clone();
        let steps = move_steps(&after, "a", "b", Some("p"), "x").unwrap();
        for (from, anchor, target) in steps { apply_move(&mut after, &from, "b", &anchor, &target).unwrap(); }
        assert_eq!(after["a"], vec!["y"]);
        assert_eq!(after["b"], vec!["p", "x", "q"]);
        let mut first = original.clone();
        let steps = move_steps(&first, "a", "b", None, "x").unwrap();
        assert_eq!(steps.len(), 2);
        for (from, anchor, target) in steps { apply_move(&mut first, &from, "b", &anchor, &target).unwrap(); }
        assert_eq!(first["a"], vec!["y"]);
        assert_eq!(first["b"], vec!["x", "p", "q"]);
        let mut empty = original.clone();
        apply_empty_move(&mut empty, "a", "empty", "x").unwrap();
        assert_eq!(empty["a"], vec!["y"]);
        assert_eq!(empty["empty"], vec!["x"]);
        assert!(move_steps(&original, "a", "b", Some("y"), "x").is_err());
    }

    #[test]
    fn persisted_binding_resolves_local_identity_without_exposing_it_as_cloud_id() {
        let mut source: Source = serde_json::from_value(json!({
            "url":"u", "document_id":"doc", "revision_id":1,
            "original_xml":"<p/>", "baseline":"<p/>", "interrupted":false
        })).unwrap();
        let mut block = Block::text("new");
        block.sync_id = Some("local-id".into());
        let key = block_key(&block).unwrap();
        assert!(cloud_id(&key, &source).is_err());
        source.bindings.insert("local-id".into(), "cloud-id".into());
        assert_eq!(cloud_id(&key, &source).unwrap(), "cloud-id");
        block.text = RichText::plain("edited later");
        assert_eq!(cloud_id(&block_key(&block).unwrap(), &source).unwrap(), "cloud-id");
    }
    #[test]
    fn replaced_source_block_uses_persisted_cloud_identity() {
        let mut source: Source = serde_json::from_value(json!({
            "url":"u", "document_id":"doc", "revision_id":1,
            "original_xml":"<p/>", "baseline":"<p/>", "interrupted":false
        })).unwrap();
        assert_eq!(cloud_id("s:old-id", &source).unwrap(), "old-id");
        source.bindings.insert("s:old-id".into(), "replacement-id".into());
        assert_eq!(cloud_id("s:old-id", &source).unwrap(), "replacement-id");
        assert_eq!(cloud_id("s:other-id", &source).unwrap(), "other-id");
    }


    #[test]
    fn native_creation_retains_text_and_rejects_unsupported_attributes() {
        let mut block = Block::text("hello");
        let payload = native_new_block(&block).unwrap();
        assert_eq!(payload["block_type"], 2);
        assert_eq!(payload["text"]["elements"][0]["text_run"]["content"], "hello");
        block.bg = Some(crate::model::ColorName("red".into()));
        assert!(native_new_block(&block).is_err());
    }

    #[test]
    fn todo_creation_and_verification_preserve_done_state() {
        let mut block = Block::text("task");
        block.kind = BlockKind::Todo { done: true };
        let payload = native_new_block(&block).unwrap();
        assert_eq!(payload["block_type"], 17);
        assert_eq!(payload["todo"]["style"]["done"], true);
        let response = json!({"data":{"block":{"block_type":17,"todo":{
            "style":{"done":true},"elements":[{"text_run":{"content":"task"}}]
        }}}});
        assert_eq!(remote_checked(&response, &block.kind).unwrap(), RichText::plain("task"));
        assert!(remote_checked(&response, &BlockKind::Todo { done: false }).is_err());
    }

    #[test]
    fn child_order_and_update_status_are_checked() {
        let keys = vec!["a".to_owned(), "b".to_owned()];
        assert_eq!(insertion_index(&keys, None).unwrap(), 0);
        assert_eq!(insertion_index(&keys, Some("a")).unwrap(), 1);
        assert!(insertion_index(&keys, Some("missing")).is_err());
        assert_eq!(remote_children(&json!({"data":{"block":{"children":["a","b"]}}})).unwrap(), keys);
        assert!(remote_children(&json!({"data":{"block":{"children":["a", 2]}}})).is_err());
        assert!(check_update_result(&json!({"data":{"result":"success","warnings":[]}})).is_ok());
        assert!(check_update_result(&json!({"data":{"result":"partial_success","warnings":[]}})).is_err());
        assert!(check_update_result(&json!({"data":{"result":"success","warnings":["warning"]}})).is_err());
    }

    #[test]
    fn title_payload_and_page_round_trip() {
        let title = RichText::plain("新标题");
        assert_eq!(title_payload(&title).unwrap(), vec![json!({"text_run": {"content": "新标题"}})]);
        let response = json!({"data": {"block": {"block_type": 1, "page": {
            "elements": [{"text_run": {"content": "新标题"}}]
        }}}});
        assert_eq!(remote_title(&response).unwrap(), title);
        let mut styled = RichText::default();
        styled.push("新标题", crate::model::Marks { bold: true, ..Default::default() });
        assert!(title_payload(&styled).is_err());
    }

    #[test]
    fn old_source_defaults_bindings_and_preserves_new_bindings() {
        let old = json!({"url":"u", "document_id":"d", "revision_id":1,
            "original_xml":"<p/>", "baseline":"<p/>", "interrupted":false});
        let mut source: Source = serde_json::from_value(old).unwrap();
        assert!(source.bindings.is_empty());
        source.bindings.insert("local".into(), "cloud".into());
        let decoded: Source = serde_json::from_value(serde_json::to_value(source).unwrap()).unwrap();
        assert_eq!(decoded.bindings.get("local").map(String::as_str), Some("cloud"));
    }

    #[test]
    fn identities_survive_serialization_but_not_copy() {
        let doc = crate::docxml::parse("<title>T</title><p id=\"b1\">hello</p><table id=\"b2\"><tr><td><p>cell</p></td></tr></table>").unwrap();
        let restored = crate::docxml::parse(&crate::docxml::serialize(&doc)).unwrap();
        assert_eq!(restored.blocks[0].source_id.as_deref(), Some("b1"));
        assert_eq!(restored.blocks[1].source_id.as_deref(), Some("b2"));
        assert_eq!(restored.blocks[0].fresh_ids().source_id, None);
    }

    #[test]
    fn original_comment_and_unknown_markup_are_rejected() {
        assert!(check_xml("<p id=\"b1\"><b>ok</b></p>", "b1").is_ok());
        assert!(check_xml("<p id=\"b1\"><span comment-id=\"c\">text</span></p>", "b1").is_err());
        assert!(check_xml("<p id=\"b1\"><mystery>text</mystery></p>", "b1").is_err());
    }

    #[test]
    fn rejects_block_and_text_comment_anchors() {
        let mut response = json!({"data": {"block": {
            "block_type": 2,
            "comment_ids": ["comment"],
            "text": {"elements": [{"text_run": {"content": "text"}}]}
        }}});
        assert!(remote_text(&response, &BlockKind::Text).is_err());
        response["data"]["block"]["comment_ids"] = json!([]);
        response["data"]["block"]["text"]["elements"][0]["text_run"]["text_element_style"] = json!({"comment_ids": ["comment"]});
        assert!(remote_text(&response, &BlockKind::Text).is_err());
    }

    #[test]
    fn feishu_color_codes_preserve_mixed_text_styles() {
        let expected = crate::docxml::parse(
            "<p><span text-color=\"red\" background-color=\"light-gray\"><b>A</b></span><span text-color=\"blue\" background-color=\"light-yellow\">B</span><u>C</u></p>"
        ).unwrap();
        let response = json!({"data": {"block": {"block_type": 2, "text": {"elements": [
            {"text_run": {"content": "A", "text_element_style": {"text_color": 1, "background_color": 15, "bold": true}}},
            {"text_run": {"content": "B", "text_element_style": {"text_color": 5, "background_color": 3}}},
            {"text_run": {"content": "C", "text_element_style": {"underline": true}}}
        ]}}}});
        assert_eq!(remote_text(&response, &BlockKind::Text).unwrap(), expected.blocks[0].text);
        let payload = elements(&expected.blocks[0].text).unwrap();
        assert_eq!(payload[0]["text_run"]["text_element_style"]["text_color"], 1);
        assert_eq!(payload[0]["text_run"]["text_element_style"]["background_color"], 15);
        assert_eq!(payload[1]["text_run"]["text_element_style"]["text_color"], 5);
        assert_eq!(payload[1]["text_run"]["text_element_style"]["background_color"], 3);
        assert!(payload[2]["text_run"]["text_element_style"].get("text_color").is_none());
        assert!(payload[2]["text_run"]["text_element_style"].get("background_color").is_none());
    }

    #[test]
    fn color_add_change_remove_and_legacy_aliases() {
        for (xml, text_color, bg_color) in [
            ("<p id=\"b1\"><b><span text-color=\"red\" background-color=\"medium-blue\">text</span></b></p>", Some(1), Some(12)),
            ("<p id=\"b1\"><b><span text-color=\"green\" background-color=\"light-yellow\">text</span></b></p>", Some(4), Some(3)),
            ("<p id=\"b1\"><b>text</b></p>", None, None),
        ] {
            let next = crate::docxml::parse(xml).unwrap();
            let payload = elements(&next.blocks[0].text).unwrap();
            let style = &payload[0]["text_run"]["text_element_style"];
            assert_eq!(style.get("text_color").and_then(Value::as_i64), text_color);
            assert_eq!(style.get("background_color").and_then(Value::as_i64), bg_color);
        }
        assert_eq!(canonical_color("medium-blue", true), Some("blue"));
        assert_eq!(canonical_color("medium-gray", true), Some("medium-gray"));
    }

    #[test]
    fn rejects_unknown_color_and_unmodeled_content() {
        assert!(check_xml("<p id=\"b1\"><span text-color=\"rgb(216,57,49)\" background-color=\"rgba(255,246,122,0.8)\">x</span></p>", "b1").is_ok());
        assert!(check_xml("<p id=\"b1\"><span text-color=\"rgb(1,2,3)\">x</span></p>", "b1").is_err());
        assert!(check_xml("<p id=\"b1\"><span text-color=\"red\" other=\"x\">x</span></p>", "b1").is_err());
        let doc = crate::docxml::parse("<p><span text-color=\"mystery\">x</span></p>").unwrap();
        assert!(elements(&doc.blocks[0].text).is_err());
        for style in [json!({"text_color": 8}), json!({"background_color": 0}), json!({"text_color": "red"}),
            json!({"link": {"url": 1}}), json!({"bold": "true"}), json!({"unknown": true})] {
            let response = json!({"data": {"block": {"block_type": 2, "text": {"elements": [
                {"text_run": {"content": "x", "text_element_style": style}}
            ]}}}});
            assert!(remote_text(&response, &BlockKind::Text).is_err());
        }
    }
}
