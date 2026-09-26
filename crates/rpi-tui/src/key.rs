//! 按键抽象(crossterm 0.29 承担字节级解析;这里只做语义归一):
//! 上层(handle_key)与编辑器只认 `Key`,不感知终端转义细节。

use ratatui::crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

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
    Esc,
    /// Ctrl+字母(小写归一)
    Ctrl(char),
    Paste(String),
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
        KeyCode::Tab => Key::Tab,
        KeyCode::Esc => Key::Esc,
        _ => Key::Other,
    }
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
    fn plain_chars_pass_through() {
        assert_eq!(key(K::Char('x'), KeyModifiers::empty()), Key::Char('x'));
        assert_eq!(key(K::Char('中'), KeyModifiers::empty()), Key::Char('中'));
    }
}
