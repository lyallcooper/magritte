//! The `:` command line: range parsing, `:s///` execution, the live
//! substitution preview, and the history shared with the `/`/`?` prompts.
//! `impl VimState` over the same state as `engine.rs`.

use super::*;

impl VimState {
    /// Live matches of the substitution being typed at the `:` prompt — the
    /// byte ranges `[range]s/pat…` would touch (first per line, every one
    /// once a `g` flag is typed), for the incremental highlight. Empty while
    /// the line isn't an `s` command or its pattern is empty/invalid.
    pub(crate) fn ex_matches(&self, text: &str, cursor: usize) -> Vec<Range<usize>> {
        const CAP: usize = 200;
        let Pending::Ex { input, visual } = &self.pending else {
            return Vec::new();
        };
        let lines = line_count(text);
        let current = line_of(text, clamp_normal(text, cursor));
        let Ok((range, rest)) = ex_range(input, current, lines, *visual) else {
            return Vec::new();
        };
        let Some(body) = rest.strip_prefix("s/") else {
            return Vec::new();
        };
        let Ok((re, _, global)) = parse_substitute(body) else {
            return Vec::new();
        };
        let (a, b) = (range.0.clamp(1, lines), range.1.clamp(1, lines));
        let start = line_offset(text, a.min(b));
        let end = line_end(text, line_offset(text, a.max(b)));
        let mut out = Vec::new();
        let mut at = start;
        for line in text[start..end].split('\n') {
            for m in re.find_iter(line) {
                if m.start() < m.end() {
                    out.push(at + m.start()..at + m.end());
                }
                if !global || out.len() >= CAP {
                    break;
                }
            }
            if out.len() >= CAP {
                break;
            }
            at += line.len() + 1;
        }
        out
    }

    /// Execute a completed `:` line: `q`/`q!`/`w`/`wq`/`x`, `help`, a bare
    /// line number, or `[range]s/pat/rep/[flags]`. Anything else echoes an
    /// error. `visual` is the line pair a Visual-mode `:` remembered for
    /// `'<,'>`.
    pub(super) fn ex_execute(
        &mut self,
        text: &str,
        cursor: usize,
        input: &str,
        visual: Option<(usize, usize)>,
    ) -> Vec<Action> {
        match input {
            "q" => return vec![Action::Quit { force: false }],
            "q!" => return vec![Action::Quit { force: true }],
            "w" | "wq" | "x" => return vec![Action::Commit],
            "h" | "help" => return vec![Action::Help],
            _ => {}
        }
        // A bare line number jumps to its first non-blank, clamped to the
        // last line, like `{count}G`.
        if !input.is_empty() && input.bytes().all(|b| b.is_ascii_digit()) {
            let m = Motion::GotoLine(Some(input.parse().unwrap_or(usize::MAX).max(1)));
            let Some(target) = motion::eval(text, cursor, 1, m, 0) else {
                return self.beep();
            };
            self.after_move(text, m, target.pos);
            return vec![Action::MoveCursor(clamp_normal(text, target.pos))];
        }
        // `[range]s/pat/rep/[flags]` — the only range-taking command.
        let lines = line_count(text);
        let current = line_of(text, cursor);
        let (range, rest) = match ex_range(input, current, lines, visual) {
            Ok(parsed) => parsed,
            Err(msg) => return self.err(msg),
        };
        let Some(body) = rest.strip_prefix("s/") else {
            return self.err(format!("Not an editor command: {input}"));
        };
        let (a, b) = (range.0.clamp(1, lines), range.1.clamp(1, lines));
        self.ex_substitute(text, a.min(b), a.max(b), body)
    }

    /// `:s/pat/rep/[flags]` over 1-based lines `first..=last`: one edit
    /// replacing the covered line span, cursor at the first non-blank of the
    /// last line with a match. No match in the range is an error (Vim's
    /// E486), as is an invalid regex or an unknown flag.
    fn ex_substitute(&mut self, text: &str, first: usize, last: usize, body: &str) -> Vec<Action> {
        let (re, rep, global) = match parse_substitute(body) {
            Ok(parsed) => parsed,
            Err(msg) => return self.err(msg),
        };
        let rep = sub_replacement(&rep);
        let start = line_offset(text, first);
        let end = line_end(text, line_offset(text, last));
        let mut out = String::with_capacity(end - start);
        // Offset within `out` of the last line with a match, for the cursor.
        let mut last_match = None;
        for (i, line) in text[start..end].split('\n').enumerate() {
            if i > 0 {
                out.push('\n');
            }
            if re.is_match(line) {
                last_match = Some(out.len());
                if global {
                    out.push_str(&re.replace_all(line, rep.as_str()));
                } else {
                    out.push_str(&re.replace(line, rep.as_str()));
                }
            } else {
                out.push_str(line);
            }
        }
        let Some(line_at) = last_match else {
            return self.err(format!("Pattern not found: {}", re.as_str()));
        };
        let post = splice(text, &(start..end), &out);
        let cursor = first_non_blank(&post, (start + line_at).min(post.len()));
        vec![Action::Edit(EditOp {
            range: start..end,
            text: out,
            cursor,
        })]
    }
}

/// Which prompt a line edit belongs to — selects the history it steps.
pub(super) enum PromptHist {
    Search,
    Ex,
}

/// What one key did to a prompt line, for the caller to map back onto its
/// `Pending` variant.
pub(super) enum PromptOutcome {
    /// The prompt stays open with this content.
    Keep(String),
    /// Backspace on an empty line: close the prompt.
    Cancel,
    /// Enter: execute this line (already pushed to its history if non-empty).
    Commit(String),
    /// Nowhere to step, or an unhandled key that must not destroy the typed
    /// line: beep, leaving `self.pending` holding the prompt.
    Beep,
}

impl VimState {
    /// One key into a `/`/`?`/`:` prompt line: char append, backspace-cancel,
    /// Up/Down/`C-p`/`C-n` history stepping, Enter commit.
    pub(super) fn prompt_key(
        &mut self,
        mut line: String,
        key: Key,
        hist: PromptHist,
    ) -> PromptOutcome {
        match key {
            Key::Char(c) => {
                self.hist_ix = None;
                line.push(c);
                PromptOutcome::Keep(line)
            }
            Key::Backspace => {
                self.hist_ix = None;
                // Backspace on an empty line cancels, like Vim.
                if line.pop().is_some() {
                    PromptOutcome::Keep(line)
                } else {
                    PromptOutcome::Cancel
                }
            }
            Key::Up | Key::Down | Key::Ctrl('p') | Key::Ctrl('n') => {
                let older = matches!(key, Key::Up | Key::Ctrl('p'));
                let hist = match hist {
                    PromptHist::Search => &self.search_hist,
                    PromptHist::Ex => &self.ex_hist,
                };
                match hist_step(hist, &mut self.hist_ix, &mut self.hist_stash, &line, older) {
                    Some(stepped) => PromptOutcome::Keep(stepped),
                    None => PromptOutcome::Beep,
                }
            }
            Key::Enter => {
                self.hist_ix = None;
                if !line.is_empty() {
                    let hist = match hist {
                        PromptHist::Search => &mut self.search_hist,
                        PromptHist::Ex => &mut self.ex_hist,
                    };
                    push_hist(hist, &line);
                }
                PromptOutcome::Commit(line)
            }
            _ => PromptOutcome::Beep,
        }
    }
}

/// Step a prompt through its history: `older` is Up/`C-p`. Returns the new
/// line, or None when there's nowhere to go (empty history, already at the
/// oldest, or Down on the live line). Browsing starts by stashing the live
/// line; Down past the newest entry restores it.
fn hist_step(
    hist: &[String],
    ix: &mut Option<usize>,
    stash: &mut String,
    current: &str,
    older: bool,
) -> Option<String> {
    if older {
        let next = match *ix {
            None if hist.is_empty() => return None,
            None => {
                *stash = current.to_string();
                hist.len() - 1
            }
            Some(0) => return None,
            Some(i) => i - 1,
        };
        *ix = Some(next);
        Some(hist[next].clone())
    } else {
        match *ix {
            None => None,
            Some(i) if i + 1 >= hist.len() => {
                *ix = None;
                Some(std::mem::take(stash))
            }
            Some(i) => {
                *ix = Some(i + 1);
                Some(hist[i + 1].clone())
            }
        }
    }
}

/// Append an executed prompt line to its history (consecutive repeats and
/// anything past 50 entries dropped).
fn push_hist(hist: &mut Vec<String>, line: &str) {
    if hist.last().map(String::as_str) == Some(line) {
        return;
    }
    hist.push(line.to_string());
    if hist.len() > 50 {
        hist.remove(0);
    }
}

/// Parse the optional leading `[range]` of an ex command — `%`, `'<,'>`
/// (only meaningful with a Visual-remembered line pair), or one or two
/// addresses (`N`, `.`, `$`) separated by `,` — returning the 1-based line
/// pair and the rest of the line. No range means the current line. `Err` is
/// the message to echo.
fn ex_range(
    input: &str,
    current: usize,
    lines: usize,
    visual: Option<(usize, usize)>,
) -> Result<((usize, usize), &str), String> {
    if let Some(rest) = input.strip_prefix('%') {
        return Ok(((1, lines), rest));
    }
    if let Some(rest) = input.strip_prefix("'<,'>") {
        return match visual {
            Some(v) => Ok((v, rest)),
            None => Err("Mark not set".into()),
        };
    }
    if let Some((a, rest)) = ex_addr(input, current, lines) {
        return match rest.strip_prefix(',') {
            Some(rest) => match ex_addr(rest, current, lines) {
                Some((b, rest)) => Ok(((a, b), rest)),
                None => Err("Invalid range".into()),
            },
            None => Ok(((a, a), rest)),
        };
    }
    Ok(((current, current), input))
}

/// One ex-range endpoint: a line number, `.` (the current line), or `$` (the
/// last), returning the rest of the input.
fn ex_addr(s: &str, current: usize, last: usize) -> Option<(usize, &str)> {
    if let Some(rest) = s.strip_prefix('.') {
        return Some((current, rest));
    }
    if let Some(rest) = s.strip_prefix('$') {
        return Some((last, rest));
    }
    let digits = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    (digits > 0).then(|| (s[..digits].parse().unwrap_or(usize::MAX), &s[digits..]))
}

/// Parse the `pat/rep/flags` body after `:s/` into the compiled pattern, the
/// raw replacement, and the `g` flag (`i` folds into the regex), so the live
/// preview and the actual substitution can't drift. `Err` is the message to
/// echo: an unknown flag, an empty pattern (no reuse of the last pattern,
/// Vim's `:s//`), or an invalid regex.
fn parse_substitute(body: &str) -> Result<(regex::Regex, String, bool), String> {
    let (pat, rep, flags) = split_substitute(body);
    let (mut global, mut icase) = (false, false);
    for f in flags.chars() {
        match f {
            'g' => global = true,
            'i' => icase = true,
            _ => return Err(format!("Trailing characters: {flags}")),
        }
    }
    if pat.is_empty() {
        return Err("Empty pattern".into());
    }
    let re = regex::RegexBuilder::new(&pat)
        .case_insensitive(icase)
        .build()
        .map_err(|_| format!("Invalid pattern: {pat}"))?;
    Ok((re, rep, global))
}

/// Split the `pat/rep/flags` after `:s/` on unescaped `/`: `\/` is a literal
/// delimiter inside either field; any other backslash pair passes through
/// untouched (the pattern is regex syntax). The trailing delimiter is
/// optional.
fn split_substitute(body: &str) -> (String, String, String) {
    let mut fields = [String::new(), String::new(), String::new()];
    let mut at = 0;
    let mut chars = body.chars();
    while let Some(c) = chars.next() {
        if at == 2 {
            fields[2].push(c);
        } else if c == '/' {
            at += 1;
        } else if c == '\\' {
            match chars.next() {
                Some('/') => fields[at].push('/'),
                Some(d) => {
                    fields[at].push('\\');
                    fields[at].push(d);
                }
                None => fields[at].push('\\'),
            }
        } else {
            fields[at].push(c);
        }
    }
    let [pat, rep, flags] = fields;
    (pat, rep, flags)
}

/// Translate a `:s` replacement into the regex crate's syntax: `&` and `\0`
/// are the whole match, `\1`..`\9` capture groups — emitted as `${N}` so a
/// trailing digit can't glue onto the reference — `\&` a literal `&`, `\\` a
/// literal backslash. A literal `$` must become `$$`, which is the regex
/// crate's only escape.
fn sub_replacement(rep: &str) -> String {
    let mut out = String::with_capacity(rep.len() + 4);
    let mut chars = rep.chars();
    while let Some(c) = chars.next() {
        match c {
            '$' => out.push_str("$$"),
            '&' => out.push_str("${0}"),
            '\\' => match chars.next() {
                Some('&') => out.push('&'),
                Some('\\') => out.push('\\'),
                Some(d @ '0'..='9') => {
                    out.push_str("${");
                    out.push(d);
                    out.push('}');
                }
                Some('$') => out.push_str("\\$$"),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            },
            c => out.push(c),
        }
    }
    out
}
