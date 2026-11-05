//! Native, identity-preserving edits of an existing document table.
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::Path;

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use crate::model::{Block, BlockKind, CellRange, Document, RichText, Table, TableCell};
use super::{block_key, block_xml, check_update_result, check_xml, cloud_id, comparable_text, elements, fetch_document, native_new_block, read_block, remote_checked, remote_children, run, save, Source};
use super::{media, table::{plan_table, TableEdit}};

struct Step { body: Value, expected: Table }
struct TextChange { key: String, kind: BlockKind, old: RichText, new: RichText, payload: Vec<Value> }
enum CellOp {
    InsertText { index: usize, key: String, block: Block, payload: Value },
    InsertMedia { index: usize, key: String, prepared: media::Prepared },
    Delete { key: String, old: Block },
    Move { key: String, anchor: String },
    Text(TextChange),
    Media { key: String, old: Block, prepared: media::Prepared },
    MediaStyle { key: String, old: Block, new: Block, prepared: media::Prepared, token: RefCell<Option<String>> },
}
struct CellPlan {
    cell: TableCell,
    old: Option<TableCell>,
    operations: Vec<CellOp>,
    receipts: RefCell<HashMap<String, media::Receipt>>,
}

pub(super) struct Prepared {
    table_key: String,
    old: Table,
    new: Table,
    steps: Vec<Step>,
    creation: Option<Value>,
    cells: Vec<CellPlan>,
    removed: Vec<TableCell>,
    affected: Vec<TableCell>,
    removed_blocks: Vec<String>,
}

impl Prepared {
    // Call only after all remote verification succeeds; interrupted runs retain
    // every binding for diagnosis.
    pub(super) fn removed_bindings(&self) -> Vec<String> {
        let mut ids = Vec::new();
        ids.extend(self.removed_blocks.iter().cloned());
        for cell in &self.removed {
            if let Some(id) = &cell.sync_id { ids.push(id.clone()); }
            if let Some(id) = &cell.source_id { ids.push(format!("s:{id}")); }
            fn blocks(nodes: &[Block], ids: &mut Vec<String>) {
                for block in nodes {
                    if let Some(id) = &block.sync_id { ids.push(id.clone()); }
                    if let Some(id) = &block.source_id { ids.push(format!("s:{id}")); }
                    blocks(&block.children, ids);
                }
            }
            blocks(&cell.blocks, &mut ids);
        }
        ids
    }
}

fn cell_key(cell: &TableCell) -> Result<String> {
    if let Some(id) = &cell.source_id { return Ok(format!("s:{id}")); }
    if let Some(id) = &cell.sync_id { return Ok(format!("l:{id}")); }
    bail!("单元格缺少持久身份")
}

fn text_change(old: &Block, new: &Block, source: &Source) -> Result<TextChange> {
    let key = block_key(old)?;
    if key != block_key(new)? { bail!("单元格内块的身份或顺序发生改变，不能原位同步"); }
    let mut before = old.clone();
    before.text = new.text.clone();
    before.source_id = new.source_id.clone();
    before.sync_id = new.sync_id.clone();
    if block_xml(&before) != block_xml(new) || !old.children.is_empty() || !new.children.is_empty() {
        bail!("单元格内块类型、属性或子项发生改变，无法安全原位同步");
    }
    if new.text.text.encode_utf16().count() > 100_000 { bail!("单元格文字超过飞书块长度限制"); }
    let id = cloud_id(&key, source)?;
    check_xml(&source.original_xml, &id)?;
    Ok(TextChange { key, kind: new.kind.clone(), old: comparable_text(&old.text)?,
        new: comparable_text(&new.text)?, payload: elements(&new.text)? })
}

fn prepare_cell(old: Option<&TableCell>, new: &TableCell, source: &Source, assets: Option<(&Path, &Path)>) -> Result<(CellPlan, Vec<String>)> {
    let before = old.map(|c| c.blocks.as_slice()).unwrap_or(&[]);
    let mut originals = HashMap::new();
    let mut current = Vec::new();
    for block in before {
        let key = block_key(block)?;
        cloud_id(&key, source)?;
        if originals.insert(key.clone(), block).is_some() { bail!("单元格子块身份重复"); }
        current.push(key);
    }
    let mut target = Vec::new();
    let mut updates = Vec::new();
    let mut operations = Vec::new();
    let mut removed = Vec::new();
    for (index, block) in new.blocks.iter().enumerate() {
        let key = block_key(block)?;
        if target.contains(&key) { bail!("单元格子块身份重复"); }
        target.push(key.clone());
        if let Some(previous) = originals.get(&key) {
            let xml = |block: &Block| crate::docxml::serialize(&Document { blocks: vec![block.clone()], ..Document::default() });
            if xml(previous) != xml(block) {
                if matches!(previous.kind, BlockKind::Image { .. } | BlockKind::File(_))
                    && matches!(block.kind, BlockKind::Image { .. } | BlockKind::File(_)) {
                    if media::image_style_candidate(previous, block) {
                        updates.push(CellOp::MediaStyle { key: key.clone(), old: (**previous).clone(), new: block.clone(),
                            prepared: media::prepare_image_style(block), token: RefCell::new(None) });
                    } else {
                        let (root, upload) = assets.ok_or_else(|| anyhow!("单元格素材修改需要临时上传目录"))?;
                        updates.push(CellOp::Media { key: key.clone(), old: (**previous).clone(), prepared: media::prepare(block, root, upload)? });
                    }
                } else {
                    updates.push(CellOp::Text(text_change(previous, block, source)?));
                }
            }
            let position = current.iter().position(|v| v == &key).ok_or_else(|| anyhow!("单元格子块身份丢失"))?;
            if position != index {
                if index == 0 {
                    let preceding = current[..position].to_vec();
                    let mut anchor = key.clone();
                    for sibling in preceding {
                        operations.push(CellOp::Move { key: sibling.clone(), anchor: anchor.clone() });
                        let at = current.iter().position(|v| v == &sibling).unwrap();
                        current.remove(at);
                        let at = current.iter().position(|v| v == &anchor).unwrap() + 1;
                        current.insert(at, sibling.clone());
                        anchor = sibling;
                    }
                } else {
                    let anchor = current[index - 1].clone();
                    operations.push(CellOp::Move { key: key.clone(), anchor: anchor.clone() });
                    current.remove(position);
                    let at = current.iter().position(|v| v == &anchor).unwrap() + 1;
                    current.insert(at, key.clone());
                }
            }
        } else {
            let local = key.strip_prefix("l:").filter(|s| !s.is_empty()).ok_or_else(|| anyhow!("新单元格子块缺少本地身份"))?;
            if source.bindings.contains_key(local) { bail!("新单元格子块身份已经绑定"); }
            if !block.children.is_empty() { bail!("新增单元格子块含嵌套内容，无法安全同步"); }
            if matches!(block.kind, BlockKind::Image { .. } | BlockKind::File(_)) {
                let (root, upload) = assets.ok_or_else(|| anyhow!("新增单元格素材需要临时上传目录"))?;
                operations.push(CellOp::InsertMedia { index, key: key.clone(), prepared: media::prepare(block, root, upload)? });
            } else {
                if block.text.text.encode_utf16().count() > 100_000 { bail!("单元格文字超过飞书块长度限制"); }
                let payload = native_new_block(block)?;
                comparable_text(&block.text)?;
                operations.push(CellOp::InsertText { index, key: key.clone(), block: block.clone(), payload });
            }
            current.insert(index, key);
        }
    }
    for key in current.clone() {
        if target.contains(&key) { continue; }
        let block = *originals.get(&key).ok_or_else(|| anyhow!("待删除单元格子块不在基线中"))?;
        if !block.children.is_empty() { bail!("删除含嵌套内容的单元格子块暂不支持"); }
        if !matches!(block.kind, BlockKind::Image { .. } | BlockKind::File(_)) {
            let id = cloud_id(&key, source)?;
            check_xml(&source.original_xml, &id)?;
            comparable_text(&block.text)?;
            elements(&block.text)?;
        }
        if let Some(local) = &block.sync_id { removed.push(local.clone()); }
        if block.source_id.is_some() { removed.push(key.clone()); }
        operations.push(CellOp::Delete { key: key.clone(), old: block.clone() });
        current.retain(|v| v != &key);
    }
    if current != target { bail!("单元格子块顺序规划失败"); }
    operations.extend(updates);
    Ok((CellPlan { cell: new.clone(), old: old.cloned(), operations, receipts: RefCell::new(HashMap::new()) }, removed))
}

// Track a generated cell through the rest of the structural plan. Only cells
// that survive as final visible cells receive local identities.
fn final_position(mut row: usize, mut column: usize, remaining: &[&TableEdit<'_>]) -> Option<(usize, usize)> {
    for edit in remaining {
        match edit {
            TableEdit::InsertRow { index } => { if row >= *index { row += 1; } }
            TableEdit::InsertColumn { index } => { if column >= *index { column += 1; } }
            TableEdit::DeleteRows { start, end } => {
                if (*start..*end).contains(&row) { return None; }
                if row >= *end { row -= end - start; }
            }
            TableEdit::DeleteColumns { start, end } => {
                if (*start..*end).contains(&column) { return None; }
                if column >= *end { column -= end - start; }
            }
            TableEdit::Merge { row_start, row_end, column_start, column_end }
                if (*row_start..*row_end).contains(&row) && (*column_start..*column_end).contains(&column)
                    && (row, column) != (*row_start, *column_start) => { return None; }
            _ => {}
        }
    }
    Some((row, column))
}

#[cfg(test)]
pub(super) fn prepare(old: &Block, new: &Block, source: &Source) -> Result<Prepared> {
    prepare_inner(old, new, source, None)
}

pub(super) fn prepare_with_assets(old: &Block, new: &Block, source: &Source, root: &Path, upload: &Path) -> Result<Prepared> {
    prepare_inner(old, new, source, Some((root, upload)))
}

fn prepare_inner(old: &Block, new: &Block, source: &Source, assets: Option<(&Path, &Path)>) -> Result<Prepared> {
    let (BlockKind::Table(before), BlockKind::Table(after)) = (&old.kind, &new.kind) else { bail!("表格块类型无效"); };
    let table_key = block_key(old)?;
    if table_key != block_key(new)? { bail!("表格身份改变"); }
    cloud_id(&table_key, source)?;
    let mut old_outer = old.clone();
    old_outer.kind = new.kind.clone();
    old_outer.source_id = new.source_id.clone();
    old_outer.sync_id = new.sync_id.clone();
    if block_xml(&old_outer) != block_xml(new) { bail!("表格块的其他属性不能同步"); }
    let edits = plan_table(before, after)?;
    let mut working = before.clone();
    let mut steps = Vec::new();
    let mut cells = Vec::new();
    let mut removed_blocks = Vec::new();
    let mut removed = Vec::new();
    let survivors: HashSet<_> = after.cells().map(|(_, _, c)| cell_key(c)).collect::<Result<_>>()?;
    for (_, _, cell) in before.cells() {
        let key = cell_key(cell)?;
        cloud_id(&key, source)?;
        if !survivors.contains(&key) { removed.push(cell.clone()); }
    }
    let mut affected = removed.clone();
    let mut affected_ids: HashSet<_> = affected.iter().map(cell_key).collect::<Result<_>>()?;
    for edit in &edits {
        let origin = match edit {
            TableEdit::Merge { row_start, column_start, .. } => Some((*row_start, *column_start)),
            TableEdit::Unmerge { row, column } => Some((*row, *column)),
            _ => None,
        };
        if let Some((r, c)) = origin {
            let (r, c) = before.origin(r, c);
            let cell = &before.rows[r][c];
            if affected_ids.insert(cell_key(cell)?) { affected.push(cell.clone()); }
        }
    }
    for (number, edit) in edits.iter().enumerate() {
        let body = match edit {
            TableEdit::InsertRow { index } => {
                let position = if *index == working.rows.len() { -1 } else { i64::try_from(*index)? };
                working.insert_row(*index);
                Some(json!({"insert_table_row": {"row_index": position}}))
            }
            TableEdit::InsertColumn { index } => {
                let position = if *index == working.ncols() { -1 } else { i64::try_from(*index)? };
                working.insert_col(*index);
                Some(json!({"insert_table_column": {"column_index": position}}))
            }
            TableEdit::DeleteRows { start, end } => {
                working.delete_rows(*start..*end);
                Some(json!({"delete_table_rows": {"row_start_index": start, "row_end_index": end}}))
            }
            TableEdit::DeleteColumns { start, end } => {
                working.delete_cols(*start..*end);
                Some(json!({"delete_table_columns": {"column_start_index": start, "column_end_index": end}}))
            }
            TableEdit::Unmerge { row, column } => {
                working.unmerge(*row, *column);
                Some(json!({"unmerge_table_cells": {"row_index": row, "column_index": column}}))
            }
            TableEdit::Merge { row_start, row_end, column_start, column_end } => {
                working.merge(CellRange::span((*row_start, *column_start), (row_end - 1, column_end - 1)));
                Some(json!({"merge_table_cells": {"row_start_index": row_start, "row_end_index": row_end,
                    "column_start_index": column_start, "column_end_index": column_end}}))
            }
            TableEdit::ColumnWidth { column, width } => {
                working.col_widths[*column] = *width as f32;
                Some(json!({"update_table_property": {"column_index": column, "column_width": width}}))
            }
            TableEdit::HeaderRow { enabled } => {
                working.header_rows = if *enabled { 1 } else { 0 };
                Some(json!({"update_table_property": {"header_row": enabled}}))
            }
            TableEdit::HeaderColumn { enabled } => {
                working.header_col = *enabled;
                Some(json!({"update_table_property": {"header_column": enabled}}))
            }
            TableEdit::Cell { old, new, .. } => {
                if old.is_none() {
                    let local = new.sync_id.as_deref().filter(|s| !s.is_empty()).ok_or_else(|| anyhow!("新增单元格缺少本地身份"))?;
                    if new.source_id.is_some() || source.bindings.contains_key(local) { bail!("新增单元格身份已绑定"); }
                }
                let (cell, deleted) = prepare_cell(*old, new, source, assets)?;
                cells.push(cell);
                removed_blocks.extend(deleted);
                None
            }
        };
        if let Some(body) = body {
            let remaining: Vec<_> = edits[number + 1..].iter().collect();
            for (r, row) in working.rows.iter_mut().enumerate() {
                for (c, cell) in row.iter_mut().enumerate() {
                    if cell.is_covered() || cell.source_id.is_some() || cell.sync_id.is_some() { continue; }
                    if let Some((nr, nc)) = final_position(r, c, &remaining) {
                        let target = &after.rows[nr][nc];
                        if !target.is_covered() {
                            if target.source_id.is_some() { bail!("新增结构错误地替换了已有单元格"); }
                            cell.sync_id = target.sync_id.clone();
                        }
                    }
                }
            }
            steps.push(Step { body, expected: working.clone() });
        }
    }
    Ok(Prepared { table_key, old: before.clone(), new: after.clone(), steps, creation: None, cells, removed, affected, removed_blocks })
}

/// Prepare creation using the native Create Blocks API (at most 9×9 per
/// create), then append extra rows/columns with native patches.
pub(super) fn prepare_new(block: &Block, source: &Source, root: &Path, upload: &Path) -> Result<Prepared> {
    let BlockKind::Table(table) = &block.kind else { bail!("新增块不是表格"); };
    if block.align != crate::model::Align::default() || block.bg.is_some() || block.folded
        || !block.text.is_empty() || !block.children.is_empty() { bail!("新增表格包含无法安全创建的块属性或子项"); }
    let table_key = block_key(block)?;
    let local = table_key.strip_prefix("l:").filter(|s| !s.is_empty()).ok_or_else(|| anyhow!("新增表格缺少本地身份"))?;
    if source.bindings.contains_key(local) { bail!("新增表格身份已绑定"); }
    plan_table(table, table)?;
    for &width in &table.col_widths {
        if !width.is_finite() || width < 50.0 || width.fract() != 0.0 || width > i32::MAX as f32 { bail!("飞书列宽必须是至少 50 像素的整数"); }
    }
    let mut cells = Vec::new();
    for (_, _, cell) in table.cells() {
        let local = cell.sync_id.as_deref().filter(|s| !s.is_empty()).ok_or_else(|| anyhow!("新增单元格缺少本地身份"))?;
        if cell.source_id.is_some() || source.bindings.contains_key(local) || cell.bg.is_some() || cell.valign.is_some() {
            bail!("新增表格单元格含已绑定身份或不支持的单元格样式");
        }
        let (cell, removed) = prepare_cell(None, cell, source, Some((root, upload)))?;
        if !removed.is_empty() { bail!("新增单元格包含已有块"); }
        cells.push(cell);
    }
    let (rows, cols) = (table.rows.len().min(9), table.ncols().min(9));
    let mut working = Table { col_widths: table.col_widths[..cols].to_vec(), header_rows: table.header_rows,
        header_col: table.header_col, rows: (0..rows).map(|_| (0..cols).map(|_| TableCell::empty()).collect()).collect() };
    let assign = |current: &mut Table| {
        for (r, row) in current.rows.iter_mut().enumerate() {
            for (c, cell) in row.iter_mut().enumerate() {
                if !table.rows[r][c].is_covered() { cell.sync_id = table.rows[r][c].sync_id.clone(); }
            }
        }
    };
    assign(&mut working);
    let initial = working.clone();
    let widths: Vec<i32> = initial.col_widths.iter().map(|width| *width as i32).collect();
    let creation = json!({"block_type": 31, "table": {"property": {
        "row_size": rows, "column_size": cols, "column_width": widths,
        "header_row": table.header_rows == 1, "header_column": table.header_col
    }}});
    let mut steps = Vec::new();
    while working.rows.len() < table.rows.len() {
        working.insert_row(working.rows.len());
        assign(&mut working);
        steps.push(Step { body: json!({"insert_table_row": {"row_index": -1}}), expected: working.clone() });
    }
    while working.ncols() < table.ncols() {
        working.insert_col(working.ncols());
        assign(&mut working);
        steps.push(Step { body: json!({"insert_table_column": {"column_index": -1}}), expected: working.clone() });
    }
    for (column, &width) in table.col_widths.iter().enumerate() {
        if working.col_widths[column] != width {
            working.col_widths[column] = width;
            steps.push(Step { body: json!({"update_table_property": {"column_index": column, "column_width": width as i32}}), expected: working.clone() });
        }
    }
    for (r, c, cell) in table.cells() {
        if cell.rowspan > 1 || cell.colspan > 1 {
            let end_r = r + cell.rowspan as usize;
            let end_c = c + cell.colspan as usize;
            working.merge(CellRange::span((r, c), (end_r - 1, end_c - 1)));
            steps.push(Step { body: json!({"merge_table_cells": {"row_start_index": r, "row_end_index": end_r,
                "column_start_index": c, "column_end_index": end_c}}), expected: working.clone() });
        }
    }
    Ok(Prepared { table_key, old: initial, new: table.clone(), steps, creation: Some(creation), cells,
        removed: Vec::new(), affected: Vec::new(), removed_blocks: Vec::new() })
}

fn table_from_xml(xml: &str, table_id: &str) -> Result<Table> {
    let document = crate::docxml::parse(xml).context("无法解析飞书表格 XML")?;
    fn find<'a>(blocks: &'a [Block], id: &str) -> Option<&'a Table> {
        for block in blocks {
            if block.source_id.as_deref() == Some(id)
                && let BlockKind::Table(table) = &block.kind { return Some(table); }
            if let Some(table) = find(&block.children, id) { return Some(table); }
            if let BlockKind::Table(table) = &block.kind {
                for (_, _, cell) in table.cells() {
                    if let Some(table) = find(&cell.blocks, id) { return Some(table); }
                }
            }
        }
        None
    }
    find(&document.blocks, table_id).cloned().ok_or_else(|| anyhow!("飞书 XML 中找不到表格或表格 ID"))
}

fn no_comments(block: &Value) -> Result<()> {
    if let Some(ids) = block.get("comment_ids")
        && !ids.as_array().is_some_and(Vec::is_empty) { bail!("表格或被影响的单元格含评论，不能安全修改"); }
    Ok(())
}

// The official FAQ specifies table children in row-major order. A merged
// full-grid layout is accepted only when native spans and XML geometry agree.
fn inspect_table(response: &Value, xml_table: &Table, expected: &Table, source: &Source, allow_unbound: bool) -> Result<Vec<(String, String)>> {
    let block = response.pointer("/data/block").ok_or_else(|| anyhow!("缺少表格块"))?;
    if block.get("block_type").and_then(Value::as_u64) != Some(31) { bail!("云端块不是表格"); }
    no_comments(block)?;
    let prop = block.pointer("/table/property").ok_or_else(|| anyhow!("飞书缺少表格属性"))?;
    if prop.get("row_size").and_then(Value::as_u64) != Some(expected.rows.len() as u64)
        || prop.get("column_size").and_then(Value::as_u64) != Some(expected.ncols() as u64)
        || xml_table.rows.len() != expected.rows.len() || xml_table.ncols() != expected.ncols() {
        bail!("云端表格行列数量不符");
    }
    if prop.get("header_row").and_then(Value::as_bool).unwrap_or(false) != (expected.header_rows == 1)
        || prop.get("header_column").and_then(Value::as_bool).unwrap_or(false) != expected.header_col { bail!("云端表头不符"); }
    if let Some(widths) = prop.get("column_width") {
        let widths = widths.as_array().ok_or_else(|| anyhow!("云端列宽无效"))?;
        if widths.len() != expected.ncols() || widths.iter().zip(&expected.col_widths).any(|(a, b)| a.as_i64().map(|x| x as f32) != Some(*b)) {
            bail!("云端列宽不符");
        }
    } else { bail!("飞书未返回列宽，无法核对表格"); }
    let native = block.pointer("/table/cells").and_then(Value::as_array).ok_or_else(|| anyhow!("飞书未返回单元格 ID"))?;
    let xml_cells: Vec<_> = xml_table.cells().collect();
    let grid_len = expected.rows.len().checked_mul(expected.ncols()).ok_or_else(|| anyhow!("表格尺寸无效"))?;
    let full_grid = native.len() == grid_len;
    let merged = expected.cells().any(|(_, _, cell)| cell.rowspan > 1 || cell.colspan > 1);
    if !full_grid && native.len() != xml_cells.len() { bail!("飞书单元格 ID 排列无法安全对应 XML"); }
    let missing_ids = xml_cells.iter().any(|(_, _, cell)| cell.source_id.is_none());
    if missing_ids && !full_grid { bail!("缺少单元格 ID 时无法确认合并区域的原生排列"); }
    if missing_ids || (merged && full_grid) {
        let children = block.get("children").and_then(Value::as_array).ok_or_else(|| anyhow!("飞书未返回表格子块顺序"))?;
        if children != native { bail!("表格子块与单元格列表顺序不一致"); }
    }
    let spans = if merged && full_grid {
        let spans = prop.get("merge_info").and_then(Value::as_array).ok_or_else(|| anyhow!("飞书未返回合并信息"))?;
        if spans.len() != grid_len { bail!("飞书合并信息不是完整网格"); }
        Some(spans)
    } else { None };
    let mut seen = HashSet::new();
    let mut bindings = Vec::new();
    for (i, (r, c, actual)) in xml_cells.iter().enumerate() {
        let index = if full_grid { *r * expected.ncols() + *c } else { i };
        let id = native[index].as_str().filter(|id| !id.is_empty() && id.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-'))
            .ok_or_else(|| anyhow!("原生单元格 ID 无效"))?;
        if !seen.insert(id) || actual.source_id.as_deref().is_some_and(|xml_id| xml_id != id) {
            bail!("原生表格与 XML 单元格身份或顺序不符");
        }
        let want = &expected.rows[*r][*c];
        if want.is_covered() || want.rowspan != actual.rowspan || want.colspan != actual.colspan { bail!("云端合并结构不符"); }
        if let Some(spans) = spans {
            let span = &spans[index];
            if span.get("row_span").and_then(Value::as_u64) != Some(u64::from(want.rowspan))
                || span.get("col_span").and_then(Value::as_u64) != Some(u64::from(want.colspan)) {
                bail!("原生合并信息与 XML 或发送快照不符");
            }
        }
        if want.bg != actual.bg || want.valign != actual.valign { bail!("云端单元格样式与发送快照不符"); }
        if want.source_id.is_some() {
            if id != cloud_id(&cell_key(want)?, source)? { bail!("既有单元格身份不符"); }
        } else if let Some(local) = &want.sync_id {
            match source.bindings.get(local) {
                Some(bound) if bound == id => {}
                Some(_) => bail!("单元格的云端绑定改变"),
                None if allow_unbound => bindings.push((local.clone(), id.to_string())),
                None => bail!("单元格缺少云端绑定"),
            }
        } else if !allow_unbound { bail!("单元格缺少持久身份"); }
    }
    if expected.cells().count() != xml_cells.len() { bail!("表格可见单元格数量不符"); }
    Ok(bindings)
}

fn check_cell_identity(response: &Value, parent: &str, cell: &TableCell, source: &Source) -> Result<()> {
    let remote = response.pointer("/data/block").ok_or_else(|| anyhow!("飞书缺少单元格块"))?;
    if remote.get("block_type").and_then(Value::as_u64) != Some(32)
        || remote.get("parent_id").and_then(Value::as_str) != Some(parent) {
        bail!("云端单元格类型或所属表格不符");
    }
    let expected = cell.blocks.iter().map(|b| cloud_id(&block_key(b)?, source)).collect::<Result<Vec<_>>>()?;
    if expected.is_empty() || remote_children(response)? != expected {
        bail!("无法根据单元格真实子块身份确认对应关系");
    }
    Ok(())
}

fn check_covered_cells(response: &Value, table: &Table, table_id: &str, source: &Source) -> Result<()> {
    let native = response.pointer("/data/block/table/cells").and_then(Value::as_array).ok_or_else(|| anyhow!("飞书缺少单元格列表"))?;
    if native.len() != table.rows.len() * table.ncols() { return Ok(()); }
    let mut seen = HashSet::new();
    for (r, c, _) in table.cells() {
        let id = native[r * table.ncols() + c].as_str().ok_or_else(|| anyhow!("单元格身份无效"))?;
        seen.insert(id.to_owned());
    }
    for (r, row) in table.rows.iter().enumerate() {
        for (c, slot) in row.iter().enumerate() {
            if !slot.is_covered() { continue; }
            let id = native[r * table.ncols() + c].as_str().filter(|s| !s.is_empty()).ok_or_else(|| anyhow!("合并覆盖格身份无效"))?;
            if !seen.insert(id.to_owned()) { continue; }
            let response = read_block(&source.document_id, id)?;
            let block = response.pointer("/data/block").ok_or_else(|| anyhow!("合并覆盖格响应无效"))?;
            if block.get("block_type").and_then(Value::as_u64) != Some(32)
                || block.get("parent_id").and_then(Value::as_str) != Some(table_id) {
                bail!("合并覆盖格的所属表格不符");
            }
            no_comments(block)?;
            if !remote_children(&response)?.is_empty() { bail!("合并覆盖格含本地无法核对的后代"); }
        }
    }
    Ok(())
}

/// Read-only identity discovery for older XML exports that omit td/th IDs.
/// No source bindings are changed unless the entire table has been checked.
pub(super) fn bind_existing(block: &Block, source: &mut Source) -> Result<()> {
    let BlockKind::Table(table) = &block.kind else { bail!("块不是表格"); };
    plan_table(table, table)?;
    for (_, _, cell) in table.cells() { cell_key(cell)?; }
    let table_id = cloud_id(&block_key(block)?, source)?;
    let xml_table = table_from_xml(&source.original_xml, &table_id)?;
    let response = read_block(&source.document_id, &table_id)?;
    let pending = inspect_table(&response, &xml_table, table, source, true)?;
    let needed: HashSet<_> = pending.iter().map(|(local, _)| local.as_str()).collect();
    let native = response.pointer("/data/block/table/cells").and_then(Value::as_array).ok_or_else(|| anyhow!("飞书缺少单元格列表"))?;
    let full_grid = native.len() == table.rows.len() * table.ncols();
    for (i, (r, c, cell)) in table.cells().enumerate() {
        if !cell.sync_id.as_deref().is_some_and(|local| needed.contains(local)) { continue; }
        let index = if full_grid { r * table.ncols() + c } else { i };
        let id = native[index].as_str().ok_or_else(|| anyhow!("飞书单元格 ID 无效"))?;
        let remote = read_block(&source.document_id, id)?;
        check_cell_identity(&remote, &table_id, cell, source)?;
    }
    for (local, id) in &pending {
        if source.bindings.iter().any(|(key, value)| key != local && value == id) {
            bail!("单元格云端 ID 已绑定到其他本地身份");
        }
    }
    for (local, id) in pending { source.bindings.insert(local, id); }
    Ok(())
}

// Table, row and cell wrapper attributes are not all represented by the local
// model. Unknown markup or comment anchors must not disappear on deletion.
fn check_table_xml(xml: &str, table_id: &str) -> Result<()> {
    use quick_xml::{events::Event, Reader};
    let mut reader = Reader::from_str(xml);
    let mut depth = 0usize;
    let mut target = None;
    let mut found = false;
    loop {
        match reader.read_event()? {
            event @ (Event::Start(_) | Event::Empty(_)) => {
                let empty = matches!(&event, Event::Empty(_));
                let element = match event { Event::Start(e) | Event::Empty(e) => e, _ => unreachable!() };
                let tag = String::from_utf8_lossy(element.name().as_ref()).into_owned();
                let attrs = element.attributes().collect::<std::result::Result<Vec<_>, _>>()?;
                let matching = tag == "table" && attrs.iter().any(|a| a.key.as_ref() == b"id" && a.unescape_value().is_ok_and(|v| v == table_id));
                if matching {
                    if found { bail!("原始 XML 含重复表格身份"); }
                    found = true;
                    target = Some(depth);
                }
                if target.is_some() && matches!(tag.as_str(), "table" | "colgroup" | "col" | "thead" | "tbody" | "tfoot" | "tr" | "td" | "th") {
                    for attr in &attrs {
                        let allowed = match tag.as_str() {
                            "table" => matches!(attr.key.as_ref(), b"id" | b"align"),
                            "col" => matches!(attr.key.as_ref(), b"width" | b"span"),
                            "td" | "th" => matches!(attr.key.as_ref(), b"id" | b"rowspan" | b"colspan" | b"background-color" | b"vertical-align"),
                            _ => false,
                        };
                        if !allowed { bail!("原始表格含无法安全删除的未知属性或评论"); }
                        attr.unescape_value()?;
                    }
                }
                if !empty { depth += 1; } else if target == Some(depth) { target = None; }
            }
            Event::End(_) => {
                depth = depth.saturating_sub(1);
                if target == Some(depth) { target = None; }
            }
            Event::Comment(_) | Event::CData(_) | Event::PI(_) | Event::DocType(_) if target.is_some() => bail!("原始表格含无法安全删除的结构或评论"),
            Event::Eof => break,
            _ => {}
        }
    }
    if !found { bail!("原始 XML 找不到对应表格"); }
    Ok(())
}

fn deleted_block(block: &Block, parent: &str, source: &Source, removed: &mut Vec<String>) -> Result<()> {
    let key = block_key(block)?;
    let cloud = cloud_id(&key, source)?;
    let response = read_block(&source.document_id, &cloud)?;
    let value = response.pointer("/data/block").ok_or_else(|| anyhow!("待删除块响应无效"))?;
    if value.get("parent_id").and_then(Value::as_str) != Some(parent) { bail!("待删除块所属单元格或父块不符"); }
    no_comments(value)?;
    if matches!(block.kind, BlockKind::Table(_)) {
        removed.extend(preflight_delete(block, source)?);
        return Ok(());
    }
    if let Some(id) = &block.sync_id { removed.push(id.clone()); }
    if block.source_id.is_some() { removed.push(key.clone()); }
    if matches!(block.kind, BlockKind::Image { .. } | BlockKind::File(_)) {
        media::preflight(source, &key, block)?;
        return Ok(());
    }
    check_xml(&source.original_xml, &cloud)?;
    if remote_checked(&response, &block.kind)? != comparable_text(&block.text)? { bail!("待删除单元格子块内容与基线不符"); }
    let children = block.children.iter().map(|b| cloud_id(&block_key(b)?, source)).collect::<Result<Vec<_>>>()?;
    if remote_children(&response)? != children { bail!("待删除单元格子块的后代与基线不符"); }
    for child in &block.children { deleted_block(child, &cloud, source, removed)?; }
    Ok(())
}

/// Read-only, recursive deletion preflight; the caller checks the document
/// revision, performs block_delete, and drops returned bindings after success.
pub(super) fn preflight_delete(block: &Block, source: &Source) -> Result<Vec<String>> {
    let BlockKind::Table(table) = &block.kind else { bail!("待删除块不是表格"); };
    if !block.children.is_empty() { bail!("表格外层含无法安全核对的后代"); }
    plan_table(table, table)?;
    let key = block_key(block)?;
    let table_id = cloud_id(&key, source)?;
    check_table_xml(&source.original_xml, &table_id)?;
    let xml_table = table_from_xml(&source.original_xml, &table_id)?;
    let response = read_block(&source.document_id, &table_id)?;
    inspect_table(&response, &xml_table, table, source, false)?;
    let native = response.pointer("/data/block/table/cells").and_then(Value::as_array).ok_or_else(|| anyhow!("飞书缺少单元格列表"))?;
    let children = remote_children(&response)?;
    if children.len() != native.len() || children.iter().zip(native).any(|(child, value)| value.as_str() != Some(child.as_str())) {
        bail!("表格包含无法安全核对的子块");
    }
    let mut removed = Vec::new();
    if let Some(id) = &block.sync_id { removed.push(id.clone()); }
    if block.source_id.is_some() { removed.push(key); }
    let full_grid = native.len() == table.rows.len() * table.ncols();
    check_covered_cells(&response, table, &table_id, source)?;
    for (index, (r, c, cell)) in table.cells().enumerate() {
        let index = if full_grid { r * table.ncols() + c } else { index };
        let cell_id = native[index].as_str().ok_or_else(|| anyhow!("云端单元格 ID 无效"))?;
        let response = read_block(&source.document_id, cell_id)?;
        check_cell_identity(&response, &table_id, cell, source)?;
        no_comments(response.pointer("/data/block").ok_or_else(|| anyhow!("缺少单元格块"))?)?;
        if let Some(id) = &cell.sync_id { removed.push(id.clone()); }
        if let Some(id) = &cell.source_id { removed.push(format!("s:{id}")); }
        for child in &cell.blocks { deleted_block(child, cell_id, source, &mut removed)?; }
    }
    Ok(removed)
}

fn preflight_cell(cell: &CellPlan, source: &Source) -> Result<()> {
    if let Some(old) = &cell.old {
        let response = read_block(&source.document_id, &cloud_id(&cell_key(old)?, source)?)?;
        let parent = response.pointer("/data/block").ok_or_else(|| anyhow!("缺少单元格块"))?;
        if parent.get("block_type").and_then(Value::as_u64) != Some(32) { bail!("云端单元格类型不符"); }
        no_comments(parent)?;
        let expected = old.blocks.iter().map(|b| cloud_id(&block_key(b)?, source)).collect::<Result<Vec<_>>>()?;
        if remote_children(&response)? != expected { bail!("单元格子项与基线不符"); }
    }
    for operation in &cell.operations {
        match operation {
            CellOp::Text(change) => {
                let response = read_block(&source.document_id, &cloud_id(&change.key, source)?)?;
                if remote_checked(&response, &change.kind)? != change.old || !remote_children(&response)?.is_empty() {
                    bail!("单元格文字或子项与基线不符");
                }
            }
            CellOp::Delete { key, old } if matches!(old.kind, BlockKind::Image { .. } | BlockKind::File(_)) => media::preflight(source, key, old)?,
            CellOp::Delete { key, old } => {
                let response = read_block(&source.document_id, &cloud_id(key, source)?)?;
                if remote_checked(&response, &old.kind)? != comparable_text(&old.text)? || !remote_children(&response)?.is_empty() {
                    bail!("被删除单元格子块与基线不符");
                }
            }
            CellOp::Media { key, old, prepared } => {
                media::preflight(source, key, old)?;
                media::check_prepared(source, prepared)?;
            }
            CellOp::MediaStyle { key, old, new, token, .. } => {
                *token.borrow_mut() = Some(media::preflight_image_style(source, key, old, new)?);
            }
            CellOp::InsertMedia { prepared, .. } => media::check_prepared(source, prepared)?,
            CellOp::InsertText { .. } | CellOp::Move { .. } => {}
        }
    }
    Ok(())
}

pub(super) fn preflight(plan: &Prepared, source: &Source) -> Result<()> {
    if plan.creation.is_some() {
        for cell in &plan.cells { preflight_cell(cell, source)?; }
        return Ok(());
    }
    let id = cloud_id(&plan.table_key, source)?;
    let remote = read_block(&source.document_id, &id)?;
    let xml = table_from_xml(&source.original_xml, &id)?;
    inspect_table(&remote, &xml, &plan.old, source, false)?;
    if plan.steps.iter().any(|step| step.body.get("unmerge_table_cells").is_some()) {
        check_covered_cells(&remote, &plan.old, &id, source)?;
    }
    for cell in &plan.cells { preflight_cell(cell, source)?; }
    for cell in &plan.affected {
        let id = cloud_id(&cell_key(cell)?, source)?;
        let response = read_block(&source.document_id, &id)?;
        let value = response.pointer("/data/block").ok_or_else(|| anyhow!("缺少单元格块"))?;
        if value.get("block_type").and_then(Value::as_u64) != Some(32) { bail!("云端单元格类型不符"); }
        no_comments(value)?;
        let expected = cell.blocks.iter().map(|b| cloud_id(&block_key(b)?, source)).collect::<Result<Vec<_>>>()?;
        if remote_children(&response)? != expected { bail!("被删除单元格的子项与基线不符"); }
        for block in &cell.blocks {
            if !block.children.is_empty() { bail!("被删除单元格含嵌套块，不能安全核对评论"); }
            let key = block_key(block)?;
            if matches!(block.kind, BlockKind::Image { .. } | BlockKind::File(_)) {
                media::preflight(source, &key, block)?;
            } else {
                let id = cloud_id(&key, source)?;
                check_xml(&source.original_xml, &id)?;
                let response = read_block(&source.document_id, &id)?;
                if remote_checked(&response, &block.kind)? != comparable_text(&block.text)? || !remote_children(&response)?.is_empty() {
                    bail!("被影响单元格内容与基线不符");
                }
            }
        }
    }
    Ok(())
}

fn bind(source: &mut Source, local: &str, remote: &str, id: &str) -> Result<()> {
    if remote.is_empty() || source.bindings.values().any(|v| v == remote) { bail!("飞书返回了空或重复的云端 ID"); }
    if source.bindings.contains_key(local) { bail!("本地身份已经绑定"); }
    source.bindings.insert(local.to_string(), remote.to_string());
    save(id, source).context("云端已修改，但无法保存单元格身份；请重新下载并核对")
}

fn execute_cell(cell: &CellPlan, id: &str, source: &mut Source) -> Result<()> {
    let cell_id = cloud_id(&cell_key(&cell.cell)?, source)?;
    let response = read_block(&source.document_id, &cell_id)?;
    let block = response.pointer("/data/block").ok_or_else(|| anyhow!("单元格响应无效"))?;
    if block.get("block_type").and_then(Value::as_u64) != Some(32) { bail!("云端单元格类型无效"); }
    let mut current = if let Some(old) = &cell.old {
        old.blocks.iter().map(block_key).collect::<Result<Vec<_>>>()?
    } else { Vec::new() };
    let mut default = None;
    if cell.old.is_none() {
        no_comments(block)?;
        let children = remote_children(&response)?;
        if children.len() > 1 { bail!("新单元格的默认子项数量不明"); }
        if let Some(actual) = children.first() {
            let block = read_block(&source.document_id, actual)?;
            if remote_checked(&block, &BlockKind::Text)? != RichText::plain("") || !remote_children(&block)?.is_empty() {
                bail!("新单元格的默认内容不为空，拒绝覆盖");
            }
            default = Some(actual.clone());
        }
    }
    for operation in &cell.operations {
        match operation {
            CellOp::InsertText { index, key, block, payload } => {
                let local = key.strip_prefix("l:").ok_or_else(|| anyhow!("新文字缺少本地身份"))?;
                let reuse = *index == 0 && current.is_empty() && matches!(block.kind, BlockKind::Text) && default.is_some();
                if reuse {
                    let actual = default.take().unwrap();
                    bind(source, local, &actual, id)?;
                    let path = format!("/open-apis/docx/v1/documents/{}/blocks/{actual}", source.document_id);
                    let body = json!({"update_text_elements": {"elements": elements(&block.text)?}}).to_string();
                    run(&["api", "PATCH", &path, "--as", "user", "--data", "-"], Some(&body), None)?;
                } else {
                    let path = format!("/open-apis/docx/v1/documents/{}/blocks/{cell_id}/children", source.document_id);
                    let body = json!({"index": index, "children": [payload]}).to_string();
                    let response = run(&["api", "POST", &path, "--as", "user", "--data", "-"], Some(&body), None)?;
                    let children = response.pointer("/data/children").and_then(Value::as_array).ok_or_else(|| anyhow!("创建单元格子块未返回 ID"))?;
                    if children.len() != 1 { bail!("创建单元格子块返回数量不符"); }
                    let actual = children[0].get("block_id").and_then(Value::as_str).filter(|s| !s.is_empty()).ok_or_else(|| anyhow!("创建单元格子块未返回 ID"))?;
                    bind(source, local, actual, id)?;
                }
                current.insert(*index, key.clone());
            }
            CellOp::InsertMedia { index, key, prepared } => {
                let receipt = media::insert(id, source, key, &cell_id, *index, prepared)?;
                cell.receipts.borrow_mut().insert(key.clone(), receipt);
                current.insert(*index, key.clone());
            }
            CellOp::Move { key, anchor } => {
                let target = cloud_id(key, source)?;
                let after = cloud_id(anchor, source)?;
                let response = run(&["docs", "+update", "--doc", &source.url, "--as", "user", "--command", "block_move_after",
                    "--block-id", &after, "--src-block-ids", &target], None, None)?;
                check_update_result(&response)?;
                let position = current.iter().position(|v| v == key).ok_or_else(|| anyhow!("移动块不在单元格内"))?;
                current.remove(position);
                let position = current.iter().position(|v| v == anchor).ok_or_else(|| anyhow!("移动锚点不在单元格内"))? + 1;
                current.insert(position, key.clone());
            }
            CellOp::Delete { key, .. } => {
                let target = cloud_id(key, source)?;
                let response = run(&["docs", "+update", "--doc", &source.url, "--as", "user", "--command", "block_delete", "--block-id", &target], None, None)?;
                check_update_result(&response)?;
                let position = current.iter().position(|v| v == key).ok_or_else(|| anyhow!("待删除块不在单元格内"))?;
                current.remove(position);
            }
            CellOp::Text(change) => {
                let cloud = cloud_id(&change.key, source)?;
                let path = format!("/open-apis/docx/v1/documents/{}/blocks/{cloud}", source.document_id);
                let body = json!({"update_text_elements": {"elements": change.payload}}).to_string();
                run(&["api", "PATCH", &path, "--as", "user", "--data", "-"], Some(&body), None)?;
            }
            CellOp::Media { key, prepared, .. } => {
                let receipt = media::replace(id, source, key, prepared)?;
                cell.receipts.borrow_mut().insert(key.clone(), receipt);
            }
            CellOp::MediaStyle { key, prepared, token, .. } => {
                let token = token.borrow();
                let receipt = media::update_image_style(source, key, prepared, token.as_deref().ok_or_else(|| anyhow!("图片样式缺少预检 token"))?)?;
                cell.receipts.borrow_mut().insert(key.clone(), receipt);
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(350));
    }
    if let Some(default) = default {
        let response = run(&["docs", "+update", "--doc", &source.url, "--as", "user", "--command", "block_delete", "--block-id", &default], None, None)?;
        check_update_result(&response)?;
    }
    let expected = cell.cell.blocks.iter().map(|b| cloud_id(&block_key(b)?, source)).collect::<Result<Vec<_>>>()?;
    if remote_children(&read_block(&source.document_id, &cell_id)?)? != expected { bail!("单元格子项写入结果不符"); }
    Ok(())
}

/// The parent and insertion index are preflighted by the caller, which must
/// persist interrupted before this function. Returned IDs are saved immediately.
pub(super) fn execute_new(plan: &Prepared, id: &str, source: &mut Source, parent: &str, index: usize) -> Result<()> {
    if !source.interrupted { bail!("表格写入前必须持久记录中断状态"); }
    let creation = plan.creation.as_ref().ok_or_else(|| anyhow!("不是新增表格计划"))?;
    let path = format!("/open-apis/docx/v1/documents/{}/blocks/{parent}/children", source.document_id);
    let body = json!({"index": index, "children": [creation]}).to_string();
    let response = run(&["api", "POST", &path, "--as", "user", "--data", "-"], Some(&body), None)?;
    let children = response.pointer("/data/children").and_then(Value::as_array).ok_or_else(|| anyhow!("创建表格未返回 ID"))?;
    if children.len() != 1 || children[0].get("block_type").and_then(Value::as_u64) != Some(31) { bail!("创建表格返回块数量或类型不符"); }
    let table_id = children[0].get("block_id").and_then(Value::as_str).filter(|s| !s.is_empty()).ok_or_else(|| anyhow!("创建表格未返回 ID"))?.to_owned();
    let local = plan.table_key.strip_prefix("l:").ok_or_else(|| anyhow!("新表格缺少本地身份"))?;
    bind(source, local, &table_id, id)?;
    std::thread::sleep(std::time::Duration::from_millis(350));
    let (_, xml) = fetch_document(&source.url, &source.document_id)?;
    let table = table_from_xml(&xml, &table_id)?;
    let response = read_block(&source.document_id, &table_id)?;
    for (local, actual) in inspect_table(&response, &table, &plan.old, source, true)? {
        bind(source, &local, &actual, id)?;
    }
    execute(plan, id, source)
}

pub(super) fn execute(plan: &Prepared, id: &str, source: &mut Source) -> Result<()> {
    if !source.interrupted { bail!("表格写入前必须持久记录中断状态"); }
    let table_id = cloud_id(&plan.table_key, source)?;
    let path = format!("/open-apis/docx/v1/documents/{}/blocks/{table_id}", source.document_id);
    for step in &plan.steps {
        run(&["api", "PATCH", &path, "--as", "user", "--data", "-"], Some(&step.body.to_string()), None)?;
        std::thread::sleep(std::time::Duration::from_millis(350));
        let (_, xml) = fetch_document(&source.url, &source.document_id)?;
        let table = table_from_xml(&xml, &table_id)?;
        let response = read_block(&source.document_id, &table_id)?;
        for (local, actual) in inspect_table(&response, &table, &step.expected, source, true)? {
            bind(source, &local, &actual, id)?;
        }
    }
    for cell in &plan.cells { execute_cell(cell, id, source)?; }
    Ok(())
}

fn verify_cell(cell: &CellPlan, source: &Source) -> Result<()> {
    let response = read_block(&source.document_id, &cloud_id(&cell_key(&cell.cell)?, source)?)?;
    let expected = cell.cell.blocks.iter().map(|b| cloud_id(&block_key(b)?, source)).collect::<Result<Vec<_>>>()?;
    if remote_children(&response)? != expected { bail!("单元格子项与发送快照不符"); }
    let receipts = cell.receipts.borrow();
    for operation in &cell.operations {
        match operation {
            CellOp::InsertText { key, block, .. } => {
                let response = read_block(&source.document_id, &cloud_id(key, source)?)?;
                if remote_checked(&response, &block.kind)? != comparable_text(&block.text)? { bail!("新增单元格文字不符"); }
            }
            CellOp::Text(change) => {
                let response = read_block(&source.document_id, &cloud_id(&change.key, source)?)?;
                if remote_checked(&response, &change.kind)? != change.new { bail!("单元格文字与发送快照不符"); }
            }
            CellOp::InsertMedia { key, prepared, .. } | CellOp::Media { key, prepared, .. } | CellOp::MediaStyle { key, prepared, .. } => {
                media::verify(source, key, prepared, receipts.get(key).ok_or_else(|| anyhow!("缺少素材写入回执"))?)?;
            }
            CellOp::Delete { .. } | CellOp::Move { .. } => {}
        }
    }
    Ok(())
}

pub(super) fn verify(plan: &Prepared, source: &Source) -> Result<()> {
    let table_id = cloud_id(&plan.table_key, source)?;
    let (_, xml) = fetch_document(&source.url, &source.document_id)?;
    let table = table_from_xml(&xml, &table_id)?;
    inspect_table(&read_block(&source.document_id, &table_id)?, &table, &plan.new, source, false)?;
    for old in &plan.affected {
        let key = cell_key(old)?;
        let Some((_, _, cell)) = plan.new.cells().find(|(_, _, cell)| cell_key(cell).ok().as_deref() == Some(key.as_str())) else { continue; };
        let response = read_block(&source.document_id, &cloud_id(&key, source)?)?;
        let expected = cell.blocks.iter().map(|b| cloud_id(&block_key(b)?, source)).collect::<Result<Vec<_>>>()?;
        if remote_children(&response)? != expected { bail!("合并或拆分后原有单元格子项不符"); }
        for block in &cell.blocks {
            if matches!(block.kind, BlockKind::Image { .. } | BlockKind::File(_)) { continue; }
            let response = read_block(&source.document_id, &cloud_id(&block_key(block)?, source)?)?;
            if remote_checked(&response, &block.kind)? != comparable_text(&block.text)? {
                bail!("合并或拆分后原有单元格内容不符");
            }
        }
    }
    for cell in &plan.cells { verify_cell(cell, source)?; }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn fixture() -> (Source, Block) {
        let xml = "<title>T</title><table id=\"table\"><colgroup><col width=\"100\"/><col width=\"100\"/></colgroup><tbody><tr><td id=\"c00\"><p id=\"p00\">one</p></td><td id=\"c01\"><p id=\"p01\">two</p></td></tr><tr><td id=\"c10\"><p id=\"p10\">three</p></td><td id=\"c11\"><p id=\"p11\">four</p></td></tr></tbody></table>";
        let doc = crate::docxml::parse(xml).unwrap();
        let source = Source { url: "https://example.test/docx/doc".into(), document_id: "doc".into(),
            revision_id: 1, original_xml: xml.into(), baseline: String::new(), interrupted: false, bindings: HashMap::new() };
        (source, doc.blocks[0].clone())
    }

    fn remote(ids: &[&str]) -> Value {
        json!({"data": {"block": {"block_type": 31, "children": ids, "table": {
            "cells": ids, "property": {"row_size": 2, "column_size": 2, "column_width": [100, 100]}
        }}}})
    }

    #[test]
    fn native_ids_must_agree_with_xml_positions_and_local_identities() {
        let (source, block) = fixture();
        let BlockKind::Table(table) = &block.kind else { panic!() };
        let xml = table_from_xml(&source.original_xml, "table").unwrap();
        assert!(inspect_table(&remote(&["c00", "c01", "c10", "c11"]), &xml, table, &source, false).is_ok());
        assert!(inspect_table(&remote(&["c00", "c10", "c01", "c11"]), &xml, table, &source, false).is_err());
        let mut wrong = table.clone();
        wrong.rows[1][0].source_id = Some("different".into());
        assert!(inspect_table(&remote(&["c00", "c01", "c10", "c11"]), &xml, &wrong, &source, false).is_err());
    }

    #[test]
    fn prepares_existing_cell_text_without_replacing_its_siblings() {
        let (source, old) = fixture();
        let mut new = old.clone();
        let BlockKind::Table(table) = &mut new.kind else { panic!() };
        table.rows[1][0].blocks[0].text = RichText::plain("updated");
        let plan = prepare(&old, &new, &source).unwrap();
        assert_eq!(plan.cells.len(), 1);
        let CellOp::Text(change) = &plan.cells[0].operations[0] else { panic!() };
        assert_eq!(change.key, "s:p10");
        assert_eq!(change.new, RichText::plain("updated"));
        assert!(plan.steps.is_empty());
        assert!(plan.removed.is_empty());
    }

    #[test]
    fn appends_row_using_documented_sentinel_and_tracks_new_cells() {
        let (source, old) = fixture();
        let mut new = old.clone();
        let BlockKind::Table(table) = &mut new.kind else { panic!() };
        table.insert_row(2);
        for (c, cell) in table.rows[2].iter_mut().enumerate() {
            cell.sync_id = Some(format!("new-cell-{c}"));
            cell.blocks[0].sync_id = Some(format!("new-text-{c}"));
            cell.blocks[0].text = RichText::plain(format!("value-{c}"));
        }
        let plan = prepare(&old, &new, &source).unwrap();
        assert_eq!(plan.steps[0].body, json!({"insert_table_row": {"row_index": -1}}));
        assert_eq!(plan.steps[0].expected.rows[2][1].sync_id.as_deref(), Some("new-cell-1"));
        assert_eq!(plan.cells.len(), 2);
    }

    #[test]
    fn inserts_at_requested_index_and_rejects_nested_changes_before_writes() {
        let (source, old) = fixture();
        let mut inserted = old.clone();
        let BlockKind::Table(table) = &mut inserted.kind else { panic!() };
        table.insert_row(1);
        for (c, cell) in table.rows[1].iter_mut().enumerate() {
            cell.sync_id = Some(format!("new-cell-{c}"));
            cell.blocks[0].sync_id = Some(format!("new-text-{c}"));
        }
        let plan = prepare(&old, &inserted, &source).unwrap();
        assert_eq!(plan.steps[0].body, json!({"insert_table_row": {"row_index": 1}}));
        assert_eq!(plan.steps[0].expected.rows[2][0].source_id.as_deref(), Some("c10"));
        let mut changed = old.clone();
        let BlockKind::Table(table) = &mut changed.kind else { panic!() };
        table.rows[0][0].blocks[0].children.push(Block::text("nested"));
        assert!(prepare(&old, &changed, &source).is_err());
    }

    #[test]
    fn native_properties_and_removed_rows_are_planned_locally() {
        let (source, old) = fixture();
        let mut new = old.clone();
        let BlockKind::Table(table) = &mut new.kind else { panic!() };
        table.delete_rows(1..2);
        table.col_widths[1] = 120.0;
        table.header_rows = 1;
        let plan = prepare(&old, &new, &source).unwrap();
        assert_eq!(plan.steps[0].body, json!({"delete_table_rows": {"row_start_index": 1, "row_end_index": 2}}));
        assert!(plan.steps.iter().any(|s| s.body == json!({"update_table_property": {"column_index": 1, "column_width": 120}})));
        assert!(plan.steps.iter().any(|s| s.body == json!({"update_table_property": {"header_row": true}})));
        assert_eq!(plan.removed.len(), 2);
    }

    #[test]
    fn merge_and_split_plan_half_open_ranges_and_preserve_origins() {
        let (source, mut old) = fixture();
        let BlockKind::Table(before) = &mut old.kind else { panic!() };
        before.rows[0][1].blocks[0].text = RichText::plain("");
        let mut merged = old.clone();
        let BlockKind::Table(table) = &mut merged.kind else { panic!() };
        table.merge(CellRange::span((0, 0), (0, 1)));
        let plan = prepare(&old, &merged, &source).unwrap();
        assert!(plan.steps.iter().any(|s| s.body == json!({"merge_table_cells": {
            "row_start_index": 0, "row_end_index": 1, "column_start_index": 0, "column_end_index": 2
        }})));
        assert_eq!(plan.affected.len(), 2);
        assert_eq!(plan.steps[0].expected.rows[0][0].source_id.as_deref(), Some("c00"));
        let mut split = merged.clone();
        let BlockKind::Table(table) = &mut split.kind else { panic!() };
        table.unmerge(0, 0);
        table.rows[0][1].sync_id = Some("split-cell".into());
        table.rows[0][1].blocks[0].sync_id = Some("split-text".into());
        let plan = prepare(&merged, &split, &source).unwrap();
        assert_eq!(plan.steps[0].body, json!({"unmerge_table_cells": {"row_index": 0, "column_index": 0}}));
        assert_eq!(plan.steps[0].expected.rows[0][1].sync_id.as_deref(), Some("split-cell"));
    }

    #[test]
    fn cell_plan_inserts_deletes_and_moves_by_identity() {
        let (source, table_block) = fixture();
        let BlockKind::Table(table) = table_block.kind else { panic!() };
        let mut old = table.rows[0][0].clone();
        old.blocks.push(table.rows[0][1].blocks[0].clone());
        old.blocks.push(table.rows[1][0].blocks[0].clone());
        let mut new = old.clone();
        let mut added = Block::text("added");
        added.sync_id = Some("new-child".into());
        new.blocks = vec![old.blocks[2].clone(), added, old.blocks[0].clone()];
        let (plan, removed) = prepare_cell(Some(&old), &new, &source, None).unwrap();
        assert!(plan.operations.iter().any(|op| matches!(op, CellOp::Move { key, anchor } if key == "s:p00" && anchor == "s:p10")));
        assert!(plan.operations.iter().any(|op| matches!(op, CellOp::InsertText { index: 1, key, .. } if key == "l:new-child")));
        assert!(plan.operations.iter().any(|op| matches!(op, CellOp::Delete { key, .. } if key == "s:p01")));
        assert!(removed.contains(&"s:p01".to_string()));
    }

    #[test]
    fn new_large_table_uses_native_create_and_append() {
        let (source, _) = fixture();
        let rows: Vec<Vec<TableCell>> = (0..10).map(|r| (0..10).map(|c| {
            let mut cell = TableCell::empty();
            cell.sync_id = Some(format!("cell-{r}-{c}"));
            cell.blocks[0].sync_id = Some(format!("text-{r}-{c}"));
            cell
        }).collect()).collect();
        let table = Table { rows, col_widths: vec![100.0; 10], header_rows: 1, header_col: false };
        let mut block = Block::new(BlockKind::Table(table), RichText::default());
        block.sync_id = Some("new-table".into());
        let plan = prepare_new(&block, &source, Path::new("."), Path::new(".")).unwrap();
        let creation = plan.creation.as_ref().unwrap();
        assert_eq!(creation.pointer("/table/property/row_size"), Some(&json!(9)));
        assert_eq!(creation.pointer("/table/property/column_size"), Some(&json!(9)));
        assert_eq!(plan.steps[0].body, json!({"insert_table_row": {"row_index": -1}}));
        assert_eq!(plan.steps[1].body, json!({"insert_table_column": {"column_index": -1}}));
        assert_eq!(plan.steps[1].expected.rows[9][9].sync_id.as_deref(), Some("cell-9-9"));
        assert_eq!(plan.cells.len(), 100);
    }

    #[test]
    fn image_presentation_update_does_not_require_reupload() {
        let (source, table_block) = fixture();
        let BlockKind::Table(table) = table_block.kind else { panic!() };
        let mut old = table.rows[0][0].clone();
        let mut image = Block::new(BlockKind::Image { src_kind: "path".into(), src: "assets/original.png".into(),
            width: Some(100.0), height: Some(80.0), caption: None, crop: None }, RichText::default());
        image.source_id = Some("image".into());
        old.blocks = vec![image];
        let mut new = old.clone();
        if let BlockKind::Image { width, .. } = &mut new.blocks[0].kind { *width = Some(120.0); }
        let (plan, _) = prepare_cell(Some(&old), &new, &source, None).unwrap();
        assert!(matches!(&plan.operations[0], CellOp::MediaStyle { key, .. } if key == "s:image"));
    }

    #[test]
    fn new_cell_never_accepts_an_unrelated_cloud_block() {
        let (source, table_block) = fixture();
        let BlockKind::Table(table) = table_block.kind else { panic!() };
        let mut cell = table.rows[0][0].clone();
        cell.source_id = None;
        cell.sync_id = Some("new-cell".into());
        assert!(prepare_cell(None, &cell, &source, None).is_err());
    }

    #[test]
    fn missing_cell_ids_use_native_order_only_with_real_child_identity() {
        let (source, block) = fixture();
        let BlockKind::Table(mut expected) = block.kind else { panic!() };
        let mut xml = source.original_xml.clone();
        for (r, row) in expected.rows.iter_mut().enumerate() {
            for (c, cell) in row.iter_mut().enumerate() {
                xml = xml.replace(&format!(" id=\"c{r}{c}\""), "");
                cell.source_id = None;
                cell.sync_id = Some(format!("local-{r}-{c}"));
            }
        }
        let parsed = table_from_xml(&xml, "table").unwrap();
        let response = remote(&["c00", "c01", "c10", "c11"]);
        let pending = inspect_table(&response, &parsed, &expected, &source, true).unwrap();
        assert_eq!(pending[0], ("local-0-0".into(), "c00".into()));
        let child = json!({"data":{"block":{"block_type":32,"parent_id":"table","children":["p00"]}}});
        assert!(check_cell_identity(&child, "table", &expected.rows[0][0], &source).is_ok());
        let other_child = json!({"data":{"block":{"block_type":32,"parent_id":"table","children":["p01"]}}});
        assert!(check_cell_identity(&other_child, "table", &expected.rows[0][0], &source).is_err());
        let mut disagree = response.clone();
        disagree["data"]["block"]["children"] = json!(["c01", "c00", "c10", "c11"]);
        assert!(inspect_table(&disagree, &parsed, &expected, &source, true).is_err());
    }

    #[test]
    fn missing_ids_on_merged_table_are_not_guessed() {
        let (source, _) = fixture();
        let xml = "<title>T</title><table id=\"table\"><colgroup><col width=\"100\"/><col width=\"100\"/></colgroup><tbody><tr><td colspan=\"2\"><p id=\"p00\">one</p></td></tr><tr><td><p id=\"p10\">three</p></td><td><p id=\"p11\">four</p></td></tr></tbody></table>";
        let parsed = table_from_xml(xml, "table").unwrap();
        let mut expected = parsed.clone();
        expected.rows[0][0].sync_id = Some("a".into());
        expected.rows[1][0].sync_id = Some("b".into());
        expected.rows[1][1].sync_id = Some("c".into());
        assert!(inspect_table(&remote(&["c00", "c10", "c11"]), &parsed, &expected, &source, true).is_err());
        let mut full = remote(&["c00", "hidden", "c10", "c11"]);
        full["data"]["block"]["table"]["property"]["merge_info"] = json!([
            {"row_span":1,"col_span":2}, {"row_span":0,"col_span":0},
            {"row_span":1,"col_span":1}, {"row_span":1,"col_span":1}
        ]);
        let pending = inspect_table(&full, &parsed, &expected, &source, true).unwrap();
        assert_eq!(pending, vec![("a".into(), "c00".into()), ("b".into(), "c10".into()), ("c".into(), "c11".into())]);
        let child = json!({"data":{"block":{"block_type":32,"parent_id":"table","children":["p10"]}}});
        assert!(check_cell_identity(&child, "table", &expected.rows[1][0], &source).is_ok());
        full["data"]["block"]["table"]["property"]["merge_info"][0]["col_span"] = json!(1);
        assert!(inspect_table(&full, &parsed, &expected, &source, true).is_err());
    }

    #[test]
    fn deleting_table_rejects_unknown_wrapper_attributes() {
        let (source, _) = fixture();
        assert!(check_table_xml(&source.original_xml, "table").is_ok());
        let xml = source.original_xml.replace("<td id=\"c00\"", "<td comment-id=\"anchor\" id=\"c00\"");
        assert!(check_table_xml(&xml, "table").is_err());
        let xml = source.original_xml.replace("<tbody>", "<tbody><!-- comment -->");
        assert!(check_table_xml(&xml, "table").is_err());
    }
}
