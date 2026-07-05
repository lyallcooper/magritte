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
- `selected_text_range(…) -> Option<UTF16Selection>` — read the selection
  (range + `reversed`).
- `text_for_range(range, …) -> Option<String>` — read any slice.
- `bounds_for_range(range, element_bounds, …) -> Option<Bounds<Pixels>>` — the
  pixel rect for a range, so we can draw our own visual-selection highlight and
  block cursor.

Plus the inherent public methods already used elsewhere: `value()`/`text()`
(`&Rope`), `cursor()`, `set_cursor_position()`, `insert()`, `unselect()`.

**Offsets:** `EntityInputHandler` speaks UTF-16 offsets; our engine works in byte
offsets over the buffer text. Convert at the boundary (walk the rope to map
byte↔UTF-16) — for a commit message this is short and cheap. Keep the conversion
in one helper so it's the only place that has to be right.

Two genuine gaps, both solvable without upstreaming or a new editor:

1. **Setting a normal selection** (for Visual-mode highlighting) isn't exposed —
   `selected_text_range` reads, nothing public sets it. We track the Visual
   anchor in our own state and **render the highlight ourselves** via
   `bounds_for_range` (a translucent rect behind the text). This is an overlay,
   not an editor.
2. **Block cursor shape** in Normal mode isn't exposed. Ship a header mode
   indicator first; optionally draw a block cursor via `bounds_for_range` later.

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

- **Motions:** `h j k l`, `0 ^ $`, `w W b B e E`, `gg G`, `f/t/F/T` (+ `;`/`,`),
  `{`/`}`, `%`. Each tagged inclusive/exclusive and charwise/linewise (per
  `:help motion.txt`), because operators need that to compute the right range.
- **Text objects:** `iw`/`aw`, and `i`/`a` for `" ' \`` and `() [] {} <>`.
- **Operators:** `d`, `c` (delete then Insert), `y` (unnamed register + system
  clipboard). Doubled forms `dd`/`cc`/`yy` are linewise.
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
- **Key routing:** intercept in the existing `CommitEditor::on_capture_key`
  (capture phase, before `InputState`). When `vim` is `Some` and mode ≠ Insert,
  route the keystroke to the engine and `cx.stop_propagation()`. In Insert, only
  catch `Esc`. Pending state (operator/`f`/surround) accumulates across keystrokes
  like the status view's `pending_prefix` machinery.
- **Commit gating:** commit fires on `PressEnter` (a subscription), not through
  `on_capture_key`. In Normal mode a bare `Enter` must not commit (it's a motion);
  gate it to Insert mode or the existing `⌘⏎`.

## Rendering

- **Mode indicator** (`NORMAL`/`INSERT`/`VISUAL`) in the editor header
  (`render_editor`), by the `⌘⏎ commit` / `⌥q reflow` hints.
- **Visual selection**: draw a translucent rect via `bounds_for_range(anchor..
  cursor)`; falls back to none if bounds aren't available (off-screen).
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
3. Visual mode + the `bounds_for_range` selection overlay.
4. Surround MVP.
5. Block cursor; polish.
6. Later: counts everywhere, `.`-repeat, registers/marks, `>`/`<`, search, a
   minimal `:` line.

## Risks / open questions

- **UTF-16 boundary.** All `EntityInputHandler` edits/selection use UTF-16
  offsets; a single conversion helper (byte↔UTF-16 over the rope) must be correct.
- **Visual/block rendering.** `bounds_for_range` is only valid after layout and
  for on-screen ranges; the overlay must degrade gracefully while scrolling.
- **Auto-wrap.** Body auto-wrap (`wrap_at_cursor`) and reflow (`⌥q`) edit the
  buffer independently; compute an operator's resulting cursor against the
  post-wrap text, or suspend auto-wrap during a Normal-mode edit.
- **Scope.** Vim is bottomless — the phased MVP is the line. Keep the `Action`
  vocabulary small and let unhandled keys `Beep` rather than half-implementing.
