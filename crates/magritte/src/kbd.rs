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

fn is_modifier(token: &str) -> bool {
    matches!(
        token,
        "cmd" | "super" | "meta" | "ctrl" | "control" | "alt" | "opt" | "option" | "shift"
    )
}

/// Validate a user `[keymap]` keystroke spec, returning a human-readable reason
/// if it's malformed — an empty step, a `+`-joined chord (we use `-`), or an
/// unknown modifier prefix. Lenient about a literal `-` key (empty segments from
/// splitting are ignored), so `cmd--` (⌘ and minus) is accepted. `None` = valid.
pub(crate) fn keystroke_error(key: &str) -> Option<String> {
    if key.is_empty() {
        return Some("empty keystroke".to_string());
    }
    for step in key.split(' ') {
        if step.is_empty() {
            return Some(format!("\"{key}\": empty step (stray space?)"));
        }
        if step.contains('+') {
            return Some(format!(
                "\"{step}\": join modifiers with '-' (e.g. ctrl-x), not '+'"
            ));
        }
        // Every segment but the last is a modifier prefix; the last is the key.
        // Empty segments (from a literal `-` key) are ignored.
        let segs: Vec<&str> = step.split('-').collect();
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

/// Whether a `-`-split keystroke is a chord: ≥2 parts where every part but the
/// last is a modifier (`ctrl-d`, `cmd-shift-x`), vs. a lone key or a literal
/// `-`.
fn is_chord(parts: &[&str]) -> bool {
    parts.len() >= 2 && parts[..parts.len() - 1].iter().all(|p| is_modifier(p))
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
/// *chord* prefixes its modifier glyphs to the key (`cmd-enter` → `⌘⏎`). A lone
/// token is word-ified (`tab` → `⇥`).
pub(crate) fn format_keys(key: &str) -> String {
    if key.contains(' ') {
        return key
            .split(' ')
            .map(format_keys)
            .collect::<Vec<_>>()
            .join(" ");
    }
    let parts: Vec<&str> = key.split('-').collect();
    if is_chord(&parts) {
        // Concatenated, macOS-style (`⌘⇧x`) — no `+` between modifier and key.
        parts.iter().map(|p| key_word(p)).collect()
    } else {
        key_word(key)
    }
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
    let parts: Vec<&str> = step.split('-').collect();
    let (label, glyph_font) = if is_chord(&parts) {
        (parts.iter().map(|p| key_word(p)).collect::<String>(), true)
    } else {
        let label = key_word(step);
        let glyph = is_glyph(&label);
        (label, glyph)
    };
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
    use super::keystroke_error;

    #[test]
    fn keystroke_validation() {
        // Valid: plain keys, sequences, chords, and the literal minus key.
        for ok in [
            "g",
            "K",
            "g r",
            "ctrl-x ctrl-c",
            "cmd-enter",
            "-",
            "cmd--",
            "ctrl-space",
        ] {
            assert!(keystroke_error(ok).is_none(), "{ok} should be valid");
        }
        // Malformed: empty, `+` join, unknown modifier, stray space.
        assert!(keystroke_error("").is_some());
        assert!(keystroke_error("cmd+x").is_some());
        assert!(keystroke_error("kmd-x").is_some());
        assert!(keystroke_error("g  r").is_some());
    }
}
