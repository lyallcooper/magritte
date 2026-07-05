# In-row character selection (mouse) — implementation plan

Status: proposal. A design/implementation plan, not a finished feature.

## Goal & scope

Let the user drag-select an arbitrary character range **within a single row** of a
read-only view (a diff line, a hunk header, a commit subject, a message line) and
copy it. This covers the common "grab part of this line" need without the
cross-row selection layer that the full feature would require.

Explicitly out of scope here:

- **Cross-row selection.** Selecting a range that spans multiple rows (and the
  model-reconstruction copy it needs) is deferred; see "Later".
- **The commit editor.** It's an `InputState` text field and already has native
  character selection.
- **The `$` command-log and blame pagers.** Non-interactive; can come later once
  the per-row mechanism exists.

Today only *line/unit* copy exists (tier 1 in the TODO): `y` / Cmd-C /
right-click-Copy yank the visual selection or the line at point. This adds
sub-line granularity by mouse.

## The core obstacle (and the fix)

A text-bearing row today renders its text as a **flex of separate colored `div`s**
— e.g. `diff_line_body` builds one `div().child(text)` per syntax-highlight span.
There is no shaped-text layout to ask "what character is under this pixel?", and
no way to paint a selection background over a byte range. gpui isn't used for
`StyledText`/`TextLayout` anywhere in the app yet.

The fix is to render each *selectable* row's text as a single
`gpui::StyledText`:

- **Colors survive.** `StyledText::with_default_highlights(default_style, runs)`
  takes `(Range<usize>, HighlightStyle)` runs — so the per-span colors we build
  in `diff_line_body` become highlight runs over one string instead of sibling
  divs. Same appearance, one text element.
- **Pixel → offset.** `StyledText::layout()` returns a `TextLayout` (an
  `Rc<RefCell<…>>` shared handle). After paint, `TextLayout::index_for_position(
  point) -> Result<usize, usize>` maps a mouse position to a byte index (`Ok`
  inside a glyph, `Err(nearest)` past the end). `position_for_index` is the
  inverse.
- **Selection paint.** Overlay the selection as one more highlight run with a
  `background_color` (`with_highlights([(range, HighlightStyle { background_color:
  Some(sel), .. })])`), composed on top of the color runs.

So the work is a targeted rendering migration of text rows to `StyledText`, plus a
small selection state machine and mouse handlers — not a new editor and no
gpui-component changes.

## Selection model

```
struct CharSelection {
    row: usize,             // index into the current view's row model
    anchor: usize,          // byte offset within that row's text
    cursor: usize,          // byte offset within that row's text
}
```

Held on the view (e.g. `Option<CharSelection>`), distinct from the line-wise
`Selection::visual`. The selected range is `min(anchor,cursor)..max(…)`. It's
cleared when a different row's text is pressed, on Esc, or on a plain click
(consistent with the which-key/prefix dismissal already added).

Because it's one row, copy is trivial: the row's own text sliced by the range —
no model reconstruction, no virtualization concern (the row is on-screen by
definition, since you're dragging on it).

## Mouse gesture

Plain left-drag is already **line-wise visual selection** (`drag_anchor →
visual`), and that's a load-bearing gesture (drag to stage a range). So
character selection uses **Alt(Option)-drag**:

- `Alt + mouse-down` on a row's text → start a char selection (anchor = index at
  point), suppress the line-visual drag.
- `Alt + mouse-move` (button held) → extend `cursor` to the index at the current
  point, clamped to the same row (in-row only — vertical drift is ignored, or
  clamps to row start/end).
- `mouse-up` → finalize; copy immediately (see below) or leave it for Cmd-C.

Alt-drag is the least-disruptive choice (keeps every existing gesture intact); it
needs a docs line for discoverability. The alternative — making plain-drag
char-select and moving line-visual onto `v`+motion / shift-click — is a larger UX
change and is not proposed here.

## Rendering & wiring

- **A `selectable_text` helper** that, given the row's `(text, color-runs)` and the
  row index, returns a `StyledText` with the color runs plus the selection
  background run (when this row is the selected one), and stashes its
  `layout()` handle where the mouse handlers can reach it.
- **Layout handles.** `TextLayout` is a cloneable shared handle but is only
  populated after prepaint. Keep a per-frame map `row_ix -> TextLayout` on the
  view (rebuilt as rows render, like other per-frame render state), so the
  `on_mouse_down`/`on_mouse_move` closures can call `index_for_position` against
  the right row's layout. The closures capture the row's cloned layout handle
  directly, so no lookup race.
- **Handlers** live on the row's container `div` (the same element that already
  has `on_mouse_down`/drag for line-visual): branch on `ev.modifiers.alt`.
- **Which rows:** the text-bearing kinds — diff `Line`, `Hunk`, `Message`,
  `Detail`, `StatLine`, `Note`, and commit/log subjects. Structural rows (section
  headers, file rows with chips) can opt in later; start with the diff/message
  text where sub-line copy is most wanted.

## Copy

On mouse-up with a non-empty range (or on Cmd-C while a char selection exists),
copy `row_text[range]` via the existing `copy_to_clipboard`, which already shows
the "Copied …" flash. A char selection takes precedence over the line/unit copy
when present, so Cmd-C does the intuitive thing.

## Testing

- **Unit tests** for the pure parts: range normalization (`anchor`/`cursor` →
  ordered range), clamping to a row's text bounds, and text extraction. These run
  headless.
- `index_for_position` depends on a painted layout, so the pixel→offset path
  can't be unit-tested without a window — cover it **live** via `scripts/dbg.sh`:
  Alt-drag across a diff line, screenshot the highlight, and confirm the copied
  value (paste into the message field or check the "Copied" flash).

## Phasing

1. **Rendering migration:** render diff `Line` (and `Hunk`/`Message`) text as
   `StyledText` with color runs; confirm no visual regression and comparable
   perf. No selection yet.
2. **Selection state + Alt-drag:** the `CharSelection` model, the mouse handlers,
   and the highlight run. Clear on plain click / Esc.
3. **Copy:** mouse-up + Cmd-C copy the range; "Copied" flash.
4. **Widen the row set** to the remaining text rows; document the Alt-drag gesture.
5. **Later:** cross-row selection (track `(row, offset)` pairs, render highlights
   only for visible rows, reconstruct copied text from the full model), and the
   pager views.

## Risks / open questions

- **Rendering-migration blast radius.** Moving diff lines from span-divs to
  `StyledText` touches a hot path. Expected to be perf-neutral-or-better (one
  element vs many), but verify: `StyledText` shapes the whole line; the current
  code relies on `uniform_list` only realizing visible rows, which still holds.
- **Layout timing.** `index_for_position` panics if called before measurement;
  guard the handlers so a mouse event on an as-yet-unlaid-out row is a no-op.
- **Row-height / wrapping.** Rows are single-line (`ROW_HEIGHT`); if any selectable
  text ever wraps, `index_for_position`'s multi-line path applies — fine, but keep
  rows single-line to keep offset math simple.
- **Gesture discoverability.** Alt-drag isn't obvious; needs a docs mention (and
  possibly a hint). Revisit if users expect plain-drag to select text.
- **Interaction with hover/line-visual.** Alt-drag must fully suppress the
  line-visual `drag_anchor` path for that gesture, and the hover highlight should
  not fight the selection background (selection wins).
