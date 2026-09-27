//! Spinner 动画帧(pi Loader 组件的对应物):状态行/编辑器边框的忙碌指示。

/// 动画帧序列:半圆旋转族(与字符同宽同高的转圈效果)。
pub const FRAMES: [&str; 4] = ["◐", "◓", "◑", "◒"];

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
