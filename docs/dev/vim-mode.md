# Vim mode for the commit editor

This document is the as-built design reference for contributors. The pure
command engine lives in `crates/magritte-ui/src/vim/`; the app-side integration
(`apply.rs`, the `:help` sheet) lives in `crates/magritte/src/vim/`. The
feature is enabled with `commit_vim_mode`.

## Goal

Provide an optional Vim editing layer for commit messages without replacing the
underlying text component. It supports Normal, Insert, and Visual modes, common
motions and text objects, core operators, surround commands, search, Ex
commands, and Visual Block mode.

The feature applies only to the commit editor in
`crates/magritte/src/commit_editor.rs`. Status and diff views continue to use
the application keymap. Users who need complete Vim fidelity can set
`commit_in_editor = true` and `commit_editor = "nvim"`.

Macros, marks, and multiple registers remain outside the current scope.

## Architecture

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

Two missing `InputState` APIs shape the implementation:

1. `selected_range()` can read a normal selection, but no public method can set
   one. Visual mode therefore keeps its own anchor and renders one translucent
   rectangle per selected line through `range_to_bounds`.
2. Normal mode needs a block cursor, but `InputState` does not expose cursor
   shape. Magritte draws its own block over
   `range_to_bounds(cursor..next_char)` and uses a narrow stub at an empty line
   or the end of the file.

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

## Command engine

`crates/magritte-ui/src/vim/` does not depend on `InputState` or GPUI. Given a
buffer, cursor, mode, and key, it returns an `Action`:

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
enum Mode { Normal, Insert, Visual { anchor: usize, kind: Char|Line|Block },
            OperatorPending(Operator), SurroundPending(SurroundOp) }
```

- **Normal** — keys are commands.
- **Insert** — keys pass straight through to `InputState` (the only mode we don't
  intercept); `Esc` returns to Normal and steps the cursor left one column.
- **Visual** — `anchor` + cursor define the range; operators act on it.
  `v`/`V`/`C-v` pick the kind (charwise/linewise/blockwise) and switch it in
  place; the current kind's own key exits.
- **OperatorPending** — after `d`/`c`/`y`, awaiting a motion or text object.
- **SurroundPending** — after `ys`/`cs`/`ds`, awaiting the text object / pair char.

### Motions, text objects, operators, and surround

- **Motions:** `h j k l`, `0 ^ $ _`, `w W b B e E`, `gg G`, `f/t/F/T` (+ `;`,
  and `,` under an operator — a bare `,` is the leader, below), `{`/`}`, `%`.
  Each tagged inclusive/exclusive and charwise/linewise (per
  `:help motion.txt`), because operators need that to compute the right range.
- **Insert entry:** `i a I A o O` (and `c` lands in Insert). `Esc` back to
  Normal steps the cursor left one column.
- **Text objects:** the full `:help text-objects` set — `iw`/`aw` (+ `W`),
  `is`/`as` (sentences), `ip`/`ap` (paragraphs, linewise), `it`/`at` (tag
  blocks), and `i`/`a` for `" ' \`` and `() [] {} <>`.
- **Operators:** `d`, `c` (delete then Insert), `y` (unnamed register + system
  clipboard). Doubled forms `dd`/`cc`/`yy` are linewise; shorthands `D`/`C`
  (to end of line), `Y`/`S` (linewise), `x`/`X`, `r{char}`, `~`, `J`.
- **Put:** `p`/`P` from the unnamed register (deletes and changes fill it too,
  as in Vim), honoring the register's kind (charwise/linewise/blockwise).
- **Visual Block** (`C-v`): the anchor and cursor define a rectangle in char
  columns (multi-byte safe); the cursor corner on a line shorter than the
  sticky column sits one past its last char, and `$` extends the block to
  each line's end until a column-setting motion (both probed against Vim).
  Operators emit one `Edit` spanning the covered lines, cursor at the
  block's top-left: `d`/`x`, `y` (a blockwise register: segments joined by
  newlines plus a column width — `p`/`P` re-insert them column-aligned,
  space-padding short lines and creating missing ones), `r`, `c`, `I`/`A`
  (Insert at the left edge / past the right one, `$A` at each line's end;
  the Esc ending the session replicates the typed text onto the block's
  other lines — skipping too-short lines for `I`/`c`, padding for `A`, and
  not at all when the insert spanned lines), `D`/`C` (the to-eol forms),
  and the line-based commands (`J`, `gq`, `:`, `X`/`Y`/`R`/`S`) shared with
  the other kinds. Unsupported blockwise commands beep rather than
  misapply.
- **Scrolling:** `zz`/`zt`/`zb` center/top/bottom the cursor line (`z.`,
  `z<CR>`, `z-` also move to the first non-blank). The engine emits
  `Action::Scroll { align }`; the app computes the offset from the input's
  laid-out line height and visible rows and sets it (clamped, applied at
  the next layout).
- **Undo/redo:** `u`/`Ctrl-r`, engine-native. The widget's own history
  groups entries by *time* (1s), which is right for typing but wrong for
  Vim (`dw..` then `u` must undo one `dw`, not all three) — so the engine
  keeps its own stack: one `(text, cursor)` snapshot per change command,
  with a whole Insert session as a single unit (and no unit at all when the
  session changed nothing). `u`/`Ctrl-r` emit a full-buffer `Edit` restoring
  the snapshot.
- **Surround:** `ysiw"`, `yss"`, motion-based `ys`, visual `S`, `cs"'`, `ds"`,
  for the bracket/quote pairs.

The engine carries an optional count for commands such as `3w` and `2dd`.

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
  `,k`) cancel (the discard-confirm flow still applies), `,q` reflow the
  whole message. A bare `,` in Normal mode is the leader — any non-leader
  key after it falls back to `,`'s reverse-find repeat and then runs (under
  an operator, `,` is always the reverse-find). `Esc` in idle Normal is a
  quiet no-op. `⌘⏎` still commits from any mode (in Normal it's caught in
  the capture phase, since the unfocused input can't).
- **User keymap (`[vim.keymap]`):** extra literal key sequences for the
  editor-level commands (`commit`/`cancel`/`discard`/`reflow`/`help`),
  parsed from the config (`config::VimConfig`, per-entry repo merge like
  `[keymap]`) into the engine at construction (`VimState::with_user_map`).
  Live reload updates the map in an open editor. Resolution order: in Normal mode, before
  the built-in dispatch, a key that starts any user sequence enters a
  `Pending::User` prefix state (shown in the indicator like other pending
  keys); an exact match fires (so it wins any collision with a built-in
  key or prefix), a live prefix waits, and a dead end beeps *without*
  replaying the swallowed keys. A mapping's first key therefore shadows
  that built-in entirely. The defaults stay bound unless a custom entry
  shadows them. Live configuration reloads replace the user map without
  resetting the current mode, pending state, or undo history.
- **`gq` is the reflow operator:** `gqq` reflows the current line(s),
  `gq{motion}`/`gq{object}` the covered lines, Visual `gq` the selection —
  each emits `Action::ReflowRange`, which the app expands to whole lines,
  reflows at 72 columns, and splices (the summary line is always skipped,
  keeping the 50-col convention). `gqq` is Vim-literal — the current line
  only, so it breaks an overlong line onto new lines and is a no-op on one
  that fits; the paragraph form is `gqip`. Reflow respects structure:
  bullets (`- * + •`, `1.`/`1)`) re-wrap as their own items with a hanging
  indent, and indented lines rejoin and re-wrap at their own indentation —
  except under the *whole-body* reflow (`⌥q`/`,q`), where they're
  preformatted and kept verbatim (the git code-block convention; an
  explicit `gq` on them means the user wants them formatted). Auto-wrap
  shares the same shaping (`wrap_prefixed`), so typing past 72 in a bullet
  or indented line continues at the hanging indent. `⌥q` remains the whole-body reflow (also
  on `,q`), applied as a minimal `replace_text_in_range` splice — so it's
  ⌘Z-able in plain mode and gets a Vim undo snapshot
  (`note_external_change`) in Vim mode.
- **`.` repeat:** the engine records each change's keys (`recording` →
  `last_change`), plus the text an Insert session typed — captured at `Esc`
  as the slice between the insert-entry point and the exit cursor
  (best-effort; covers plain typing). `.` emits `Action::Repeat` and the app
  replays the keys through `feed_vim`, re-inserts the text, and closes with
  `Esc` — so anything key-driven repeats, surround included.
- **`/` search:** a `Pending::Search` prompt collects the pattern (shown
  live in the mode indicator), and the overlay highlights every match as
  it's typed (incsearch-style, capped at 200). Patterns are regexes (Rust
  syntax) with smartcase — no *literal* uppercase means case-insensitive
  (escapes like `\W` don't count) — and an invalid or still-partial
  pattern simply matches nothing. `Enter` jumps (wrapping),
  `Esc`/empty-`Backspace` cancel, `n`/`N` repeat, `?` searches backward.
- **`:` ex line:** a `Pending::Ex` prompt with the same editing as `/`
  (chars append, `Backspace` edits and cancels when empty, `Esc` cancels),
  executed on `Enter`: `:q`/`:q!` cancel (`!` skips the discard confirm),
  `:w`/`:wq`/`:x` commit, a bare `:N` jumps to line N (clamped, like
  `{count}G`), and `[range]s/pat/rep/[flags]` substitutes. The range is
  the current line, `%`, `N,M` with `.`/`$` endpoints, or `'<,'>` — a
  Visual-mode `:` leaves Visual and prefills that, remembering the
  selected lines. `pat` is a plain regex (no smartcase; `i` flag for case,
  `g` for every occurrence per line), `rep` takes Vim's `&`/`\0`–`\9`
  backrefs (`\&`/`\\` for the literals), and `\/` escapes the delimiter.
  One `Edit` covers the line span, cursor at the last changed line's first
  non-blank. While an `s` command is being typed, the overlay highlights
  the lines' matches live (first per line, all once a `g` flag is typed).
  `:help` opens a static cheat-sheet transient. A failed command emits
  `Action::Error(msg)` — echoed in red by the indicator until the next
  key, alongside the visual bell (`Action::Beep` alone rings the bell: a
  brief red tint on the mode chip). `:` commands are never the
  `.`-repeatable change, matching Vim.
- **Prompt history:** executed `/` queries and `:` lines are kept (50 each,
  consecutive repeats collapsed); `Up`/`Down` (or `C-p`/`C-n`) at either
  prompt recall them, with the live line stashed and restored by `Down`
  past the newest entry.
- **`>`/`<` indent operators:** `>>`/`<<` on lines, `>{motion}`/objects,
  Visual `>`/`<`; one step is two spaces (the hanging-bullet width — Vim's
  8 would be wrong here); indent skips blank lines, dedent strips a step of
  spaces or a tab.
- **Mouse:** a click in Normal/Visual aborts any pending operator/count and
  places the cursor; a completed drag-selection becomes a charwise Visual
  selection (anchor at its start, native selection dropped for the
  overlay). The blur-back is held while the button is down so the drag can
  complete against the focused input.

## Rendering

- **Mode indicator** (`NORMAL`/`INSERT`/`VISUAL` chip) as an overlay inside
  the message box at its bottom-right corner, with the pending keys or the
  `/`/`:` prompt text to the left of the chip; the header hints show the
  vim keys (`ZZ`/`ZQ`/`gq`).
- **Visual selection**: split `anchor..cursor` into per-line byte ranges and
  draw a translucent rect per line via `range_to_bounds`; falls back to none if
  bounds aren't available (not laid out / off-screen). A blockwise selection
  paints the engine's per-line block ranges the same way (lines the block
  overhangs yield empty ranges and no rect).
- **Block cursor**: drawn by the same overlay via
  `range_to_bounds(cursor..next char)`, with a half-width stub on empty
  lines and at EOF.
- **Which-key**: once a multi-key sequence (operator-pending, the
  `g`/`Z`/`z`/`,` prefixes, surround, `i`/`a` objects, or a user-map
  prefix — not the `/`/`:` prompts or a bare count) has sat pending for
  `which_key_delay_ms` (the app-wide which-key delay), a compact panel of its continuations appears above the mode
  indicator. The engine owns the rows (`which_key_hints`: static tables
  per pending state, capped at ~10 — a hint, not a manual — with a user
  prefix listing its own sequences); the app owns the timing (a
  generation-scoped timer re-armed on every key, mirroring the visual
  bell) and paints the panel as an inert overlay — no mouse handlers and
  not a `Popup`, which would capture the very keys it hints at.

## Testing

- **Engine unit tests** (the bulk): table-driven `(text, cursor, keys) -> (text,
  cursor, mode)` for every motion/text-object/operator/surround case, headless.
  Seed the tables from the reference behaviors above so corners match Vim.
- **Live smoke** via `scripts/dbg.sh`: enable the flag on a scratch repo and
  exercise `ciw`/`dd`/`ysiw"`/`cs"'`, checking the mode indicator and result.

## Implementation history

1. Built the engine skeleton, Normal-mode motions, routing, mode indicator, and
   configuration flag.
2. Added operators and text objects through `replace_text_in_range` so edits
   participated in undo from the start.
3. Added Visual mode and the per-line selection overlay.
4. Added surround commands.
5. Added the block cursor and interaction polish.
6. Remaining extensions include named registers, marks, macros, `C-d`/`C-u`
   paging, Visual paste of a blockwise register, and blockwise
   `~`/`u`/`U`/`>`/`<`.

## Constraints

- **UTF-16 boundary.** Only the write path (`replace_text_in_range`) is UTF-16;
  the single byte→UTF-16 conversion helper must be correct.
- **Visual/block rendering.** `range_to_bounds` is only valid after layout and
  for on-screen ranges; the overlay must degrade gracefully while scrolling.
- **Auto-wrap.** Body wrapping pauses in Normal and Visual modes. It runs only
  for `Change` events in Insert mode. The wrap itself is
  applied as a minimal `replace_text_in_range` splice (`splice_value`), so it
  lands in the undo history grouped with the typing that caused it — a later
  `u`/⌘Z can't restore mismatched offsets. Reflow (`⌥q`) stays available in
  both modes as an explicit command.
- **Scope.** The `Action` vocabulary stays small. Unsupported keys return
  `Beep` instead of applying partial behavior.
