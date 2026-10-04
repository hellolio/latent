//! 按键抽象(crossterm 0.29 承担字节级解析;这里只做语义归一):
//! 上层(handle_key)与编辑器只认 `Key`,不感知终端转义细节。

use ratatui::crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
};

use crate::selection::MouseAction;

/// 归一化按键。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Key {
    Char(char),
    Enter,
    ShiftEnter,
    AltEnter,
    Backspace,
    Delete,
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
    PageUp,
    PageDown,
    Tab,
    /// Shift+Tab(会话模式循环,13 文档 §10.2)
    BackTab,
    Esc,
    /// Ctrl+字母(小写归一)
    Ctrl(char),
    Paste(String),
    /// 鼠标滚轮上行(fullscreen 模式捕获鼠标后到达)
    ScrollUp,
    /// 鼠标滚轮下行
    ScrollDown,
    /// 左键选择手势(按下/拖动/抬起;fullscreen 应用层选区)
    Mouse(MouseAction),
    /// 终端尺寸变化
    Resize,
    /// 未映射的按键(方向键 + 修饰符组合等;编辑器不消费)
    Other,
}

/// crossterm `Event` → `Key`;None 表示本应用不关心的事件。
pub fn from_event(event: &Event) -> Option<Key> {
    match event {
        Event::Key(key) if key.kind == KeyEventKind::Press => Some(from_key_event(key)),
        Event::Paste(text) => Some(Key::Paste(text.clone())),
        Event::Resize(_, _) => Some(Key::Resize),
        Event::Mouse(mouse) => {
            let col = mouse.column;
            let row = mouse.row;
            // 扩展选区手势:Shift(xterm 约定下多数终端截留给原生选择)
            // 或 Alt(通常正常上报)修饰的左键按下
            let extend = mouse.modifiers.contains(KeyModifiers::SHIFT)
                || mouse.modifiers.contains(KeyModifiers::ALT);
            match mouse.kind {
                MouseEventKind::ScrollUp => Some(Key::ScrollUp),
                MouseEventKind::ScrollDown => Some(Key::ScrollDown),
                MouseEventKind::Down(MouseButton::Left) => {
                    Some(Key::Mouse(MouseAction::Down { col, row, extend }))
                }
                MouseEventKind::Drag(MouseButton::Left) => {
                    Some(Key::Mouse(MouseAction::Drag { col, row }))
                }
                MouseEventKind::Up(MouseButton::Left) => {
                    Some(Key::Mouse(MouseAction::Up { col, row }))
                }
                // 右/中键与移动不消费
                _ => None,
            }
        }
        _ => None,
    }
}

/// crossterm `KeyEvent` → `Key`。
pub fn from_key_event(key: &KeyEvent) -> Key {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    match key.code {
        KeyCode::Char('c') if ctrl => Key::Ctrl('c'),
        KeyCode::Char(c) if ctrl => Key::Ctrl(c.to_ascii_lowercase()),
        KeyCode::Char(_c) if alt => Key::Other,
        KeyCode::Char(c) => Key::Char(c),
        KeyCode::Enter if shift => Key::ShiftEnter,
        KeyCode::Enter if alt => Key::AltEnter,
        KeyCode::Enter => Key::Enter,
        KeyCode::Backspace => Key::Backspace,
        KeyCode::Delete => Key::Delete,
        KeyCode::Left => Key::Left,
        KeyCode::Right => Key::Right,
        KeyCode::Up if ctrl => Key::Other,
        KeyCode::Up => Key::Up,
        KeyCode::Down if ctrl => Key::Other,
        KeyCode::Down => Key::Down,
        KeyCode::Home => Key::Home,
        KeyCode::End => Key::End,
        KeyCode::PageUp => Key::PageUp,
        KeyCode::PageDown => Key::PageDown,
        // crossterm 把 CSI Z(\x1b[Z)解析为独立的 KeyCode::BackTab(自带 SHIFT),
        // 不是 Tab+SHIFT 组合 —— 必须单独映射
        KeyCode::BackTab => Key::BackTab,
        KeyCode::Tab if shift => Key::BackTab,
        KeyCode::Tab => Key::Tab,
        KeyCode::Esc => Key::Esc,
        _ => Key::Other,
    }
}

// 本地修饰键兜底(仅 macOS):协议缺失的终端(WezTerm 默认配置、
// Terminal.app、iTerm2 <3.5 等)对 Shift+Enter 只发裸 CR,crossterm 解析
// 为不带修饰的 Enter。这里用 CoreGraphics 查询物理 Shift 键状态,把
// “裸 Enter + 物理 Shift 按下”归一为 ShiftEnter。对齐上游 pi 的
// Apple Terminal 兜底,但放宽到全部本地 macOS 会话:协议终端里裸 CR
// 本就是未修饰 Enter,不会误判(pi 只对 Apple_Terminal 启用)。
//
// SSH 会话(SSH_CONNECTION/SSH_TTY 任一存在)禁用:物理键盘在远端,
// 本地修饰键状态与输入方不一致。兜底不可用时 Ctrl+J / Alt+Enter
// 仍可换行。

/// 纯函数:裸 Enter 且本地 Shift 按下 → ShiftEnter,其余原样返回。
fn native_shift_enter(key: Key, native_shift_pressed: bool) -> Key {
    match key {
        Key::Enter if native_shift_pressed => Key::ShiftEnter,
        other => other,
    }
}

/// 纯函数:给定目标平台是否 macOS、SSH 环境变量是否存在,判定兜底可用性。
fn fallback_active_on(is_macos: bool, ssh_env_present: bool) -> bool {
    is_macos && !ssh_env_present
}

fn ssh_env_present() -> bool {
    std::env::var_os("SSH_CONNECTION").is_some() || std::env::var_os("SSH_TTY").is_some()
}

/// 键盘线程入口:协议终端的修饰 Enter 已由 `from_key_event` 归一,这里只
/// 对裸 Enter 做本地兜底;非 macOS 或远程会话原样返回。
pub fn normalize_native_enter(key: Key) -> Key {
    let is_macos = cfg!(target_os = "macos");
    let fallback = fallback_active_on(is_macos, ssh_env_present());
    if key == Key::Enter && fallback && is_native_shift_pressed() {
        native_shift_enter(key, true)
    } else {
        key
    }
}

/// 查询物理 Shift(左或右)是否按下。macOS 用 readkey 安全封装
/// CoreGraphics `CGEventSourceFlagsState`,无需辅助功能权限;其余平台无
/// 本地兜底,恒 false。
#[cfg(target_os = "macos")]
fn is_native_shift_pressed() -> bool {
    use readkey::Keycode;
    Keycode::Shift.is_pressed() || Keycode::RightShift.is_pressed()
}

#[cfg(not(target_os = "macos"))]
fn is_native_shift_pressed() -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::crossterm::event::KeyCode as K;

    fn key(code: KeyCode, modifiers: KeyModifiers) -> Key {
        from_key_event(&KeyEvent::new(code, modifiers))
    }

    #[test]
    fn ctrl_letters_are_normalized_lowercase() {
        assert_eq!(key(K::Char('C'), KeyModifiers::CONTROL), Key::Ctrl('c'));
        assert_eq!(key(K::Char('c'), KeyModifiers::CONTROL), Key::Ctrl('c'));
    }

    #[test]
    fn enter_variants_by_modifier() {
        assert_eq!(key(K::Enter, KeyModifiers::empty()), Key::Enter);
        assert_eq!(key(K::Enter, KeyModifiers::SHIFT), Key::ShiftEnter);
        assert_eq!(key(K::Enter, KeyModifiers::ALT), Key::AltEnter);
    }

    #[test]
    fn shift_tab_maps_to_back_tab() {
        // 真实终端发送 CSI Z,crossterm 解析为 KeyCode::BackTab + SHIFT
        assert_eq!(
            from_key_event(&KeyEvent::new(K::BackTab, KeyModifiers::SHIFT)),
            Key::BackTab
        );
        // 兜底:部分终端把 Shift+Tab 报成 Tab+SHIFT
        assert_eq!(key(K::Tab, KeyModifiers::SHIFT), Key::BackTab);
        assert_eq!(key(K::Tab, KeyModifiers::empty()), Key::Tab);
    }

    #[test]
    fn plain_chars_pass_through() {
        assert_eq!(key(K::Char('x'), KeyModifiers::empty()), Key::Char('x'));
        assert_eq!(key(K::Char('中'), KeyModifiers::empty()), Key::Char('中'));
    }

    #[test]
    fn mouse_wheel_and_left_button_map_to_selection() {
        use ratatui::crossterm::event::{MouseButton, MouseEvent};
        let mouse = |kind, modifiers| {
            from_event(&Event::Mouse(MouseEvent {
                kind,
                column: 4,
                row: 7,
                modifiers,
            }))
        };
        assert_eq!(mouse(MouseEventKind::ScrollUp, KeyModifiers::empty()), Some(Key::ScrollUp));
        assert_eq!(mouse(MouseEventKind::ScrollDown, KeyModifiers::empty()), Some(Key::ScrollDown));
        assert_eq!(
            mouse(MouseEventKind::Down(MouseButton::Left), KeyModifiers::empty()),
            Some(Key::Mouse(MouseAction::Down {
                col: 4,
                row: 7,
                extend: false
            }))
        );
        // Shift/Alt+左键:扩展选区手势(Shift 常被终端截留,Alt 是可靠替代)
        assert_eq!(
            mouse(MouseEventKind::Down(MouseButton::Left), KeyModifiers::SHIFT),
            Some(Key::Mouse(MouseAction::Down {
                col: 4,
                row: 7,
                extend: true
            }))
        );
        assert_eq!(
            mouse(MouseEventKind::Down(MouseButton::Left), KeyModifiers::ALT),
            Some(Key::Mouse(MouseAction::Down {
                col: 4,
                row: 7,
                extend: true
            }))
        );
        assert_eq!(
            mouse(MouseEventKind::Drag(MouseButton::Left), KeyModifiers::empty()),
            Some(Key::Mouse(MouseAction::Drag { col: 4, row: 7 }))
        );
        assert_eq!(
            mouse(MouseEventKind::Up(MouseButton::Left), KeyModifiers::empty()),
            Some(Key::Mouse(MouseAction::Up { col: 4, row: 7 }))
        );
        // 右/中键与移动不消费
        assert_eq!(mouse(MouseEventKind::Down(MouseButton::Right), KeyModifiers::empty()), None);
        assert_eq!(mouse(MouseEventKind::Moved, KeyModifiers::empty()), None);
    }

    #[test]
    fn native_shift_enter_remaps_only_bare_enter() {
        assert_eq!(super::native_shift_enter(Key::Enter, true), Key::ShiftEnter);
        // 无 Shift:裸 Enter 保持提交语义
        assert_eq!(super::native_shift_enter(Key::Enter, false), Key::Enter);
        // 协议终端已归一的 ShiftEnter 不再二次处理
        assert_eq!(super::native_shift_enter(Key::ShiftEnter, true), Key::ShiftEnter);
        // Alt+Enter(CR)与普通字符不受影响
        assert_eq!(super::native_shift_enter(Key::AltEnter, true), Key::AltEnter);
        assert_eq!(super::native_shift_enter(Key::Char('中'), true), Key::Char('中'));
    }

    #[test]
    fn fallback_active_gates_on_platform_and_ssh() {
        assert!(super::fallback_active_on(true, false));
        assert!(!super::fallback_active_on(true, true));
        assert!(!super::fallback_active_on(false, false));
        assert!(!super::fallback_active_on(false, true));
    }
}
