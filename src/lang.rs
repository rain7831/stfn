//! 纯本地语言检测，不访问网络。
//!
//! v0.1 只服务中英互译：
//! - 含 CJK 统一表意文字 -> 中文；
//! - 含 ASCII 字母 -> 英文；
//! - 含日文假名 / 韩文 -> 暂不支持；
//! - 纯数字 / 符号 -> 空输入。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// 当前支持的翻译语言。
pub enum Lang {
    Zh,
    En,
}

// 语言代码：API 使用 code/bing_code，UI 使用 label。
impl Lang {
    pub fn code(self) -> &'static str {
        match self {
            Self::Zh => "zh",
            Self::En => "en",
        }
    }

    pub fn bing_code(self) -> &'static str {
        match self {
            Self::Zh => "zh-Hans",
            Self::En => "en",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Zh => "中",
            Self::En => "EN",
        }
    }

    /// 把 provider 返回的语言代码归一化；未知语言返回 None。
    pub fn from_code(code: &str) -> Option<Self> {
        let code = code.to_ascii_lowercase();
        if code == "zh" || code.starts_with("zh-") {
            Some(Self::Zh)
        } else if code == "en" || code.starts_with("en-") {
            Some(Self::En)
        } else {
            None
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// 输入语言检测结果。Unsupported 用于日文/韩文/其他字母文字。
pub enum Detection {
    Empty,
    Zh,
    En,
    Unsupported,
}

/// 扫描字符类别判断语言；混排中英按中文处理，避免引入额外语言检测依赖。
pub fn detect(text: &str) -> Detection {
    let text = text.trim();
    if text.is_empty() {
        return Detection::Empty;
    }

    let mut has_cjk = false;
    let mut has_ascii_letter = false;
    let mut has_other_letter = false;
    let mut has_kana_or_hangul = false;

    for ch in text.chars() {
        let cp = ch as u32;
        if (0x3040..=0x30FF).contains(&cp)
            || (0x31F0..=0x31FF).contains(&cp)
            || (0xAC00..=0xD7AF).contains(&cp)
            || (0x1100..=0x11FF).contains(&cp)
        {
            has_kana_or_hangul = true;
        } else if is_cjk(cp) {
            has_cjk = true;
        } else if ch.is_ascii_alphabetic() {
            has_ascii_letter = true;
        } else if ch.is_alphabetic() {
            has_other_letter = true;
        }
    }

    if has_kana_or_hangul {
        Detection::Unsupported
    } else if has_cjk {
        Detection::Zh
    } else if has_ascii_letter {
        Detection::En
    } else if has_other_letter {
        Detection::Unsupported
    } else {
        Detection::Empty
    }
}

// CJK 统一表意文字及扩展区。
fn is_cjk(cp: u32) -> bool {
    (0x4E00..=0x9FFF).contains(&cp)
        || (0x3400..=0x4DBF).contains(&cp)
        || (0xF900..=0xFAFF).contains(&cp)
        || (0x20000..=0x2FA1F).contains(&cp)
        || (0x30000..=0x3134F).contains(&cp)
}
