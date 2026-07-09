//! The plain edit builders: character deletes/replaces, case toggling,
//! line shifting and joining, and the register puts. `impl VimState` over
//! the same state as `engine.rs`.

use super::*;

impl VimState {
    pub(super) fn delete_chars_forward(
        &mut self,
        text: &str,
        cursor: usize,
        count: usize,
    ) -> Vec<Action> {
        let end = line_end(text, cursor);
        let mut to = cursor;
        for _ in 0..count {
            if to >= end {
                break;
            }
            to = next_char(text, to);
        }
        if to == cursor {
            return self.beep();
        }
        let yanked = text[cursor..to].to_string();
        self.register = Some(Register::charwise(yanked.clone()));
        let post = splice(text, &(cursor..to), "");
        vec![
            Action::Yank(yanked),
            Action::Edit(EditOp {
                range: cursor..to,
                text: String::new(),
                cursor: clamp_normal_after(&post, cursor),
            }),
        ]
    }

    pub(super) fn delete_chars_backward(
        &mut self,
        text: &str,
        cursor: usize,
        count: usize,
    ) -> Vec<Action> {
        let start = line_start(text, cursor);
        let mut from = cursor;
        for _ in 0..count {
            if from <= start {
                break;
            }
            from = prev_char(text, from);
        }
        if from == cursor {
            return self.beep();
        }
        let yanked = text[from..cursor].to_string();
        self.register = Some(Register::charwise(yanked.clone()));
        vec![
            Action::Yank(yanked),
            Action::Edit(EditOp {
                range: from..cursor,
                text: String::new(),
                cursor: from,
            }),
        ]
    }

    pub(super) fn replace_chars(
        &mut self,
        text: &str,
        cursor: usize,
        c: char,
        count: usize,
    ) -> Vec<Action> {
        // `r` fails when there aren't `count` chars left on the line.
        let end = line_end(text, cursor);
        let mut to = cursor;
        for _ in 0..count {
            if to >= end {
                return self.beep();
            }
            to = next_char(text, to);
        }
        // `{count}r<CR>` replaces all `count` chars with a single line break,
        // cursor at the start of the new line (`:help r`).
        let (replacement, cursor_after) = if c == '\n' {
            ("\n".to_string(), cursor + 1)
        } else {
            let replacement: String = std::iter::repeat_n(c, count).collect();
            let after = cursor + replacement.len() - c.len_utf8();
            (replacement, after)
        };
        vec![Action::Edit(EditOp {
            range: cursor..to,
            text: replacement,
            cursor: cursor_after,
        })]
    }

    pub(super) fn toggle_case(&mut self, text: &str, cursor: usize, count: usize) -> Vec<Action> {
        let end = line_end(text, cursor);
        let mut to = cursor;
        for _ in 0..count {
            if to >= end {
                break;
            }
            to = next_char(text, to);
        }
        if to == cursor {
            return self.beep();
        }
        let toggled: String = text[cursor..to]
            .chars()
            .flat_map(toggle_char_case)
            .collect();
        let post = splice(text, &(cursor..to), &toggled);
        let after = clamp_normal(&post, cursor + toggled.len());
        vec![Action::Edit(EditOp {
            range: cursor..to,
            text: toggled,
            cursor: after,
        })]
    }

    /// `>`/`<`: shift the whole lines covered by `range` by one indent
    /// [`STEP`]. Indent skips blank lines, like Vim; dedent strips up to one
    /// step of spaces or a tab.
    pub(super) fn shift_lines(
        &mut self,
        text: &str,
        range: Range<usize>,
        dedent: bool,
    ) -> Vec<Action> {
        let start = line_start(text, range.start.min(text.len()));
        let last = prev_char(text, range.end.min(text.len())).max(range.start);
        let end = line_end(text, last.max(start));
        let mut shifted = String::with_capacity(end - start + STEP.len() * 4);
        for (i, line) in text[start..end].split('\n').enumerate() {
            if i > 0 {
                shifted.push('\n');
            }
            if dedent {
                let trimmed = line
                    .strip_prefix(STEP)
                    .or_else(|| line.strip_prefix('\t'))
                    .or_else(|| line.strip_prefix(' '))
                    .unwrap_or(line);
                shifted.push_str(trimmed);
            } else if line.trim().is_empty() {
                shifted.push_str(line);
            } else {
                shifted.push_str(STEP);
                shifted.push_str(line);
            }
        }
        if shifted == text[start..end] {
            // Nothing to dedent: park on the first non-blank, like Vim's
            // silent `<<` on an unindented line.
            return vec![Action::MoveCursor(first_non_blank(text, start))];
        }
        let post = splice(text, &(start..end), &shifted);
        let cursor = first_non_blank(&post, start.min(post.len()));
        vec![Action::Edit(EditOp {
            range: start..end,
            text: shifted,
            cursor,
        })]
    }

    /// Join the lines covered by `range` (at least the cursor's line and the
    /// next): each newline (plus the following line's indent) becomes one
    /// space, unless the text before it already ends in whitespace.
    pub(super) fn join_range(&mut self, text: &str, range: Range<usize>) -> Vec<Action> {
        let start = line_start(text, range.start);
        let mut end = line_end(text, range.end.max(range.start));
        // A charwise range within one line still joins it with the next.
        if line_end(text, start) == end && end < text.len() {
            end = line_end(text, next_char(text, end));
        }
        if line_end(text, start) == end {
            return self.beep(); // nothing to join with
        }
        let mut joined = String::new();
        let mut cursor_after = None;
        let mut rest = start;
        while rest < end {
            let le = line_end(text, rest);
            joined.push_str(&text[rest..le]);
            if le >= end {
                break;
            }
            // Skip the newline and the next line's leading blanks.
            let mut next = le + 1;
            while matches!(char_at(text, next), Some(' ' | '\t')) {
                next = next_char(text, next);
            }
            cursor_after = Some(start + joined.len());
            // One space at the seam — unless the left side already ends in
            // whitespace or there is nothing to join on the right (a blank or
            // empty line at the end contributes nothing, like Vim).
            if !joined.is_empty()
                && !joined.ends_with([' ', '\t'])
                && char_at(text, next).is_some_and(|c| c != '\n')
            {
                joined.push(' ');
            }
            rest = next;
        }
        let cursor = cursor_after.unwrap_or(start);
        let post = splice(text, &(start..end), &joined);
        vec![Action::Edit(EditOp {
            range: start..end,
            text: joined,
            cursor: clamp_normal(&post, cursor),
        })]
    }

    /// Visual `p`/`P`: the selection is replaced by the register, honoring
    /// each side's linewise-ness, and the replaced text takes the register's
    /// place (Vim's swap idiom).
    pub(super) fn visual_put(
        &mut self,
        text: &str,
        range: Range<usize>,
        sel_linewise: bool,
        reg: Register,
    ) -> Vec<Action> {
        let cut = text[range.clone()].to_string();
        let sel_had_newline = cut.ends_with('\n');
        let cut = if sel_linewise && !sel_had_newline {
            format!("{cut}\n")
        } else {
            cut
        };
        self.register = Some(Register {
            text: cut.clone(),
            kind: if sel_linewise {
                RegKind::Line
            } else {
                RegKind::Char
            },
        });
        // A linewise register pastes as whole lines (splitting a charwise
        // selection's line); a charwise register over a linewise selection
        // becomes its own line. Match the selection's trailing-newline
        // presence so an EOF paste doesn't grow a stray empty line.
        let reg_linewise = reg.kind == RegKind::Line;
        let mut pasted = match (reg_linewise, sel_linewise) {
            (false, false) => reg.text.clone(),
            (true, false) => format!("\n{}", reg.text),
            (false, true) => format!("{}\n", reg.text),
            (true, true) => reg.text.clone(),
        };
        if sel_linewise && !sel_had_newline {
            // A linewise selection ending at EOF has no trailing newline to
            // give back; don't grow a stray empty last line.
            if let Some(s) = pasted.strip_suffix('\n') {
                pasted = s.to_string();
            }
        }
        let post = splice(text, &range, &pasted);
        let cursor = if reg_linewise || sel_linewise {
            // First non-blank of the first pasted line.
            let first = range.start + usize::from(pasted.starts_with('\n'));
            first_non_blank(&post, first.min(post.len()))
        } else {
            // Charwise over charwise: the last pasted char.
            let end = range.start + pasted.len();
            clamp_normal(
                &post,
                if end > range.start {
                    prev_char(&post, end)
                } else {
                    range.start
                },
            )
        };
        vec![
            Action::Yank(cut),
            Action::Edit(EditOp {
                range,
                text: pasted,
                cursor,
            }),
        ]
    }

    pub(super) fn put(
        &mut self,
        text: &str,
        cursor: usize,
        count: usize,
        after: bool,
    ) -> Vec<Action> {
        let Some(reg) = self.register.clone() else {
            return self.beep();
        };
        // Cap the expansion so an absurd count can't OOM the app.
        const PUT_LIMIT: usize = 4 << 20;
        let count = count.min((PUT_LIMIT / reg.text.len().max(1)).max(1));
        if let RegKind::Block { width } = reg.kind {
            return self.put_block(text, cursor, &reg.text, width, count, after);
        }
        let body = reg.text.repeat(count);
        if reg.kind == RegKind::Line {
            let at = if after {
                let le = line_end(text, cursor);
                (le + usize::from(le < text.len())).min(text.len())
            } else {
                line_start(text, cursor)
            };
            // Pasting after the last line (no trailing newline): lead with a
            // newline and drop the register's trailing one.
            let (range, pasted) =
                if after && at == text.len() && !text.ends_with('\n') && !text.is_empty() {
                    (
                        at..at,
                        format!("\n{}", body.strip_suffix('\n').unwrap_or(&body)),
                    )
                } else {
                    (at..at, body)
                };
            let first_line = at + usize::from(pasted.starts_with('\n'));
            let post = splice(text, &range, &pasted);
            let cursor = first_non_blank(&post, first_line.min(post.len()));
            vec![Action::Edit(EditOp {
                range,
                text: pasted,
                cursor,
            })]
        } else {
            let at = if after && char_at(text, cursor).is_some_and(|c| c != '\n') {
                next_char(text, cursor)
            } else {
                cursor
            };
            let post = splice(text, &(at..at), &body);
            // Cursor on the last char of the pasted text.
            let cursor = clamp_normal(&post, prev_char(&post, at + body.len()).max(at));
            vec![Action::Edit(EditOp {
                range: at..at,
                text: body,
                cursor,
            })]
        }
    }

    /// Paste a blockwise register: each segment lands at the same char
    /// column on successive lines (`p` one column right of the cursor, like
    /// charwise `p`; `P` at it), with shorter lines space-padded out to the
    /// column, missing lines created, segments padded to the block width
    /// when text follows them, and a count repeating each segment
    /// horizontally. Cursor at the paste's top-left. (All probed.)
    fn put_block(
        &mut self,
        text: &str,
        cursor: usize,
        reg: &str,
        width: usize,
        count: usize,
        after: bool,
    ) -> Vec<Action> {
        let at = if after && char_at(text, cursor).is_some_and(|c| c != '\n') {
            next_char(text, cursor)
        } else {
            cursor
        };
        let col = char_col(text, at);
        let width = width.saturating_mul(count);
        let start = line_start(text, at);
        let mut out = String::new();
        let mut end = start;
        let mut line = Some(start);
        for (i, seg) in reg.split('\n').enumerate() {
            let seg = seg.repeat(count);
            if i > 0 {
                out.push('\n');
            }
            let Some(at_line) = line else {
                // Past the last line: create one, padded out to the column.
                if !seg.is_empty() {
                    out.extend(std::iter::repeat_n(' ', col));
                    out.push_str(&seg);
                }
                continue;
            };
            let le = line_end(text, at_line);
            let chars = char_col(text, le);
            if chars >= col {
                let ins = offset_of_col(text, at_line, col);
                out.push_str(&text[at_line..ins]);
                out.push_str(&seg);
                if ins < le {
                    // Text follows: pad the segment to the block width so
                    // the columns stay aligned.
                    out.extend(std::iter::repeat_n(
                        ' ',
                        width.saturating_sub(seg.chars().count()),
                    ));
                }
                out.push_str(&text[ins..le]);
            } else {
                out.push_str(&text[at_line..le]);
                if !seg.is_empty() {
                    out.extend(std::iter::repeat_n(' ', col - chars));
                    out.push_str(&seg);
                }
            }
            end = le;
            line = (le < text.len()).then(|| le + 1);
        }
        let post = splice(text, &(start..end), &out);
        vec![Action::Edit(EditOp {
            range: start..end,
            text: out,
            cursor: clamp_normal(&post, at),
        })]
    }
}
