//! The render palette: resolved colors for one frame, derived from
//! gpui-component's active theme so the chrome matches the Input/Kbd/Icon
//! widgets (light or dark), with a neutral-light fallback before the first
//! theme resolution.

use gpui::Hsla;
use gpui_component::ActiveTheme;

use crate::*;

/// Resolved colors for one render, derived from gpui-component's active theme
/// so the chrome matches the Input/Kbd/Icon widgets (light or dark).
#[derive(Clone, Copy)]
pub(crate) struct Palette {
    pub(crate) bg: Hsla,
    pub(crate) fg: Hsla,
    pub(crate) dim: Hsla,
    pub(crate) border: Hsla,
    pub(crate) selection: Hsla,
    pub(crate) hover: Hsla,
    pub(crate) visual: Hsla,
    pub(crate) section: Hsla,
    pub(crate) hunk: Hsla,
    pub(crate) panel: Hsla,
    pub(crate) modified: Hsla,
    pub(crate) added: Hsla,
    pub(crate) removed: Hsla,
    pub(crate) added_bg: Hsla,
    pub(crate) removed_bg: Hsla,
    pub(crate) banner: Hsla,
    /// Local branch names in ref decorations, magit's blue `branch-local` face
    /// (the title bar's current-branch chip is the header anchor; inline ref
    /// decorations use this color instead).
    pub(crate) branch_local: Hsla,
    /// Remote-tracking refs (`origin/main`), magit's green `branch-remote` face.
    pub(crate) branch_remote: Hsla,
    /// Tag names (`v0.4.0`), magit's yellow `tag` face.
    pub(crate) tag: Hsla,
}

impl Palette {
    pub(crate) fn from_theme(cx: &App) -> Self {
        let t = cx.theme();
        // Diff/status colors come from the highlight theme's git status colors
        // (created/deleted/modified → success/error/warning), not the base
        // semantic tokens: many themes (e.g. Solarized) leave the base tokens
        // muted and put the vivid git colors in the highlight block. These
        // accessors fall back to the base tokens when a theme omits them.
        // Every face is read directly from the theme — the app never blends
        // colors at runtime. Translucent overlays (the visual-mode region, the
        // diff line bands, the warning banner) carry their alpha in the theme's
        // hex (`#rrggbbaa`), so they're read verbatim too.
        let status = &t.highlight_theme.style.status;
        Palette {
            bg: t.background,
            fg: t.foreground,
            dim: t.muted_foreground,
            border: t.border,
            selection: t.accent, // accent.background — selected row
            hover: t.list_hover, // list.hover.background
            visual: t.selection, // selection.background (translucent)
            section: t.primary,
            hunk: status.info(cx),
            panel: t.secondary, // elevated surface for the panel
            modified: status.warning(cx),
            added: status.success(cx),
            removed: status.error(cx),
            added_bg: status.success_background(cx),
            removed_bg: status.error_background(cx),
            banner: status.warning_background(cx),
            // Ref colors follow magit's faces: remote branches green, tags
            // yellow. They share the theme's success/warning hues (as the diff
            // added/modified colors do) — context disambiguates a ref from a
            // diff line.
            branch_local: t.primary,
            branch_remote: status.success(cx),
            tag: status.warning(cx),
        }
    }
}

impl Default for Palette {
    fn default() -> Self {
        let g = |v: u32| gpui::rgb(v).into();
        let ga = |v: u32| gpui::rgba(v).into();
        Palette {
            bg: g(0xffffff),
            fg: g(0x1a1a1a),
            dim: g(0x8a8a8a),
            border: g(0xe2e2e2),
            selection: g(0xeaeaea),
            hover: g(0xf5f5f5),
            visual: ga(0x007aff52),
            section: g(0x2f6feb),
            hunk: g(0x6f42c1),
            panel: g(0xf6f6f6),
            modified: g(0xb08800),
            added: g(0x1a7f37),
            removed: g(0xcf222e),
            added_bg: ga(0x1a7f371f),
            removed_bg: ga(0xcf222e1f),
            banner: ga(0xb088002e),
            branch_local: g(0x2f6feb),
            branch_remote: g(0x1a7f37),
            tag: g(0xb08800),
        }
    }
}
