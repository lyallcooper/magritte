//! Keycap (kbd) chips and keystroke label formatting — turning a keystroke spec
//! like `g r`, `cmd-enter`, or `-a` into the bordered badges shown throughout the
//! chrome (menus, transients, hints).

use gpui::prelude::*;
use gpui::{div, px, AnyElement, Hsla, SharedString};

use crate::with_alpha;

/// Spell out one keystroke token as a word label. Modifier and named keys
/// become words (`Cmd`, `Enter`, `Esc`, `Tab`) rather than the macOS glyphs,
/// which render poorly in our monospace chrome. Plain letters keep their case
/// (`F` vs `f`) so case alone distinguishes the shifted key — no `Shift` shown.
fn key_word(token: &str) -> String {
    match token {
        "cmd" | "super" | "meta" => "Cmd".into(),
        "ctrl" | "control" => "Ctrl".into(),
        "alt" | "opt" | "option" => "Opt".into(),
        "shift" => "Shift".into(),
        "enter" | "return" => "Return".into(),
        "esc" | "ESC" | "escape" => "Esc".into(),
        "tab" | "TAB" => "Tab".into(),
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
/// space-separated (e.g. `g r`, `c c`) and rendered with the keys spaced inside
/// one keycap. A *chord* joins modifiers to a key with `-` (e.g. `cmd-enter` →
/// `Cmd+Enter`). A lone token is word-ified (`tab` → `Tab`).
pub(crate) fn format_keys(key: &str) -> String {
    if key.contains(' ') {
        return key.split(' ').map(key_word).collect::<Vec<_>>().join(" ");
    }
    let parts: Vec<&str> = key.split('-').collect();
    let is_chord = parts.len() >= 2 && parts[..parts.len() - 1].iter().all(|p| is_modifier(p));
    if is_chord {
        parts
            .iter()
            .map(|p| key_word(p))
            .collect::<Vec<_>>()
            .join("+")
    } else {
        key_word(key)
    }
}

/// A keyboard key badge: a keycap chip with a word-style label (see
/// [`format_keys`]). `font` is the monospace family.
pub(crate) fn key_chip(key: &str, color: Hsla, font: &SharedString) -> AnyElement {
    chip_box(color, font)
        .child(SharedString::from(format_keys(key)))
        .into_any_element()
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
