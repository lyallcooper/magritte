//! The transient model — magit's popup command menus (`P` push, `F` pull,
//! `f` fetch, `c` commit, …).
//!
//! A [`Transient`] is a declarative tree of groups and suffixes (switches and
//! actions). The model is UI-agnostic and generic over `C`, the app-defined
//! operation an [`Action`] fires: it carries keys and descriptions as data,
//! but knows nothing about rendering or what the operations mean. The
//! frontend defines its command vocabulary and menu builders, renders the
//! popup, tracks which switches are toggled on, and dispatches keys; when an
//! action fires it runs the action's command with the active switch arguments.

/// A toggleable flag (e.g. `-f` → `--force-with-lease`). Owned strings so a
/// user-injected `[transient.<id>]` switch is the same type as a built-in one.
#[derive(Debug, Clone)]
pub struct Switch {
    pub key: String,
    pub arg: String,
    pub description: String,
    /// Whether the flag starts toggled on when the transient opens (the user can
    /// still turn it off), like magit's `--autostash` on the rebase popup. Most
    /// switches start off. For a switch with a [`config_key`](Self::config_key)
    /// the frontend overwrites this from the git-config value at open time.
    pub default_on: bool,
    /// The negated flag (e.g. `--no-gpg-sign`) for a switch whose default comes
    /// from git config: when the toggle differs from the configured default, the
    /// frontend emits the positive `arg` (turned on) or this `negation` (turned
    /// off), so the user can override the default either way. `None` for a plain
    /// switch, which simply emits `arg` when on and nothing when off.
    pub negation: Option<String>,
    /// The git-config key whose value seeds [`default_on`](Self::default_on)
    /// (e.g. `commit.gpgSign`), resolved by the frontend at open time. `None` for
    /// a switch with a fixed default.
    pub config_key: Option<String>,
    /// Arguments this switch cannot combine with (magit's `:incompatible`):
    /// toggling this switch on turns off any active switch whose `arg` is
    /// listed here. Checked symmetrically, so declaring one side suffices.
    pub exclusive_with: Vec<String>,
}

impl Switch {
    /// Declare `args` mutually exclusive with this switch — toggling it on
    /// turns them off.
    pub fn exclusive_with(mut self, args: &[&str]) -> Self {
        self.exclusive_with = args.iter().map(|a| a.to_string()).collect();
        self
    }

    /// A switch that starts off (the common case).
    pub fn new(
        key: impl Into<String>,
        arg: impl Into<String>,
        description: impl Into<String>,
    ) -> Self {
        Switch {
            key: key.into(),
            arg: arg.into(),
            description: description.into(),
            default_on: false,
            negation: None,
            config_key: None,
            exclusive_with: Vec::new(),
        }
    }

    /// A switch that starts on; the user toggles it off.
    pub fn on(
        key: impl Into<String>,
        arg: impl Into<String>,
        description: impl Into<String>,
    ) -> Self {
        Switch {
            default_on: true,
            ..Switch::new(key, arg, description)
        }
    }

    /// A switch whose default reflects a git-config value (`config_key`): it
    /// starts on when that config is enabled, and toggling it then emits the
    /// `negation` flag (e.g. `--no-gpg-sign`) rather than dropping the positive
    /// one — so the user can override the configured default in either direction.
    pub fn negatable(
        key: impl Into<String>,
        arg: impl Into<String>,
        negation: impl Into<String>,
        config_key: impl Into<String>,
        description: impl Into<String>,
    ) -> Self {
        Switch {
            negation: Some(negation.into()),
            config_key: Some(config_key.into()),
            ..Switch::new(key, arg, description)
        }
    }
}

/// Where a value-reading [`Opt`] sources its autocomplete candidates. The
/// frontend turns these into picker choices; the user can always type a value
/// not in the list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Completion {
    /// No candidates — free text only.
    None,
    /// A fixed set of values; the user picks one (no free text), e.g. the
    /// commit-order flags.
    OneOf(&'static [&'static str]),
    /// An app-defined dynamic candidate source (e.g. repository authors or
    /// tracked file paths), loaded by the frontend, which matches the tag
    /// against the sources it knows how to resolve.
    Source(&'static str),
}

/// A value-reading option (magit's transient option, e.g. `-F` → `--grep=<x>`).
/// The frontend prompts for a value (with [`Completion`] candidates), stores it,
/// and passes `{arg}{value}` to git. `arg` carries any trailing `=` so both
/// long (`--grep=`) and short (`-G`) forms concatenate correctly.
#[derive(Debug, Clone, Copy)]
pub struct Opt {
    pub key: &'static str,
    pub arg: &'static str,
    pub description: &'static str,
    pub completion: Completion,
    /// A pathspec limit (`-- <value>`): emitted after the revision rather than
    /// as a `{arg}{value}` flag, so the frontend gathers it separately.
    pub pathspec: bool,
}

/// An invokable command (e.g. `p` → push). `C` is the app-defined operation the
/// action fires (Magritte's git `Command` enum). The description is dynamic so
/// the push/pull/fetch menus can name their resolved targets
/// (`master → origin/master`).
#[derive(Debug, Clone)]
pub struct Action<C> {
    pub key: &'static str,
    /// A second key that invokes the same action, shown as `key/also_key`. Used
    /// when push-remote and upstream collapse to one entry (they hit the same
    /// ref), so both `p` and `u` still work.
    pub also_key: Option<&'static str>,
    pub description: String,
    pub command: C,
    /// Whether the description is a concrete remote-tracking ref/remote (so the
    /// frontend colors it like one). False for placeholders ("…, setting it")
    /// and non-ref actions ("elsewhere", "all remotes").
    pub ref_label: bool,
}

impl<C> Action<C> {
    /// An action row — the one-per-menu-entry shorthand the builders use.
    pub fn suffix(key: &'static str, description: impl Into<String>, command: C) -> Suffix<C> {
        Suffix::Action(Action {
            key,
            also_key: None,
            description: description.into(),
            command,
            ref_label: false,
        })
    }

    /// A push/pull/fetch target row. `is_ref` marks a configured remote ref
    /// (colored like one); a placeholder label passes false.
    pub fn target(
        key: &'static str,
        description: impl Into<String>,
        command: C,
        is_ref: bool,
    ) -> Suffix<C> {
        Suffix::Action(Action {
            key,
            also_key: None,
            description: description.into(),
            command,
            ref_label: is_ref,
        })
    }

    /// A collapsed push-remote/upstream target invokable by either key
    /// (rendered `key/also`); always a configured ref.
    pub fn suffix_dual(
        key: &'static str,
        also: &'static str,
        description: impl Into<String>,
        command: C,
    ) -> Suffix<C> {
        Suffix::Action(Action {
            key,
            also_key: Some(also),
            description: description.into(),
            command,
            ref_label: true,
        })
    }
}

/// A keys-and-description row with no toggle state of its own (e.g. the rows of
/// the `?` dispatch menu). The frontend decides what a row does when invoked;
/// `keys` is the *current* binding (so the menu reflects remaps) and may list
/// several keystrokes (e.g. `g g`).
#[derive(Debug, Clone)]
pub struct Info {
    pub keys: String,
    /// Owned so user `[[command]]` titles (not `'static`) can appear too.
    pub description: String,
    /// Clicking the row dispatches `keys` (the `?` menu's rows). False for
    /// purely documentary rows — the vim cheat sheet, whose keys are modal
    /// editor input that the app keymap can't dispatch.
    pub clickable: bool,
}

/// A user-injected suffix (from the `[transient]` config): a key + label that
/// runs a registry command by id. Core stores the id opaquely; the frontend
/// resolves it against the command registry.
#[derive(Debug, Clone)]
pub struct Custom {
    pub key: String,
    pub description: String,
    pub id: String,
}

/// How a git-config [`Variable`] is set when its transient row is invoked.
#[derive(Debug, Clone)]
pub enum VariableKind {
    /// Cycle through a fixed set of values on each press, wrapping through
    /// "unset" back to the first (magit's `magit--git-variable:choices`).
    Choices {
        choices: Vec<String>,
        /// Another config key whose value stands in when this one is unset
        /// (e.g. `branch.<b>.rebase` falls back to `pull.rebase`). Shown for
        /// context; the frontend resolves it at open time.
        fallback: Option<String>,
        /// The value git assumes when this and the fallback are both unset.
        default: Option<String>,
    },
    /// Prompt for a free-text value (magit's plain `magit--git-variable`).
    Value { completion: Completion },
}

/// A git-config variable shown in a transient's "Configure" section (magit's
/// `magit-branch-configure` / `magit-remote-configure`): its current value is
/// displayed and set/cycled in place. Owned strings, since the config key has
/// the branch/remote scope substituted at build time.
#[derive(Debug, Clone)]
pub struct Variable {
    pub key: String,
    /// The fully-resolved config key, scope already substituted (e.g.
    /// `branch.main.rebase`).
    pub variable: String,
    pub description: String,
    pub kind: VariableKind,
    /// The current value read from git config when the transient opens; `None`
    /// when unset. The frontend fills and updates this in place.
    pub value: Option<String>,
    /// The resolved value of a [`VariableKind::Choices`] `fallback` key, shown
    /// when `value` is unset. Filled by the frontend at open time.
    pub fallback_value: Option<String>,
}

impl Variable {
    /// A cycling choice variable (magit's `[true|false]` rows).
    pub fn choices<C>(
        key: impl Into<String>,
        variable: impl Into<String>,
        description: impl Into<String>,
        choices: &[&str],
        fallback: Option<&str>,
        default: Option<&str>,
    ) -> Suffix<C> {
        Suffix::Variable(Variable {
            key: key.into(),
            variable: variable.into(),
            description: description.into(),
            kind: VariableKind::Choices {
                choices: choices.iter().map(|c| c.to_string()).collect(),
                fallback: fallback.map(str::to_string),
                default: default.map(str::to_string),
            },
            value: None,
            fallback_value: None,
        })
    }

    /// A cycling choice variable whose choices are computed at build time (e.g.
    /// the repo's remotes for a `pushRemote`/`pushDefault` row).
    pub fn choices_of<C>(
        key: impl Into<String>,
        variable: impl Into<String>,
        description: impl Into<String>,
        choices: Vec<String>,
        fallback: Option<&str>,
    ) -> Suffix<C> {
        Suffix::Variable(Variable {
            key: key.into(),
            variable: variable.into(),
            description: description.into(),
            kind: VariableKind::Choices {
                choices,
                fallback: fallback.map(str::to_string),
                default: None,
            },
            value: None,
            fallback_value: None,
        })
    }

    /// A free-text variable (prompt for the value).
    pub fn value<C>(
        key: impl Into<String>,
        variable: impl Into<String>,
        description: impl Into<String>,
        completion: Completion,
    ) -> Suffix<C> {
        Suffix::Variable(Variable {
            key: key.into(),
            variable: variable.into(),
            description: description.into(),
            kind: VariableKind::Value { completion },
            value: None,
            fallback_value: None,
        })
    }
}

#[derive(Debug, Clone)]
pub enum Suffix<C> {
    Switch(Switch),
    Action(Action<C>),
    Option(Opt),
    Info(Info),
    Custom(Custom),
    Variable(Variable),
}

#[derive(Debug, Clone)]
pub struct Group<C> {
    pub title: Vec<TitleSpan>,
    pub suffixes: Vec<Suffix<C>>,
}

/// A piece of a dialog title/prompt: plain text, or an accented name the
/// frontend styles distinctly so it stands out from the surrounding words
/// (e.g. the branch `main` in "Push main to").
#[derive(Debug, Clone)]
pub enum TitleSpan {
    Text(String),
    Accent(String),
}

impl TitleSpan {
    pub fn text(s: impl Into<String>) -> Self {
        TitleSpan::Text(s.into())
    }
    pub fn accent(s: impl Into<String>) -> Self {
        TitleSpan::Accent(s.into())
    }
}

/// A title that's a single run of plain text.
pub fn plain_title(s: impl Into<String>) -> Vec<TitleSpan> {
    vec![TitleSpan::Text(s.into())]
}

#[derive(Debug, Clone)]
pub struct Transient<C> {
    pub title: Vec<TitleSpan>,
    pub groups: Vec<Group<C>>,
}

impl<C> Transient<C> {
    /// All suffixes across all groups, flattened — the accessors below are
    /// filters over this.
    fn suffixes(&self) -> impl Iterator<Item = &Suffix<C>> {
        self.groups.iter().flat_map(|g| g.suffixes.iter())
    }

    fn suffixes_mut(&mut self) -> impl Iterator<Item = &mut Suffix<C>> {
        self.groups.iter_mut().flat_map(|g| g.suffixes.iter_mut())
    }

    /// All switches across all groups.
    pub fn switches(&self) -> impl Iterator<Item = &Switch> {
        self.suffixes().filter_map(|s| match s {
            Suffix::Switch(sw) => Some(sw),
            _ => None,
        })
    }

    /// All value-reading options across all groups.
    pub fn options(&self) -> impl Iterator<Item = &Opt> {
        self.suffixes().filter_map(|s| match s {
            Suffix::Option(o) => Some(o),
            _ => None,
        })
    }

    /// The option bound to `key`, if any.
    pub fn option_for(&self, key: &str) -> Option<&Opt> {
        self.options().find(|o| o.key == key)
    }

    /// The action bound to `key` (its primary or secondary key), if any.
    pub fn action_for(&self, key: &str) -> Option<&Action<C>> {
        self.suffixes().find_map(|s| match s {
            Suffix::Action(a) if a.key == key || a.also_key == Some(key) => Some(a),
            _ => None,
        })
    }

    /// The user-injected custom suffix bound to `key`, if any.
    pub fn custom_for(&self, key: &str) -> Option<&Custom> {
        self.suffixes().find_map(|s| match s {
            Suffix::Custom(c) if c.key == key => Some(c),
            _ => None,
        })
    }

    /// All git-config variables across all groups.
    pub fn variables_ref(&self) -> impl Iterator<Item = &Variable> {
        self.suffixes().filter_map(|s| match s {
            Suffix::Variable(v) => Some(v),
            _ => None,
        })
    }

    /// All git-config variables across all groups (mutable — the frontend fills
    /// their current values at open time and updates them on set).
    pub fn variables_mut(&mut self) -> impl Iterator<Item = &mut Variable> {
        self.suffixes_mut().filter_map(|s| match s {
            Suffix::Variable(v) => Some(v),
            _ => None,
        })
    }

    /// The config variable bound to `key`, if any.
    pub fn variable_for(&self, key: &str) -> Option<&Variable> {
        self.variables_ref().find(|v| v.key == key)
    }

    /// The config variable bound to `key`, mutably (to update its value in place
    /// after a set).
    pub fn variable_for_mut(&mut self, key: &str) -> Option<&mut Variable> {
        self.variables_mut().find(|v| v.key == key)
    }

    /// Whether some action/custom suffix key strictly extends `prefix` — i.e.
    /// the keystrokes typed so far could still resolve to a multi-key suffix
    /// (magit's `fu`/`pu` jump keys).
    pub fn has_key_prefix(&self, prefix: &str) -> bool {
        self.suffixes()
            .flat_map(|s| match s {
                Suffix::Action(a) => vec![Some(a.key), a.also_key],
                Suffix::Custom(c) => vec![Some(c.key.as_str())],
                _ => vec![],
            })
            .flatten()
            .any(|key| key.len() > prefix.len() && key.starts_with(prefix))
    }
}
