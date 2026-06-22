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
        "Gemfile" | "Rakefile" | "Guardfile" | "Podfile" | "Vagrantfile" | "Brewfile"
        | "Capfile" | "Berksfile" | "Fastfile" | "Appfile" => return Some("ruby"),
        ".bashrc" | ".bash_profile" | ".bash_aliases" | ".bash_logout" | ".profile" | ".zshrc"
        | ".zprofile" | ".zshenv" | ".zlogin" | ".zlogout" => return Some("bash"),
        ".eslintrc" | ".babelrc" | ".prettierrc" | ".swcrc" => return Some("json"),
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

/// Resolve a file's language, in priority order: an explicit modeline
/// (vim/emacs, head then tail) overrides everything; then extension/filename;
/// then a shebang sniff of the first line. `head`/`tail` are the first and
/// last chunk of the file's bytes (lossy UTF-8).
pub fn detect_language(path: &str, head: &str, tail: &str) -> Option<&'static str> {
    if let Some(lang) = detect_modeline(head).or_else(|| detect_modeline(tail)) {
        return Some(lang);
    }
    if let Some(lang) = language_for_path(path) {
        return Some(lang);
    }
    head.lines().next().and_then(language_from_shebang)
}

/// Find a vim or emacs modeline in `text`. Emacs takes precedence over vim.
fn detect_modeline(text: &str) -> Option<&'static str> {
    for line in text.lines() {
        if let Some(mode) = emacs_mode(line) {
            if let Some(lang) = lang_from_mode(mode) {
                return Some(lang);
            }
        }
    }
    for line in text.lines() {
        if let Some(ft) = vim_filetype(line) {
            if let Some(lang) = lang_from_mode(ft) {
                return Some(lang);
            }
        }
    }
    None
}

/// Extract `mode:` (or a bare mode) from an emacs `-*- ... -*-` modeline.
fn emacs_mode(line: &str) -> Option<&str> {
    let after = &line[line.find("-*-")? + 3..];
    let content = after[..after.find("-*-")?].trim();
    for part in content.split(';') {
        let part = part.trim();
        if let Some(v) = part.strip_prefix("mode:") {
            return Some(v.trim());
        }
    }
    // Bare form: `-*- python -*-`.
    if !content.is_empty() && !content.contains([':', ';']) {
        return Some(content);
    }
    None
}

/// Extract `ft=`/`filetype=` from a vim modeline (`vi:`/`vim:`/`ex:`), handling
/// both the bare and `set ...:` forms.
fn vim_filetype(line: &str) -> Option<&str> {
    let start = ["vim:", "vi:", "ex:"]
        .iter()
        .filter_map(|m| line.find(m).map(|i| i + m.len()))
        .min()?;
    let mut rest = line[start..].trim_start();
    rest = rest
        .strip_prefix("set ")
        .or_else(|| rest.strip_prefix("se "))
        .unwrap_or(rest);
    rest.split([' ', '\t', ':'])
        .find_map(|opt| opt.strip_prefix("filetype=").or_else(|| opt.strip_prefix("ft=")))
        .map(str::trim)
}

/// Map a vim filetype or emacs mode name to one of our highlighter languages.
fn lang_from_mode(name: &str) -> Option<&'static str> {
    let lower = name.trim().to_ascii_lowercase();
    let n = lower.strip_suffix("-mode").unwrap_or(lower.as_str());
    Some(match n {
        "python" | "python3" => "python",
        "ruby" | "enh-ruby" => "ruby",
        "rust" | "rustic" => "rust",
        "go" => "go",
        "c" => "c",
        "c++" | "cpp" => "cpp",
        "javascript" | "js" | "js2" | "node" => "javascript",
        "typescript" | "ts" => "typescript",
        "tsx" => "tsx",
        "sh" | "bash" | "shell-script" | "shell" => "bash",
        "css" | "scss" => "css",
        "html" | "web" | "mhtml" => "html",
        "yaml" => "yaml",
        "toml" | "conf-toml" => "toml",
        "json" | "js-json" => "json",
        "markdown" | "gfm" => "markdown",
        "java" => "java",
        "lua" => "lua",
        "php" => "php",
        "scala" => "scala",
        "sql" => "sql",
        "kotlin" => "kotlin",
        "elixir" => "elixir",
        "zig" => "zig",
        "makefile" | "make" | "gnumakefile" => "make",
        _ => return None,
    })
}

/// Detect a language from a shebang line (e.g. `#!/usr/bin/env python3`),
/// for files with no recognizable extension. Returns `None` for interpreters
/// we don't have a highlighter for.
pub fn language_from_shebang(line: &str) -> Option<&'static str> {
    let rest = line.strip_prefix("#!")?;
    let mut parts = rest.split_whitespace();
    let first = parts.next()?;
    // `#!/usr/bin/env python3` → take the next word; else the binary name.
    let interp = if first.rsplit('/').next() == Some("env") {
        parts.next()?
    } else {
        first.rsplit('/').next().unwrap_or(first)
    };
    // Strip a trailing version suffix only at the path level (python3 stays).
    Some(match interp {
        "python" | "python3" | "python2" => "python",
        "bash" | "sh" | "zsh" | "dash" | "ksh" => "bash",
        "ruby" => "ruby",
        "node" | "nodejs" | "deno" | "bun" => "javascript",
        "lua" | "luajit" => "lua",
        "php" => "php",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shebangs() {
        assert_eq!(language_from_shebang("#!/usr/bin/env python3"), Some("python"));
        assert_eq!(language_from_shebang("#!/bin/bash"), Some("bash"));
        assert_eq!(language_from_shebang("#! /usr/bin/ruby"), Some("ruby"));
        assert_eq!(language_from_shebang("#!/usr/bin/env node"), Some("javascript"));
        assert_eq!(language_from_shebang("not a shebang"), None);
        assert_eq!(language_from_shebang("#!/usr/bin/perl"), None); // unsupported
    }

    #[test]
    fn vim_modelines() {
        assert_eq!(vim_filetype("# vim: set ft=python ts=4 et:"), Some("python"));
        assert_eq!(vim_filetype("// vim: ft=rust"), Some("rust"));
        assert_eq!(vim_filetype("/* vi: set filetype=cpp: */"), Some("cpp"));
        assert_eq!(vim_filetype("no modeline here"), None);
    }

    #[test]
    fn emacs_modelines() {
        assert_eq!(emacs_mode("# -*- mode: python; tab-width: 4 -*-"), Some("python"));
        assert_eq!(emacs_mode("/* -*- c++ -*- */"), Some("c++"));
        assert_eq!(emacs_mode("plain line"), None);
    }

    #[test]
    fn modeline_overrides_extension() {
        // A .txt file declaring python via emacs/vim modeline.
        assert_eq!(detect_language("notes.txt", "-*- mode: python -*-\n", ""), Some("python"));
        assert_eq!(detect_language("notes.txt", "x = 1\n# vim: ft=ruby\n", ""), Some("ruby"));
        // No modeline: fall back to extension.
        assert_eq!(detect_language("a.rs", "fn main() {}", ""), Some("rust"));
        // Emacs wins over vim when both present.
        assert_eq!(
            detect_language("x", "-*- mode: go -*-\n# vim: ft=ruby\n", ""),
            Some("go")
        );
    }

    #[test]
    fn special_filenames_and_shebang_fallback() {
        assert_eq!(language_for_path("Makefile"), Some("make"));
        assert_eq!(language_for_path("config/.bashrc"), Some("bash"));
        assert_eq!(language_for_path("Vagrantfile"), Some("ruby"));
        // Extensionless, no modeline → shebang.
        assert_eq!(detect_language("bin/runme", "#!/bin/sh\necho hi\n", ""), Some("bash"));
    }
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
