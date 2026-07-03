//! Keycap (kbd) chips and keystroke label formatting — turning a keystroke spec
//! like `g r`, `cmd-enter`, or `-a` into the bordered badges shown throughout the
//! chrome (menus, transients, hints).

use gpui::prelude::*;
use gpui::{div, px, AnyElement, Hsla, SharedString};

use crate::with_alpha;

/// The Return keycap glyph (`⏎`). It reads better than "Ret" but many
/// monospace fonts render it thin/tofu, so keycaps draw it in the system UI
/// font (passed as `ui_font`) — not the user's configured UI font, which a
/// custom display face could also lack.
pub(crate) const RETURN_GLYPH: &str = "⏎";

/// Spell out one keystroke token as a word label. Modifier and named keys
/// become words (`Cmd`, `Esc`, `Tab`) — or the `⏎` glyph for Return — rather
/// than the macOS modifier glyphs, which render poorly in our monospace chrome.
/// Plain letters keep their case (`F` vs `f`) so case alone distinguishes the
/// shifted key — no `Shift` shown.
fn key_word(token: &str) -> String {
    match token {
        "cmd" | "super" | "meta" => "Cmd".into(),
        "ctrl" | "control" => "Ctrl".into(),
        "alt" | "opt" | "option" => "Opt".into(),
        "shift" => "Shift".into(),
        "enter" | "return" => RETURN_GLYPH.into(),
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
/// space-separated (e.g. `g r`, `ctrl-x ctrl-c`) with each step formatted in
/// turn. A *chord* joins modifiers to a key with `-` (e.g. `cmd-enter` →
/// `Cmd+Enter`). A lone token is word-ified (`tab` → `Tab`).
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
        parts
            .iter()
            .map(|p| key_word(p))
            .collect::<Vec<_>>()
            .join("+")
    } else {
        key_word(key)
    }
}

/// A keyboard key badge: one keycap per key. A chord renders each modifier and
/// the key as separate caps joined by `+` (`[Ctrl]+[g]`); a sequence renders
/// each step spaced (`[g] [r]`, `[Ctrl]+[x] [Ctrl]+[c]`). `font` is the
/// monospace family.
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

/// One keystroke step (possibly a chord) as caps joined by `+`. The `⏎` Return
/// glyph is drawn in the UI font (it's thin/tofu in many monospace fonts);
/// every other cap stays monospace so keys read as keys.
fn chord_caps(step: &str, color: Hsla, font: &SharedString, ui_font: &SharedString) -> gpui::Div {
    let parts: Vec<&str> = step.split('-').collect();
    let labels: Vec<String> = if is_chord(&parts) {
        parts.iter().map(|p| key_word(p)).collect()
    } else {
        vec![key_word(step)]
    };
    let mut row = div().flex().items_center().gap(px(2.0));
    for (i, label) in labels.into_iter().enumerate() {
        if i > 0 {
            row = row.child(div().text_color(color).child(SharedString::from("+")));
        }
        let cap = if label == RETURN_GLYPH {
            chip_box(color, font).child(
                div()
                    .font_family(ui_font.clone())
                    .child(SharedString::from(label)),
            )
        } else {
            chip_box(color, font).child(SharedString::from(label))
        };
        row = row.child(cap);
    }
    row
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
