//! SSE 解码(pi 各适配器共用的 decodeSseLine/iterateSseMessages 端口)。
//!
//! 按行喂入;空行 = 事件边界,flush 出当前事件。`\n` 字节不可能是 UTF-8 多字节
//! 序列的一部分(多字节序列全部 ≥ 0x80),因此按字节切行后逐行转 UTF-8 是安全的。

/// 一个已 flush 的 SSE 事件(event 字段 + 拼接的 data)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseEvent {
    pub event: Option<String>,
    pub data: String,
}

#[derive(Debug, Default)]
pub struct SseDecoder {
    event: Option<String>,
    data: Vec<String>,
}

impl SseDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// 喂入一行(不带行终止符;调用方负责剥离 \r\n 或 \n)。返回 None 表示事件未完。
    pub fn feed_line(&mut self, line: &str) -> Option<SseEvent> {
        if line.is_empty() {
            return self.flush();
        }
        if line.starts_with(':') {
            return None; // 注释行
        }
        let (field, value) = match line.split_once(':') {
            Some((f, v)) => (f, v.strip_prefix(' ').unwrap_or(v)),
            None => (line, ""),
        };
        match field {
            "event" => self.event = Some(value.to_string()),
            "data" => self.data.push(value.to_string()),
            _ => {} // id/retry 及未知字段忽略
        }
        None
    }

    /// 流结束时 flush 残留事件。
    pub fn flush(&mut self) -> Option<SseEvent> {
        if self.event.is_none() && self.data.is_empty() {
            return None;
        }
        let event = SseEvent {
            event: self.event.take(),
            data: self.data.join("\n"),
        };
        self.data.clear();
        Some(event)
    }
}

/// 字节流 → SSE 行解码器:缓冲原始字节,按 \n 切行、剥离 \r,产出完整行。
#[derive(Default)]
pub struct LineDecoder {
    buffer: Vec<u8>,
}

impl LineDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// 喂入字节块,产出其中所有完整行(UTF-8)。
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<String> {
        self.buffer.extend_from_slice(chunk);
        let mut lines = Vec::new();
        while let Some(pos) = self.buffer.iter().position(|&b| b == b'\n') {
            let line_bytes: Vec<u8> = self.buffer.drain(..=pos).collect();
            let mut line = &line_bytes[..line_bytes.len() - 1]; // 去掉 \n
            if line.last() == Some(&b'\r') {
                line = &line[..line.len() - 1];
            }
            lines.push(String::from_utf8_lossy(line).into_owned());
        }
        lines
    }

    /// 流结束:把残留字节作为最后一行产出。
    pub fn finish(&mut self) -> Option<String> {
        if self.buffer.is_empty() {
            return None;
        }
        let mut line = std::mem::take(&mut self.buffer);
        if line.last() == Some(&b'\r') {
            line.pop();
        }
        Some(String::from_utf8_lossy(&line).into_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode(input: &str) -> Vec<SseEvent> {
        let mut decoder = SseDecoder::new();
        let mut out = Vec::new();
        for line in input.split('\n') {
            let line = line.strip_suffix('\r').unwrap_or(line);
            if let Some(ev) = decoder.feed_line(line) {
                out.push(ev);
            }
        }
        if let Some(ev) = decoder.flush() {
            out.push(ev);
        }
        out
    }

    #[test]
    fn decodes_event_and_multi_data_lines() {
        let events = decode("event: message_start\ndata: {\"a\":\ndata: 1}\n\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event.as_deref(), Some("message_start"));
        assert_eq!(events[0].data, "{\"a\":\n1}");
    }

    #[test]
    fn ignores_comments_and_unknown_fields() {
        let events = decode(": ping\nid: 1\ndata: hello\n\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "hello");
    }

    #[test]
    fn line_decoder_handles_split_multibyte_across_chunks() {
        let mut decoder = LineDecoder::new();
        assert!(decoder.feed("data: 你".as_bytes()).is_empty());
        let lines = decoder.feed("好\n".as_bytes());
        assert_eq!(lines, vec!["data: 你好"]);
    }

    #[test]
    fn line_decoder_crlf_and_finish() {
        let mut decoder = LineDecoder::new();
        let lines = decoder.feed(b"a\r\nb");
        assert_eq!(lines, vec!["a"]);
        assert_eq!(decoder.finish(), Some("b".to_string()));
    }
}
