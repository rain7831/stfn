//! primary selection 文本清洗测试。
use stfn::selection::{sanitize, SelectionError, MAX_CHARS};

#[test]
fn trims_and_keeps_inner_newlines() {
    assert_eq!(sanitize("  hello\nworld\t ").unwrap(), "hello\nworld");
}

#[test]
fn normalizes_crlf_and_drops_control_chars() {
    assert_eq!(sanitize("a\r\nb\u{0}c").unwrap(), "a\nbc");
}

#[test]
fn empty_or_control_only_is_rejected() {
    assert_eq!(sanitize("   \n\t ").unwrap_err(), SelectionError::Empty);
    assert_eq!(sanitize("\u{0}\u{1}").unwrap_err(), SelectionError::Empty);
}

#[test]
fn keeps_exactly_five_thousand_chars() {
    let text = "a".repeat(MAX_CHARS);
    let cleaned = sanitize(&text).unwrap();
    assert_eq!(cleaned.chars().count(), MAX_CHARS);
}

#[test]
fn rejects_overlong_selection_instead_of_truncating() {
    let text = "a".repeat(MAX_CHARS + 1);
    assert_eq!(sanitize(&text).unwrap_err(), SelectionError::TooLong);
}

#[test]
fn error_messages_are_user_facing() {
    assert!(SelectionError::Empty.message().contains("选中"));
    assert!(SelectionError::TooLong.message().contains("5000"));
}
