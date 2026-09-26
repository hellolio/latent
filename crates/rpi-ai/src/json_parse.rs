//! 流式 toolcall 参数的尽力 JSON 修复解析(pi 的 utils/json-parse.ts,02 文档 §6)。
//!
//! 三层降级:原样解析 → 修复控制字符/坏转义后解析 → 补全未闭合的串/容器后解析;
//! 全部失败返回空对象(流式快照永远可用,不 panic)。

use serde_json::Value;

const VALID_JSON_ESCAPES: [char; 8] = ['"', '\\', '/', 'b', 'f', 'n', 'r', 't'];

fn is_hex_digit(c: char) -> bool {
    c.is_ascii_hexdigit()
}

/// 修复字符串字面量内的裸控制字符与非法转义(逐字符状态机,pi 的 repairJson)。
pub fn repair_json(json: &str) -> String {
    let mut repaired = String::with_capacity(json.len());
    let mut chars = json.chars().peekable();
    let mut in_string = false;
    while let Some(c) = chars.next() {
        if !in_string {
            repaired.push(c);
            if c == '"' {
                in_string = true;
            }
            continue;
        }
        match c {
            '"' => {
                repaired.push('"');
                in_string = false;
            }
            '\\' => {
                let next = chars.peek().copied();
                match next {
                    None => repaired.push_str("\\\\"),
                    Some('u') => {
                        // 先消费 'u'(Peekable::clone 会保留 peek 项),再看 4 位是否十六进制
                        chars.next();
                        let rest: String = chars.clone().take(4).collect();
                        if rest.chars().count() == 4 && rest.chars().all(is_hex_digit) {
                            repaired.push_str("\\u");
                            repaired.push_str(&rest);
                            for _ in 0..4 {
                                chars.next();
                            }
                        } else {
                            repaired.push_str("\\\\");
                            repaired.push('u');
                        }
                    }
                    Some(n) if VALID_JSON_ESCAPES.contains(&n) => {
                        repaired.push('\\');
                        repaired.push(n);
                        chars.next();
                    }
                    Some(_) => repaired.push_str("\\\\"),
                }
            }
            c if (c as u32) < 0x20 => {
                // 控制字符转义进字符串字面量
                match c {
                    '\u{8}' => repaired.push_str("\\b"),
                    '\u{c}' => repaired.push_str("\\f"),
                    '\n' => repaired.push_str("\\n"),
                    '\r' => repaired.push_str("\\r"),
                    '\t' => repaired.push_str("\\t"),
                    other => repaired.push_str(&format!("\\u{:04x}", other as u32)),
                }
            }
            c => repaired.push(c),
        }
    }
    repaired
}

/// 补全流式截断的 JSON:关闭未闭合的字符串与容器(深度优先、按栈逆序)。
/// 例如 `{"a": [1,2` → `{"a": [1,2]}`、`{"k": "he` → `{"k": "he"}`。
/// 截断在字面量中间(`tr`/`12e`)时补全结果可能仍非法,返回 None。
pub fn complete_partial_json(json: &str) -> Option<String> {
    let mut stack: Vec<char> = Vec::new();
    let mut in_string = false;
    let mut escaped = false;
    for c in json.chars() {
        if escaped {
            escaped = false;
            continue;
        }
        match c {
            '\\' if in_string => escaped = true,
            '"' => in_string = !in_string,
            '{' | '[' if !in_string => stack.push(c),
            '}' | ']' if !in_string => {
                stack.pop();
            }
            _ => {}
        }
    }
    if stack.is_empty() && !in_string && !escaped {
        return None; // 无需补全;调用方先尝试原样解析
    }
    let mut out = json.to_string();
    if escaped {
        out.push('\\');
    }
    if in_string {
        out.push('"');
    }
    for open in stack.into_iter().rev() {
        out.push(if open == '{' { '}' } else { ']' });
    }
    Some(out)
}

/// 解析(必要时修复);原样与修复后都失败才报错。
pub fn parse_json_with_repair(json: &str) -> Result<Value, serde_json::Error> {
    match serde_json::from_str(json) {
        Ok(v) => Ok(v),
        Err(err) => {
            let repaired = repair_json(json);
            if repaired != json {
                serde_json::from_str(&repaired)
            } else {
                Err(err)
            }
        }
    }
}

/// 尽力解析流式累积的(可能不完整的)JSON;任何失败都返回空对象。
pub fn parse_streaming_json(partial: Option<&str>) -> Value {
    let Some(text) = partial else { return Value::Object(Default::default()) };
    if text.trim().is_empty() {
        return Value::Object(Default::default());
    }
    if let Ok(v) = parse_json_with_repair(text) {
        return v;
    }
    if let Some(completed) = complete_partial_json(text) {
        if let Ok(v) = serde_json::from_str(&completed) {
            return v;
        }
        let repaired = repair_json(&completed);
        if let Ok(v) = serde_json::from_str(&repaired) {
            return v;
        }
    }
    Value::Object(Default::default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn repairs_control_chars_and_bad_escapes() {
        assert_eq!(repair_json("\"a\x01b\""), "\"a\\u0001b\"");
        assert_eq!(repair_json("\"a\\qb\""), "\"a\\\\qb\"");
        // 合法转义保持不变
        assert_eq!(repair_json("\"a\\nb\""), "\"a\\nb\"");
        assert_eq!(repair_json("\"\\u00e9\""), "\"\\u00e9\"");
    }

    #[test]
    fn parses_with_repair() {
        assert_eq!(parse_json_with_repair("\"a\nb\"").unwrap(), json!("a\nb"));
        assert!(parse_json_with_repair("{invalid").is_err());
    }

    #[test]
    fn streaming_parse_handles_partial_values() {
        assert_eq!(parse_streaming_json(Some("")), json!({}));
        assert_eq!(parse_streaming_json(Some("{\"a\": 1}")), json!({"a": 1}));
        assert_eq!(parse_streaming_json(Some("{\"a\": \"he")), json!({"a": "he"}));
        assert_eq!(parse_streaming_json(Some("{\"a\": [1, 2")), json!({"a": [1, 2]}));
        assert_eq!(parse_streaming_json(Some("{\"a\": {\"b\": tr")), json!({}));
    }

    #[test]
    fn utf8_and_escapes_survive_completion() {
        // 字符串内的 \" 不应误判字符串边界
        assert_eq!(parse_streaming_json(Some("{\"a\": \"x\\\"")), json!({"a": "x\""}));
    }
}
