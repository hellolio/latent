//! Spinner 动画帧(pi Loader 组件的对应物):状态行/编辑器边框的忙碌指示。

/// 动画帧序列(取 pi loader 的星形族)。
pub const FRAMES: [&str; 6] = ["✶", "✸", "✹", "✺", "✹", "✸"];

/// 第 `tick` 拍的帧(tick 递增,自动取模)。
pub fn frame(tick: usize) -> &'static str {
    FRAMES[tick % FRAMES.len()]
}

/// thinking 块的静态前缀(✻,pi assistant thinking 标记)。
pub const THINKING_MARK: &str = "✻";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_cycle() {
        assert_eq!(frame(0), FRAMES[0]);
        assert_eq!(frame(FRAMES.len()), FRAMES[0]);
        assert_eq!(frame(3), FRAMES[3]);
    }
}
