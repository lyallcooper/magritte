# Vim mode for the commit editor — implementation plan

Status: proposal. This is a design/implementation plan, not a finished feature.

## Goal

Give the in-app commit-message editor a usable Vim mode: modal editing
(Normal/Insert/Visual), the common motions and text objects, the core operators
(`d`/`c`/`y`), and a surround MVP. It is opt-in and scoped to the commit editor
(`Screen::Editor`, `crates/magritte/src/commit_editor.rs`); it does not touch the
status/diff views, which already have their own vi-style keymap.

Non-goals (for the first cut): ex commands (`:%s/…`), macros/registers beyond the
unnamed register, marks, folds, visual-block, `.`-repeat, counts on every motion.
These are called out in "Later" so the model leaves room for them.

The existing external-editor path stays the recommended route for full fidelity:
setting `commit_editor = "nvim"` (`commit_in_editor`) launches the user's real
Vim. This plan is about the *in-app* editor for people who don't want to shell
out.

## The core constraint: what `InputState` exposes

The commit editor stores and renders its text with `gpui_component::input::InputState`
(a Rope-backed code editor). From the `magritte` crate, only a thin slice of its
API is public:

- Read: `value()`, `text()` (`&Rope`), `cursor()` (byte offset), `cursor_position()`
  (`Position { row, col }`), `selected_range()` (`Range<usize>`).
- Write: `set_value()` (replaces the *whole* value, clears undo history, resets
  the cursor), `insert()` (at the cursor), `set_cursor_position()`, `unselect()`.

Crucially, these are **not** public: `replace_text_in_range` (replace an arbitrary
range), `select_to` / any selection *setter*, and cursor-shape control. That has
two consequences:

1. **Operators must rebuild the whole buffer.** With only `set_value` +
   `set_cursor_position`, a `dw` becomes "read text, compute the range, `set_value`
   with it spliced out, then `set_cursor_position`". That works but throws away
   `InputState`'s own undo granularity (every operator is one full replace) and
   re-runs the LSP/diagnostics layer each time.
2. **We can't drive `InputState`'s selection.** Visual mode can't highlight text
   by moving the Input's own selection, because there's no public setter and the
   Input renders only its own selection.

So the architecture question is really "how much do we work around `InputState`
vs. change it".

## Approach options

**A. Modal layer over `InputState`, public API only.** Keep `InputState` for
storage + rendering; add a modal command layer in the app. Motions and operators
compute byte ranges from `text()`/`cursor()` and apply via `set_value` +
`set_cursor_position`. Visual mode is tracked in *our* state and rendered as our
own overlay (or deferred). Pro: no upstream dependency, ships incrementally. Con:
undo becomes coarse (per-operator), and visual highlighting needs our own
rendering; block cursor in Normal mode isn't available.

**B. Approach A + a few upstreamed `InputState` methods.** Land a small PR on
gpui-component exposing `replace_range(range, text)`, `set_selected_range(range)`,
and a cursor-shape/`overwrite`-style flag. Then operators splice ranges (real
undo granularity), Visual mode uses the Input's own selection + rendering, and
Normal mode shows a block cursor. Pro: clean, the Input keeps owning text and
rendering. Con: depends on an upstream merge (or a temporary fork pin — we already
pin gpui-component by git rev, so a fork rev is low-friction).

**C. In-house modal editor on a Rope.** Replace `InputState` in the commit editor
with our own element over `ropey`/gpui text layout: full control of modes,
selection, cursor shape, undo, and `.`-repeat. Pro: no constraints. Con: large —
text layout, IME, mouse selection, scrolling, and the summary-ruler/diagnostic all
have to be reimplemented.

**Recommendation:** build the **modal command engine first as a pure, testable
core** (Approach A's engine — see below), wire it to `InputState` via the public
API for an MVP, and in parallel upstream the three small `InputState` methods
(Approach B) so operators get real undo and Visual mode uses the Input's own
selection. Approach C stays the fallback only if upstreaming is rejected; the
engine we build is reusable against any of the three backends because it operates
on `(text, cursor) -> edits/new-cursor`, never on `InputState` directly.

## The command engine (backend-independent, pure, unit-tested)

Everything hard about Vim is independent of gpui. Put it in a new module
`crates/magritte/src/vim/` (or a small `magritte-vim` crate if we want it
reusable) that knows nothing about `InputState`:

```
Input:  text: &str (or &Rope), cursor: usize (byte offset), mode, pending op
Output: an Action — one of:
          MoveCursor(usize)
          Edit { replace: Range<usize>, with: String, cursor: usize }
          SetMode(Mode)
          Yank { range: Range<usize>, linewise: bool }
          Beep            // unhandled / invalid
```

The app layer is the only part that touches `InputState`: it reads `(text,
cursor)`, feeds a keystroke to the engine, and applies the returned `Action`
(splice via `set_value`/`insert`, move via `set_cursor_position`, copy on `Yank`).
This keeps the engine trivially unit-testable on plain strings with no graphics
stack — the same discipline as `magritte-core`.

### Modes

```
enum Mode { Normal, Insert, Visual { anchor: usize, linewise: bool },
            OperatorPending(Operator), SurroundPending(SurroundOp) }
```

- **Normal** — keys are commands; the buffer is read-only to typing.
- **Insert** — keys pass through to `InputState` unchanged (this is the *only*
  mode where we don't intercept). `Esc` returns to Normal (and, Vim-style, steps
  the cursor left one column).
- **Visual** — `anchor` + current cursor define the selection range; operators act
  on it. `linewise` for `V`.
- **OperatorPending** — after `d`/`c`/`y`, waiting for a motion or text object; the
  resolved range feeds the operator.
- **SurroundPending** — after `ys`/`cs`/`ds`, waiting for the text object and/or
  the pair character.

### Motions (produce a target offset or a range)

MVP: `h j k l`, `0 ^ $`, `w W b B e E`, `gg G`, `f{c} t{c} F{c} T{c}` (+ `;`/`,`),
`{` `}` (paragraph), `%` (matching bracket). Line motions (`j`/`k`, `dd`) are
*linewise*; the rest are *charwise* — the engine tags each so operators know
whether to include the trailing newline. Counts (`3w`) can come later but the
engine should thread an optional `count` from the start so it isn't a rewrite.

### Text objects (produce a range)

MVP: `iw`/`aw` (word), and the quote/bracket pairs `i"` `a"` `i'` `i(` `a(`
`i[` `i{` `i<` `` i` `` and their `a` variants. Text objects and motions both
resolve to a `Range<usize>` so operators consume them uniformly.

### Operators

MVP: `d` (delete), `c` (change = delete then enter Insert), `y` (yank to the
unnamed register + system clipboard, via the existing `copy_to_clipboard`).
Doubled forms `dd`/`cc`/`yy` are linewise. Each operator = `Edit { replace, with,
cursor }` (+ `Yank` for `y`/`c`).

### Surround MVP

Target parity with the TODO note: `ysiw"`, `yss"`, motion-based `ys{motion}{pair}`,
visual-mode surround (`S"` in Visual), `cs"'` (change surround), `ds"` (delete
surround); pairs for `() [] {} <>` and the quotes `" ' \``. Surround is modeled as
its own pending state that, once it has a range (from a text object/motion or the
visual selection) and a pair char, emits one `Edit` that inserts/rewrites/removes
the delimiters.

## Wiring to the editor (app layer)

- **State.** Add `vim: Option<VimState>` to `CommitEditor` (`None` when the mode is
  off). `VimState` holds the `Mode`, the pending count, the last `f`/`t` search (for
  `;`/`,`), and the unnamed register.
- **Enable flag.** A `vim_mode` bool in the commit-editor config
  (`config.rs` + a Settings toggle next to "Summary ruler"/"Body auto-wrap"). When
  on, `open_editor` starts in Normal mode.
- **Key routing.** The interception point already exists:
  `CommitEditor::on_capture_key` (`commit_editor.rs:581`) runs in the capture phase
  *before* `InputState` sees the key. When `vim` is `Some` and the mode is not
  `Insert`, route the keystroke to the engine and `cx.stop_propagation()` so the
  Input never receives it. In `Insert` mode, only intercept `Esc` (→ Normal) and
  let everything else fall through to `InputState`. The prefix/pending state
  (operator-pending, `f`-pending, surround-pending) accumulates across keystrokes
  much like the status view's `pending_prefix` machinery in `input.rs`.
- **Applying an `Action`.**
  - `MoveCursor(off)` → `state.set_cursor_position(offset_to_position(text, off))`.
  - `Edit { replace, with, cursor }` → today, `set_value(splice(text, replace,
    with))` then `set_cursor_position`. With the upstreamed `replace_range`, call
    that instead for real undo.
  - `Yank { range, .. }` → `copy_to_clipboard(text[range])` (reuses the existing
    helper) and stash into the register.
  - `SetMode` → update `vim.mode`; on entering Insert from `c`, the edit has
    already removed the range.
- **`Enter`/commit.** The commit editor commits on `PressEnter` via a subscription
  (`commit_editor.rs:371`), not through `on_capture_key`. In Normal mode, a bare
  `Enter` should *not* commit (it's a motion in Vim); gate the commit so it only
  fires from Insert mode or via the existing `⌘⏎` binding. Reflow (`⌥q`) and the
  summary ruler are unaffected.

## Rendering

- **Mode indicator.** Show `NORMAL`/`INSERT`/`VISUAL` in the editor header
  (`render_editor`, `render.rs`), near the `⌘⏎ commit` / `⌥q reflow` hints.
- **Cursor shape.** A block cursor in Normal/Visual is the expected cue. This is
  the one piece `InputState` doesn't expose; either upstream a cursor-shape flag
  (Approach B) or, short term, ship the bar cursor + the header indicator and note
  the limitation.
- **Visual selection.** With Approach B, drive `InputState`'s own selection via the
  upstreamed setter so it renders normally. With Approach A only, defer visual
  highlighting (or draw our own range overlay) — track this as a known gap.

## Testing

- **Engine unit tests** (the bulk): table-driven cases of `(text, cursor, keys) ->
  (text, cursor, mode)` for every motion, text object, operator, and surround case,
  run headless in `cargo test` — no window, like `magritte-core`. This is where
  correctness lives.
- **Live smoke test** via `scripts/dbg.sh`: enable the flag on a scratch repo, open
  the editor, exercise `iw`/`ciw`/`dd`/`ysiw"`/`cs"'`, and screenshot the mode
  indicator + result.

## Phasing

1. **Engine skeleton + Normal-mode motions.** `vim/` module, `Mode`, motion set,
   `MoveCursor` only. Wire routing + header indicator + config flag. No edits yet.
2. **Operators + text objects.** `d`/`c`/`y`, `iw`/`aw`, quote/bracket objects,
   linewise `dd`/`cc`/`yy`; apply via `set_value` (accept coarse undo for now).
3. **Visual mode.** Charwise + linewise; operators over the selection.
4. **Surround MVP.** `ys`/`cs`/`ds` + visual `S`.
5. **Upstream `InputState`** `replace_range` / `set_selected_range` / cursor-shape;
   switch operators to range-replace (real undo), Visual to the Input's selection,
   and Normal to a block cursor.
6. **Later:** counts everywhere, `.`-repeat, registers/marks, `>`/`<` indent,
   search (`/`), a minimal `:` line.

## Risks / open questions

- **`InputState` API ceiling.** Approach A alone can't do block cursor, real undo
  granularity, or Input-rendered visual selection. Decide early whether we're
  willing to pin a gpui-component fork rev (we already pin by rev) to get the three
  small methods, versus living with the A-only limitations for the MVP.
- **Undo.** Per-operator `set_value` collapses undo to whole-buffer steps and
  re-runs diagnostics; acceptable for a commit message, but a reason to prefer
  Approach B sooner rather than later.
- **Auto-wrap interaction.** Body auto-wrap (`wrap_at_cursor`) and reflow (`⌥q`)
  edit the buffer independently; make sure an operator's cursor result is computed
  against the post-wrap text, or disable auto-wrap while a Normal-mode edit applies.
- **Scope creep.** Vim is bottomless; the phased MVP above is the line. Keep the
  engine's `Action` vocabulary small and let unhandled keys `Beep` rather than
  half-implementing.
