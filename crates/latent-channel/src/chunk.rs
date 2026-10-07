//! 出站长文分段(上游 `src/auto-reply/chunk.ts` —— 常量逐个核实):
//!
//! - `DEFAULT_CHUNK_LIMIT = 4000`;per-channel 覆盖(`channels.<id>.textChunkLimit`,
//!   账号级优先于渠道级 —— 归并由调用方解析)。QQ 建议配 2000(NTQQ 保守值)、
//!   企微 2048 字节、TG 4000;
//! - mode:`length`(默认,硬切但优先**括号感知断点** —— 先窗口内括号外的
//!   换行,再最后一个空白)/ `newline`(按空行段落打包);
//! - markdown 版:切割处**闭合 code fence 并在续块重开**(``` 配对不破坏)。

use regex::Regex;
use std::sync::LazyLock;

/// 默认分段上限(字符数)。
pub const DEFAULT_CHUNK_LIMIT: usize = 4000;

/// 分段模式(上游 `DEFAULT_CHUNK_MODE = "length"`)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ChunkMode {
    #[default]
    Length,
    Newline,
}

/// 分段选项。
#[derive(Debug, Clone, Copy)]
pub struct ChunkOptions {
    pub limit: usize,
    pub mode: ChunkMode,
    /// true = 切割处闭合 code fence 并在续块重开
    pub markdown: bool,
}

impl Default for ChunkOptions {
    fn default() -> Self {
        ChunkOptions {
            limit: DEFAULT_CHUNK_LIMIT,
            mode: ChunkMode::Length,
            markdown: false,
        }
    }
}

/// 空行段落分隔(上游 newline 模式的 `/\n[\t ]*\n+/`)。
static BLANK_LINE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\n[\t ]*\n+").unwrap());

/// 主入口:按选项把长文切成段(顺序发送,段间可配延时)。
pub fn chunk_text(text: &str, options: ChunkOptions) -> Vec<String> {
    if options.limit == 0 || text.chars().count() <= options.limit {
        return vec![text.to_string()];
    }
    let raw = match options.mode {
        ChunkMode::Length => chunk_by_length(text, options.limit),
        ChunkMode::Newline => chunk_by_paragraph(text, options.limit),
    };
    if options.markdown {
        close_and_reopen_fences(raw)
    } else {
        raw
    }
}

/// length 模式:窗口内优先「括号外换行」,其次最后一个空白,最后硬切。
fn chunk_by_length(text: &str, limit: usize) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let total = chars.len();
    let mut chunks = Vec::new();
    let mut start = 0;
    while start < total {
        if total - start <= limit {
            chunks.push(chars[start..].iter().collect::<String>().trim_end().to_string());
            break;
        }
        let window_end = start + limit;
        let cut = find_break(&chars, start, window_end);
        chunks.push(chars[start..cut].iter().collect::<String>().trim_end().to_string());
        start = cut;
        // 跳过切点处的换行(空白断点把空格留在段尾已裁掉,换行由此吸收)
        while start < total && chars[start] == '\n' {
            start += 1;
        }
    }
    chunks
}

/// 断点选择(窗口 `(start, end)` 内):
/// 1. 括号外的换行(取最后一个)—— 切点在换行之前(换行不进上一段,
///    由调用方跳过);
/// 2. 最后一个空白(空格/tab,留在上一段尾部);
/// 3. 兜底硬切在 end。
///
/// 括号深度同时计 ASCII 与全角括号(P2-17:聊天场景中文全角 `（）【】｛｝`
/// 不计入 depth 时,括号内换行会被误当选为断点)。
fn find_break(chars: &[char], start: usize, end: usize) -> usize {
    let mut depth = 0usize;
    let mut last_newline = None;
    let mut last_space = None;
    for (offset, &c) in chars[start..end].iter().enumerate() {
        match c {
            '(' | '[' | '{' | '（' | '【' | '｛' => depth += 1,
            ')' | ']' | '}' | '）' | '】' | '｝' => depth = depth.saturating_sub(1),
            '\n' if depth == 0 => last_newline = Some(start + offset),
            ' ' | '\t' if depth == 0 => last_space = Some(start + offset),
            _ => {}
        }
    }
    if let Some(pos) = last_newline {
        return pos;
    }
    if let Some(pos) = last_space {
        return pos + 1;
    }
    end
}

/// newline 模式:按空行段落打包;单段超限时回退 length 模式切分。
fn chunk_by_paragraph(text: &str, limit: usize) -> Vec<String> {
    let paragraphs: Vec<&str> = BLANK_LINE.split(text).collect();
    let mut chunks = Vec::new();
    let mut current = String::new();
    for paragraph in paragraphs {
        if paragraph.chars().count() > limit {
            // 单段超限:先落当前积累,再按 length 切该段(末段留作当前积累)
            if !current.is_empty() {
                chunks.push(std::mem::take(&mut current));
            }
            let pieces = chunk_by_length(paragraph, limit);
            for piece in &pieces[..pieces.len() - 1] {
                chunks.push(piece.clone());
            }
            current = pieces.last().cloned().unwrap_or_default();
            continue;
        }
        let candidate = if current.is_empty() {
            paragraph.to_string()
        } else {
            format!("{current}\n\n{paragraph}")
        };
        if candidate.chars().count() <= limit {
            current = candidate;
        } else {
            chunks.push(std::mem::take(&mut current));
            current = paragraph.to_string();
        }
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    if chunks.is_empty() {
        chunks.push(String::new());
    }
    chunks
}

/// markdown fence 修复:某段在 fence 内结束时补闭合 ```(续块以原 info
/// string 重开),保证后续渲染 ``` 配对不破坏。
fn close_and_reopen_fences(chunks: Vec<String>) -> Vec<String> {
    let mut out = Vec::with_capacity(chunks.len());
    let mut in_fence = false;
    let mut lang = String::new();
    for chunk in chunks {
        let mut state = in_fence;
        let mut state_lang = lang.clone();
        for line in chunk.lines() {
            let trimmed = line.trim_start();
            if trimmed.starts_with("```") {
                if state {
                    state = false;
                } else {
                    state = true;
                    state_lang = trimmed
                        .trim_start_matches('`')
                        .trim()
                        .to_string();
                }
            }
        }
        let mut body = chunk;
        if state && !body.ends_with("```") {
            if !body.ends_with('\n') {
                body.push('\n');
            }
            body.push_str("```");
        }
        if in_fence {
            out.push(format!("```{}\n{}", lang, body));
        } else {
            out.push(body);
        }
        in_fence = state;
        lang = state_lang;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_text_passes_through() {
        assert_eq!(
            chunk_text("短消息", ChunkOptions::default()),
            vec!["短消息"]
        );
    }

    #[test]
    fn length_mode_prefers_newline_outside_brackets() {
        // 窗口 20:两处换行,第二处(索引 20+)超窗;括号内的换行不算断点
        let text = "第一行\n第二行(括号\n内换行)\n第三行内容继续到底";
        let chunks = chunk_text(
            text,
            ChunkOptions {
                limit: 12,
                mode: ChunkMode::Length,
                markdown: false,
            },
        );
        assert_eq!(chunks[0], "第一行", "应优先括号外换行作为断点");
    }

    /// P2-17:全角括号同样计入括号深度 —— 括号内换行不当断点。
    #[test]
    fn fullwidth_brackets_count_toward_depth() {
        // 括号深度:第二处换行在全角括号内,不得作断点;第一处换行在括号外
        let text = "第一行\n第二行（全角括号\n内换行）\n第三行内容继续到底";
        let chunks = chunk_text(
            text,
            ChunkOptions {
                limit: 12,
                mode: ChunkMode::Length,
                markdown: false,
            },
        );
        assert_eq!(chunks[0], "第一行", "全角括号内的换行不是断点: {chunks:?}");
    }

    #[test]
    fn length_mode_falls_back_to_whitespace_then_hard_cut() {
        // 无换行:退到最后的空格
        let chunks = chunk_text(
            "aaaa bbbb cccc dddd",
            ChunkOptions {
                limit: 9,
                mode: ChunkMode::Length,
                markdown: false,
            },
        );
        assert_eq!(chunks[0], "aaaa", "空白断点切分,段尾空格裁掉");
        // 无空白:硬切
        let chunks = chunk_text(
            "abcdefghij",
            ChunkOptions {
                limit: 4,
                mode: ChunkMode::Length,
                markdown: false,
            },
        );
        assert_eq!(chunks, vec!["abcd", "efgh", "ij"]);
    }

    #[test]
    fn newline_mode_packs_paragraphs() {
        let text = "para-one\n\npara-two\n\npara-three";
        let chunks = chunk_text(
            text,
            ChunkOptions {
                limit: 20,
                mode: ChunkMode::Newline,
                markdown: false,
            },
        );
        assert_eq!(chunks, vec!["para-one\n\npara-two", "para-three"]);
    }

    #[test]
    fn markdown_fences_closed_and_reopened() {
        let body = format!(
            "前置说明\n```rust\n{}\n```\n结尾",
            "fn a() {}".repeat(4)
        );
        let chunks = chunk_text(
            &body,
            ChunkOptions {
                limit: 30,
                mode: ChunkMode::Length,
                markdown: true,
            },
        );
        assert!(chunks.len() >= 2);
        // 每一段内 ``` 必须配对(闭合/重开生效)
        for chunk in &chunks {
            let fences = chunk
                .lines()
                .filter(|line| line.trim_start().starts_with("```"))
                .count();
            assert_eq!(fences % 2, 0, "段内 fence 必须配对: {chunk:?}");
        }
        // 续块以原语言重开
        assert!(chunks[1].starts_with("```rust\n"), "续块应重开 fence: {}", chunks[1]);
        // 拼接后可还原出完整内容(去掉补的 fence 对)
        let joined = chunks.join("\n");
        assert!(joined.contains("fn a() {}"));
    }

    #[test]
    fn default_limit_matches_upstream() {
        assert_eq!(DEFAULT_CHUNK_LIMIT, 4000);
        assert_eq!(ChunkOptions::default().mode, ChunkMode::Length);
    }
}
