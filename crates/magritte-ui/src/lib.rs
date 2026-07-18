//! App-agnostic UI toolkit shared by Magritte-family apps: picker, keystroke,
//! and commit-text primitives. Nothing in this crate knows about git — the
//! app supplies its own domain types and semantics.

pub mod commit_text;
pub mod generation;
pub mod kbd;
pub mod picker;

use gpui::Hsla;

pub fn with_alpha(mut color: Hsla, alpha: f32) -> Hsla {
    color.a = alpha;
    color
}
