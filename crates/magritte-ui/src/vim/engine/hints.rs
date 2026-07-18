//! The which-key panel's rows for each pending state.

use super::*;

impl VimState {
    /// The which-key rows for the current pending state: `(keys, description)`
    /// pairs for the most useful continuations — a hint, not a manual, so each
    /// state stays at ~10 rows. Empty when nothing multi-key is pending,
    /// including the `/`/`?`/`:` prompts and a bare count. A pending
    /// `[vim.keymap]` prefix lists its own continuations.
    pub fn which_key_hints(&self) -> Vec<(String, String)> {
        let own = |rows: &[(&str, &str)]| -> Vec<(String, String)> {
            rows.iter()
                .map(|(k, d)| (k.to_string(), d.to_string()))
                .collect()
        };
        match &self.pending {
            Pending::AwaitMotion(consumer) => {
                // The operator's own doubled key is its linewise form.
                let line_key = match consumer {
                    Consumer::Op { op, .. } => Some(op.key().to_string()),
                    Consumer::SurroundAdd => Some("s".to_string()),
                    Consumer::Reflow { keep: false } => Some("q".to_string()),
                    Consumer::Reflow { keep: true } => Some("w".to_string()),
                    Consumer::Shift { dedent, .. } => {
                        Some(if *dedent { "<" } else { ">" }.to_string())
                    }
                    Consumer::Move => None,
                };
                let mut rows: Vec<(String, String)> = Vec::new();
                if let Some(key) = line_key {
                    rows.push((key, "Whole line".to_string()));
                }
                // `s` after the operator is the surround family (ys/ds/cs).
                if let Consumer::Op { op, .. } = consumer {
                    let surround = match op {
                        Op::Yank => "Add surround",
                        Op::Delete => "Delete surround",
                        Op::Change => "Change surround",
                    };
                    rows.push(("s".to_string(), surround.to_string()));
                }
                rows.extend(own(&[
                    ("w", "To next word"),
                    ("e", "To word end"),
                    ("b", "Back a word"),
                    ("$", "To line end"),
                    ("0", "To line start"),
                    ("gg · G", "First / last line"),
                    ("f · t", "Find / till a char"),
                    ("iw · aw", "Inner / around word"),
                    ("i\" · i( · ip", "Quotes / parens / paragraph"),
                ]));
                rows
            }
            Pending::Object { .. } => own(&[
                ("w · W", "Word"),
                ("s", "Sentence"),
                ("p", "Paragraph"),
                ("\" · ' · `", "Quoted string"),
                ("( · [ · {", "Bracket block"),
                ("t", "Tag block"),
            ]),
            Pending::G(consumer) => {
                let mut rows = vec![("g".to_string(), "First line".to_string())];
                if *consumer == Consumer::Move {
                    rows.push(("q".to_string(), "Reflow operator".to_string()));
                }
                rows
            }
            Pending::Z => own(&[("Z", "Commit"), ("Q", "Cancel")]),
            Pending::Zscroll => own(&[
                ("z", "Center cursor line"),
                ("t", "Cursor line to top"),
                ("b", "Cursor line to bottom"),
            ]),
            Pending::Comma => own(&[
                (",", "Commit"),
                ("c", "Commit"),
                ("k", "Cancel"),
                ("q", "Reflow message"),
            ]),
            Pending::SurroundChar { .. } | Pending::SurroundChangeTo { .. } => own(&[
                ("\" · ' · `", "Quotes"),
                ("( · [ · { · <", "Brackets, inner spaces"),
                (") · ] · } · >", "Brackets, snug"),
            ]),
            Pending::SurroundDelete | Pending::SurroundChangeFrom => own(&[
                ("\" · ' · `", "Quotes"),
                ("( · [ · { · <", "Nearest bracket pair"),
                ("t", "Tag"),
            ]),
            Pending::User(typed) => {
                let mut rows: Vec<(String, String)> = self
                    .user_map
                    .iter()
                    .filter_map(|(seq, cmd)| {
                        seq.strip_prefix(typed.as_str())
                            .filter(|rest| !rest.is_empty())
                            .map(|rest| {
                                // Vim notation: the remaining keystrokes
                                // read as one unspaced sequence cap.
                                let keys = rest.to_string();
                                (keys, cmd.describe().to_string())
                            })
                    })
                    .collect();
                rows.truncate(10);
                rows
            }
            Pending::Find { .. }
            | Pending::Replace
            | Pending::Search { .. }
            | Pending::Ex { .. }
            | Pending::None => Vec::new(),
        }
    }
}
