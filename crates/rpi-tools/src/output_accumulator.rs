//! 流式输出聚合(05 文档 §4 output-accumulator.ts):增量解码、有界内存、
//! 超限时完整输出落临时文件(`details.fullOutputPath`)。

use std::io::Write;
use std::path::PathBuf;

use crate::truncate::{truncate_tail, TruncationResult};

/// bash 输出临时文件前缀(pi 用 `pi-bash`)。
const TEMP_FILE_PREFIX: &str = "rpi-bash";

pub struct OutputSnapshot {
    pub content: String,
    pub truncation: TruncationResult,
    pub full_output_path: Option<PathBuf>,
}

/// 增量聚合流式输出:只保留解码尾部(有界),完整输出超限时落盘。
pub struct OutputAccumulator {
    max_lines: usize,
    max_bytes: usize,
    /// 尾部缓冲上限(字节):max_bytes 的两倍,保证 snapshot 有足够文本
    max_rolling_bytes: usize,
    /// 流式 UTF-8 解码器的不完整多字节序列残留
    pending_utf8: Vec<u8>,
    /// spill 前的原始字节缓冲;spill 后清空,新字节直接写临时文件
    raw_chunks: Vec<u8>,
    tail_text: String,
    tail_bytes: usize,
    /// 尾部缓冲是否从行边界开始(trim_tail 后可能不是)
    tail_starts_at_line_boundary: bool,
    total_raw_bytes: usize,
    total_decoded_bytes: usize,
    completed_lines: usize,
    current_line_bytes: usize,
    has_open_line: bool,
    finished: bool,
    temp_path: Option<PathBuf>,
    temp_file: Option<std::fs::File>,
}

impl OutputAccumulator {
    pub fn new(max_lines: usize, max_bytes: usize) -> Self {
        OutputAccumulator {
            max_lines,
            max_bytes,
            max_rolling_bytes: (max_bytes * 2).max(1),
            pending_utf8: Vec::new(),
            raw_chunks: Vec::new(),
            tail_text: String::new(),
            tail_bytes: 0,
            tail_starts_at_line_boundary: true,
            total_raw_bytes: 0,
            total_decoded_bytes: 0,
            completed_lines: 0,
            current_line_bytes: 0,
            has_open_line: false,
            finished: false,
            temp_path: None,
            temp_file: None,
        }
    }

    /// 追加原始字节(可按任意块边界切分,含多字节字符中段)。
    pub fn append(&mut self, data: &[u8]) {
        assert!(
            !self.finished,
            "cannot append to a finished output accumulator"
        );
        self.total_raw_bytes += data.len();

        self.pending_utf8.extend_from_slice(data);
        // 流式解码:取最长合法 UTF-8 前缀,残留(至多 3 字节)留下轮
        let valid = match std::str::from_utf8(&self.pending_utf8) {
            Ok(_) => self.pending_utf8.len(),
            Err(error) => error.valid_up_to(),
        };
        if valid > 0 {
            let text = String::from_utf8_lossy(&self.pending_utf8[..valid]).into_owned();
            self.pending_utf8.drain(..valid);
            self.append_decoded_text(&text);
        }

        if self.temp_file.is_some() {
            self.write_to_temp_file(data);
        } else if self.should_use_temp_file() {
            self.ensure_temp_file();
            self.write_to_temp_file(data);
        } else {
            self.raw_chunks.extend_from_slice(data);
        }
    }

    /// 结束输入:冲刷解码残留;已超限则确保落盘。
    pub fn finish(&mut self) {
        if self.finished {
            return;
        }
        self.finished = true;
        if !self.pending_utf8.is_empty() {
            let text = String::from_utf8_lossy(&self.pending_utf8).into_owned();
            self.pending_utf8.clear();
            self.append_decoded_text(&text);
        }
        if self.should_use_temp_file() {
            self.ensure_temp_file();
        }
        if let Some(file) = self.temp_file.as_mut() {
            let _ = file.flush();
        }
    }

    /// 截断视图:末尾保留 max_lines/max_bytes;`persist_if_truncated` 为真且
    /// 已截断时确保完整输出在临时文件里。
    pub fn snapshot(&mut self, persist_if_truncated: bool) -> OutputSnapshot {
        let snapshot_text = if self.tail_starts_at_line_boundary {
            self.tail_text.as_str()
        } else {
            // 丢掉首个残行,保证 snapshot 不以半行开头(pi 的 getSnapshotText)
            match self.tail_text.find('\n') {
                Some(index) => &self.tail_text[index + 1..],
                None => self.tail_text.as_str(),
            }
        };
        let tail = truncate_tail(snapshot_text, self.max_lines, self.max_bytes);
        let truncated =
            self.total_lines() > self.max_lines || self.total_decoded_bytes > self.max_bytes;
        let truncated_by = if !truncated {
            "none"
        } else if self.total_decoded_bytes > self.max_bytes {
            "bytes"
        } else {
            "lines"
        };
        let truncation = TruncationResult {
            content: tail.content.clone(),
            truncated,
            truncated_by,
            total_lines: self.total_lines(),
            total_bytes: self.total_decoded_bytes,
            output_lines: tail.output_lines,
            head: false,
        };
        if persist_if_truncated && truncated {
            self.ensure_temp_file();
        }
        OutputSnapshot {
            content: tail.content,
            truncation,
            full_output_path: self.temp_path.clone(),
        }
    }

    /// 当前尾部视图(TUI 实时显示用)。
    pub fn tail(&self) -> String {
        self.tail_text.clone()
    }

    /// 强制开启完整输出落盘(后台化用):把此前缓冲的原始字节一次写入临时
    /// 文件,此后全部增量直写;返回文件路径(创建失败 = None,退化为内存
    /// 尾部视图,完整输出不保证)。
    pub fn force_temp_file(&mut self) -> Option<PathBuf> {
        self.ensure_temp_file();
        self.temp_path.clone()
    }

    pub fn total_lines(&self) -> usize {
        self.completed_lines + usize::from(self.has_open_line)
    }

    fn append_decoded_text(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        let bytes = text.len();
        self.total_decoded_bytes += bytes;
        self.tail_text.push_str(text);
        self.tail_bytes += bytes;
        if self.tail_bytes > self.max_rolling_bytes * 2 {
            self.trim_tail();
        }

        let newlines = text.matches('\n').count();
        if newlines == 0 {
            self.current_line_bytes += bytes;
            self.has_open_line = true;
        } else {
            self.completed_lines += newlines;
            let tail = &text[text.rfind('\n').unwrap() + 1..];
            self.current_line_bytes = tail.len();
            self.has_open_line = !tail.is_empty();
        }
    }

    /// 尾部缓冲裁到 max_rolling_bytes(UTF-8 字符边界对齐)。
    fn trim_tail(&mut self) {
        let buffer = self.tail_text.as_bytes();
        if buffer.len() <= self.max_rolling_bytes {
            self.tail_bytes = buffer.len();
            return;
        }
        let mut start = buffer.len() - self.max_rolling_bytes;
        while start < buffer.len() && (buffer[start] & 0xc0) == 0x80 {
            start += 1;
        }
        self.tail_starts_at_line_boundary = start == 0 || buffer[start - 1] == b'\n';
        self.tail_text = self.tail_text[start..].to_string();
        self.tail_bytes = self.tail_text.len();
    }

    fn should_use_temp_file(&self) -> bool {
        self.total_raw_bytes > self.max_bytes
            || self.total_decoded_bytes > self.max_bytes
            || self.total_lines() > self.max_lines
    }

    fn ensure_temp_file(&mut self) {
        if self.temp_path.is_some() {
            return;
        }
        let id = uuid::Uuid::now_v7().simple().to_string();
        let path = std::env::temp_dir().join(format!("{TEMP_FILE_PREFIX}-{id}.log"));
        let file = std::fs::File::create(&path);
        match file {
            Ok(mut file) => {
                let chunks = std::mem::take(&mut self.raw_chunks);
                let _ = file.write_all(&chunks);
                self.temp_path = Some(path);
                self.temp_file = Some(file);
            }
            Err(_) => {
                // 落盘失败:退化为内存缓冲(输出提示里不出现 fullOutputPath)
                self.raw_chunks.clear();
            }
        }
    }

    fn write_to_temp_file(&mut self, data: &[u8]) {
        if let Some(file) = self.temp_file.as_mut() {
            let _ = file.write_all(data);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::truncate::{DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES};

    #[test]
    fn accumulates_and_reports_totals() {
        let mut acc = OutputAccumulator::new(DEFAULT_MAX_LINES, DEFAULT_MAX_BYTES);
        acc.append(b"line one\n");
        acc.append(b"li");
        acc.append("ne tw".as_bytes());
        acc.append(&[0x6f]); // 'o' 按单字节
        acc.finish();
        let snapshot = acc.snapshot(true);
        assert_eq!(snapshot.content, "line one\nline two");
        assert!(!snapshot.truncation.truncated);
        assert_eq!(snapshot.truncation.total_lines, 2);
        assert_eq!(snapshot.truncation.total_bytes, "line one\nline two".len());
        assert!(snapshot.full_output_path.is_none());
    }

    #[test]
    fn handles_multibyte_split_across_chunks() {
        let mut acc = OutputAccumulator::new(DEFAULT_MAX_LINES, DEFAULT_MAX_BYTES);
        let text = "中文输出\n";
        let bytes = text.as_bytes();
        let (a, b) = bytes.split_at(4); // 切在多字节字符中段
        acc.append(a);
        acc.append(b);
        acc.finish();
        let snapshot = acc.snapshot(true);
        assert_eq!(snapshot.content, "中文输出");
        assert_eq!(snapshot.truncation.total_lines, 1);
    }

    #[test]
    fn truncates_tail_and_spills_full_output() {
        let mut acc = OutputAccumulator::new(10, DEFAULT_MAX_BYTES);
        for i in 0..50 {
            acc.append(format!("line {i}\n").as_bytes());
        }
        acc.finish();
        let snapshot = acc.snapshot(true);
        assert!(snapshot.truncation.truncated);
        assert_eq!(snapshot.truncation.output_lines, 10);
        assert!(snapshot.content.starts_with("line 40"));
        assert!(snapshot.content.ends_with("line 49"));
        let path = snapshot.full_output_path.expect("超限必须落盘");
        let full = std::fs::read_to_string(&path).unwrap();
        assert_eq!(full.lines().count(), 50);
        assert!(path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with(TEMP_FILE_PREFIX));
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn bounded_memory_tail() {
        let mut acc = OutputAccumulator::new(10, 1024);
        for _ in 0..1000 {
            acc.append(&vec![b'x'; 1024]);
        }
        acc.finish();
        assert!(
            acc.tail().len() <= 1024 * 4 + 4,
            "尾部缓冲必须有界: {}",
            acc.tail().len()
        );
        let snapshot = acc.snapshot(true);
        assert!(snapshot.truncation.truncated);
        assert_eq!(snapshot.truncation.truncated_by, "bytes");
    }
}
