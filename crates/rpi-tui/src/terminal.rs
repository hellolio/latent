//! 终端抽象(08 文档 `terminal.ts` 的 Rust 对应物):raw 模式、终端尺寸、
//! 括号粘贴/同步输出的启停。
//!
//! 零依赖实现:raw 模式与尺寸查询经 `stty`(POSIX 系统自带),不引 libc。
//! 打开时保存原始终端配置,`restore` 精确恢复;Drop 兜底恢复,保证 panic
//! 路径下终端不留残废状态。

use std::io::{IsTerminal, Read, Write};
use std::process::{Command, Stdio};

pub struct Terminal {
    saved_settings: String,
    restored: bool,
}

impl Terminal {
    /// 打开终端:要求 stdin 是 TTY;进入 raw 模式、启用括号粘贴。
    pub fn open() -> std::io::Result<Terminal> {
        if !std::io::stdin().is_terminal() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotConnected,
                "stdin 不是 TTY,interactive 模式需要终端(非 TTY 请用 print 模式)",
            ));
        }
        let saved_settings = stty(&["-g"])?;
        stty(&["raw", "-echo"])?;
        let mut terminal = Terminal { saved_settings, restored: false };
        terminal.write_raw(ansi::BRACKETED_PASTE_ON);
        Ok(terminal)
    }

    /// 终端尺寸 `(rows, cols)`;查询失败时给保守默认 24x80。
    pub fn size() -> (usize, usize) {
        match stty(&["size"]) {
            Ok(output) => {
                let mut parts = output.split_whitespace();
                let rows = parts.next().and_then(|v| v.parse().ok()).unwrap_or(24);
                let cols = parts.next().and_then(|v| v.parse().ok()).unwrap_or(80);
                (rows.max(1), cols.max(1))
            }
            Err(_) => (24, 80),
        }
    }

    pub fn write_raw(&mut self, payload: &str) {
        let mut stdout = std::io::stdout();
        let _ = stdout.write_all(payload.as_bytes());
        let _ = stdout.flush();
    }

    /// 恢复终端(幂等):关括号粘贴、恢复 stty 配置。
    pub fn restore(&mut self) {
        if self.restored {
            return;
        }
        self.restored = true;
        self.write_raw(ansi::BRACKETED_PASTE_OFF);
        self.write_raw(ansi::SHOW_CURSOR);
        let _ = stty(&[&self.saved_settings]);
    }

    /// 从 stdin 读取字节(调用方在线程里循环调用)。
    pub fn read_bytes(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        std::io::stdin().read(buf)
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        self.restore();
    }
}

fn stty(args: &[&str]) -> std::io::Result<String> {
    let output = Command::new("stty")
        .args(args)
        .stdin(Stdio::inherit())
        .stderr(Stdio::null())
        .output()?;
    if !output.status.success() {
        return Err(std::io::Error::other(format!(
            "stty {args:?} 失败: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

use crate::ansi;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn size_returns_positive_dimensions() {
        let (rows, cols) = Terminal::size();
        assert!(rows >= 1 && cols >= 1);
    }

    // open()/restore() 需要 TTY,CI 无终端环境下跳过人工验证;
    // 行为正确性由 interactive 模式的端到端使用兜底。
}
