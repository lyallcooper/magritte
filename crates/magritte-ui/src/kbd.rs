//! Keycap (kbd) chips and keystroke label formatting — turning a keystroke spec
//! like `g r`, `cmd-enter`, or `-a` into the bordered badges shown throughout the
//! chrome (menus, transients, hints).

use gpui::prelude::*;
use gpui::{div, px, AnyElement, Hsla, SharedString};

use crate::with_alpha;

/// Keycap glyphs (`⏎` Return, `⇥` Tab, `⌫` Backspace). They read better than the
/// words but many monospace fonts render them thin/tofu, so keycaps draw them in
/// the system UI font (passed as `ui_font`) — not the user's configured UI font,
/// which a custom display face could also lack.
pub const RETURN_GLYPH: &str = "⏎";
pub const TAB_GLYPH: &str = "⇥";
pub const BACKSPACE_GLYPH: &str = "⌫";
/// Modifier glyphs (`⌘` Cmd, `⌥` Opt, `⌃` Ctrl, `⇧` Shift) — the standard macOS
/// key symbols, shown prefixed to the key (`⌘x`) rather than as `Cmd+x`.
pub const CMD_GLYPH: &str = "⌘";
pub const OPT_GLYPH: &str = "⌥";
pub const CTRL_GLYPH: &str = "⌃";
pub const SHIFT_GLYPH: &str = "⇧";

/// Whether a rendered label is one of the symbol glyphs drawn in the UI font
/// rather than the monospace keycap font (they're thin/tofu in many mono fonts).
fn is_glyph(label: &str) -> bool {
    label == RETURN_GLYPH || label == TAB_GLYPH || label == BACKSPACE_GLYPH
}

/// Spell out one keystroke base key as a label. Return/Tab/Backspace become
/// `⏎`/`⇥`/`⌫`; the other named keys spell out (`Esc`, `Space`). Plain letters
/// keep their case (`F` vs `f`) so case alone distinguishes the shifted key.
fn key_word(token: &str) -> String {
    match token {
        "enter" | "return" => RETURN_GLYPH.into(),
        "tab" => TAB_GLYPH.into(),
        "backspace" => BACKSPACE_GLYPH.into(),
        "esc" | "escape" => "Esc".into(),
        "space" => "Space".into(),
        "delete" => "Del".into(),
        _ => token.to_string(),
    }
}

/// The runtime name of a named key, accepting the common aliases (`Esc`,
/// `Ret`, `SPC`, …) case-insensitively; `None` for anything else. The one
/// table behind both [`normalize_key_name`] and [`is_known_key`].
fn named_key(base: &str) -> Option<&'static str> {
    match base.to_ascii_lowercase().as_str() {
        "esc" | "escape" => Some("escape"),
        "ret" | "return" | "enter" => Some("enter"),
        "spc" | "space" => Some("space"),
        "tab" => Some("tab"),
        "bs" | "backspace" => Some("backspace"),
        "del" | "delete" => Some("delete"),
        "up" => Some("up"),
        "down" => Some("down"),
        "left" => Some("left"),
        "right" => Some("right"),
        "home" => Some("home"),
        "end" => Some("end"),
        "pageup" => Some("pageup"),
        "pagedown" => Some("pagedown"),
        "insert" => Some("insert"),
        _ => None,
    }
}

/// Normalize a base key name to the runtime form the app matches against
/// (`escape`/`enter`/`space`/…), accepting the common aliases (`Esc`, `Ret`,
/// `SPC`, …). Single-character keys are returned verbatim, case preserved, so
/// `K` stays distinct from `k`.
pub fn normalize_key_name(base: &str) -> String {
    named_key(base).unwrap_or(base).to_string()
}

/// Whether a base token names a key we recognize: a single character, a named
/// key (including its aliases), or a function key (`f1`..`f12`).
fn is_known_key(base: &str) -> bool {
    if base.chars().count() == 1 || named_key(base).is_some() {
        return true;
    }
    base.to_ascii_lowercase()
        .strip_prefix('f')
        .and_then(|n| n.parse::<u8>().ok())
        .is_some_and(|n| (1..=12).contains(&n))
}

/// The four keyboard modifiers, plus a lenient name parser. We accept the common
/// spellings (and the glyphs) case-insensitively so `Cmd`/`Command`/`super` and
/// `⌘` all mean the same key.
#[derive(Clone, Copy)]
enum Modifier {
    Cmd,
    Ctrl,
    Alt,
    Shift,
}

fn parse_modifier(token: &str) -> Option<Modifier> {
    match token.to_ascii_lowercase().as_str() {
        "cmd" | "command" | "super" | "meta" | CMD_GLYPH => Some(Modifier::Cmd),
        "ctrl" | "control" | CTRL_GLYPH => Some(Modifier::Ctrl),
        "alt" | "opt" | "option" | OPT_GLYPH => Some(Modifier::Alt),
        "shift" | SHIFT_GLYPH => Some(Modifier::Shift),
        _ => None,
    }
}

fn is_modifier(token: &str) -> bool {
    parse_modifier(token).is_some()
}

/// The modifier flags a keystroke step carries.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct Mods {
    pub cmd: bool,
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
}

impl Mods {
    /// Whether this key carries at least one modifier.
    pub fn any(self) -> bool {
        self.cmd || self.ctrl || self.alt || self.shift
    }
}

/// Peel the leading modifier tokens off one keystroke step, returning the flags
/// and the remaining base key. Modifiers may be joined with `-` or `+` and named
/// in any accepted spelling (`cmd-`, `Command+`, `⌥`); a trailing separator is
/// treated as the literal key (so `cmd--` is ⌘ plus minus).
pub fn parse_step(step: &str) -> (Mods, &str) {
    let mut rest = step;
    let mut mods = Mods::default();
    while let Some(idx) = rest.find(['-', '+']) {
        let head = &rest[..idx];
        let after = &rest[idx + 1..];
        // A separator with nothing after it is the literal `-`/`+` key.
        if after.is_empty() {
            break;
        }
        match parse_modifier(head) {
            Some(Modifier::Cmd) => mods.cmd = true,
            Some(Modifier::Ctrl) => mods.ctrl = true,
            Some(Modifier::Alt) => mods.alt = true,
            Some(Modifier::Shift) => mods.shift = true,
            None => break,
        }
        rest = after;
    }
    (mods, rest)
}

/// Canonical keystroke string for a keypress: word modifier prefixes (`cmd-`,
/// `ctrl-`, `alt-`, in that order) followed by the key. Shift folds into a
/// printable character (`k` -> `K`, `1` -> `!`); for named keys it remains an
/// explicit prefix (`shift-tab`).
pub fn chord(key: &str, shift: bool, ctrl: bool, alt: bool, cmd: bool) -> String {
    let (base, shift_prefix) = match (shift, shifted_char(key)) {
        (true, Some(c)) => (c, false),
        (true, None) => (key.to_string(), true),
        (false, _) => (key.to_string(), false),
    };
    let mut s = String::new();
    if cmd {
        s.push_str("cmd-");
    }
    if ctrl {
        s.push_str("ctrl-");
    }
    if alt {
        s.push_str("alt-");
    }
    if shift_prefix {
        s.push_str("shift-");
    }
    s.push_str(&base);
    s
}

/// Canonicalize a configured keystroke into the same form [`chord`] emits at
/// runtime. Modifier names and separators are lenient, and named-key aliases
/// (`Esc`, `Ret`, `SPC`) normalize to their runtime names.
pub fn canonical_keystroke(key: &str) -> String {
    key.split(' ')
        .map(|step| {
            let (mods, base) = parse_step(step);
            let base = normalize_key_name(base);
            chord(&base, mods.shift, mods.ctrl, mods.alt, mods.cmd)
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// The character produced by Shift on a US keyboard. `None` means Shift does
/// not reshape the key, as with Tab, Space, Escape, and arrows.
fn shifted_char(key: &str) -> Option<String> {
    let shifted = match key {
        "1" => "!",
        "2" => "@",
        "3" => "#",
        "4" => "$",
        "5" => "%",
        "6" => "^",
        "7" => "&",
        "8" => "*",
        "9" => "(",
        "0" => ")",
        "-" => "_",
        "=" => "+",
        "[" => "{",
        "]" => "}",
        "\\" => "|",
        ";" => ":",
        "'" => "\"",
        "," => "<",
        "." => ">",
        "/" => "?",
        "`" => "~",
        _ if key.len() == 1 && key.chars().all(|c| c.is_ascii_alphabetic()) => {
            return Some(key.to_uppercase());
        }
        // A platform may report an already-shifted symbol rather than the
        // underlying physical key. Shift was how it was typed, so it folds away.
        _ if key.len() == 1 && !key.chars().all(|c| c.is_ascii_alphanumeric()) => {
            return Some(key.to_string());
        }
        _ => return None,
    };
    Some(shifted.to_string())
}

/// Validate a user `[keymap]` keystroke spec, returning a human-readable reason
/// if it's malformed — an empty step, an unknown modifier prefix, or a base that
/// names no key (so `abc` is rejected, `esc`/`ctrl-tab`/`cmd-N` are not).
/// Modifiers may be joined with `-` or `+` and spelled loosely (`Cmd`, `⌘`).
/// Lenient about a literal `-`/`+` key. `None` = valid.
pub fn keystroke_error(key: &str) -> Option<String> {
    if key.is_empty() {
        return Some("empty keystroke".to_string());
    }
    for step in key.split(' ') {
        if step.is_empty() {
            return Some(format!("\"{key}\": empty step (stray space?)"));
        }
        // Every segment but the last is a modifier prefix; the last is the key.
        // Empty segments (from a literal `-`/`+` key) are ignored.
        let segs: Vec<&str> = step.split(['-', '+']).collect();
        for (i, seg) in segs.iter().enumerate() {
            let is_last = i == segs.len() - 1;
            if is_last || seg.is_empty() {
                continue;
            }
            if !is_modifier(seg) {
                return Some(format!("\"{step}\": unknown modifier \"{seg}\""));
            }
        }
        let (_, base) = parse_step(step);
        if !is_known_key(base) {
            return Some(format!("\"{step}\": \"{base}\" is not a key"));
        }
    }
    None
}

/// Whether a base key is a shifted letter — an uppercase ASCII letter, which in
/// our canonical form encodes Shift (`cmd-N` is ⌘ + Shift + n).
fn is_shifted_letter(base: &str) -> bool {
    base.len() == 1 && base.chars().all(|c| c.is_ascii_uppercase())
}

/// One keystroke step decomposed for display: the modifier-glyph prefix (in
/// macOS order ⌃⌥⇧⌘), the base key's label, and whether that label is itself a
/// symbol glyph. Shift shows explicitly when combined with another modifier
/// (`cmd-N` → prefix `⇧⌘`, base `N`); a lone shifted letter keeps encoding Shift
/// by its case (`N`, no `⇧`).
struct StepParts {
    prefix: String,
    base: String,
    base_is_glyph: bool,
}

fn step_parts(step: &str) -> StepParts {
    let (mut mods, base) = parse_step(step);
    let has_other = mods.cmd || mods.ctrl || mods.alt;
    if has_other && is_shifted_letter(base) {
        mods.shift = true;
    }
    let mut prefix = String::new();
    if mods.ctrl {
        prefix.push_str(CTRL_GLYPH);
    }
    if mods.alt {
        prefix.push_str(OPT_GLYPH);
    }
    if mods.shift {
        prefix.push_str(SHIFT_GLYPH);
    }
    if mods.cmd {
        prefix.push_str(CMD_GLYPH);
    }
    let label = key_word(base);
    let base_is_glyph = is_glyph(&label);
    StepParts {
        prefix,
        base: label,
        base_is_glyph,
    }
}

/// The keycap chip shell: a bordered, tinted rounded box. Callers fill in the
/// label (or, for switches, a multi-span label). The border makes adjacent
/// chips read as distinct keys rather than blending together. `font` is the
/// monospace family — keys read as keys, never the proportional UI font.
pub fn chip_box(color: Hsla, font: &SharedString) -> gpui::Div {
    div()
        .px(px(5.0))
        .min_w(px(18.0))
        .flex()
        .justify_center()
        .text_center()
        .rounded(px(3.0))
        .border_1()
        .border_color(with_alpha(color, 0.45))
        .text_color(color)
        .font_family(font.clone())
        .bg(with_alpha(color, 0.12))
}

/// The display label for a keystroke spec. A multi-keystroke *sequence* is
/// space-separated (e.g. `g r`, `⌃x ⌃c`) with each step formatted in turn. A
/// *chord* prefixes its modifier glyphs to the key in macOS order (`cmd-enter` →
/// `⌘⏎`, `cmd-N` → `⇧⌘N`). A lone token is word-ified (`tab` → `⇥`).
pub fn format_keys(key: &str) -> String {
    key.split(' ')
        .map(|step| {
            let p = step_parts(step);
            format!("{}{}", p.prefix, p.base)
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// A keyboard key badge: one keycap per keystroke *step*. A chord is a single
/// cap with the modifier glyphs prefixed to the key (`[⌘x]`, `[⌃⏎]`); a sequence
/// renders each step spaced (`[g] [r]`, `[⌃x] [⌃c]`). A `·` step draws as a
/// bare dot instead of a cap — the separator between alternative bindings in
/// one label (`ZZ · :wq`), needed since a space already means "next step".
/// Vim-mode sequences arrive unspaced (`ZZ`, `gq`) and render as one cap, in
/// vim notation; app keymap sequences stay one cap per keystroke (`g r`).
/// `font` is the monospace family; `ui_font` draws the glyphs.
pub fn key_chip(key: &str, color: Hsla, font: &SharedString, ui_font: &SharedString) -> AnyElement {
    let mut row = div().flex().items_center().gap(px(4.0));
    for step in key.split(' ') {
        if step == "·" {
            row = row.child(
                div()
                    .text_color(with_alpha(color, 0.6))
                    .child(SharedString::from("·")),
            );
        } else {
            row = row.child(chord_caps(step, color, font, ui_font));
        }
    }
    row.into_any_element()
}

/// One keystroke step as a single keycap. The modifier glyphs (`⌘/⌥/⌃/⇧`) draw
/// in the UI font — they're thin/tofu in many monospace faces — but the key
/// itself stays in the monospace keycap font so `⌘x` reads with the `x` in the
/// user's font. A lone symbol key (`⏎`/`⇥`/`⌫`) still uses the UI font.
fn chord_caps(step: &str, color: Hsla, font: &SharedString, ui_font: &SharedString) -> gpui::Div {
    let parts = step_parts(step);
    let mut cap = chip_box(color, font);
    if !parts.prefix.is_empty() {
        cap = cap.child(
            div()
                .font_family(ui_font.clone())
                .child(SharedString::from(parts.prefix)),
        );
    }
    let base = if parts.base_is_glyph {
        div()
            .font_family(ui_font.clone())
            .child(SharedString::from(parts.base))
    } else {
        // Inherit the chip's monospace font so the key reads as a key.
        div().child(SharedString::from(parts.base))
    };
    cap.child(base)
}

/// A switch keycap (`-a`). When a `-` prefix is pending (we're awaiting the
/// switch letter), only the dash *inside* the keycap changes color to the
/// accent, while the keycap itself stays neutral (magit's prefix feedback).
pub fn switch_chip(
    key: &str,
    color: Hsla,
    accent: Hsla,
    pending: bool,
    font: &SharedString,
) -> AnyElement {
    let rest = key.strip_prefix('-').unwrap_or(key);
    let dash_color = if pending { accent } else { color };
    chip_box(color, font)
        .child(div().text_color(dash_color).child(SharedString::from("-")))
        .child(
            div()
                .text_color(color)
                .child(SharedString::from(rest.to_string())),
        )
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::{canonical_keystroke, chord, format_keys, keystroke_error};

    #[test]
    fn keystroke_validation() {
        // Valid: plain keys, sequences, chords (either separator), loose modifier
        // and key spellings, function keys, and the literal minus/plus keys.
        for ok in [
            "g",
            "K",
            "g r",
            "ctrl-x ctrl-c",
            "cmd-enter",
            "cmd+x",
            "Command-N",
            "Cmd+Shift+n",
            "esc",
            "Escape",
            "Ret",
            "SPC",
            "shift-tab",
            "cmd-escape",
            "f5",
            "-",
            "cmd--",
            "cmd-+",
            "ctrl-space",
        ] {
            assert!(keystroke_error(ok).is_none(), "{ok} should be valid");
        }
        // Malformed: empty, unknown modifier, multi-char non-key, stray space.
        assert!(keystroke_error("").is_some());
        assert!(keystroke_error("kmd-x").is_some());
        assert!(keystroke_error("abc").is_some());
        assert!(keystroke_error("cmd-abc").is_some());
        assert!(keystroke_error("f13").is_some());
        assert!(keystroke_error("g  r").is_some());
    }

    #[test]
    fn display_labels() {
        // Modifier + Shift shows an explicit ⇧, in macOS order (⌃⌥⇧⌘).
        assert_eq!(format_keys("cmd-N"), "⇧⌘N");
        assert_eq!(format_keys("cmd-ctrl-alt-x"), "⌃⌥⌘x");
        assert_eq!(format_keys("cmd-enter"), "⌘⏎");
        assert_eq!(format_keys("shift-tab"), "⇧⇥");
        // A lone shifted letter keeps encoding Shift by its case — no ⇧.
        assert_eq!(format_keys("K"), "K");
        // Esc spells out (no glyph); other named/word keys word-ify.
        assert_eq!(format_keys("escape"), "Esc");
        assert_eq!(format_keys("ctrl-x ctrl-c"), "⌃x ⌃c");
        assert_eq!(format_keys("tab"), "⇥");
    }

    #[test]
    fn chords_and_config_specs_share_a_canonical_form() {
        assert_eq!(canonical_keystroke("Cmd+N"), "cmd-N");
        assert_eq!(canonical_keystroke("command-n"), "cmd-n");
        assert_eq!(canonical_keystroke("Cmd+Shift+n"), "cmd-N");
        assert_eq!(canonical_keystroke("Control+Option+x"), "ctrl-alt-x");
        assert_eq!(canonical_keystroke("cmd--"), "cmd--");
        assert_eq!(canonical_keystroke("g Ret"), "g enter");
        assert_eq!(chord("n", true, false, false, true), "cmd-N");
        assert_eq!(chord("tab", true, false, false, false), "shift-tab");
    }
}
