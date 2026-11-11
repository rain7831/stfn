//! primary selection 文本清洗。
//!
//! 只读取 Wayland / niri 的 primary selection（由 `ui` 通过 GTK 完成），
//! 这里保持纯函数，便于集成测试：
//! - trim 首尾空白，保留内部换行；
//! - 去掉 `\n` / `\t` 之外的控制字符（含 `\r`，因此 CRLF 归一为 LF）；
//! - 空文本直接拒绝；
//! - 超过 5000 字符整段拒绝，而不是截断。
pub const MAX_CHARS: usize = 5000;

/// 选区清洗错误；`message()` 直接用于输入区状态提示。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionError {
    Empty,
    TooLong,
}

impl SelectionError {
    pub fn message(self) -> &'static str {
        match self {
            Self::Empty => "没有读到选中文本，请先选中或复制",
            Self::TooLong => "选中文本超过 5000 字符",
        }
    }
}

/// 清洗选区文本；失败时不返回任何可填入内容。
pub fn sanitize(raw: &str) -> Result<String, SelectionError> {
    let mut cleaned = String::with_capacity(raw.len());
    for ch in raw.trim().chars() {
        match ch {
            '\n' | '\t' => cleaned.push(ch),
            ch if ch.is_control() => {}
            ch => cleaned.push(ch),
        }
    }

    let cleaned = cleaned.trim();
    if cleaned.is_empty() {
        return Err(SelectionError::Empty);
    }
    if cleaned.chars().count() > MAX_CHARS {
        return Err(SelectionError::TooLong);
    }
    Ok(cleaned.to_string())
}
