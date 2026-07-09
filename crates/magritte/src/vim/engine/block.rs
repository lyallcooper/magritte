//! The blockwise (Visual Block) side of the engine: the rectangle geometry
//! between the anchor and the cursor, and the operators over it (`d`/`c`/`y`,
//! `D`/`C`, `I`/`A`, `r`, `>`/`<`). `impl VimState` over the same state as
//! `engine.rs`.

use super::*;

/// A pending blockwise Insert session (`c`/`I`/`A` in Visual Block): the Esc
/// that ends it replays the typed text onto the block's other lines.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(super) struct BlockInsert {
    /// 1-based line numbers to replicate onto (the rows below the top one).
    pub(super) rows: Range<usize>,
    /// Char column to insert at; `None` appends at each line's end (`$`-`A`).
    pub(super) col: Option<usize>,
    /// Pad shorter lines with spaces to reach `col` (`A`); otherwise skip
    /// lines that don't reach it (`I`/`c`).
    pub(super) pad: bool,
    /// Column the cursor lands on after the replication (the block's left
    /// edge for `I`/`A`); `None` keeps the plain Esc step-left (`c`).
    pub(super) exit_col: Option<usize>,
}

/// A resolved blockwise selection: the per-line byte ranges plus the char
/// columns that define the rectangle.
pub(super) struct BlockGeom {
    /// One range per covered line, top to bottom (empty on lines the block
    /// overhangs).
    ranges: Vec<Range<usize>>,
    /// Leftmost and rightmost char columns, both inside the block.
    left: usize,
    right: usize,
    /// `$`-extended: the block runs to each line's end (`:help v_b_dollar`).
    to_eol: bool,
    /// 1-based line numbers covered (end exclusive).
    rows: Range<usize>,
}

impl BlockGeom {
    /// The register's column width: the rectangle's, or the widest segment
    /// for a `$` block.
    fn width(&self, text: &str) -> usize {
        if self.to_eol {
            self.ranges
                .iter()
                .map(|r| text[r.clone()].chars().count())
                .max()
                .unwrap_or(0)
        } else {
            self.right - self.left + 1
        }
    }
}

impl VimState {
    /// The blockwise selection's per-line byte ranges, top to bottom (for the
    /// overlay and the block operators): the char-column rectangle between
    /// the anchor and the cursor, clamped per line. A line shorter than the
    /// left column yields an empty range at its end. `None` outside Visual
    /// Block.
    pub(crate) fn block_ranges(&self, text: &str, cursor: usize) -> Option<Vec<Range<usize>>> {
        self.block_geom(text, cursor).map(|g| g.ranges)
    }

    pub(super) fn block_geom(&self, text: &str, cursor: usize) -> Option<BlockGeom> {
        if self.mode
            != (Mode::Visual {
                kind: VisualKind::Block,
            })
        {
            return None;
        }
        let a = clamp_normal(text, self.anchor);
        let c = clamp_normal(text, cursor);
        let to_eol = self.desired_col == Some(usize::MAX);
        // The cursor corner aims at the sticky column: on a line shorter
        // than curswant it sits one past the last char (probed: `C-v` at
        // column 5, `j` onto a 2-char line, `d` deletes columns 3-5).
        let actual = char_col(text, c);
        let line_chars = char_col(text, line_end(text, c));
        let ccol = self
            .desired_col
            .unwrap_or(actual)
            .min(line_chars)
            .max(actual);
        let acol = char_col(text, a);
        let (left, right) = (acol.min(ccol), acol.max(ccol));
        let (lo, hi) = (a.min(c), a.max(c));
        let mut ranges = Vec::new();
        let last_line = line_start(text, hi);
        let mut at = line_start(text, lo);
        loop {
            let start = offset_of_col(text, at, left);
            let end = if to_eol {
                line_end(text, at)
            } else {
                offset_of_col(text, at, right + 1)
            };
            ranges.push(start..end);
            if at >= last_line {
                break;
            }
            at = line_end(text, at) + 1;
        }
        Some(BlockGeom {
            ranges,
            left,
            right,
            to_eol,
            rows: line_of(text, lo)..line_of(text, hi) + 1,
        })
    }

    // --- Blockwise (Visual Block) operators --------------------------------

    pub(super) fn block_op(&mut self, text: &str, cursor: usize, op: Op) -> Vec<Action> {
        let Some(geom) = self.block_geom(text, cursor) else {
            return self.beep();
        };
        self.mode = Mode::Normal;
        self.take_count();
        self.block_apply(text, geom, op)
    }

    /// Blockwise `D`/`C`: the block extends to each line's end first.
    pub(super) fn block_op_eol(&mut self, text: &str, cursor: usize, op: Op) -> Vec<Action> {
        self.desired_col = Some(usize::MAX);
        self.block_op(text, cursor, op)
    }

    /// Apply an operator to a resolved block: one edit spanning the covered
    /// lines, the segments into the register as a block, the cursor at the
    /// block's top-left.
    fn block_apply(&mut self, text: &str, geom: BlockGeom, op: Op) -> Vec<Action> {
        // The cursor lands at the top-left; the sticky column re-anchors
        // there (probed: a yank isn't an edit, so handle_key won't reset it).
        self.desired_col = None;
        let segments: Vec<&str> = geom.ranges.iter().map(|r| &text[r.clone()]).collect();
        let reg_text = segments.join("\n");
        self.register = Some(Register {
            text: reg_text.clone(),
            kind: RegKind::Block {
                width: geom.width(text),
            },
        });
        let top_left = geom.ranges[0].start;
        if op == Op::Yank {
            return vec![
                Action::Yank(reg_text),
                Action::MoveCursor(clamp_normal(text, top_left)),
            ];
        }
        let (range, out) = block_splice(text, &geom.ranges, |_| String::new());
        let changed = out != text[range.clone()];
        let mut actions = vec![Action::Yank(reg_text)];
        if op == Op::Change {
            // Insert at the top-left; the Esc ending the session replicates
            // the typed text onto the block's other lines.
            self.block_insert = Some(BlockInsert {
                rows: geom.rows.start + 1..geom.rows.end,
                col: Some(geom.left),
                pad: false,
                exit_col: None,
            });
            self.mode = Mode::Insert;
            if changed {
                actions.push(Action::Edit(EditOp {
                    range,
                    text: out,
                    cursor: top_left,
                }));
            } else {
                actions.push(Action::MoveCursor(top_left.min(text.len())));
            }
        } else if changed {
            let post = splice(text, &range, &out);
            actions.push(Action::Edit(EditOp {
                range,
                text: out,
                cursor: clamp_normal_after(&post, top_left),
            }));
        } else {
            // The block overhangs every line: nothing to delete.
            actions.push(Action::MoveCursor(clamp_normal(text, top_left)));
        }
        actions
    }

    /// Blockwise `>`/`<`: shift at the block's *left edge*, not the line
    /// start (`:help v_b_>`) — indent inserts `count` [`STEP`]s at the left
    /// column of each covered line (empty lines skipped, short lines get the
    /// blanks at their end); dedent strips up to that much whitespace there.
    /// The cursor parks at the block's left column on the top line (probed).
    pub(super) fn block_shift(&mut self, text: &str, cursor: usize, dedent: bool) -> Vec<Action> {
        let Some(geom) = self.block_geom(text, cursor) else {
            return self.beep();
        };
        self.mode = Mode::Normal;
        let count = self.take_count().max(1);
        let start = line_start(text, geom.ranges[0].start);
        let end = line_end(text, geom.ranges.last().unwrap().start);
        let mut out = String::with_capacity(end - start + STEP.len() * count * geom.ranges.len());
        for (i, seg) in geom.ranges.iter().enumerate() {
            if i > 0 {
                out.push('\n');
            }
            let (ls, le) = (line_start(text, seg.start), line_end(text, seg.start));
            if dedent {
                out.push_str(&text[ls..seg.start]);
                let mut rest = &text[seg.start..le];
                for _ in 0..count {
                    rest = rest
                        .strip_prefix(STEP)
                        .or_else(|| rest.strip_prefix('\t'))
                        .or_else(|| rest.strip_prefix(' '))
                        .unwrap_or(rest);
                }
                out.push_str(rest);
            } else if text[ls..le].trim().is_empty() {
                out.push_str(&text[ls..le]);
            } else {
                out.push_str(&text[ls..seg.start]);
                for _ in 0..count {
                    out.push_str(STEP);
                }
                out.push_str(&text[seg.start..le]);
            }
        }
        let post = splice(text, &(start..end), &out);
        let pos = clamp_normal(&post, offset_of_col(&post, start, geom.left));
        if out == text[start..end] {
            return vec![Action::MoveCursor(pos)];
        }
        vec![Action::Edit(EditOp {
            range: start..end,
            text: out,
            cursor: pos,
        })]
    }

    /// Blockwise `r`: every char inside the rectangle becomes `c`; the
    /// cursor lands on the block's top-left.
    pub(super) fn block_replace(&mut self, text: &str, geom: BlockGeom, c: char) -> Vec<Action> {
        let (range, out) = block_splice(text, &geom.ranges, |seg| seg.chars().map(|_| c).collect());
        let top_left = geom.ranges[0].start;
        if out == text[range.clone()] {
            return vec![Action::MoveCursor(clamp_normal(text, top_left))];
        }
        let post = splice(text, &range, &out);
        vec![Action::Edit(EditOp {
            range,
            text: out,
            cursor: clamp_normal_after(&post, top_left),
        })]
    }

    /// Blockwise `I`/`A`: Insert at the block's left edge, or just past its
    /// right one (`$`-blocks append at each line's end); the Esc closing the
    /// session replicates the typed text onto the other lines.
    pub(super) fn block_insert_cmd(
        &mut self,
        text: &str,
        cursor: usize,
        append: bool,
    ) -> Vec<Action> {
        let Some(geom) = self.block_geom(text, cursor) else {
            return self.beep();
        };
        self.mode = Mode::Insert;
        self.take_count();
        let rows = geom.rows.start + 1..geom.rows.end;
        let top = geom.ranges[0].start;
        if !append {
            // The top line always reaches the left column (each corner sits
            // on its own line); shorter lines below are skipped at Esc.
            self.block_insert = Some(BlockInsert {
                rows,
                col: Some(geom.left),
                pad: false,
                exit_col: Some(geom.left),
            });
            return vec![Action::MoveCursor(top.min(text.len()))];
        }
        if geom.to_eol {
            self.block_insert = Some(BlockInsert {
                rows,
                col: None,
                pad: false,
                exit_col: Some(geom.left),
            });
            return vec![Action::MoveCursor(line_end(text, top))];
        }
        let col = geom.right + 1;
        self.block_insert = Some(BlockInsert {
            rows,
            col: Some(col),
            pad: true,
            exit_col: Some(geom.left),
        });
        let le = line_end(text, top);
        let chars = char_col(text, le);
        if chars < col {
            // Pad the top line out to the block's right edge first (the
            // lines below pad at Esc).
            let pad: String = " ".repeat(col - chars);
            return vec![Action::Edit(EditOp {
                range: le..le,
                text: pad.clone(),
                cursor: le + pad.len(),
            })];
        }
        vec![Action::MoveCursor(offset_of_col(text, top, col))]
    }
}

/// Rebuild a block's covered line span with each segment mapped through `f`,
/// returning the span and its replacement.
fn block_splice(
    text: &str,
    ranges: &[Range<usize>],
    f: impl Fn(&str) -> String,
) -> (Range<usize>, String) {
    let start = line_start(text, ranges[0].start);
    let end = line_end(text, ranges.last().unwrap().start);
    let mut out = String::with_capacity(end - start);
    for (i, seg) in ranges.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push_str(&text[line_start(text, seg.start)..seg.start]);
        out.push_str(&f(&text[seg.clone()]));
        out.push_str(&text[seg.end..line_end(text, seg.start)]);
    }
    (start..end, out)
}

/// The replication edit closing a blockwise insert session: `typed` inserted
/// on each of `bi.rows` at `bi.col` (space-padded or skipped on lines that
/// don't reach it, per `bi.pad`), or appended at each line's end for
/// `col: None`. `None` when nothing changes.
pub(super) fn block_replicate(
    text: &str,
    bi: &BlockInsert,
    typed: &str,
) -> Option<(Range<usize>, String)> {
    let first = bi.rows.start;
    let last = (bi.rows.end.saturating_sub(1)).min(line_count(text));
    if first > last {
        return None;
    }
    let start = line_offset(text, first);
    let end = line_end(text, line_offset(text, last));
    let mut out = String::with_capacity(end - start + typed.len() * (last - first + 1));
    for (i, line) in text[start..end].split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
        }
        match bi.col {
            None => {
                out.push_str(line);
                out.push_str(typed);
            }
            Some(col) => {
                let chars = line.chars().count();
                if chars >= col {
                    let at = line.char_indices().nth(col).map_or(line.len(), |(b, _)| b);
                    out.push_str(&line[..at]);
                    out.push_str(typed);
                    out.push_str(&line[at..]);
                } else if bi.pad {
                    out.push_str(line);
                    out.extend(std::iter::repeat_n(' ', col - chars));
                    out.push_str(typed);
                } else {
                    out.push_str(line);
                }
            }
        }
    }
    (out != text[start..end]).then_some((start..end, out))
}
