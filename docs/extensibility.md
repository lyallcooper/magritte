# Extending Magritte

Magritte is configurable without touching code, all driven by the config file
documented in [config.md](config.md). This page is the short tour.

## What you can do

- **Remap or unbind any key.** A `[keymap]` table maps a keystroke to a command
  id, or to `"unbound"` to drop a default. Every command has a stable id and
  most can be bound, not just the top-level ones — see
  [config.md → Keymap](config.md#keymap) for the full list.
- **Add to a transient.** A `[transient.<id>]` table appends suffixes to a menu
  — e.g. `b X` to delete a branch — Magit's `transient-append-suffix`. See
  [config.md → Transients](config.md#transients).
- **Define your own commands.** A `[[command]]` table runs a git argument list
  (optionally chained), surfaced in the `:` palette and bindable in `[keymap]`
  like any built-in. See [config.md → Commands](config.md#commands).
- **Prefix sequences.** Any key that begins a sequence becomes a prefix, with an
  on-screen which-key hint and a configurable timeout.
- **The `:` palette.** Opens a fuzzy picker over every command, with or without
  a binding — the way to reach rarely-used commands and discover their keys.

## Custom commands

```toml
[[command]]
id = "user.sync"
title = "Sync (pull --rebase, then push)"
run = ["pull", "--rebase"]   # a git argument list — no shell, no injection
then = ["push"]              # optional follow-up; runs only if the first succeeds

[[command]]
id = "user.wip"
title = "WIP commit"
run = ["commit", "-a", "-m", "WIP"]
```

- Run on the same background path as built-ins, so they're logged in the `$`
  command log and never block the UI.
- `{file}`, `{commit}`, and `{branch}` placeholders are resolved at run time
  against the selection at point. (Prompting for richer input is not supported.)
- Argument lists, never shell strings, so there's nothing to inject into. A
  command that looks destructive (`clean`, `--hard`, `--force`) is confirmed
  first, like the built-in destructive ops.

Not supported: an embedded scripting language, arbitrary (non-git) programs in a
`[[command]]` (use the `!` prompt for a one-off shell command), or live
transient rewriting beyond the `[transient.<id>]` additions above. Magritte
shells out to git rather than hosting a Lisp environment.
