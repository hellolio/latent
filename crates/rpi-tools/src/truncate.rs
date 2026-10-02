//! 统一截断(05 文档 §2 truncate.ts):双限截断(行数/字节,先到为准),
//! **永不返回半行**(bash tail 截断边界除外)。

pub const DEFAULT_MAX_LINES: usize = 2000;
pub const DEFAULT_MAX_BYTES: usize = 50 * 1024;
/// grep 匹配行的单行长度上限(05 文档 §2)
pub const GREP_MAX_LINE_LENGTH: usize = 500;

/// 工具输出自我约束上限:与 agent 转录裁剪(tool_result_max_chars)同一配置源
/// 派生 —— 自我上限 = agent 上限 - 余量,保证"输出 + 提示文本"恒不触发
/// agent 层的头尾裁剪(避免转录中间挖洞)。装配层经工厂注入,缺省即派生默认值。
#[derive(Debug, Clone, Copy)]
pub struct OutputLimits {
    pub max_lines: usize,
    pub max_bytes: usize,
    pub max_chars: usize,
}

impl Default for OutputLimits {
    fn default() -> Self {
        OutputLimits {
            max_lines: DEFAULT_MAX_LINES,
            max_bytes: DEFAULT_MAX_BYTES,
            // agent 默认 20k - 2k 余量(rpi_agent::tool_self_output_limit 的默认派生)
            max_chars: rpi_agent::tool_self_output_limit(rpi_agent::DEFAULT_TOOL_RESULT_MAX_CHARS),
        }
    }
}

impl OutputLimits {
    /// 生效字节限:字节 ≤ max_chars 时字符数必然 ≤ max_chars(UTF-8 chars ≤ bytes),
    /// 故字节限取两者较小即可同时满足行/字节/字符三重预算。
    /// max_chars == 0 表示未限制(哨兵值,非字面预算),回退 max_bytes。
    pub fn effective_max_bytes(&self) -> usize {
        if self.max_chars == 0 {
            return self.max_bytes;
        }
        self.max_bytes.min(self.max_chars)
    }
}

/// 单行按字符截断(grep 用;05 文档 truncateLine)。返回 (文本, 是否被截断)。
pub fn truncate_line(line: &str, max_chars: usize) -> (String, bool) {
    let mut chars = line.chars();
    let head: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_none() {
        (head, false)
    } else {
        (head, true)
    }
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TruncationResult {
    pub content: String,
    pub truncated: bool,
    /// lines | bytes | none
    pub truncated_by: &'static str,
    pub total_lines: usize,
    pub total_bytes: usize,
    pub output_lines: usize,
    /// 保留头部(read)或尾部(bash)
    pub head: bool,
}

impl TruncationResult {
    /// 单行超字节限被按字节截断(offset 提示对它无意义)
    pub fn truncation_by_bytes(&self) -> bool {
        self.truncated_by == "bytes"
    }
}

fn split_lines(content: &str) -> Vec<&str> {
    // 按行切分并保留行内容;末尾空段来自结尾换行,不计为一行
    let mut lines: Vec<&str> = content.lines().collect();
    if lines.is_empty() && !content.is_empty() {
        lines.push(content);
    }
    lines
}

/// 保留头部截断(read 用)。
pub fn truncate_head(content: &str, max_lines: usize, max_bytes: usize) -> TruncationResult {
    let lines = split_lines(content);
    let total_lines = lines.len();
    let total_bytes = content.len();

    let mut output: Vec<&str> = Vec::new();
    let mut bytes = 0usize;
    let mut truncated = false;
    let mut truncated_by = "lines";
    for line in &lines {
        if output.len() >= max_lines {
            truncated = true;
            break;
        }
        // 末行无换行符,不多算 +1:内容恰好等于 max_bytes 时不误报截断
        if bytes + line.len() > max_bytes {
            truncated = true;
            truncated_by = "bytes";
            break;
        }
        bytes += line.len() + 1;
        output.push(line);
    }

    // 单行超字节限(如 minified JS):按字节(char 边界)截断,
    // 否则输出为空 + 续读提示不变,read 会陷入无意义重读循环
    if truncated && output.is_empty() && max_bytes > 0 && !content.is_empty() {
        let mut cut_bytes = max_bytes.saturating_sub(1);
        while cut_bytes > 0 && !content.is_char_boundary(cut_bytes) {
            cut_bytes -= 1;
        }
        return TruncationResult {
            content: content[..cut_bytes].to_string(),
            truncated: true,
            truncated_by: "bytes",
            total_lines,
            total_bytes,
            output_lines: 1,
            head: true,
        };
    }

    TruncationResult {
        content: output.join("\n"),
        truncated,
        truncated_by: if truncated { truncated_by } else { "none" },
        total_lines,
        total_bytes,
        output_lines: output.len(),
        head: true,
    }
}

/// 保留尾部截断(bash 用):超出时保留末尾 max_lines 行 / max_bytes 字节。
pub fn truncate_tail(content: &str, max_lines: usize, max_bytes: usize) -> TruncationResult {
    let lines = split_lines(content);
    let total_lines = lines.len();
    let total_bytes = content.len();

    // 先按行数裁
    let start_by_lines = total_lines.saturating_sub(max_lines);
    let mut selected = &lines[start_by_lines..];

    // 再按字节裁(从尾部往前累计)
    let mut start = selected.len();
    let mut bytes = 0usize;
    for line in selected.iter().rev() {
        bytes += line.len() + 1;
        if bytes > max_bytes {
            break;
        }
        start -= 1;
    }
    let truncated = start_by_lines > 0 || start > 0;

    // 单行超字节限(bash 长输出):保留末尾 max_bytes 字节(char 边界),避免空输出
    if truncated && start == selected.len() && max_bytes > 0 && !content.is_empty() {
        let mut cut_bytes = content.len().saturating_sub(max_bytes.saturating_sub(1));
        while cut_bytes < content.len() && !content.is_char_boundary(cut_bytes) {
            cut_bytes += 1;
        }
        return TruncationResult {
            content: content[cut_bytes..].to_string(),
            truncated: true,
            truncated_by: "bytes",
            total_lines,
            total_bytes,
            output_lines: 1,
            head: false,
        };
    }
    selected = &selected[start..];
    let content = selected.join("\n");
    TruncationResult {
        content,
        truncated,
        truncated_by: if truncated {
            if start_by_lines > 0 {
                "lines"
            } else {
                "bytes"
            }
        } else {
            "none"
        },
        total_lines,
        total_bytes,
        output_lines: selected.len(),
        head: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn head_truncation_respects_lines_and_reports_total() {
        let content = (0..100)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let result = truncate_head(&content, 10, DEFAULT_MAX_BYTES);
        assert!(result.truncated);
        assert_eq!(result.output_lines, 10);
        assert_eq!(result.total_lines, 100);
        assert!(result.content.starts_with("line 0"));
        // 永不半行:最后一行完整
        assert!(result.content.ends_with("line 9"));
    }

    #[test]
    fn tail_truncation_keeps_end() {
        let content = (0..100)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let result = truncate_tail(&content, 10, DEFAULT_MAX_BYTES);
        assert!(result.truncated);
        assert_eq!(result.output_lines, 10);
        assert!(result.content.starts_with("line 90"));
        assert!(result.content.ends_with("line 99"));
    }

    #[test]
    fn byte_limit_truncation_reports_bytes_and_caps_output() {
        // 多行、字节限先触发:truncated_by=bytes,输出不超上限
        let content = format!("{}\n{}", "x".repeat(100), "y".repeat(100));
        let result = truncate_head(&content, 10, 120);
        assert!(result.truncated);
        assert_eq!(result.truncated_by, "bytes");
        assert!(result.content.len() <= 120);

        // 单行超字节限:按字节截断(多字节字符不 panic、不超上限)
        let wide = "中".repeat(10_000);
        let result = truncate_tail(&wide, 10, 1024);
        assert!(result.truncated);
        assert_eq!(result.truncated_by, "bytes");
        assert!(result.content.len() <= 1024);
        assert!(result.content.chars().all(|c| c == '中'));
    }

    #[test]
    fn under_limit_not_truncated() {
        let result = truncate_head("hello\nworld", 10, 1000);
        assert!(!result.truncated);
        assert_eq!(result.content, "hello\nworld");
    }

    #[test]
    fn content_exactly_at_byte_limit_is_not_truncated() {
        // 末行无换行符不多算 +1:总字节恰好等于上限时不误报
        let content = format!("{}\n{}", "x".repeat(60), "y".repeat(59)); // 60 + 1 + 59 = 120
        let result = truncate_head(&content, 10, 120);
        assert!(!result.truncated, "恰好 120 字节不应报截断");
        assert_eq!(result.content, content);
    }
}

#[cfg(test)]
mod output_limits_tests {
    use super::*;

    #[test]
    fn default_derives_from_agent_cap() {
        let limits = OutputLimits::default();
        assert_eq!(
            limits.max_chars,
            rpi_agent::tool_self_output_limit(rpi_agent::DEFAULT_TOOL_RESULT_MAX_CHARS)
        );
        assert_eq!(limits.effective_max_bytes(), limits.max_bytes.min(limits.max_chars));
    }

    #[test]
    fn zero_max_chars_means_unlimited_not_zero_budget() {
        // agent 层"关闭裁剪"(cap=0 → 派生哨兵 0)不得被解释为字面预算 0,
        // 否则 bash/read 输出会被归零(回归测试)
        let limits = OutputLimits {
            max_lines: 2000,
            max_bytes: 50 * 1024,
            max_chars: 0,
        };
        assert_eq!(limits.effective_max_bytes(), 50 * 1024);
    }
}
