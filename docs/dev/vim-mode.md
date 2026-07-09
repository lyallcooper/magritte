# Vim mode for the commit editor — implementation plan

Status: proposal. A design/implementation plan, not a finished feature.

## Goal

Give the in-app commit-message editor a high-quality, opt-in Vim mode: modal
editing (Normal/Insert/Visual), the common motions and text objects, the core
operators (`d`/`c`/`y`), and a surround MVP. Scoped to the commit editor
(`Screen::Editor`, `crates/magritte/src/commit_editor.rs`); it does not touch the
status/diff views, which have their own vi-style keymap.

The external-editor path stays the recommended route for full fidelity
(`commit_editor = "nvim"` launches the user's real Vim). This is for people who
want modal editing without shelling out.

Non-goals for the first cut (left room for, in "Later"): ex commands (`:%s/…`),
macros, marks, visual-block, multiple registers, search (`/`).

## Architecture: a modal command layer over `InputState`

We keep gpui-component's `InputState` as the text widget (it owns storage, text
layout, IME, scrolling, undo, and rendering) and add a **modal command layer** on
top. We do **not** fork/upstream gpui-component, and we do **not** build a text
editor from scratch.

This is viable because `InputState` implements gpui's public `EntityInputHandler`
trait, whose methods are callable from our crate and give us exactly the
primitives a Vim mode needs:

- `replace_text_in_range(Some(range), text, …)` — replace an arbitrary range.
  It records undo history (`push_history`) and emits `InputEvent::Change`, so
  operator edits are **properly undoable** and re-trigger the summary-ruler
  diagnostic — no whole-buffer `set_value` clobbering.
- `text_for_range(range, …) -> Option<String>` — read any slice.

Plus the inherent public methods, which for *reading* are byte-native (no UTF-16
anywhere): `value()`/`text()` (`&Rope`), `cursor()` (documented UTF-8 byte
offset), `selected_range()` (byte range), `set_cursor_position()`, `insert()`,
`unselect()` — and, for rendering, **`range_to_bounds(&Range<usize>)`**, a
public inherent method that takes a byte range and returns its laid-out pixel
rect using the input's stored element bounds. Prefer it over the trait's
`bounds_for_range`, which is built for IME-panel positioning: it clamps the
result to a single line (`end_origin.y = start_origin.y`) and needs the element
bounds passed in, which `InputState` doesn't expose (`last_bounds` is
`pub(super)`).

**Offsets:** only the *write* path (`replace_text_in_range`) speaks UTF-16;
every read we need is already in bytes. So the engine works in byte offsets and
one helper converts a byte range to UTF-16 at the single write boundary. Keep
that conversion in one place so it's the only code that has to be right.

Two genuine gaps, both solvable without upstreaming or a new editor:

1. **Setting a normal selection** (for Visual-mode highlighting) isn't exposed —
   `selected_range()` reads, nothing public sets it. We track the Visual anchor
   in our own state and **render the highlight ourselves** via
   `range_to_bounds`. A multi-line selection is *not* one rectangle: decompose
   it into per-line byte ranges and draw one translucent rect per line. This is
   an overlay, not an editor.
2. **Block cursor shape** in Normal mode isn't exposed. Ship a header mode
   indicator first; optionally draw a block cursor via
   `range_to_bounds(cursor..next char)` later.

## Build our own engine — but cross-check against proven emulations

The hard, bug-prone part of Vim is the character-level logic: word boundaries,
`iw`/`aw`/quote/bracket text objects, `f`/`t` semantics, inclusive vs exclusive
motions, linewise vs charwise operators, and the operator+motion grammar. We
build this ourselves (full control, no unstable third-party API to track, tailored
to our `Action` model and testable in isolation) rather than importing a crate
like `helix-core`.

To get the semantics *right*, cross-reference known-good implementations while
writing the tests — match observable behavior, don't copy code:

- **Vim/Neovim docs** — the canonical spec: `:help motion.txt` (inclusive/
  exclusive, linewise), `:help text-objects`, `:help word-motions`,
  `:help operator`. This is the source of truth for edge cases.
- **IdeaVim** (JetBrains, Apache-2.0) — a clean, well-structured action/handler
  model; the freest to read for *implementation* ideas (permissive license). Good
  for operator+motion composition and text-object boundary cases.
- **Emacs evil** (evil-mode) — the most faithful Vim emulation; excellent for
  precise behavioral corner cases. GPL — consult for *behavior*, not code.
- **Zed's `vim` crate** — same gpui/rope world, so behaviorally instructive for
  how this feels in a gpui editor. GPL — reference behavior, not code.
- **vim-surround** (tpope) — the reference for surround semantics (`ys`/`cs`/`ds`,
  which pairs add inner spaces, cursor placement after).

License discipline: IdeaVim and the Vim docs we can read freely; evil and Zed-vim
are GPL, so we treat them as behavior references (what a keystroke *does*), never
paste their code. Encode the agreed behavior as our own tests.

## The command engine (pure, backend-independent, unit-tested)

A new module (`crates/magritte/src/vim/`, or a small `magritte-vim` crate) that
knows nothing about `InputState` or gpui. It maps a keystroke, given the current
buffer and mode, to an `Action`:

```
input:  text: &Rope (or &str), cursor: usize (byte), mode, pending state
output: Action — one of
          MoveCursor(usize)
          Edit { replace: Range<usize>, with: String, cursor: usize }
          Yank { range: Range<usize>, linewise: bool }
          SetMode(Mode)
          Beep            // unhandled / invalid — no-op + optional flash
```

The app layer is the only code that touches `InputState`: read `(text, cursor)`,
feed the keystroke to the engine, apply the `Action` (`replace_text_in_range` for
`Edit`, `set_cursor_position` for `MoveCursor`, clipboard for `Yank`). Because the
engine is a pure function of `(text, cursor, mode, key)`, it's tested on plain
strings headlessly — the same discipline as `magritte-core`.

### Modes

```
enum Mode { Normal, Insert, Visual { anchor: usize, linewise: bool },
            OperatorPending(Operator), SurroundPending(SurroundOp) }
```

- **Normal** — keys are commands.
- **Insert** — keys pass straight through to `InputState` (the only mode we don't
  intercept); `Esc` returns to Normal and steps the cursor left one column.
- **Visual** — `anchor` + cursor define the range; operators act on it.
- **OperatorPending** — after `d`/`c`/`y`, awaiting a motion or text object.
- **SurroundPending** — after `ys`/`cs`/`ds`, awaiting the text object / pair char.

### Motions, text objects, operators, surround (MVP)

- **Motions:** `h j k l`, `0 ^ $ _`, `w W b B e E`, `gg G`, `f/t/F/T` (+ `;`,
  and `,` under an operator — a bare `,` is the leader, below), `{`/`}`, `%`.
  Each tagged inclusive/exclusive and charwise/linewise (per
  `:help motion.txt`), because operators need that to compute the right range.
- **Insert entry:** `i a I A o O` (and `c` lands in Insert). `Esc` back to
  Normal steps the cursor left one column.
- **Text objects:** `iw`/`aw`, and `i`/`a` for `" ' \`` and `() [] {} <>`.
- **Operators:** `d`, `c` (delete then Insert), `y` (unnamed register + system
  clipboard). Doubled forms `dd`/`cc`/`yy` are linewise; shorthands `D`/`C`
  (to end of line), `Y`/`S` (linewise), `x`/`X`, `r{char}`, `~`, `J`.
- **Put:** `p`/`P` from the unnamed register (deletes and changes fill it too,
  as in Vim), honoring the register's linewise flag.
- **Undo/redo:** `u`/`Ctrl-r`, engine-native. The widget's own history
  groups entries by *time* (1s), which is right for typing but wrong for
  Vim (`dw..` then `u` must undo one `dw`, not all three) — so the engine
  keeps its own stack: one `(text, cursor)` snapshot per change command,
  with a whole Insert session as a single unit (and no unit at all when the
  session changed nothing). `u`/`Ctrl-r` emit a full-buffer `Edit` restoring
  the snapshot.
- **Surround:** `ysiw"`, `yss"`, motion-based `ys`, visual `S`, `cs"'`, `ds"`,
  for the bracket/quote pairs.

Thread an optional `count` (`3w`, `2dd`) from day one so it isn't a later rewrite,
even if the first cut ignores it.

## Wiring to the editor

- **State:** `vim: Option<VimState>` on `CommitEditor` (`None` = mode off).
  `VimState` holds the `Mode`, pending count, last `f`/`t` (for `;`/`,`), and the
  unnamed register.
- **Enable flag:** a `vim_mode` bool in the commit-editor config (`config.rs` + a
  Settings toggle by "Summary ruler"/"Body auto-wrap"). When on, `open_editor`
  starts in Normal.
- **Key routing rides on focus.** Insert mode focuses the input (typing, IME,
  and the input's own keybindings work normally; we only catch `Esc`).
  Normal/Visual focus the *view*: the input paints no caret, its `Input`-
  context bindings never match (context comes from the focused element), and
  every key flows through `CommitEditor::on_capture_key` into the engine —
  handled keys `cx.stop_propagation()`, unhandled printables are swallowed so
  they can't insert. `sync_vim_focus` re-asserts the focus after every
  applied key (`set_cursor_position` focuses the input as a side effect), and
  an `InputEvent::Focus` subscriber blurs back after a mouse click has placed
  the cursor. Beware the alternative: gpui dispatches a *bound action* even
  when a capture listener stops the keystroke (the same reason main.rs
  overrides Root's `tab`), so interception-while-focused doesn't work.
  Pending state (operator/`f`/surround/search) accumulates across keystrokes
  like the status view's `pending_prefix` machinery.
- **Editor commands** use evil's commit-buffer keys so `Esc` and the editing
  keys stay free for modal editing: `ZZ` (or `,,`/`,c`) commit, `ZQ` (or
  `,k`) cancel (the discard-confirm flow still applies). A bare `,` in
  Normal mode is the leader — any non-leader key after it falls back to
  `,`'s reverse-find repeat and then runs (under an operator, `,` is always
  the reverse-find). `Esc` in idle Normal is a quiet no-op. `⌘⏎` still
  commits from any mode (in Normal it's caught in the capture phase, since
  the unfocused input can't).
- **`gq` is the reflow operator:** `gqq` reflows the current line(s),
  `gq{motion}`/`gq{object}` the covered lines, Visual `gq` the selection —
  each emits `Action::ReflowRange`, which the app expands to whole lines,
  reflows at 72 columns, and splices (the summary line is always skipped,
  keeping the 50-col convention). `⌥q` remains the whole-body reflow.
- **`.` repeat:** the engine records each change's keys (`recording` →
  `last_change`), plus the text an Insert session typed — captured at `Esc`
  as the slice between the insert-entry point and the exit cursor
  (best-effort; covers plain typing). `.` emits `Action::Repeat` and the app
  replays the keys through `feed_vim`, re-inserts the text, and closes with
  `Esc` — so anything key-driven repeats, surround included.
- **`/` search:** a `Pending::Search` prompt collects the query (shown live
  in the mode bar), and the overlay highlights every match as it's typed
  (incsearch-style, capped at 200). `Enter` jumps — a literal substring
  with smartcase (an all-lowercase query matches any case, any uppercase
  makes it exact), wrapping — `Esc`/empty-`Backspace` cancel, `n`/`N`
  repeat, `?` searches backward.

## Rendering

- **Mode line** (`NORMAL`/`INSERT`/`VISUAL` plus the pending keys or search
  prompt) under the message editor, above the diff preview
  (`render_editor`), vim-style; the header hints show the vim keys
  (`ZZ`/`ZQ`/`gq`).
- **Visual selection**: split `anchor..cursor` into per-line byte ranges and
  draw a translucent rect per line via `range_to_bounds`; falls back to none if
  bounds aren't available (not laid out / off-screen).
- **Block cursor**: header indicator first; optional self-drawn block later.

## Testing

- **Engine unit tests** (the bulk): table-driven `(text, cursor, keys) -> (text,
  cursor, mode)` for every motion/text-object/operator/surround case, headless.
  Seed the tables from the reference behaviors above so corners match Vim.
- **Live smoke** via `scripts/dbg.sh`: enable the flag on a scratch repo and
  exercise `ciw`/`dd`/`ysiw"`/`cs"'`, checking the mode indicator and result.

## Phasing

1. Engine skeleton + Normal-mode motions; routing + header indicator + config
   flag (`MoveCursor` only, no edits).
2. Operators + text objects (`d`/`c`/`y`, `iw`/`aw`, quote/bracket objects,
   linewise `dd`/`cc`/`yy`) via `replace_text_in_range` (real undo from day one).
3. Visual mode + the `range_to_bounds` per-line selection overlay.
4. Surround MVP.
5. Block cursor; polish.
6. Later: registers/marks, `>`/`<`, regex search (`/` is a literal substring
   today), a minimal `:` line, and mouse integration (a click should abort a
   pending operator, and a native drag-selection should become — or at least
   clear on entering — Visual mode; today the two selection models simply
   coexist).

## Risks / open questions

- **UTF-16 boundary.** Only the write path (`replace_text_in_range`) is UTF-16;
  the single byte→UTF-16 conversion helper must be correct.
- **Visual/block rendering.** `range_to_bounds` is only valid after layout and
  for on-screen ranges; the overlay must degrade gracefully while scrolling.
- **Auto-wrap.** *Decided:* suspend body auto-wrap while in Normal/Visual mode
  (only wrap on `Change` events that arrive in Insert mode). `on_editor_changed`
  rewrites the buffer with `set_value`, which sets `history.ignore` — the rewrap
  is invisible to undo history, so letting it run after an operator edit could
  make a later `u` restore mismatched offsets. Reflow (`⌥q`) stays available in
  both modes as an explicit command.
- **Scope.** Vim is bottomless — the phased MVP is the line. Keep the `Action`
  vocabulary small and let unhandled keys `Beep` rather than half-implementing.
