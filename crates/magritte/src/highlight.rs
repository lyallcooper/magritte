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
use std::rc::Rc;
use std::ops::Range;
use std::sync::Once;

use gpui::{App, Hsla};
use gpui_component::highlighter::{LanguageConfig, LanguageRegistry, SyntaxHighlighter};
use gpui_component::{ActiveTheme, Rope};
use magritte_core::{FileDiff, LineKind};

/// A run of text with a resolved color.
pub type Span = (String, Hsla);

/// Highlighted spans for each `(hunk_index, line_index)` of a file diff.
/// Values are shared (`Rc`) with the row model, so a rebuild clones a handle
/// per line instead of re-copying every span.
pub type FileHighlights = HashMap<(usize, usize), Rc<[Span]>>;

fn register_extra_highlight_queries() {
    static REGISTER: Once = Once::new();

    REGISTER.call_once(|| {
        let registry = LanguageRegistry::singleton();
        registry.register(
            "swift",
            &LanguageConfig::new(
                "swift",
                tree_sitter_swift::LANGUAGE.into(),
                vec![],
                tree_sitter_swift::HIGHLIGHTS_QUERY,
                tree_sitter_swift::INJECTIONS_QUERY,
                tree_sitter_swift::LOCALS_QUERY,
            ),
        );
        registry.register(
            "csharp",
            &LanguageConfig::new(
                "csharp",
                tree_sitter_c_sharp::LANGUAGE.into(),
                vec![],
                tree_sitter_c_sharp::HIGHLIGHTS_QUERY,
                "",
                "",
            ),
        );
        registry.register(
            "proto",
            &LanguageConfig::new(
                "proto",
                tree_sitter_proto::LANGUAGE.into(),
                vec![],
                include_str!("highlight_queries/proto.scm"),
                "",
                "",
            ),
        );
        registry.register(
            "kotlin",
            &LanguageConfig::new(
                "kotlin",
                tree_sitter_kotlin_sg::LANGUAGE.into(),
                vec![],
                include_str!("highlight_queries/kotlin.scm"),
                "",
                "",
            ),
        );
        registry.register(
            "cmake",
            &LanguageConfig::new(
                "cmake",
                tree_sitter_cmake::LANGUAGE.into(),
                vec![],
                include_str!("highlight_queries/cmake.scm"),
                "",
                "",
            ),
        );
    });
}

/// Map a file path to a tree-sitter language name, or `None` if we don't
/// highlight it. Names must match a language enabled in gpui-component's
/// `tree-sitter-languages` feature (and accepted by `Language::from_name`).
pub fn language_for_path(path: &str) -> Option<&'static str> {
    let name = path.rsplit('/').next().unwrap_or(path);
    // Special filenames that carry no useful extension.
    match name {
        "CMakeLists.txt" => return Some("cmake"),
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
        "cs" => "csharp",
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
        "proto" => "proto",
        "scala" | "sc" | "sbt" => "scala",
        "sql" => "sql",
        "svelte" => "svelte",
        "swift" => "swift",
        "cmake" => "cmake",
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
        .find_map(|opt| {
            opt.strip_prefix("filetype=")
                .or_else(|| opt.strip_prefix("ft="))
        })
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
        "csharp" | "c#" | "cs" => "csharp",
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
        "proto" | "protobuf" => "proto",
        "scala" => "scala",
        "sql" => "sql",
        "kotlin" => "kotlin",
        "elixir" => "elixir",
        "swift" => "swift",
        "cmake" => "cmake",
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

/// A diff larger than this (total lines across hunks) is rendered as plain
/// text rather than syntax-highlighted, so a huge file never blocks the UI
/// thread parsing it. Highlighting runs in the foreground (the gpui-component
/// tree-sitter highlighter isn't trivially `Send`), so a cap is how we keep
/// expanding a big file responsive.
const MAX_HIGHLIGHT_LINES: usize = 2000;

/// Highlight every line of a file diff. `default` is the fallback text color
/// for unstyled spans (context, gaps between tokens). Returns an empty map for
/// diffs over [`MAX_HIGHLIGHT_LINES`], so the caller falls back to plain text.
pub fn highlight_diff(file: &FileDiff, lang: &str, cx: &App, default: Hsla) -> FileHighlights {
    let total_lines: usize = file.hunks.iter().map(|h| h.lines.len()).sum();
    if total_lines > MAX_HIGHLIGHT_LINES {
        return FileHighlights::new();
    }
    register_extra_highlight_queries();
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

        // Parse each side exactly once, slicing every line on that side before
        // switching. `SyntaxHighlighter::update` replaces the parse tree, so we
        // must group by side — interleaving would re-parse on every transition.
        let has_new = placements.iter().any(|(_, on_new, _)| *on_new);
        let has_old = placements.iter().any(|(_, on_new, _)| !*on_new);
        if has_new {
            highlighter.update(None, &Rope::from(new_block.as_str()), None);
            for (line_ix, on_new, range) in &placements {
                if *on_new {
                    let spans = line_spans(&highlighter, &new_block, range, hl_theme, default);
                    out.insert((hunk_ix, *line_ix), Rc::from(spans));
                }
            }
        }
        if has_old {
            highlighter.update(None, &Rope::from(old_block.as_str()), None);
            for (line_ix, on_new, range) in &placements {
                if !*on_new {
                    let spans = line_spans(&highlighter, &old_block, range, hl_theme, default);
                    out.insert((hunk_ix, *line_ix), Rc::from(spans));
                }
            }
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

/// Read the first and last ~1 KB of a file (lossy UTF-8) for modeline/shebang
/// detection. Returns empty strings on error.
pub(crate) fn file_head_tail(path: &std::path::Path) -> (String, String) {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut file) = std::fs::File::open(path) else {
        return (String::new(), String::new());
    };
    let mut head = [0u8; 1024];
    let hn = file.read(&mut head).unwrap_or(0);
    // Tail: only when the file is larger than the head we already read.
    let mut tail = [0u8; 1024];
    let tn = match file.seek(SeekFrom::End(-1024)) {
        Ok(_) => file.read(&mut tail).unwrap_or(0),
        Err(_) => 0,
    };
    (
        String::from_utf8_lossy(&head[..hn]).into_owned(),
        String::from_utf8_lossy(&tail[..tn]).into_owned(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shebangs() {
        assert_eq!(
            language_from_shebang("#!/usr/bin/env python3"),
            Some("python")
        );
        assert_eq!(language_from_shebang("#!/bin/bash"), Some("bash"));
        assert_eq!(language_from_shebang("#! /usr/bin/ruby"), Some("ruby"));
        assert_eq!(
            language_from_shebang("#!/usr/bin/env node"),
            Some("javascript")
        );
        assert_eq!(language_from_shebang("not a shebang"), None);
        assert_eq!(language_from_shebang("#!/usr/bin/perl"), None); // unsupported
    }

    #[test]
    fn vim_modelines() {
        assert_eq!(
            vim_filetype("# vim: set ft=python ts=4 et:"),
            Some("python")
        );
        assert_eq!(vim_filetype("// vim: ft=rust"), Some("rust"));
        assert_eq!(vim_filetype("/* vi: set filetype=cpp: */"), Some("cpp"));
        assert_eq!(vim_filetype("no modeline here"), None);
    }

    #[test]
    fn emacs_modelines() {
        assert_eq!(
            emacs_mode("# -*- mode: python; tab-width: 4 -*-"),
            Some("python")
        );
        assert_eq!(emacs_mode("/* -*- c++ -*- */"), Some("c++"));
        assert_eq!(emacs_mode("plain line"), None);
    }

    #[test]
    fn modeline_overrides_extension() {
        // A .txt file declaring python via emacs/vim modeline.
        assert_eq!(
            detect_language("notes.txt", "-*- mode: python -*-\n", ""),
            Some("python")
        );
        assert_eq!(
            detect_language("notes.txt", "x = 1\n# vim: ft=ruby\n", ""),
            Some("ruby")
        );
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
        assert_eq!(language_for_path("CMakeLists.txt"), Some("cmake"));
        assert_eq!(language_for_path("config/.bashrc"), Some("bash"));
        assert_eq!(language_for_path("Vagrantfile"), Some("ruby"));
        // Extensionless, no modeline → shebang.
        assert_eq!(
            detect_language("bin/runme", "#!/bin/sh\necho hi\n", ""),
            Some("bash")
        );
    }

    #[test]
    fn extra_language_paths_and_modelines() {
        assert_eq!(language_for_path("Sources/App.swift"), Some("swift"));
        assert_eq!(language_for_path("Program.cs"), Some("csharp"));
        assert_eq!(language_for_path("schema/service.proto"), Some("proto"));
        assert_eq!(language_for_path("cmake/toolchain.cmake"), Some("cmake"));
        assert_eq!(detect_language("x", "// -*- mode: csharp -*-\n", ""), Some("csharp"));
        assert_eq!(detect_language("x", "// vim: ft=proto\n", ""), Some("proto"));
    }

    #[test]
    fn extra_highlight_queries_produce_styles() {
        register_extra_highlight_queries();
        for (lang, sample) in [
            ("swift", "let greeting = \"hello\"\n"),
            ("csharp", "public class Program { static void Main() {} }\n"),
            ("proto", "syntax = \"proto3\";\nmessage User { string name = 1; }\n"),
            ("cmake", "cmake_minimum_required(VERSION 3.20)\nproject(Magritte)\n"),
            ("kotlin", "data class User(val name: String)\nfun main() = println(\"hello\")\n"),
            ("kotlin", "    val greeting = \"hello\"\n    println(greeting)\n}\n\nsealed interface AddedState\nobject Added : AddedState\n"),
        ] {
            let mut highlighter = SyntaxHighlighter::new(lang);
            highlighter.update(None, &Rope::from(sample), None);
            let styles = highlighter.styles(&(0..sample.len()), &gpui_component::highlighter::HighlightTheme::default_dark());
            assert!(
                styles.iter().any(|(_, style)| style.color.is_some()),
                "{lang} should produce at least one colored span (highlighter={}, spans={})",
                highlighter.language(),
                styles.len()
            );
        }
    }

    #[test]
    fn kotlin_diff_fragment_added_lines_are_highlighted() {
        register_extra_highlight_queries();
        let block = "    val greeting = \"hello\"\n    println(greeting)\n}\n\nsealed interface AddedState\nobject Added : AddedState\n";
        let line_start = block.find("sealed interface").unwrap();
        let line_end = line_start + block[line_start..].lines().next().unwrap().len();
        let mut highlighter = SyntaxHighlighter::new("kotlin");
        highlighter.update(None, &Rope::from(block), None);
        assert_eq!(highlighter.language().as_ref(), "kotlin");
        let spans = line_spans(
            &highlighter,
            block,
            &(line_start..line_end),
            &gpui_component::highlighter::HighlightTheme::default_light(),
            gpui::black(),
        );
        assert!(
            spans.iter().any(|(_, color)| *color != gpui::black()),
            "added Kotlin line should have at least one non-default colored span: {spans:?}"
        );
    }
}
