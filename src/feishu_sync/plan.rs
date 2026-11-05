//! Pure, identity-based planning for manual synchronization.
use std::collections::{HashMap, HashSet};

use anyhow::{bail, Result};
use crate::model::{Block, BlockKind, Document, RichText};

#[derive(Debug)]
pub(super) enum Edit<'a> {
    Insert { parent: Option<&'a Block>, after: Option<&'a Block>, block: &'a Block },
    Delete { block: &'a Block },
    Move { block: &'a Block, parent: Option<&'a Block>, after: Option<&'a Block> },
    Update { old: &'a Block, new: &'a Block },
    Table { old: &'a Block, new: &'a Block },
    Title { old: &'a RichText, new: &'a RichText },
}

// Source IDs and persistent local IDs are separate namespaces.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Key<'a> { Source(&'a str), Sync(&'a str), Local(usize) }

#[derive(Clone, Copy)]
struct Node<'a> { block: &'a Block, parent: Option<Key<'a>>, index: usize }

type Nodes<'a> = HashMap<Key<'a>, Node<'a>>;

fn local(block: &Block) -> Key<'_> { Key::Local(block as *const Block as usize) }

// Ordinary children are planned separately. Table cells are owned by the
// table operation, so their persistent identities must remain in the comparison.
fn content(block: &Block) -> String {
    let copy = Block { id: block.id, source_id: None, sync_id: None, kind: block.kind.clone(),
        text: block.text.clone(), align: block.align, bg: block.bg.clone(), children: Vec::new(), folded: block.folded };
    crate::docxml::serialize(&Document { blocks: vec![copy], ..Document::default() })
}

fn validate_ids(blocks: &[Block], ids: &mut HashSet<String>, sync_ids: &mut HashSet<String>) -> Result<()> {
    for block in blocks {
        if let Some(id) = &block.source_id
            && (id.is_empty() || !ids.insert(id.clone())) { bail!("云端块 ID 为空或重复，无法安全同步: {id}"); }
        if let Some(id) = &block.sync_id
            && (id.is_empty() || !sync_ids.insert(id.clone())) { bail!("本地同步 ID 为空或重复，无法安全同步: {id}"); }
        validate_ids(&block.children, ids, sync_ids)?;
        if let BlockKind::Table(table) = &block.kind {
            for row in &table.rows {
                for cell in row {
                    if let Some(id) = &cell.source_id
                        && (id.is_empty() || !ids.insert(id.clone())) { bail!("云端单元格 ID 为空或重复，无法安全同步: {id}"); }
                    if let Some(id) = &cell.sync_id
                        && (id.is_empty() || !sync_ids.insert(id.clone())) { bail!("本地同步 ID 为空或重复，无法安全同步: {id}"); }
                    validate_ids(&cell.blocks, ids, sync_ids)?;
                }
            }
        }
    }
    Ok(())
}

fn index_old<'a>(blocks: &'a [Block], parent: Option<Key<'a>>, nodes: &mut Nodes<'a>, order: &mut Vec<Key<'a>>) {
    for (index, block) in blocks.iter().enumerate() {
        let key = block.source_id.as_deref().map(Key::Source)
            .or_else(|| block.sync_id.as_deref().map(Key::Sync)).unwrap_or_else(|| local(block));
        nodes.insert(key, Node { block, parent, index });
        order.push(key);
        index_old(&block.children, Some(key), nodes, order);
    }
}

fn index_new<'a>(blocks: &'a [Block], parent: Option<Key<'a>>, old: &Nodes<'a>, aliases: &HashMap<&'a str, Key<'a>>, nodes: &mut Nodes<'a>, order: &mut Vec<Key<'a>>) -> Result<()> {
    // An identity-less legacy block can only pass through unchanged at the
    // same position; never infer a writable cloud identity from its content.
    let previous: Vec<_> = old.iter().filter(|(_, n)| n.parent == parent && n.block.source_id.is_none() && n.block.sync_id.is_none()).collect();
    let mut matched = HashSet::new();
    for (index, block) in blocks.iter().enumerate() {
        let source_key = block.source_id.as_deref().map(Key::Source).filter(|key| old.contains_key(key));
        let sync_key = block.sync_id.as_deref().and_then(|id| aliases.get(id).copied());
        if let (Some(a), Some(b)) = (source_key, sync_key)
            && a != b { bail!("云端块 ID 与本地同步 ID 指向不同的基线块"); }
        let key = if let Some(key) = source_key.or(sync_key) {
            if let (Some(a), Some(b)) = (old[&key].block.source_id.as_deref(), block.source_id.as_deref())
                && a != b { bail!("本地同步 ID 对应的云端块 ID 已改变"); }
            key
        } else if let Some(id) = block.source_id.as_deref() {
            bail!("块 {id} 不在同步基线中，无法确定云端身份");
        } else if let Some((key, _)) = previous.iter().find(|(_, node)| node.index == index && content(node.block) == content(block)) {
            matched.insert(**key);
            **key
        } else if let Some(id) = block.sync_id.as_deref() {
            Key::Sync(id)
        } else {
            local(block)
        };
        if nodes.insert(key, Node { block, parent, index }).is_some() { bail!("多个本地块指向同一个基线块"); }
        order.push(key);
        index_new(&block.children, Some(key), old, aliases, nodes, order)?;
    }
    for (key, _) in previous {
        if !matched.contains(key) { bail!("没有云端身份的基线块被修改、删除或移动，无法安全同步"); }
    }
    Ok(())
}

// Keep a longest ordered subsequence of same-parent survivors in place. All
// other survivors must move; insertions and deletions do not cause moves.
fn stationary<'a>(new_order: &[Key<'a>], old: &Nodes<'a>, new: &Nodes<'a>) -> Result<HashSet<Key<'a>>> {
    let mut groups: HashMap<Option<Key<'a>>, Vec<(usize, Key<'a>)>> = HashMap::new();
    for &key in new_order {
        if let (Some(a), Some(b)) = (old.get(&key), new.get(&key))
            && a.parent == b.parent { groups.entry(b.parent).or_default().push((a.index, key)); }
    }
    let mut keep = HashSet::new();
    for group in groups.values() {
        let fixed: Vec<_> = group.iter().enumerate().filter(|(_, (_, key))| matches!(key, Key::Local(_))).collect();
        if fixed.windows(2).any(|pair| pair[0].1.0 >= pair[1].1.0) {
            bail!("没有云端身份的基线块相对顺序改变，无法安全同步");
        }
        let mut start = 0;
        let mut lower = None;
        for boundary in fixed.iter().map(|(i, (pos, key))| (*i, Some(*pos), Some(*key)))
            .chain(std::iter::once((group.len(), None, None)))
        {
            let (end, upper, fixed_key) = boundary;
            let mut tails: Vec<usize> = Vec::new();
            let mut previous = vec![None; end - start];
            for i in start..end {
                let position = group[i].0;
                if lower.is_some_and(|p| position <= p) || upper.is_some_and(|p| position >= p) { continue; }
                let slot = tails.partition_point(|&j| group[j].0 < position);
                if slot > 0 { previous[i - start] = Some(tails[slot - 1]); }
                if slot == tails.len() { tails.push(i); } else { tails[slot] = i; }
            }
            let mut cursor = tails.last().copied();
            while let Some(i) = cursor { keep.insert(group[i].1); cursor = previous[i - start]; }
            if let Some(key) = fixed_key { keep.insert(key); }
            start = end + 1;
            lower = upper;
        }
    }
    Ok(keep)
}

/// Operations are ordered: parents precede their children, anchors precede the
/// operation using them, and deletes follow moves out of deleted subtrees.
/// An Insert creates only its block, not its children; children get their own
/// operations. References in parent/after point into `new`, while Move/Delete
/// targets point into `old`. A Table operation owns its cell changes.
pub(super) fn plan<'a>(old: &'a Document, new: &'a Document) -> Result<Vec<Edit<'a>>> {
    // Icons are local presentation, including those set from the sidebar.
    if old.cover != new.cover { bail!("不支持同步文档封面更改"); }
    let mut old_ids = HashSet::new();
    let mut new_ids = HashSet::new();
    let mut old_sync_ids = HashSet::new();
    let mut new_sync_ids = HashSet::new();
    validate_ids(&old.blocks, &mut old_ids, &mut old_sync_ids)?;
    validate_ids(&new.blocks, &mut new_ids, &mut new_sync_ids)?;
    let mut old_nodes = HashMap::new();
    let mut old_order = Vec::new();
    index_old(&old.blocks, None, &mut old_nodes, &mut old_order);
    let aliases: HashMap<_, _> = old_nodes.iter().filter_map(|(&key, node)| node.block.sync_id.as_deref().map(|id| (id, key))).collect();
    let mut new_nodes = HashMap::new();
    let mut new_order = Vec::new();
    index_new(&new.blocks, None, &old_nodes, &aliases, &mut new_nodes, &mut new_order)?;
    for id in old_ids.intersection(&new_ids) {
        let key = Key::Source(id);
        if old_nodes.contains_key(&key) != new_nodes.contains_key(&key) {
            bail!("块 {id} 在表格内外移动，无法安全规划");
        }
    }
    let keep = stationary(&new_order, &old_nodes, &new_nodes)?;
    let mut edits = Vec::new();
    if old.title != new.title { edits.push(Edit::Title { old: &old.title, new: &new.title }); }
    let mut preceding: HashMap<Option<Key<'a>>, &'a Block> = HashMap::new();
    for key in new_order {
        let node = new_nodes[&key];
        let parent = node.parent.map(|p| new_nodes[&p].block);
        let after = preceding.get(&node.parent).copied();
        if let Some(before) = old_nodes.get(&key) {
            if !keep.contains(&key) {
                if matches!(key, Key::Local(_)) { bail!("没有持久身份的基线块无法安全移动"); }
                edits.push(Edit::Move { block: before.block, parent, after });
            }
            if content(before.block) != content(node.block) {
                if matches!(key, Key::Local(_)) { bail!("没有持久身份的基线块无法安全修改"); }
                if matches!(before.block.kind, BlockKind::Table(_)) != matches!(node.block.kind, BlockKind::Table(_)) {
                    bail!("表格块类型改变，无法作为原生表格变更安全同步");
                }
                if matches!((&before.block.kind, &node.block.kind), (BlockKind::Table(_), BlockKind::Table(_))) {
                    edits.push(Edit::Table { old: before.block, new: node.block });
                } else {
                    edits.push(Edit::Update { old: before.block, new: node.block });
                }
            }
        } else {
            edits.push(Edit::Insert { parent, after, block: node.block });
        }
        preceding.insert(node.parent, node.block);
    }
    for key in old_order {
        let node = old_nodes[&key];
        if !new_nodes.contains_key(&key) && node.parent.is_none_or(|p| new_nodes.contains_key(&p)) {
            if matches!(key, Key::Local(_)) { bail!("没有持久身份的基线块无法安全删除"); }
            edits.push(Edit::Delete { block: node.block });
        }
    }
    Ok(edits)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{FileBlock, FileView, Table, TableCell};

    fn block(id: &str) -> Block {
        let mut block = Block::text(id);
        block.source_id = Some(id.into());
        block
    }

    fn doc(blocks: Vec<Block>) -> Document { Document { blocks, ..Document::default() } }

    fn label(block: Option<&Block>) -> String {
        block.map(|b| b.source_id.as_deref().unwrap_or(&b.text.text)).unwrap_or("-").to_owned()
    }

    fn sequence(edits: &[Edit<'_>]) -> Vec<String> {
        edits.iter().map(|e| match e {
            Edit::Insert { parent, after, block } => format!("insert {} {} {}", label(*parent), label(*after), label(Some(block))),
            Edit::Delete { block } => format!("delete {}", label(Some(block))),
            Edit::Move { block, parent, after } => format!("move {} {} {}", label(Some(block)), label(*parent), label(*after)),
            Edit::Update { old, new } => format!("update {} {}", label(Some(old)), new.text.text),
            Edit::Table { old, .. } => format!("table {}", label(Some(old))),
            Edit::Title { old, new } => format!("title {} {}", old.text, new.text),
        }).collect()
    }

    #[test]
    fn persistent_id_creates_once_then_matches_for_move_and_update() {
        let mut added = Block::text("added");
        added.sync_id = Some("local-1".into());
        let original = doc(vec![block("a"), block("b")]);
        let first = doc(vec![block("a"), added.clone(), block("b")]);
        assert_eq!(sequence(&plan(&original, &first).unwrap()), ["insert - a added"]);
        assert!(plan(&first, &first).unwrap().is_empty());
        let mut changed = added.clone();
        changed.text = RichText::plain("changed");
        let mut parent = block("b");
        parent.children.push(changed);
        let next = doc(vec![block("a"), parent]);
        assert_eq!(sequence(&plan(&first, &next).unwrap()), ["move added b -", "update added changed"]);
        let mut with_cloud_id = added;
        with_cloud_id.source_id = Some("cloud-1".into());
        let resolved = doc(vec![block("a"), with_cloud_id, block("b")]);
        assert!(plan(&first, &resolved).unwrap().is_empty());
    }

    #[test]
    fn duplicate_or_conflicting_persistent_ids_are_rejected() {
        let mut a = block("a");
        a.sync_id = Some("local-a".into());
        let mut b = block("b");
        b.sync_id = Some("local-b".into());
        let old = doc(vec![a.clone(), b.clone()]);
        b.sync_id = Some("local-a".into());
        assert!(plan(&old, &doc(vec![a.clone(), b])).is_err());
        a.sync_id = Some("local-b".into());
        assert!(plan(&old, &doc(vec![a])).is_err());
    }

    #[test]
    fn inserts_deletes_and_raw_preservation() {
        let raw = crate::docxml::parse("<unknown id=\"raw\"/>").unwrap().blocks.remove(0);
        let old = doc(vec![block("a"), raw.clone(), block("b"), block("c")]);
        let new = doc(vec![block("a"), Block::text("one"), Block::text("two"), raw, block("c")]);
        assert_eq!(sequence(&plan(&old, &new).unwrap()), ["insert - a one", "insert - one two", "delete b"]);
    }
    #[test]
    fn unchanged_identityless_legacy_block_does_not_block_other_text_edits() {
        let raw = Block::new(BlockKind::Raw { tag: "x".into(), xml: "<x/>".into() }, RichText::default());
        let old = doc(vec![raw, block("known")]);
        let mut new = old.clone();
        new.blocks[0].sync_id = Some("new-local-id".into());
        new.blocks[1].text = RichText::plain("changed");
        assert_eq!(sequence(&plan(&old, &new).unwrap()), ["update known changed"]);
        new.blocks.swap(0, 1);
        assert!(plan(&old, &new).is_err());
    }


    #[test]
    fn reorder_uses_minimum_moves_and_updates() {
        let old = doc(vec![block("a"), block("b"), block("c"), block("d")]);
        let mut changed = block("b");
        changed.text = RichText::plain("changed");
        let new = doc(vec![changed, block("d"), block("a"), block("c")]);
        assert_eq!(sequence(&plan(&old, &new).unwrap()), ["move b - -", "update b changed", "move d - b"]);
    }

    #[test]
    fn nested_moves_out_before_deleting_parent_and_new_parent_first() {
        let mut parent = block("p");
        parent.kind = BlockKind::Bullet;
        parent.children = vec![block("x"), block("gone")];
        let old = doc(vec![parent, block("q")]);
        let mut fresh = Block::text("fresh");
        fresh.kind = BlockKind::Bullet;
        fresh.children = vec![block("x"), Block::text("child")];
        let new = doc(vec![block("q"), fresh]);
        assert_eq!(sequence(&plan(&old, &new).unwrap()), ["insert - q fresh", "move x fresh -", "insert fresh x child", "delete p"]);
    }

    #[test]
    fn title_and_table_are_distinct_changes() {
        let table = Table { col_widths: vec![100.], header_rows: 0, header_col: false, rows: vec![vec![TableCell::empty()]] };
        let mut b = block("table");
        b.kind = BlockKind::Table(table);
        let old = doc(vec![b.clone()]);
        if let BlockKind::Table(t) = &mut b.kind { t.rows[0][0].blocks[0].text = RichText::plain("cell"); }
        let mut new = doc(vec![b]);
        new.title = RichText::plain("new title");
        assert_eq!(sequence(&plan(&old, &new).unwrap()), ["title  new title", "table table"]);
    }

    #[test]
    fn local_sidebar_icon_does_not_block_other_document_changes() {
        let old = doc(vec![block("a")]);
        let mut new = old.clone();
        new.icon = Some("📁".into());
        assert!(plan(&old, &new).unwrap().is_empty());
        new.title = RichText::plain("new title");
        assert_eq!(sequence(&plan(&old, &new).unwrap()), ["title  new title"]);
    }
    #[test]
    fn reordering_identical_cell_blocks_still_produces_table_edit() {
        let old = crate::docxml::parse("<table id=\"table\"><tbody><tr><td id=\"cell\"><p id=\"a\">same</p><p id=\"b\">same</p></td></tr></tbody></table>").unwrap();
        let mut new = old.clone();
        let BlockKind::Table(table) = &mut new.blocks[0].kind else { panic!("table expected") };
        table.rows[0][0].blocks.swap(0, 1);
        assert!(matches!(plan(&old, &new).unwrap().as_slice(), [Edit::Table { .. }]));
    }


    #[test]
    fn identities_are_global_and_ambiguous_baselines_fail() {
        let old = doc(vec![block("a"), block("b")]);
        let new = doc(vec![block("b"), block("b")]);
        assert!(plan(&old, &new).unwrap_err().to_string().contains("重复"));
        let unknown = doc(vec![block("other")]);
        assert!(plan(&old, &unknown).unwrap_err().to_string().contains("不在同步基线"));
        let raw = Block::new(BlockKind::Raw { tag: "x".into(), xml: "<x/>".into() }, RichText::default());
        assert!(plan(&doc(vec![raw.clone(), raw.clone()]), &doc(vec![raw])).is_err());
    }

    #[test]
    fn image_insert_is_a_single_block_operation() {
        let image = Block::new(BlockKind::Image { src_kind: "path".into(), src: "assets/a.png".into(), width: None, height: None, caption: None, crop: None }, RichText::default());
        let old = doc(vec![block("a")]);
        let new = doc(vec![block("a"), image]);
        assert_eq!(sequence(&plan(&old, &new).unwrap()), ["insert - a "]);
    }

    #[test]
    fn attachment_and_list_item_insertions() {
        let file = Block::new(BlockKind::File(FileBlock { src_kind: "path".into(), src: "assets/a.pdf".into(), name: "a.pdf".into(), view: FileView::Card }), RichText::default());
        let mut list = block("list");
        list.kind = BlockKind::Bullet;
        let old = doc(vec![list.clone()]);
        let mut item = Block::text("item");
        item.kind = BlockKind::Bullet;
        list.children.push(item);
        let new = doc(vec![list, file]);
        assert_eq!(sequence(&plan(&old, &new).unwrap()), ["insert list - item", "insert - list "]);
    }
}
