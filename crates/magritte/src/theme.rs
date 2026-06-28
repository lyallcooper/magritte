//! Theme and font resolution: discovering selectable font families, mapping the
//! config's appearance/theme choices onto gpui-component's theme system, and
//! registering our bundled theme sets. The settings screen and the view's
//! font/appearance plumbing call in here.

use gpui::{App, SharedString, WindowAppearance};
// Only the non-macOS system-font fallbacks read `cx.theme()`; on macOS those are
// literals, so the trait would be unused there.
#[cfg(not(target_os = "macos"))]
use gpui_component::ActiveTheme;

use crate::config;

/// Label for the font-picker entry that follows the OS default monospace.
pub(crate) const SYSTEM_FONT_LABEL: &str = "System Default";
/// Label for the UI-font entry that reuses the monospace font — the default, so
/// the UI stays all-monospace until you opt into a proportional UI.
pub(crate) const UI_FONT_DEFAULT_LABEL: &str = "Same as monospace";
/// Config sentinel (and the "System Default" UI-font entry) for the platform's
/// proportional system UI font, distinct from an empty value (= monospace).
pub(crate) const SYSTEM_UI_FONT: &str = "system-ui";

/// All monospace font families available to the text system, sorted.
/// Membership is decided by the font's own monospace trait as reported by the
/// OS (CoreText's `kCTFontMonoSpaceTrait`) rather than by measuring glyph
/// widths — the trait reliably excludes symbol fonts (e.g. Webdings) and
/// proportional CJK fonts whose Latin glyphs happen to be equal-width, both of
/// which fooled the old width heuristic.
pub(crate) fn monospace_font_names(cx: &App) -> Vec<SharedString> {
    let mut names: Vec<SharedString> = cx
        .text_system()
        .all_font_names()
        .into_iter()
        // Skip dot-prefixed system/fallback tokens (".SystemUIFont", ".ZedSans",
        // ".ZedMono", …). They aren't user-selectable families, and probing them
        // by name makes CoreText log "should use CTFontCreateUIFontForLanguage".
        .filter(|name| !name.starts_with('.') && is_monospace_font(name))
        .map(SharedString::from)
        .collect();
    names.sort_by_key(|f| f.to_lowercase());
    names.dedup();
    names
}

/// All selectable font families (for the proportional UI-font picker), sorted.
/// Unlike [`monospace_font_names`] this keeps proportional families too.
pub(crate) fn all_font_names(cx: &App) -> Vec<SharedString> {
    let mut names: Vec<SharedString> = cx
        .text_system()
        .all_font_names()
        .into_iter()
        .filter(|name| !name.starts_with('.'))
        .map(SharedString::from)
        .collect();
    names.sort_by_key(|f| f.to_lowercase());
    names.dedup();
    names
}

/// Whether a font family declares the monospace trait to the OS font system.
#[cfg(target_os = "macos")]
fn is_monospace_font(name: &str) -> bool {
    use core_text::font::new_from_name;
    use core_text::font_descriptor::SymbolicTraitAccessors;
    new_from_name(name, 12.0)
        .map(|font| font.symbolic_traits().is_monospace())
        .unwrap_or(false)
}

#[cfg(not(target_os = "macos"))]
fn is_monospace_font(_name: &str) -> bool {
    // No OS trait query wired up off macOS (not a current target).
    true
}

/// Whether the system appearance is currently dark.
fn system_is_dark(cx: &App) -> bool {
    matches!(
        cx.window_appearance(),
        WindowAppearance::Dark | WindowAppearance::VibrantDark
    )
}

/// The effective theme mode for a config: forced light/dark, or the system's
/// appearance when set to "auto".
fn effective_mode(cfg: &config::Config, cx: &App) -> gpui_component::ThemeMode {
    match cfg.appearance.as_str() {
        "light" => gpui_component::ThemeMode::Light,
        "dark" => gpui_component::ThemeMode::Dark,
        _ if system_is_dark(cx) => gpui_component::ThemeMode::Dark,
        _ => gpui_component::ThemeMode::Light,
    }
}

/// Point the theme's light/dark slots at the config's chosen themes and switch
/// to the effective mode (following the system when appearance is "auto").
pub(crate) fn apply_appearance(cfg: &config::Config, cx: &mut App) {
    let registry = gpui_component::ThemeRegistry::global(cx);
    // Fall back to our default theme when the configured name isn't found —
    // e.g. a config referencing a theme we've since dropped — rather than
    // leaving gpui-component's built-in default.
    let pick = |name: &str, fallback: &str| {
        registry
            .themes()
            .get(name)
            .or_else(|| registry.themes().get(fallback))
            .cloned()
    };
    let light = pick(cfg.light_theme(), config::DEFAULT_LIGHT_THEME);
    let dark = pick(cfg.dark_theme(), config::DEFAULT_DARK_THEME);
    {
        let theme = gpui_component::Theme::global_mut(cx);
        if let Some(t) = light {
            theme.light_theme = t;
        }
        if let Some(t) = dark {
            theme.dark_theme = t;
        }
    }
    gpui_component::Theme::change(effective_mode(cfg, cx), None, cx);
}

/// Config *values* worth warning about on load: an unknown appearance mode, or a
/// theme name that isn't in the registry. (Keymap and transient problems are
/// reported separately by `build_keymap`.) Each bad value still falls back
/// safely — `apply_appearance` uses the defaults — so this only tells the user
/// their setting was ignored rather than changing behavior.
pub(crate) fn config_value_warnings(cfg: &config::Config, cx: &App) -> Vec<String> {
    let mut warnings = Vec::new();
    match cfg.appearance.as_str() {
        "" | "auto" | "light" | "dark" => {}
        other => warnings.push(format!(
            "config: unknown appearance \"{other}\" (expected auto, light, or dark)"
        )),
    }
    let registry = gpui_component::ThemeRegistry::global(cx);
    for (field, name) in [
        ("light_theme", &cfg.light_theme),
        ("dark_theme", &cfg.dark_theme),
    ] {
        if !name.is_empty() && registry.themes().get(name.as_str()).is_none() {
            warnings.push(format!("config: unknown {field} \"{name}\""));
        }
    }
    warnings
}

/// The platform's system monospace UI font. On macOS this is the SF Mono-based
/// `.AppleSystemUIFontMonospaced` (what `NSFont.monospacedSystemFont` returns),
/// which Apple does not expose as a normal selectable font family.
#[cfg(target_os = "macos")]
fn system_mono_font(_cx: &App) -> SharedString {
    SharedString::from(".AppleSystemUIFontMonospaced")
}
#[cfg(not(target_os = "macos"))]
fn system_mono_font(cx: &App) -> SharedString {
    cx.theme().mono_font_family.clone()
}

/// The platform's system proportional UI font (the analog of
/// [`system_mono_font`]): `.AppleSystemUIFont` on macOS, else the theme's.
#[cfg(target_os = "macos")]
fn system_ui_font(_cx: &App) -> SharedString {
    SharedString::from(".AppleSystemUIFont")
}
#[cfg(not(target_os = "macos"))]
fn system_ui_font(cx: &App) -> SharedString {
    cx.theme().font_family.clone()
}

/// The monospace font family to render with: the user's configured choice, or
/// the platform's system monospace UI font when unset (the "System Default"
/// font-picker entry, stored as an empty config value so it stays adaptive).
pub(crate) fn resolve_font(cfg: &config::Config, cx: &App) -> SharedString {
    if cfg.font.is_empty() {
        system_mono_font(cx)
    } else {
        SharedString::from(cfg.font.clone())
    }
}

/// The UI font for prose chrome (menus, headings, labels): empty reuses the
/// monospace [`resolve_font`] (the default, so nothing changes until opted in),
/// the [`SYSTEM_UI_FONT`] sentinel uses the platform proportional font, and any
/// other value is a chosen family.
pub(crate) fn resolve_ui_font(cfg: &config::Config, cx: &App) -> SharedString {
    match cfg.ui_font.as_str() {
        "" => resolve_font(cfg, cx),
        SYSTEM_UI_FONT => system_ui_font(cx),
        name => SharedString::from(name.to_string()),
    }
}

/// Our own theme sets, embedded at compile time. Each file is a `ThemeSet` of
/// light/dark `ThemeConfig`s authored against the official palettes (replacing
/// gpui-component's bundled themes, which were loose ports).
const BUNDLED_THEMES: &[&str] = &[
    include_str!("../themes/github.json"),
    include_str!("../themes/solarized.json"),
    include_str!("../themes/selenized.json"),
    include_str!("../themes/gruvbox.json"),
    include_str!("../themes/catppuccin.json"),
    include_str!("../themes/nord.json"),
    include_str!("../themes/dracula.json"),
    include_str!("../themes/tao.json"),
];

/// Register our bundled theme sets into the global registry at startup.
pub(crate) fn register_bundled_themes(cx: &mut App) {
    let registry = gpui_component::ThemeRegistry::global_mut(cx);
    for set in BUNDLED_THEMES {
        if let Err(e) = registry.load_themes_from_str(set) {
            eprintln!("magritte: failed to load a bundled theme set: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every face `Palette::from_theme` (and the chrome) reads. If a bundled
    /// theme omits one, gpui-component silently falls back to a default color
    /// and the theme looks subtly wrong — this catches that at build time.
    #[test]
    fn bundled_themes_cover_every_face() {
        // Keys read out of the `colors` block.
        const COLORS: &[&str] = &[
            "background",
            "foreground",
            "muted.foreground",
            "border",
            "accent.background",     // selected row
            "list.hover.background", // hover wash
            "selection.background",  // visual-mode region
            "primary.background",    // section headings
            "secondary.background",  // elevated panel
            "base.red",
            "base.green",
            "base.yellow",
            "base.blue",
        ];
        // Keys read out of the `highlight` block (git status faces). `warning`
        // is the "modified" text color, which OVERRIDES base.yellow when set,
        // so it must be present and deliberate (not inherited).
        const HIGHLIGHT: &[&str] = &[
            "warning",
            "success.background", // added line band
            "error.background",   // removed line band
            "warning.background", // banner
        ];
        for set in BUNDLED_THEMES {
            let v: serde_json::Value =
                serde_json::from_str(set).expect("bundled theme is valid JSON");
            let themes = v["themes"].as_array().expect("theme set has `themes`");
            assert!(!themes.is_empty(), "theme set has no themes");
            for theme in themes {
                let name = theme["name"].as_str().unwrap_or("<unnamed>");
                let colors = theme["colors"]
                    .as_object()
                    .unwrap_or_else(|| panic!("{name}: no `colors` block"));
                for key in COLORS {
                    assert!(
                        colors.contains_key(*key),
                        "theme {name:?} is missing colors.{key}"
                    );
                }
                let highlight = theme["highlight"]
                    .as_object()
                    .unwrap_or_else(|| panic!("{name}: no `highlight` block"));
                for key in HIGHLIGHT {
                    assert!(
                        highlight.contains_key(*key),
                        "theme {name:?} is missing highlight.{key}"
                    );
                }
            }
        }
    }
}
