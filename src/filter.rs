//! 敏感内容识别：命中即不写入历史（设计文档第 7 节）。
//!
//! 所有规则只做高置信匹配。取舍原则是「宁可漏掉一段敏感内容，也不要因为
//! 误判把用户正常复制的文本吃掉」——误伤会让工具直接不可用，而漏判只是
//! 退化成普通剪贴板管理器。

/// 环境变量：设为 `1` 时完全关闭过滤。
pub const DISABLE_ENV: &str = "DITTO_NO_FILTER";

/// 规则表条目：(规则名, 匹配函数)。规则名会显示在状态栏，便于用户知道为什么没入历史。
type Rule = (&'static str, fn(&str) -> bool);

const RULES: &[Rule] = &[
    ("private-key", looks_like_private_key),
    ("jwt", looks_like_jwt),
    ("aws-access-key", looks_like_aws_key),
    ("github-token", looks_like_github_token),
    ("slack-token", looks_like_slack_token),
    ("openai-key", looks_like_openai_key),
    ("high-entropy", looks_like_high_entropy),
];

/// 正式入口：读环境变量开关。
pub fn is_sensitive(text: &str) -> Option<&'static str> {
    if filter_disabled(std::env::var(DISABLE_ENV).ok().as_deref()) {
        return None;
    }
    detect(text)
}

/// 纯函数版本，不读环境变量，便于测试（避免测试并行时互相污染进程环境变量）。
pub fn detect(text: &str) -> Option<&'static str> {
    RULES
        .iter()
        .find(|(_, matches)| matches(text))
        .map(|(name, _)| *name)
}

fn filter_disabled(value: Option<&str>) -> bool {
    value == Some("1")
}

// ---------------------------------------------------------------- 规则实现

fn looks_like_private_key(text: &str) -> bool {
    text.contains("-----BEGIN") && text.contains("PRIVATE KEY-----")
}

/// JWT：三段点分、首段以 `eyJ` 开头（base64 的 `{"` ）、总长 > 40。
fn looks_like_jwt(text: &str) -> bool {
    let text = text.trim();
    if text.chars().count() <= 40 {
        return false;
    }
    let parts: Vec<&str> = text.split('.').collect();
    parts.len() == 3
        && parts[0].starts_with("eyJ")
        && parts
            .iter()
            .all(|p| !p.is_empty() && !p.contains(char::is_whitespace))
}

fn looks_like_aws_key(text: &str) -> bool {
    ["AKIA", "ASIA"]
        .iter()
        .any(|prefix| has_prefixed_run(text, prefix, 16, |c| c.is_ascii_uppercase() || c.is_ascii_digit()))
}

fn looks_like_github_token(text: &str) -> bool {
    ["ghp_", "gho_", "ghu_", "ghs_", "ghr_"]
        .iter()
        .any(|prefix| has_prefixed_run(text, prefix, 36, is_token_char))
}

fn looks_like_slack_token(text: &str) -> bool {
    ["xoxb-", "xoxa-", "xoxp-", "xoxr-", "xoxs-"]
        .iter()
        .any(|prefix| has_prefixed_run(text, prefix, 10, is_token_char))
}

fn looks_like_openai_key(text: &str) -> bool {
    has_prefixed_run(text, "sk-", 20, is_token_char)
}

/// 通用高熵串：长、无空白，且同时含大写、小写、数字。
///
/// 这里刻意**不**把「含符号」算作一个字符类。若按「四类中占三类」判定，
/// 长 URL 会同时满足大小写+数字+符号而被误伤；而 base64 凭据的典型特征
/// 恰恰是大小写字母与数字混排。URL/路径再额外豁免一次，双保险。
fn looks_like_high_entropy(text: &str) -> bool {
    let text = text.trim();
    if text.chars().count() < 48 || text.chars().any(char::is_whitespace) {
        return false;
    }
    if text.contains("://") || text.starts_with("www.") || text.starts_with('/') || text.starts_with('~') {
        return false;
    }
    text.chars().any(|c| c.is_ascii_uppercase())
        && text.chars().any(|c| c.is_ascii_lowercase())
        && text.chars().any(|c| c.is_ascii_digit())
}

fn is_token_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-'
}

/// 在文本任意位置寻找 `prefix`，并检查其后是否紧跟至少 `min_len` 个满足 `ok` 的字符。
fn has_prefixed_run(text: &str, prefix: &str, min_len: usize, ok: fn(char) -> bool) -> bool {
    let mut from = 0;
    while let Some(found) = text[from..].find(prefix) {
        let body_start = from + found + prefix.len();
        let run = text[body_start..].chars().take_while(|c| ok(*c)).count();
        if run >= min_len {
            return true;
        }
        from = body_start;
    }
    false
}

// -------------------------------------------------------------------- 测试

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_private_key_block() {
        let key = "-----BEGIN RSA PRIVATE KEY-----\nMIIEowIBAAKCAQEA\n-----END RSA PRIVATE KEY-----";
        assert_eq!(detect(key), Some("private-key"));
    }

    #[test]
    fn detects_jwt() {
        let jwt = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dBjftJeZ4CVPmB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        assert_eq!(detect(jwt), Some("jwt"));
    }

    #[test]
    fn detects_aws_access_key() {
        assert_eq!(detect("AKIAIOSFODNN7EXAMPLE"), Some("aws-access-key"));
    }

    #[test]
    fn detects_github_token() {
        let token = format!("ghp_{}", "a1B2c3D4e5".repeat(4));
        assert_eq!(detect(&token), Some("github-token"));
    }

    #[test]
    fn detects_slack_token() {
        assert_eq!(detect("xoxb-1234567890-abcdefghijkl"), Some("slack-token"));
    }

    #[test]
    fn detects_openai_key() {
        let key = format!("sk-{}", "A1b2C3d4E5f6G7h8I9j0".repeat(2));
        assert_eq!(detect(&key), Some("openai-key"));
    }

    #[test]
    fn detects_high_entropy_secret() {
        // 48 位、大小写+数字混排、无空白：典型 base64 凭据形态
        let secret = "aB3dE5gH7jK9mN1pQ3sT5vW7yZ9aB1cD3eF5gH7jK9mN1pQ3sT5vW7yZ";
        assert_eq!(detect(secret), Some("high-entropy"));
    }

    // --- 反例：这些必须放行，否则工具会吃掉用户正常复制的内容 ---

    #[test]
    fn allows_chinese_prose() {
        let prose = "今天下午的会议纪要：我们讨论了三个方案，最终决定先做交互原型，下周再评估数据层。另外记得把上次的接口文档补上，还有一个待确认的点是缓存策略是否需要调整，这个问题需要产品再确认一下。";
        assert!(prose.chars().count() > 48);
        assert_eq!(detect(prose), None);
    }

    #[test]
    fn allows_long_url() {
        let url = "https://example.com/api/v2/very/long/path/segment?query=Value123&other=AbcDef456&more=xyz";
        assert!(url.chars().count() > 48);
        assert_eq!(detect(url), None);
    }

    #[test]
    fn allows_multiline_code_snippet() {
        let code = "fn main() {\n    let x = FooBar123::new();\n    println!(\"{x:?}\");\n}";
        assert_eq!(detect(code), None);
    }

    #[test]
    fn allows_short_token_like_text() {
        assert_eq!(detect("sk-short"), None);
        assert_eq!(detect("AKIA123"), None);
        assert_eq!(detect("ghp_abc"), None);
    }

    #[test]
    fn allows_plain_and_empty_text() {
        assert_eq!(detect(""), None);
        assert_eq!(detect("hello world"), None);
        assert_eq!(detect("hello, world!"), None);
    }

    #[test]
    fn allows_long_lowercase_hex() {
        let hex = "9f2c4b6d8e0a1c3e5f7b9d0a2c4e6f8b1d3a5c7e9f0b2d4a6c8e1f3b5d7a9c0e";
        assert!(hex.chars().count() >= 48);
        assert_eq!(detect(hex), None);
    }

    #[test]
    fn disable_flag_only_accepts_one() {
        assert!(filter_disabled(Some("1")));
        assert!(!filter_disabled(Some("0")));
        assert!(!filter_disabled(Some("true")));
        assert!(!filter_disabled(None));
    }
}
