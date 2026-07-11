# Mouse text selection

This document records the implemented selection model for contributors working
on read-only Git output.

Character selection works in the status view, log, commit details, and the `d`
diff view. Selectable content includes file paths, commit subjects, stash
messages, hunk headings, diff lines, commit messages, and diff statistics.

One drag supports two levels of precision. A drag within the starting row
selects characters. A drag across rows selects whole rows and reuses the status
view's action range. Dragging back to the starting row returns to character
selection. `y` and `Cmd-C` copy a character selection before considering a row
selection or the value at the cursor.

`Esc`, keyboard movement, or a plain click clears the selection. When a row is
selected, the first click clears it and the next click performs the row action.
A single click otherwise places the cursor or toggles a fold. A double-click
opens the row.

The remaining gaps need either a selection model with several targets per row
or support in another rendering surface:

- Short commit hashes and log dates. These rows contain several separate text
  segments, but only the primary subject or path supports character selection.
  `y` copies the full hash from a short-hash row.
- Section headers (navigation chrome) and ref chips (styled pills).
- The `$` command-log and blame pagers (their own render paths).

## Goal

Plain left-drag selection should behave naturally in read-only views:

- Within one line, select characters from a diff, subject, or message.
- Across lines, select whole rows so stage, unstage, and copy can act on the
  existing row range.

The drag span chooses the precision, so no modifier is needed.

Out of scope:

- **Cross-line *character* selection.** Multi-line selection is intentionally
  line-wise (that's what stage-a-range wants, and it reuses machinery we already
  have) — so there is no cross-row char layer to build.
- **The commit editor** (an `InputState` field with native char selection).
- **The `$` command-log and blame pagers** (non-interactive; later).

Today only line/unit copy exists: `y` / Cmd-C / right-click-Copy yank the visual
(line-wise) selection or the line at point. This adds sub-line granularity to the
same drag.

## The core obstacle (and the fix)

A text-bearing row renders its text as a **flex of separate colored `div`s** —
`diff_line_body` builds one `div().child(text)` per syntax-highlight span. There's
no shaped-text layout to hit-test ("what character is under this pixel?") and no
way to paint a selection background over a byte range. gpui's `StyledText`/
`TextLayout` aren't used anywhere in the app yet.

The fix is to render each *selectable* row's text as a single `gpui::StyledText`:

- **Colors survive.** `with_default_highlights(default_style, runs)` takes
  `(Range<usize>, HighlightStyle)` runs, so the per-span colors become highlight
  runs over one string — same look, one element.
- **Pixel → offset.** `StyledText::layout()` returns a `TextLayout` (cloneable
  shared handle). After paint, `TextLayout::index_for_position(point) ->
  Result<usize, usize>` maps a mouse position to a byte offset (`Ok` inside a
  glyph, `Err(nearest)` past the end); `position_for_index` is the inverse.
- **Selection paint.** Overlay the char selection as one more highlight run with a
  `background_color`, composed on top of the color runs.

This is a targeted rendering migration of text rows to `StyledText` plus a small
state machine — no new editor, no gpui-component changes. The cross-line case
needs none of it (it uses the existing whole-row highlight).

## Selection model

Two representations, chosen by how far the drag has gone from its anchor row:

```
// New: a character range within one row.
struct CharSelection { row: usize, anchor: usize, cursor: usize } // byte offsets

// Existing: line-wise region (Selection::visual), whole rows
// min(anchor_row, row)..=max(...).
```

The view holds `Option<CharSelection>` alongside the existing line-wise
`Selection`. Invariant: at most one is active. A drag confined to the anchor row
drives `CharSelection`; a drag that has touched another row drives the line-wise
`visual` (and clears `CharSelection`).

Both are cleared on a plain click elsewhere or Esc (consistent with the prefix/
which-key dismissal already added).

## Gesture: one drag, two granularities

Extend the existing row mouse handlers (which today set `drag_anchor`/`visual`):

- **mouse-down** on a row's text: record the anchor row and — via that row's
  `TextLayout::index_for_position` — the anchor **byte offset**. Keep setting the
  cursor row as today (a bare click still just selects/positions). Start with no
  active selection (an empty char range).
- **mouse-move** (button held):
  - **same row as anchor** → char mode: set `CharSelection { row, anchor,
    cursor = index_at_point }`, clear line-wise `visual`.
  - **different row** → line mode: set `visual = anchor_row` (the existing
    behavior), clear `CharSelection`.
  - The two transitions compose both ways, so dragging down into line-mode and
    back up to the origin row returns to char-mode automatically.
- **mouse-up**: finalize. A non-empty char range copies (below); a line-wise
  region behaves exactly as today.

`shift-click` keeps its current line-wise extend behavior.

## Rendering and wiring

- **A `selectable_text` helper** that, given a row's `(text, color-runs, row_ix)`,
  returns a `StyledText` with the color runs plus the char-selection background
  run (when this row owns the active `CharSelection`), and exposes its `layout()`
  handle to the handlers.
- **Layout handles.** `TextLayout` is populated only after prepaint. The
  mouse-down/move closures capture the row's cloned `TextLayout` directly (it's an
  `Rc`), so hit-testing calls `index_for_position` on the right row without a
  lookup. Guard against a not-yet-laid-out layout (early mouse event → no-op).
- **Handlers** stay on the row container `div` that already has
  `on_mouse_down`/`on_mouse_move`/`on_mouse_up`; the char/line branch is decided
  by comparing the current row to the anchor row (no modifier check).
- **Selectable rows:** the text-bearing kinds include diff `Line`, `Hunk`,
  `Message`, `Detail`, `StatLine`, and `Note` rows plus commit and log subjects.
  Structural rows such as section headers and file chips can opt in later.

## Copy

- **Char range:** `y` or Cmd-C copies the selected slice through
  `copy_to_clipboard`. Mouse-up completes the selection but does not copy it. A
  character selection takes precedence over line and row copy.
- **Line-wise region:** unchanged — the existing yank of the visual selection.

## Testing

- Headless unit tests cover range normalization, row-bound clamping, text
  extraction, and character-versus-line selection.
- The pixel-to-offset path needs a painted layout. Use `scripts/dbg.sh` to drag
  within one diff line, across several lines, and back to the anchor row. Check
  the selection and copy result at each stage.

## Implementation history

1. Migrated diff and message rows to `StyledText` with color runs.
2. Added `CharSelection`, drag precision switching, and selection highlights.
3. Added explicit copy with character-selection precedence.
4. Extended selection to the remaining text-bearing rows.
5. Left the command-log and blame pagers for a separate rendering pass.

## Constraints

- `StyledText` runs on a hot rendering path, but `uniform_list` creates only
  visible rows.
- `index_for_position` requires a measured layout. Events on an unpainted row
  must remain no-ops.
- Strict row equality determines the selection precision. Movement within the
  anchor row stays character-wise; entering another row becomes line-wise.
- `index_for_position` returns the nearest position beyond a line's end. Treat
  that value as the end offset so dragging right selects to the end of the line.
- The character-selection background must take precedence over row hover.
