# Mouse text selection: char-wise within a line, line-wise across lines

Status: proposal. A design/implementation plan, not a finished feature.

## Goal

Make plain left-drag selection do the intuitive thing in the read-only views:

- **Within a single line** — select a character range (drag to grab part of a
  diff line, a commit subject, a message line), then copy it.
- **Across lines** — once the drag leaves the starting row, fall back to the
  existing **line-wise** selection (whole rows), which already powers stage/
  unstage/copy over a range.

No modifier key: the *same* plain drag is char-wise while it stays on the anchor
row and line-wise the moment it spans rows (and reverts to char-wise if it comes
back to a single row). This is the resolution to the earlier "drag is already
line-select" gesture conflict — we don't add an Alt gesture, we let the span of
the drag pick the granularity.

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

## Rendering & wiring

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
- **Selectable rows:** the text-bearing kinds — diff `Line`, `Hunk`, `Message`,
  `Detail`, `StatLine`, `Note`, commit/log subjects. Structural rows (section
  headers, chip'd file rows) can opt in later; start with the diff/message text.

## Copy

- **Char range:** on mouse-up (or Cmd-C while a `CharSelection` is active), copy
  the row's text sliced by the range via the existing `copy_to_clipboard` (with
  its "Copied …" flash). A char selection takes precedence over line/unit copy.
- **Line-wise region:** unchanged — the existing yank of the visual selection.

## Testing

- **Unit tests** (headless) for the pure parts: char-range normalization,
  clamping to a row's text bounds, single-row text extraction, and the
  char-vs-line decision given `(anchor_row, current_row)`.
- The pixel→offset path needs a painted layout, so exercise it **live** via
  `scripts/dbg.sh`: drag within a diff line (char highlight + copy), then drag
  across lines (line-wise region), then back (char again); screenshot each.

## Phasing

1. **Rendering migration:** render diff `Line` (then `Hunk`/`Message`) as
   `StyledText` with color runs; verify no visual regression and comparable perf.
2. **Char selection + gesture:** `CharSelection`, the same-row/other-row branch in
   the drag handlers, and the background highlight run. Clear on plain click / Esc.
3. **Copy:** mouse-up + Cmd-C copy the char range; "Copied" flash; precedence over
   line copy.
4. **Widen** the selectable row set.
5. **Later:** the pager views.

## Risks / open questions

- **Rendering-migration blast radius.** Moving diff lines from span-divs to
  `StyledText` touches a hot path; expected perf-neutral-or-better (one element vs
  many). `uniform_list` still realizes only visible rows.
- **Layout timing.** `index_for_position` requires a measured layout; guard the
  handlers so an event on an unpainted row is a no-op.
- **Same-row threshold.** Decide the exact rule for "still on the anchor row"
  (strict row equality is simplest); vertical drift within a single-line row's
  band stays char-wise, crossing into the next row's band flips to line-wise.
- **Drag start precision.** `index_for_position` returns `Err(nearest)` past a
  line's end — treat that as the end offset so dragging off the right edge selects
  to end-of-line rather than doing nothing.
- **Hover vs selection paint.** The char-selection background should win over the
  row hover wash.
