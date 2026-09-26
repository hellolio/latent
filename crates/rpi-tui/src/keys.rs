//! 终端按键解析(08 文档 `keys.ts` 的 Rust 对应物):字节流 → `Key` 事件。
//!
//! 增量式解析器:喂入任意长度的字节切片,吐出解析完成的按键。不完整的
//! 转义序列跨 feed 边界缓存;括号粘贴(`ESC[200~`/`ESC[201~`)区间内字节
//! 聚成 `Key::Paste`,不逐字符解释。

/// 解析后的按键事件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Key {
    Char(char),
    Enter,
    Tab,
    Backspace,
    Delete,
    Escape,
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
    PageUp,
    PageDown,
    /// Ctrl + 字母(a-z)或 Ctrl+@ 等;`Ctrl('c')` 即 ^C。
    Ctrl(char),
    /// 括号粘贴的整段文本。
    Paste(String),
}

/// 增量式按键解析器。
#[derive(Default)]
pub struct KeyParser {
    buf: Vec<u8>,
    in_paste: bool,
}

impl KeyParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// 喂入字节,返回本轮解析出的按键。
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<Key> {
        self.buf.extend_from_slice(bytes);
        let mut keys = Vec::new();
        while !self.buf.is_empty() {
            match self.next_key() {
                Some(key) => keys.push(key),
                None => break, // 缓冲区剩不完整序列,等下一轮
            }
        }
        keys
    }

    fn next_key(&mut self) -> Option<Key> {
        let first = self.buf[0];
        if self.in_paste {
            return self.next_paste_chunk();
        }
        match first {
            0x1b => self.next_escape(),
            b'\r' | b'\n' => {
                self.buf.remove(0);
                Some(Key::Enter)
            }
            b'\t' => {
                self.buf.remove(0);
                Some(Key::Tab)
            }
            0x7f | 0x08 => {
                self.buf.remove(0);
                Some(Key::Backspace)
            }
            // C0 控制字符:C1 冲突区(0x1b 已处理)之外按 Ctrl+letter 解释
            // (0x01=^a … 0x1a=^z;\r/\n/\t/\x08 已在上面分支处理)
            0x00..=0x1a => {
                self.buf.remove(0);
                Some(Key::Ctrl((first + b'a' - 1) as char))
            }
            0x1c..=0x1f => {
                self.buf.remove(0);
                Some(Key::Ctrl((first - 0x1c + b'w') as char))
            }
            _ => {
                // UTF-8 多字节:按前导字节长度取完整字符;不完整则等待
                let len = utf8_len(first);
                if self.buf.len() < len {
                    return None;
                }
                let bytes: Vec<u8> = self.buf.drain(..len).collect();
                match std::str::from_utf8(&bytes) {
                    Ok(s) => s.chars().next().map(Key::Char),
                    // 无效 UTF-8:丢弃(映射为 Escape 会误触发 abort 语义)
                    Err(_) => self.next_key(),
                }
            }
        }
    }

    fn next_escape(&mut self) -> Option<Key> {
        if self.buf.len() < 2 {
            // 只有 lone ESC:无法确定是 Escape 还是序列前缀。
            // 终端下 ESC 键通常后随超时;字节流模型里保守视为 Escape。
            self.buf.remove(0);
            return Some(Key::Escape);
        }
        match self.buf[1] {
            b'[' => self.next_csi(),
            b'O' => {
                if self.buf.len() < 3 {
                    return None;
                }
                let key = match self.buf[2] {
                    b'A' => Key::Up,
                    b'B' => Key::Down,
                    b'C' => Key::Right,
                    b'D' => Key::Left,
                    b'H' => Key::Home,
                    b'F' => Key::End,
                    _ => Key::Escape,
                };
                self.buf.drain(..3);
                Some(key)
            }
            _ => {
                // ESC + 普通键:Alt 语义,本 UI 不用,按 Escape + Char 拆开
                self.buf.remove(0);
                Some(Key::Escape)
            }
        }
    }

    fn next_csi(&mut self) -> Option<Key> {
        // 找终止字节(0x40-0x7E);括号粘贴序列在此分流
        let end = self.buf[2..].iter().position(|b| (0x40..=0x7e).contains(b))? + 2; // 序列未完整则 None
        let params = self.buf[2..end].to_vec();
        let final_byte = self.buf[end];
        self.buf.drain(..=end);
        let param_str = String::from_utf8_lossy(&params).to_string();

        match (final_byte, param_str.as_str()) {
            (b'~', "200") => {
                self.in_paste = true;
                self.next_paste_chunk()
            }
            (b'~', "201") => Some(Key::Escape), // 无开始标记的粘贴结束,忽略
            (b'A', _) => Some(Key::Up),
            (b'B', _) => Some(Key::Down),
            (b'C', _) => Some(Key::Right),
            (b'D', _) => Some(Key::Left),
            (b'H', _) => Some(Key::Home),
            (b'F', _) => Some(Key::End),
            (b'~', "1" | "7") => Some(Key::Home),
            (b'~', "4" | "8") => Some(Key::End),
            (b'~', "3") => Some(Key::Delete),
            (b'~', "5") => Some(Key::PageUp),
            (b'~', "6") => Some(Key::PageDown),
            _ => Some(Key::Escape), // 未识别序列丢弃
        }
    }

    /// 粘贴区间内整段收集,直到 `ESC[201~`;结束序列未到则等待更多字节。
    fn next_paste_chunk(&mut self) -> Option<Key> {
        if let Some(pos) = find_subsequence(&self.buf, b"\x1b[201~") {
            let text: Vec<u8> = self.buf.drain(..pos).collect();
            self.buf.drain(..6);
            self.in_paste = false;
            if text.is_empty() {
                return self.next_key();
            }
            return Some(Key::Paste(String::from_utf8_lossy(&text).into_owned()));
        }
        None
    }
}

fn utf8_len(first: u8) -> usize {
    match first {
        0x00..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf7 => 4,
        _ => 1,
    }
}

fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    (0..=haystack.len() - needle.len()).find(|i| &haystack[*i..*i + needle.len()] == needle)
}

/// 键位匹配(08 文档 `matchesKey`):`"ctrl+c"`/`"enter"`/`"esc"`/`"tab"`/
/// `"up"`/`"down"`/普通字符。键位表可配置,禁止硬编码键检查(08 §3)。
pub fn matches_key(key: &Key, binding: &str) -> bool {
    let binding = binding.to_ascii_lowercase();
    match key {
        // 字符键位大小写不敏感("q" 同时匹配 Q;要区分时用 "shift+q" 扩展)
        Key::Char(c) => binding == c.to_lowercase().to_string(),
        Key::Enter => binding == "enter" || binding == "return",
        Key::Tab => binding == "tab",
        Key::Backspace => binding == "backspace",
        Key::Delete => binding == "delete",
        Key::Escape => binding == "escape" || binding == "esc",
        Key::Left => binding == "left",
        Key::Right => binding == "right",
        Key::Up => binding == "up",
        Key::Down => binding == "down",
        Key::Home => binding == "home",
        Key::End => binding == "end",
        Key::PageUp => binding == "pageup",
        Key::PageDown => binding == "pagedown",
        Key::Ctrl(c) => binding == format!("ctrl+{c}"),
        Key::Paste(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(bytes: &[u8]) -> Vec<Key> {
        KeyParser::new().feed(bytes)
    }

    #[test]
    fn plain_chars_and_enter() {
        assert_eq!(parse(b"hi\r"), vec![Key::Char('h'), Key::Char('i'), Key::Enter]);
        assert_eq!(parse(b"\n"), vec![Key::Enter]);
        assert_eq!(parse(b"\t"), vec![Key::Tab]);
        assert_eq!(parse(b"\x7f"), vec![Key::Backspace]);
    }

    #[test]
    fn control_keys() {
        assert_eq!(parse(b"\x03"), vec![Key::Ctrl('c')]);
        assert_eq!(parse(b"\x01"), vec![Key::Ctrl('a')]);
        assert_eq!(parse(b"\x1a"), vec![Key::Ctrl('z')]);
    }

    #[test]
    fn arrow_and_navigation_sequences() {
        assert_eq!(
            parse(b"\x1b[A\x1b[B\x1b[C\x1b[D"),
            vec![Key::Up, Key::Down, Key::Right, Key::Left]
        );
        assert_eq!(parse(b"\x1b[3~"), vec![Key::Delete]);
        assert_eq!(parse(b"\x1b[H\x1b[F"), vec![Key::Home, Key::End]);
        assert_eq!(parse(b"\x1b[5~\x1b[6~"), vec![Key::PageUp, Key::PageDown]);
        assert_eq!(parse(b"\x1bOA\x1bOH"), vec![Key::Up, Key::Home]);
    }

    #[test]
    fn lone_escape() {
        assert_eq!(parse(b"\x1b"), vec![Key::Escape]);
    }

    #[test]
    fn utf8_multibyte_split_across_feeds() {
        let mut parser = KeyParser::new();
        assert!(parser.feed(&"中".bytes().take(2).collect::<Vec<u8>>()).is_empty());
        assert_eq!(parser.feed(&"中".bytes().skip(2).collect::<Vec<u8>>()), vec![Key::Char('中')]);
    }

    #[test]
    fn bracketed_paste_groups_text() {
        // "hello 中文" 的 UTF-8 字节 + 粘贴结束序列
        let input = b"\x1b[200~hello \xe4\xb8\xad\xe6\x96\x87\x1b[201~x";
        assert_eq!(
            parse(input),
            vec![
                Key::Paste("hello 中文".into()),
                Key::Char('x'),
            ]
        );
    }

    #[test]
    fn paste_split_across_feeds() {
        let mut parser = KeyParser::new();
        assert!(parser.feed(b"\x1b[200~ab").is_empty()); // 结束序列未到,等待
        assert!(parser.feed(b"c\x1b[20").is_empty());
        assert_eq!(parser.feed(b"1~"), vec![Key::Paste("abc".into())]);
    }

    #[test]
    fn empty_paste_yields_nothing_extra() {
        let mut parser = KeyParser::new();
        let mut keys = parser.feed(b"\x1b[200~\x1b[201~x");
        assert_eq!(keys.pop(), Some(Key::Char('x')));
        assert!(keys.is_empty());
    }

    #[test]
    fn matches_key_bindings() {
        assert!(matches_key(&Key::Ctrl('c'), "ctrl+c"));
        assert!(matches_key(&Key::Enter, "enter"));
        assert!(matches_key(&Key::Escape, "esc"));
        assert!(matches_key(&Key::Char('q'), "q"));
        assert!(!matches_key(&Key::Ctrl('c'), "ctrl+x"));
        assert!(!matches_key(&Key::Paste("ab".into()), "a"));
    }

    #[test]
    fn unrecognized_csi_drops_to_escape() {
        assert_eq!(parse(b"\x1b[99z"), vec![Key::Escape]);
    }
}
