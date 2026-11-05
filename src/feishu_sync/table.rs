//! Identity-based, request-free planning for native Feishu table edits.
use std::collections::{HashMap, HashSet};

use anyhow::{bail, Result};
use crate::model::{Block, Document, Table, TableCell};

#[derive(Debug)]
pub(super) enum TableEdit<'a> {
    InsertRow { index: usize },
    InsertColumn { index: usize },
    DeleteRows { start: usize, end: usize },
    DeleteColumns { start: usize, end: usize },
    Merge { row_start: usize, row_end: usize, column_start: usize, column_end: usize },
    Unmerge { row: usize, column: usize },
    ColumnWidth { column: usize, width: i32 },
    HeaderRow { enabled: bool },
    HeaderColumn { enabled: bool },
    /// Locate the old cell via the cloud table structure, not by synthesizing an ID.
    /// For an inserted or split cell, locate its new cloud cell after structural edits.
    Cell { old: Option<&'a TableCell>, new: &'a TableCell },
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum Key<'a> { Source(&'a str), Local(&'a str) }

fn key(cell: &TableCell) -> Option<Key<'_>> {
    cell.source_id.as_deref().map(Key::Source).or_else(|| cell.sync_id.as_deref().map(Key::Local))
}

// DocxXML carries persistent block identity and content, but excludes the
// process-local BlockId assigned on each parse.
fn blocks_equal(a: &[Block], b: &[Block]) -> bool {
    let document = |blocks: &[Block]| Document { blocks: blocks.to_vec(), ..Document::default() };
    crate::docxml::serialize(&document(a)) == crate::docxml::serialize(&document(b))
}

fn tables_equal(a: &Table, b: &Table) -> bool {
    a.col_widths == b.col_widths && a.header_rows == b.header_rows && a.header_col == b.header_col
        && a.rows.len() == b.rows.len()
        && a.rows.iter().zip(&b.rows).all(|(ar, br)| {
            ar.len() == br.len() && ar.iter().zip(br).all(|(ac, bc)| {
                ac.source_id == bc.source_id && ac.sync_id == bc.sync_id
                    && ac.bg == bc.bg && ac.valign == bc.valign
                    && ac.rowspan == bc.rowspan && ac.colspan == bc.colspan
                    && blocks_equal(&ac.blocks, &bc.blocks)
            })
        })
}

fn index(table: &Table) -> Result<HashMap<Key<'_>, (usize, usize)>> {
    if table.rows.is_empty() || table.ncols() == 0 || table.rows.iter().any(|r| r.len() != table.ncols()) {
        bail!("表格不是非空矩形，无法安全同步");
    }
    let mut occupied = vec![vec![false; table.ncols()]; table.rows.len()];
    let mut ids = HashMap::new();
    for (r, c, cell) in table.cells() {
        let (rs, cs) = (cell.rowspan as usize, cell.colspan as usize);
        if rs == 0 || cs == 0 || rs > table.rows.len() - r || cs > table.ncols() - c {
            bail!("表格单元格跨度无效");
        }
        for row in occupied.iter_mut().skip(r).take(rs) {
            for slot in row.iter_mut().skip(c).take(cs) {
                if *slot { bail!("表格合并区域重叠"); }
                *slot = true;
            }
        }
        if let Some(k) = key(cell) {
            let s = match k { Key::Source(s) | Key::Local(s) => s };
            if s.is_empty() || ids.insert(k, (r, c)).is_some() { bail!("表格单元格身份为空或重复"); }
        }
    }
    if occupied.iter().flatten().any(|x| !x) { bail!("表格包含孤立的合并占位格"); }
    Ok(ids)
}

// A line without a surviving identity is not evidence of its position. Reject
// replacement/reordering rather than guessing from equal contents or indices.
fn axis(old_len: usize, new_len: usize, pairs: &[(usize, usize)]) -> Result<(Vec<Option<usize>>, Vec<usize>)> {
    let mut old_to_new = vec![None; old_len];
    let mut new_to_old = vec![None; new_len];
    for &(a, b) in pairs {
        if old_to_new[a].is_some_and(|v| v != b) || new_to_old[b].is_some_and(|v| v != a) {
            bail!("表格行列发生移动或无法确定对应关系");
        }
        old_to_new[a] = Some(b);
        new_to_old[b] = Some(a);
    }
    let ordered: Vec<_> = old_to_new.iter().flatten().copied().collect();
    if ordered.windows(2).any(|w| w[0] >= w[1]) { bail!("飞书不支持安全移动表格行列"); }
    Ok((new_to_old, old_to_new.iter().enumerate().filter_map(|(i, x)| x.is_none().then_some(i)).collect()))
}

pub(super) fn plan_table<'a>(old: &'a Table, new: &'a Table) -> Result<Vec<TableEdit<'a>>> {
    let old_ids = index(old)?;
    let new_ids = index(new)?;
    if old.header_rows > 1 || new.header_rows > 1 { bail!("飞书只支持首行表头"); }
    if tables_equal(old, new) { return Ok(Vec::new()); }
    if old.cells().any(|(_, _, cell)| key(cell).is_none()) {
        bail!("旧表格单元格缺少持久身份，不能根据坐标猜测云端身份");
    }
    if new.cells().any(|(_, _, cell)| key(cell).is_none()) {
        bail!("新表格单元格缺少持久同步身份");
    }
    let mut pairs = Vec::new();
    for (k, &(r, c)) in &old_ids {
        if let Some(&(nr, nc)) = new_ids.get(k) { pairs.push((r, c, nr, nc)); }
    }
    let same_grid = old.rows.len() == new.rows.len() && old.ncols() == new.ncols()
        && pairs.iter().all(|&(r, c, nr, nc)| r == nr && c == nc);
    let spans_changed = same_grid && (0..old.rows.len()).any(|r| (0..old.ncols()).any(|c| {
        let a = &old.rows[r][c]; let b = &new.rows[r][c];
        a.rowspan != b.rowspan || a.colspan != b.colspan
    }));
    let mut edits = Vec::new();
    let merged = old.cells().any(|(_, _, c)| c.rowspan > 1 || c.colspan > 1)
        || new.cells().any(|(_, _, c)| c.rowspan > 1 || c.colspan > 1);
    let (rows, cols) = if spans_changed || merged {
        if !same_grid || (!spans_changed && (pairs.len() != old_ids.len() || pairs.len() != new_ids.len())) {
            bail!("合并区域与行列增删同时变化，无法安全规划");
        }
        ((0..new.rows.len()).map(Some).collect(), (0..new.ncols()).map(Some).collect())
    } else {
        let (rows, removed_rows) = axis(old.rows.len(), new.rows.len(), &pairs.iter().map(|&(r, _, nr, _)| (r, nr)).collect::<Vec<_>>())?;
        let (cols, removed_cols) = axis(old.ncols(), new.ncols(), &pairs.iter().map(|&(_, c, _, nc)| (c, nc)).collect::<Vec<_>>())?;
        if rows.iter().all(Option::is_none) || cols.iter().all(Option::is_none) { bail!("没有可保留的表格行或列身份"); }
        for (removed, is_row) in [(&removed_rows, true), (&removed_cols, false)] {
            let mut end = removed.len();
            while end > 0 {
                let last = removed[end - 1];
                let mut start = end - 1;
                while start > 0 && removed[start - 1] + 1 == removed[start] { start -= 1; }
                if is_row { edits.push(TableEdit::DeleteRows { start: removed[start], end: last + 1 }); }
                else { edits.push(TableEdit::DeleteColumns { start: removed[start], end: last + 1 }); }
                end = start;
            }
        }
        for (r, x) in rows.iter().enumerate() { if x.is_none() { edits.push(TableEdit::InsertRow { index: r }); } }
        for (c, x) in cols.iter().enumerate() { if x.is_none() { edits.push(TableEdit::InsertColumn { index: c }); } }
        (rows, cols)
    };
    if spans_changed {
        // Changed merged regions may not conceal non-empty old cells: the API
        // does not document how their child blocks are retained or ordered.
        for (r, c, a) in old.cells() {
            if a.rowspan > 1 || a.colspan > 1 {
                let b = &new.rows[r][c];
                if a.rowspan != b.rowspan || a.colspan != b.colspan {
                    edits.push(TableEdit::Unmerge { row: r, column: c });
                }
            }
        }
        for (r, c, b) in new.cells() {
            if b.rowspan > 1 || b.colspan > 1 {
                let a = &old.rows[r][c];
                if a.rowspan != b.rowspan || a.colspan != b.colspan {
                    for rr in r..r + b.rowspan as usize { for cc in c..c + b.colspan as usize {
                        if (rr, cc) != (r, c) {
                            let x = &old.rows[rr][cc];
                            if !x.is_covered() && x.blocks.iter().any(|block| !block.text.is_empty() || !block.children.is_empty() || !block.kind.has_text()) {
                                bail!("合并非空单元格可能改变云端子块，无法安全规划");
                            }
                        }
                    }}
                    edits.push(TableEdit::Merge { row_start: r, row_end: r + b.rowspan as usize, column_start: c, column_end: c + b.colspan as usize });
                }
            }
        }
    }
    for (c, &width) in new.col_widths.iter().enumerate() {
        let previous = cols[c].map(|x| old.col_widths[x]);
        if previous != Some(width) {
            if !width.is_finite() || width < 50.0 || width > i32::MAX as f32 || width.fract() != 0.0 { bail!("飞书列宽必须是至少 50 像素的整数"); }
            edits.push(TableEdit::ColumnWidth { column: c, width: width as i32 });
        }
    }
    if old.header_rows != new.header_rows { edits.push(TableEdit::HeaderRow { enabled: new.header_rows == 1 }); }
    if old.header_col != new.header_col { edits.push(TableEdit::HeaderColumn { enabled: new.header_col }); }
    let mut used = HashSet::new();
    for (r, c, b) in new.cells() {
        let matched = key(b).and_then(|k| old_ids.get(&k).copied());
        if let Some((or, oc)) = matched {
            if rows[r] != Some(or) || cols[c] != Some(oc) { bail!("表格单元格发生移动，飞书无法安全同步"); }
            used.insert((or, oc));
        }
        let a = matched.map(|(or, oc)| &old.rows[or][oc]);
        if let Some(a) = a {
            if a.bg != b.bg || a.valign != b.valign { bail!("飞书表格 API 不支持单元格背景或垂直对齐更新"); }
            if !blocks_equal(&a.blocks, &b.blocks) { edits.push(TableEdit::Cell { old: Some(a), new: b }); }
        } else {
            if b.bg.is_some() || b.valign.is_some() { bail!("飞书表格 API 不支持设置单元格背景或垂直对齐"); }
            edits.push(TableEdit::Cell { old: None, new: b });
        }
    }
    // Every surviving, uncovered old cell must retain its identity; exceptions
    // are cells deliberately covered by a newly merged region.
    for (r, c, a) in old.cells() {
        if used.contains(&(r, c)) { continue; }
        let nr = rows.iter().position(|x| *x == Some(r));
        let nc = cols.iter().position(|x| *x == Some(c));
        if let (Some(nr), Some(nc)) = (nr, nc) {
            if !spans_changed || !new.rows[nr][nc].is_covered() { bail!("既有表格单元格身份丢失或被替换，无法安全同步"); }
            if a.bg.is_some() || a.valign.is_some() { bail!("合并将丢失单元格属性"); }
        }
    }
    Ok(edits)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Block, CellRange};

    fn table(rows: usize, cols: usize) -> Table {
        let mut grid = Vec::new();
        for r in 0..rows {
            let mut row = Vec::new();
            for c in 0..cols {
                let mut cell = TableCell::empty();
                cell.source_id = Some(format!("cell-{r}-{c}"));
                row.push(cell);
            }
            grid.push(row);
        }
        Table { col_widths: vec![100.0; cols], header_rows: 0, header_col: false, rows: grid }
    }

    fn assign_new(table: &mut Table) {
        for (r, row) in table.rows.iter_mut().enumerate() {
            for (c, cell) in row.iter_mut().enumerate() {
                if !cell.is_covered() && key(cell).is_none() {
                    cell.sync_id = Some(format!("new-{r}-{c}"));
                }
            }
        }
    }

    #[test]
    fn inserts_and_deletes_preserve_existing_cells() {
        let old = table(3, 3);
        let mut new = old.clone();
        new.delete_rows(1..2);
        new.delete_cols(1..2);
        new.insert_row(1);
        new.insert_col(1);
        assign_new(&mut new);
        let edits = plan_table(&old, &new).unwrap();
        assert!(matches!(edits[0], TableEdit::DeleteRows { start: 1, end: 2 }));
        assert!(matches!(edits[1], TableEdit::DeleteColumns { start: 1, end: 2 }));
        assert!(matches!(edits[2], TableEdit::InsertRow { index: 1 }));
        assert!(matches!(edits[3], TableEdit::InsertColumn { index: 1 }));
        assert!(!edits.iter().any(|e| matches!(e, TableEdit::Cell { old: Some(_), .. })));
        assert_eq!(edits.iter().filter(|e| matches!(e, TableEdit::Cell { old: None, .. })).count(), 5);
    }

    #[test]
    fn merges_and_splits_empty_cells() {
        let old = table(2, 2);
        let mut merged = old.clone();
        merged.merge(CellRange::span((0, 0), (1, 1)));
        let edits = plan_table(&old, &merged).unwrap();
        assert!(edits.iter().any(|e| matches!(e, TableEdit::Merge { row_start: 0, row_end: 2, column_start: 0, column_end: 2 })));
        let mut split = merged.clone();
        split.unmerge(0, 0);
        assign_new(&mut split);
        let edits = plan_table(&merged, &split).unwrap();
        assert!(edits.iter().any(|e| matches!(e, TableEdit::Unmerge { row: 0, column: 0 })));
        assert_eq!(edits.iter().filter(|e| matches!(e, TableEdit::Cell { old: None, .. })).count(), 3);
    }

    #[test]
    fn reports_content_and_properties() {
        let old = table(2, 2);
        let mut new = old.clone();
        new.rows[1][1].blocks = vec![Block::text("changed")];
        new.col_widths[1] = 120.0;
        new.header_rows = 1;
        new.header_col = true;
        let edits = plan_table(&old, &new).unwrap();
        assert!(edits.iter().any(|e| matches!(e, TableEdit::Cell { old: Some(cell), .. } if cell.source_id.as_deref() == Some("cell-1-1"))));
        assert!(edits.iter().any(|e| matches!(e, TableEdit::ColumnWidth { column: 1, width: 120 })));
        assert!(edits.iter().any(|e| matches!(e, TableEdit::HeaderRow { enabled: true })));
        assert!(edits.iter().any(|e| matches!(e, TableEdit::HeaderColumn { enabled: true })));
    }

    #[test]
    fn rejects_identityless_baseline_and_nonempty_merge() {
        let mut old = table(2, 2);
        let mut new = old.clone();
        old.rows[1][0].source_id = None;
        new.rows[1][1].blocks = vec![Block::text("changed")];
        assert!(plan_table(&old, &new).is_err());
        let mut old = table(2, 2);
        old.rows[1][1].blocks = vec![Block::text("keep")];
        let mut new = old.clone();
        new.merge(CellRange::span((0, 0), (1, 1)));
        assert!(plan_table(&old, &new).is_err());
    }
}

#[cfg(test)]
mod parse_identity_tests {
    use super::*;
    use crate::model::{BlockKind, RichText};

    #[test]
    fn separately_parsed_tables_ignore_transient_ids() {
        let xml = "<title>T</title><table><tbody><tr><td id=\"a\"><p id=\"pa\">one</p></td><td id=\"b\"><p id=\"pb\">two</p></td></tr><tr><td id=\"c\"><p id=\"pc\">three</p></td><td id=\"d\"><p id=\"pd\">four</p></td></tr></tbody></table>";
        let old = crate::docxml::parse(xml).unwrap();
        let mut new = crate::docxml::parse(xml).unwrap();
        let BlockKind::Table(old_table) = &old.blocks[0].kind else { panic!() };
        let BlockKind::Table(new_table) = &mut new.blocks[0].kind else { panic!() };
        assert!(plan_table(old_table, new_table).unwrap().is_empty());
        new_table.rows[1][1].blocks[0].text = RichText::plain("changed");
        let edits = plan_table(old_table, new_table).unwrap();
        assert_eq!(edits.len(), 1);
        assert!(matches!(edits[0], TableEdit::Cell { old: Some(cell), .. } if cell.source_id.as_deref() == Some("d")));
    }
}
