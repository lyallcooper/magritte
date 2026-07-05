//! Keycap (kbd) chips and keystroke label formatting — turning a keystroke spec
//! like `g r`, `cmd-enter`, or `-a` into the bordered badges shown throughout the
//! chrome (menus, transients, hints).

use gpui::prelude::*;
use gpui::{div, px, AnyElement, Hsla, SharedString};

use crate::with_alpha;

/// Keycap glyphs (`⏎` Return, `⇥` Tab, `⎋` Esc, `⌫` Backspace). They read better
/// than the words but many monospace fonts render them thin/tofu, so keycaps
/// draw them in the system UI font (passed as `ui_font`) — not the user's
/// configured UI font, which a custom display face could also lack.
pub(crate) const RETURN_GLYPH: &str = "⏎";
pub(crate) const TAB_GLYPH: &str = "⇥";
pub(crate) const ESC_GLYPH: &str = "⎋";
pub(crate) const BACKSPACE_GLYPH: &str = "⌫";
/// Modifier glyphs (`⌘` Cmd, `⌥` Opt, `⌃` Ctrl, `⇧` Shift) — the standard macOS
/// key symbols, shown prefixed to the key (`⌘x`) rather than as `Cmd+x`.
pub(crate) const CMD_GLYPH: &str = "⌘";
pub(crate) const OPT_GLYPH: &str = "⌥";
pub(crate) const CTRL_GLYPH: &str = "⌃";
pub(crate) const SHIFT_GLYPH: &str = "⇧";

/// Whether a rendered label is one of the symbol glyphs drawn in the UI font
/// rather than the monospace keycap font (they're thin/tofu in many mono fonts).
fn is_glyph(label: &str) -> bool {
    label == RETURN_GLYPH || label == TAB_GLYPH || label == ESC_GLYPH || label == BACKSPACE_GLYPH
}

/// Spell out one keystroke token as a label. Modifiers become the macOS glyphs
/// (`⌘`/`⌥`/`⌃`/`⇧`); Return/Tab/Esc/Backspace become `⏎`/`⇥`/`⎋`/`⌫`. Plain
/// letters keep their case (`F` vs `f`) so case alone distinguishes the shifted
/// key — no `⇧` shown for those.
fn key_word(token: &str) -> String {
    match token {
        "cmd" | "super" | "meta" => CMD_GLYPH.into(),
        "ctrl" | "control" => CTRL_GLYPH.into(),
        "alt" | "opt" | "option" => OPT_GLYPH.into(),
        "shift" => SHIFT_GLYPH.into(),
        "enter" | "return" => RETURN_GLYPH.into(),
        "esc" | "ESC" | "escape" => ESC_GLYPH.into(),
        "tab" | "TAB" => TAB_GLYPH.into(),
        "backspace" | "delete" => BACKSPACE_GLYPH.into(),
        "space" => "Space".into(),
        _ => token.to_string(),
    }
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
#[derive(Clone, Copy, Default)]
pub(crate) struct Mods {
    pub(crate) cmd: bool,
    pub(crate) ctrl: bool,
    pub(crate) alt: bool,
    pub(crate) shift: bool,
}

impl Mods {
    fn any(&self) -> bool {
        self.cmd || self.ctrl || self.alt || self.shift
    }
}

/// Peel the leading modifier tokens off one keystroke step, returning the flags
/// and the remaining base key. Modifiers may be joined with `-` or `+` and named
/// in any accepted spelling (`cmd-`, `Command+`, `⌥`); a trailing separator is
/// treated as the literal key (so `cmd--` is ⌘ plus minus).
pub(crate) fn parse_step(step: &str) -> (Mods, &str) {
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

/// Validate a user `[keymap]` keystroke spec, returning a human-readable reason
/// if it's malformed — an empty step or an unknown modifier prefix. Modifiers may
/// be joined with `-` or `+` and spelled loosely (`Cmd`, `Command`, `⌘`). Lenient
/// about a literal `-`/`+` key (empty segments from splitting are ignored), so
/// `cmd--` (⌘ and minus) is accepted. `None` = valid.
pub(crate) fn keystroke_error(key: &str) -> Option<String> {
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
    }
    None
}

/// Whether a base key is a shifted letter — an uppercase ASCII letter, which in
/// our canonical form encodes Shift (`cmd-N` is ⌘ + Shift + n).
fn is_shifted_letter(base: &str) -> bool {
    base.len() == 1 && base.chars().all(|c| c.is_ascii_uppercase())
}

/// The display label for one keystroke step, plus whether it must render in the
/// UI font (it holds a modifier or symbol glyph). Modifiers are ordered in the
/// macOS sequence (⌃⌥⇧⌘) and Shift shows explicitly when combined with another
/// modifier (`cmd-N` → `⇧⌘N`); a lone shifted letter keeps encoding Shift by its
/// case (`N`, no `⇧`).
fn step_label(step: &str) -> (String, bool) {
    let (mut mods, base) = parse_step(step);
    let has_other = mods.cmd || mods.ctrl || mods.alt;
    if has_other && is_shifted_letter(base) {
        mods.shift = true;
    }
    if !mods.any() {
        let label = key_word(base);
        let is_glyph = is_glyph(&label);
        return (label, is_glyph);
    }
    let mut s = String::new();
    if mods.ctrl {
        s.push_str(CTRL_GLYPH);
    }
    if mods.alt {
        s.push_str(OPT_GLYPH);
    }
    if mods.shift {
        s.push_str(SHIFT_GLYPH);
    }
    if mods.cmd {
        s.push_str(CMD_GLYPH);
    }
    s.push_str(&key_word(base));
    (s, true)
}

/// The keycap chip shell: a bordered, tinted rounded box. Callers fill in the
/// label (or, for switches, a multi-span label). The border makes adjacent
/// chips read as distinct keys rather than blending together. `font` is the
/// monospace family — keys read as keys, never the proportional UI font.
pub(crate) fn chip_box(color: Hsla, font: &SharedString) -> gpui::Div {
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
pub(crate) fn format_keys(key: &str) -> String {
    key.split(' ')
        .map(|step| step_label(step).0)
        .collect::<Vec<_>>()
        .join(" ")
}

/// A keyboard key badge: one keycap per keystroke *step*. A chord is a single
/// cap with the modifier glyphs prefixed to the key (`[⌘x]`, `[⌃⏎]`); a sequence
/// renders each step spaced (`[g] [r]`, `[⌃x] [⌃c]`). `font` is the monospace
/// family; `ui_font` draws the glyphs.
pub(crate) fn key_chip(
    key: &str,
    color: Hsla,
    font: &SharedString,
    ui_font: &SharedString,
) -> AnyElement {
    let mut row = div().flex().items_center().gap(px(4.0));
    for step in key.split(' ') {
        row = row.child(chord_caps(step, color, font, ui_font));
    }
    row.into_any_element()
}

/// One keystroke step as a single keycap. A chord prefixes its modifier glyphs
/// to the key (`⌘x`), rendered in the UI font since it holds the `⌘/⌥/⌃/⇧`
/// glyphs; a lone symbol key (`⏎`/`⇥`/`⎋`/`⌫`) likewise uses the UI font. Any
/// other lone key stays monospace so keys read as keys.
fn chord_caps(step: &str, color: Hsla, font: &SharedString, ui_font: &SharedString) -> gpui::Div {
    let (label, glyph_font) = step_label(step);
    if glyph_font {
        chip_box(color, font).child(
            div()
                .font_family(ui_font.clone())
                .child(SharedString::from(label)),
        )
    } else {
        chip_box(color, font).child(SharedString::from(label))
    }
}

/// A switch keycap (`-a`). When a `-` prefix is pending (we're awaiting the
/// switch letter), only the dash *inside* the keycap changes color to the
/// accent, while the keycap itself stays neutral (magit's prefix feedback).
pub(crate) fn switch_chip(
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
    use super::{format_keys, keystroke_error};

    #[test]
    fn keystroke_validation() {
        // Valid: plain keys, sequences, chords (either separator), loose modifier
        // spellings, and the literal minus/plus keys.
        for ok in [
            "g",
            "K",
            "g r",
            "ctrl-x ctrl-c",
            "cmd-enter",
            "cmd+x",
            "Command-N",
            "Cmd+Shift+n",
            "-",
            "cmd--",
            "cmd-+",
            "ctrl-space",
        ] {
            assert!(keystroke_error(ok).is_none(), "{ok} should be valid");
        }
        // Malformed: empty, unknown modifier, stray space.
        assert!(keystroke_error("").is_some());
        assert!(keystroke_error("kmd-x").is_some());
        assert!(keystroke_error("g  r").is_some());
    }

    #[test]
    fn display_labels() {
        // Modifier + Shift shows an explicit ⇧, in macOS order (⌃⌥⇧⌘).
        assert_eq!(format_keys("cmd-N"), "⇧⌘N");
        assert_eq!(format_keys("cmd-ctrl-alt-x"), "⌃⌥⌘x");
        assert_eq!(format_keys("cmd-enter"), "⌘⏎");
        // A lone shifted letter keeps encoding Shift by its case — no ⇧.
        assert_eq!(format_keys("K"), "K");
        // Sequences format each step; symbol/word keys word-ify.
        assert_eq!(format_keys("ctrl-x ctrl-c"), "⌃x ⌃c");
        assert_eq!(format_keys("tab"), "⇥");
    }

    #[test]
    fn canonicalizes_loose_specs() {
        use crate::commands::canonical_keystroke;
        assert_eq!(canonical_keystroke("Cmd+N"), "cmd-N");
        assert_eq!(canonical_keystroke("command-n"), "cmd-n");
        assert_eq!(canonical_keystroke("Cmd+Shift+n"), "cmd-N");
        assert_eq!(canonical_keystroke("Control+Option+x"), "ctrl-alt-x");
        assert_eq!(canonical_keystroke("cmd--"), "cmd--");
        assert_eq!(canonical_keystroke("g r"), "g r");
    }
}
