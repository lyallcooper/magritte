# Sketch: customizable commands & the `:` command palette

> Status: **design sketch**, not implemented. Covers TODO #42 (user-customizable
> actions — remap/unbind/add) and #43 (an `M-x`-style command palette on `:`).
> The two share one foundation, so they're designed together.

## The shared foundation: a command registry

Everything here rests on one thing we don't fully have yet: a **single registry
of named commands**. Today our commands live in three hand-kept places — the
`on_key` match, `run_dispatch`, and the transient definitions — keyed by raw
keystroke, not by a stable identity. Remapping, unbinding, a palette, and custom
commands all need to refer to a command by a **stable id**, independent of which
key invokes it.

```rust
struct Command {
    id: &'static str,          // stable: "stage", "log.current", "branch.delete"
    title: &'static str,       // human label for the palette: "Stage"
    // Whether it makes sense right now (e.g. "stage" needs a selectable row).
    enabled: fn(&StatusView) -> bool,
    run: fn(&mut StatusView, &mut Window, &mut Context<StatusView>),
}

fn commands() -> &'static [Command] { /* the one source of truth */ }
```

This is the table I prototyped and then pulled back from (see the dispatch
work) — the difference is that *here it earns its keep*, because three user-facing
features consume it. The earlier objection (a fn-pointer table is less greppable
than a `match`) still holds for pure dispatch, but once ids are referenced by
config and a palette, the registry is the right shape.

Commands that need an argument (a branch, a file, a message) keep doing what
they do now: `run` opens the relevant picker/transient. The registry doesn't try
to model arguments — it just invokes, and the command gathers its own input.
That keeps the registry flat and avoids re-modeling the transient system.

### Migration path (incremental, low-risk)

1. Introduce the registry alongside today's code; give each existing command an
   id. `run_dispatch(id)` becomes a registry lookup. (We already proved this
   shape works.)
2. Point `on_key`'s command keys at the registry via the keymap (below), leaving
   the genuinely special handling — `g`-prefix, visual mode, the dash/option
   prefixes inside transients — as bespoke code. Not everything is a "command";
   modal/prefix state stays hand-written.
3. The `?` dispatch menu and the `:` palette both render from the registry.

## #43 — the `:` command palette (do this first; it's the cheap win)

`:` opens the **existing vertico picker** over `commands()` (filtered to
`enabled`), matched by `title` (and maybe `id`). Enter runs the command's `run`.

This is almost entirely free once the registry exists — it's the picker we
already use everywhere, with command titles as the candidate list and "run the
selected command" as the action. It's also the natural fallback for commands
that don't have (or shouldn't spend) a keybinding.

- Entry: `:` from the status view (no conflict — `:` is unbound).
- Candidates: `commands().filter(enabled).map(title)`; `CreateMode::None`
  (selection only — you can't invoke a command that doesn't exist).
- Action: a new `PickerAction::RunCommand { id }`.
- Nice-to-have: show each command's current keybinding on the right of its row
  (purely from the keymap), so the palette doubles as discoverable help.

**Why first:** it delivers most of the value (every command reachable, no
memorization) with no config format and no new persistence — just the registry +
the picker we already have.

## #42 — remap / unbind / custom commands (config-driven)

### Remap & unbind

A `[keymap]` table in `config.toml` maps a keystroke to a command id (or to the
sentinel `"unbound"`):

```toml
[keymap]
"K" = "branch.delete"   # bind K
"x" = "unbound"         # remove the default discard binding
"c" = "commit"          # (already the default; explicit is fine)
```

At startup the **effective keymap** = built-in defaults overlaid with the user
table. `on_key` (for non-modal keys) resolves keystroke → id → registry. Unknown
ids on load are reported (a startup warning line), not silently dropped.

Open question — *scope of remappable keys*: the cleanest v1 is to make only the
top-level command keys remappable, and keep modal/prefix machinery (`g`-prefix,
transient `-`/`--` option entry, visual mode) fixed. Trying to make those
user-remappable is a lot of complexity for little gain.

### Custom commands

A `[[command]]` array lets users add their own, scoped much simpler than magit
(we're not hosting a Lisp environment — we shell out):

```toml
[[command]]
id = "user.sync"
title = "Sync (pull --rebase then push)"
run = ["pull", "--rebase"]   # a git argument list (not a shell string — no shell parsing/injection)
then = ["push"]              # optional follow-up; stop if the first fails
refresh = true               # re-read status afterward (default true)

[[command]]
id = "user.wip"
title = "WIP commit"
run = ["commit", "-a", "-m", "WIP"]
```

- Custom commands run via the same background-executor path as our built-ins
  (`Repo::run`), so they're logged in the `$` command log and never block the UI.
- They appear in the `:` palette and can be bound in `[keymap]` like any
  built-in — `id` is the link.
- **Args, scoped simply:** support a few placeholders resolved at run time —
  `{file}` (selected file), `{commit}` (commit at point), `{branch}` (current).
  Anything richer (prompting, completion) is deferred; a custom command that
  needs to *ask* for input is a bigger feature.
- **Safety:** arg lists, not shell strings — no shell, no injection. Destructive
  built-ins keep their confirmations; custom commands are the user's own rope, but
  we could flag ones containing `reset --hard`/`clean`/`push --force` for a confirm.

### What we deliberately *don't* copy from magit

magit's `transient-append-suffix` et al. let users mutate transients and bind
arbitrary Elisp. We're not in Emacs, so: no live transient mutation, no embedded
language, no per-buffer keymaps. The 80% — rebind, unbind, add a
shell-out command, reach anything via `:` — covers the real desire without that
machinery.

## Suggested order

1. **Registry** (foundation; incremental, no user-visible change).
2. **`:` palette** (#43) — immediate value, reuses the picker.
3. **`[keymap]` remap/unbind** (#42a).
4. **`[[command]]` custom commands** (#42b), placeholders last.

Each step is independently shippable and useful.
