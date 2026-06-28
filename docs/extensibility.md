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
- **Define your own commands.** A `[[command]]` table runs a shell command,
  surfaced in the `:` palette and bindable in `[keymap]` like any built-in. See
  [config.md → Commands](config.md#commands).
- **Prefix sequences.** Any key that begins a sequence becomes a prefix, with an
  on-screen which-key hint and a configurable timeout.
- **The `:` palette.** Opens a fuzzy picker over every command, with or without
  a binding — the way to reach rarely-used commands and discover their keys.

## Custom commands

```toml
[[command]]
id = "user.sync"
title = "Sync"
run = "git pull --rebase && git push"

[[command]]
id = "user.wip"
title = "WIP commit"
run = "git commit -a -m WIP"
```

- `run` is a shell command (`sh -c`, in the repo root), so `&&`, pipes, and any
  program work — not just git. It runs on the same background path as built-ins,
  logged in the `$` command log, never blocking the UI.
- `{file}`, `{commit}`, and `{branch}` placeholders are resolved (shell-quoted)
  from the selection at run time. Prompting for richer input is not supported.
- A command that looks destructive (`clean`, `--hard`, `--force`) is confirmed
  first, like the built-in destructive ops.

Not supported: an embedded scripting language, or live transient rewriting
beyond the `[transient.<id>]` additions above. Magritte drives the `git` CLI
rather than hosting a Lisp environment.
