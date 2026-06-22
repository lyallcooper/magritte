//! Syntax highlighting for diff lines via gpui-component's tree-sitter
//! highlighter.
//!
//! For each hunk we reconstruct the new-side text (context + added lines) and
//! old-side text (context + removed lines) as contiguous blocks, highlight each
//! once, and map the styled runs back to individual diff lines. This preserves
//! within-hunk context (so multi-token constructs highlight correctly) while
//! keeping the cost to two parses per hunk. Computed once at diff-load time and
//! cached, so rendering/scrolling never re-parses.

use std::collections::HashMap;
use std::ops::Range;

use gpui::{App, Hsla};
use gpui_component::highlighter::SyntaxHighlighter;
use gpui_component::{ActiveTheme, Rope};
use magritte_core::{FileDiff, LineKind};

/// A run of text with a resolved color.
pub type Span = (String, Hsla);

/// Highlighted spans for each `(hunk_index, line_index)` of a file diff.
pub type FileHighlights = HashMap<(usize, usize), Vec<Span>>;

/// Map a file path to a tree-sitter language name, or `None` if we don't
/// highlight it. Names must match a language enabled in gpui-component's
/// `tree-sitter-languages` feature (and accepted by `Language::from_name`).
pub fn language_for_path(path: &str) -> Option<&'static str> {
    // NOTE: gpui-component ships grammars for ~35 languages but registers some
    // (swift, csharp, graphql, proto, cmake) with EMPTY highlight queries at
    // this rev — those parse but produce no colors, so we don't map them here
    // (they'd just render as plain text either way).
    let name = path.rsplit('/').next().unwrap_or(path);
    // Special filenames that carry no useful extension.
    match name {
        "Makefile" | "makefile" | "GNUmakefile" => return Some("make"),
        "Gemfile" | "Rakefile" | "Guardfile" | "Podfile" => return Some("ruby"),
        _ => {}
    }
    let ext = name.rsplit('.').next().unwrap_or("");
    Some(match ext {
        "rs" => "rust",
        "js" | "jsx" | "mjs" | "cjs" => "javascript",
        "ts" | "mts" | "cts" => "typescript",
        "tsx" => "tsx",
        "py" | "pyi" => "python",
        "go" => "go",
        "c" | "h" => "c",
        "cc" | "cpp" | "cxx" | "hpp" | "hxx" | "hh" | "c++" => "cpp",
        "css" | "scss" => "css",
        "html" | "htm" => "html",
        "yaml" | "yml" => "yaml",
        "toml" => "toml",
        "sh" | "bash" | "zsh" => "bash",
        "rb" => "ruby",
        "md" | "markdown" | "mdx" => "markdown",
        "java" => "java",
        "json" | "jsonc" => "json",
        "astro" => "astro",
        "diff" | "patch" => "diff",
        "ejs" => "ejs",
        "ex" | "exs" => "elixir",
        "erb" => "erb",
        "kt" | "kts" | "ktm" => "kotlin",
        "lua" => "lua",
        "mk" => "make",
        "php" | "php3" | "php4" | "php5" | "phtml" => "php",
        "scala" | "sc" | "sbt" => "scala",
        "sql" => "sql",
        "svelte" => "svelte",
        "zig" => "zig",
        _ => return None,
    })
}

/// Highlight every line of a file diff. `default` is the fallback text color
/// for unstyled spans (context, gaps between tokens).
pub fn highlight_diff(file: &FileDiff, lang: &str, cx: &App, default: Hsla) -> FileHighlights {
    let theme = cx.theme();
    let hl_theme = &theme.highlight_theme;
    let mut highlighter = SyntaxHighlighter::new(lang);
    let mut out = FileHighlights::new();

    for (hunk_ix, hunk) in file.hunks.iter().enumerate() {
        // Build the new- and old-side blocks, recording each line's byte range.
        let mut new_block = String::new();
        let mut old_block = String::new();
        // (line_index_in_hunk, on_new_side, byte_range_in_block)
        let mut placements: Vec<(usize, bool, Range<usize>)> = Vec::new();

        for (line_ix, line) in hunk.lines.iter().enumerate() {
            match line.kind {
                LineKind::Context => {
                    let s = new_block.len();
                    new_block.push_str(&line.content);
                    placements.push((line_ix, true, s..new_block.len()));
                    new_block.push('\n');
                    old_block.push_str(&line.content);
                    old_block.push('\n');
                }
                LineKind::Added => {
                    let s = new_block.len();
                    new_block.push_str(&line.content);
                    placements.push((line_ix, true, s..new_block.len()));
                    new_block.push('\n');
                }
                LineKind::Removed => {
                    let s = old_block.len();
                    old_block.push_str(&line.content);
                    placements.push((line_ix, false, s..old_block.len()));
                    old_block.push('\n');
                }
                LineKind::NoNewline => {}
            }
        }

        // Parse each side once, then slice per line.
        let mut parsed_new = false;
        let mut parsed_old = false;
        for (line_ix, on_new, range) in placements {
            let block = if on_new {
                if !parsed_new {
                    highlighter.update(None, &Rope::from(new_block.as_str()), None);
                    parsed_new = true;
                    parsed_old = false;
                }
                &new_block
            } else {
                if !parsed_old {
                    highlighter.update(None, &Rope::from(old_block.as_str()), None);
                    parsed_old = true;
                    parsed_new = false;
                }
                &old_block
            };
            let spans = line_spans(&highlighter, block, &range, hl_theme, default);
            out.insert((hunk_ix, line_ix), spans);
        }
    }
    out
}

/// Resolve the styled runs for one line's byte range, filling gaps with the
/// default color so all text is rendered.
fn line_spans(
    highlighter: &SyntaxHighlighter,
    block: &str,
    range: &Range<usize>,
    hl_theme: &gpui_component::highlighter::HighlightTheme,
    default: Hsla,
) -> Vec<Span> {
    let mut runs = highlighter.styles(range, hl_theme);
    runs.sort_by_key(|(r, _)| r.start);

    let mut spans: Vec<Span> = Vec::new();
    let mut pos = range.start;
    let push = |text: Option<&str>, color: Hsla, spans: &mut Vec<Span>| {
        if let Some(t) = text {
            if !t.is_empty() {
                spans.push((t.to_string(), color));
            }
        }
    };

    for (r, style) in runs {
        let s = r.start.max(range.start);
        let e = r.end.min(range.end);
        if s >= e {
            continue;
        }
        if s > pos {
            push(block.get(pos..s), default, &mut spans);
        }
        push(block.get(s..e), style.color.unwrap_or(default), &mut spans);
        pos = e;
    }
    if pos < range.end {
        push(block.get(pos..range.end), default, &mut spans);
    }
    spans
}
