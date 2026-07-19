use std::path::Path;

use unicode_normalization::UnicodeNormalization;

/// Git and macOS filesystem events may report the same filename with different
/// Unicode normalization. Use one identity form anywhere their paths meet.
pub(crate) fn key(path: &Path) -> String {
    path.to_string_lossy().nfc().collect()
}

pub(crate) fn text_key(path: &str) -> String {
    path.nfc().collect()
}

pub(crate) fn matches(path: &Path, text: &str) -> bool {
    key(path) == text_key(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn composed_and_decomposed_paths_have_the_same_identity() {
        assert!(matches(Path::new("caf\u{e9}.txt"), "cafe\u{301}.txt"));
    }
}
