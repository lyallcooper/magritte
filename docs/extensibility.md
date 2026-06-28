# Extending Magritte

Magritte is configurable without touching code. Everything below is driven by
the config file documented in [config.md](config.md); this page is the short
tour, plus the one feature still on the roadmap.

## Available now

- **Remap or unbind any key.** A `[keymap]` table maps a keystroke to a command
  id, or to `"unbound"` to drop a default. Every command has a stable id and
  most can be bound, not just the top-level ones — see
  [config.md → Keymap](config.md#keymap) for the full list.
- **Add to a transient.** A `[transient.<id>]` table appends suffixes to a menu
  — e.g. `b X` to delete a branch — Magit's `transient-append-suffix`. See
  [config.md → Transients](config.md#transients).
- **Prefix sequences.** Any key that begins a two-key binding becomes a prefix,
  with an on-screen which-key hint and a configurable timeout.
- **The `:` palette.** Opens a fuzzy picker over every command, with or without
  a binding — the way to reach rarely-used commands and discover their keys.

## Planned: custom commands

A `[[command]]` array would let you define your own shell-out commands: a git
argument list, optionally chained, surfaced in the `:` palette and bindable in
`[keymap]` like any built-in.

```toml
[[command]]
id = "user.sync"
title = "Sync (pull --rebase, then push)"
run = ["pull", "--rebase"]   # a git argument list — no shell, no injection
then = ["push"]              # optional follow-up; stops if the first fails

[[command]]
id = "user.wip"
title = "WIP commit"
run = ["commit", "-a", "-m", "WIP"]
```

The intended scope:

- Runs on the same background path as built-ins, so it's logged in the `$`
  command log and never blocks the UI.
- Placeholders resolved at run time — `{file}`, `{commit}`, `{branch}` — for the
  selection at point. Prompting for richer input is deferred.
- Argument lists, never shell strings, so there's nothing to inject into.
  Destructive built-ins keep their confirmations; custom commands containing
  `reset --hard` / `clean` / `push --force` may be flagged for a confirm.

Not planned: an embedded scripting language, or live transient rewriting beyond
the `[transient.<id>]` additions above. Magritte shells out to git rather than
hosting a Lisp environment, so the goal is to cover the common 80% — rebind,
unbind, extend a menu, add a shell-out, reach anything via `:` — without that
machinery.
