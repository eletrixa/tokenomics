//! Key event → `Action` mapping (pure).
//!
//! Project: Tokenomics — monitor LLM subscription accounts (usage, limits, time-left) in a TUI
//! Module:  src/tui/keys.rs
//! Deps:    crossterm (event types)
//! Tested:  inline `#[cfg(test)]` — documented keymap incl. `o` → `ToggleApi` (spec 025 §A)
//!
//! Design constraints:
//! - Pure: a key event maps to at most one `Action`. No I/O.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

/// A user intent produced by a key press.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Quit the dashboard.
    Quit,
    /// Move the selection up.
    Up,
    /// Move the selection down.
    Down,
    /// Re-read the store now.
    Refresh,
    /// Toggle the help overlay.
    Help,
    /// Toggle showing inactive (unsubscribed) accounts.
    ToggleInactive,
    /// Toggle the API billing (real-dollar) overview section (spec 025 §A).
    ToggleApi,
}

/// Map a key event to an action, or `None`. Ignores key-release events (Windows sends them).
pub fn map(key: KeyEvent) -> Option<Action> {
    if key.kind == KeyEventKind::Release {
        return None;
    }
    // Raw mode suppresses the kernel's Ctrl+C→SIGINT, so it arrives here as a key event.
    if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('c' | 'd'))
    {
        return Some(Action::Quit);
    }
    let action = match key.code {
        KeyCode::Char('q') | KeyCode::Esc => Action::Quit,
        KeyCode::Up | KeyCode::Char('k') => Action::Up,
        KeyCode::Down | KeyCode::Char('j') => Action::Down,
        KeyCode::Char('r') => Action::Refresh,
        KeyCode::Char('?') => Action::Help,
        KeyCode::Char('i') => Action::ToggleInactive,
        KeyCode::Char('o') => Action::ToggleApi,
        _ => return None,
    };
    Some(action)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn maps_documented_keys() {
        assert_eq!(map(key(KeyCode::Char('q'))), Some(Action::Quit));
        assert_eq!(map(key(KeyCode::Esc)), Some(Action::Quit));
        assert_eq!(map(key(KeyCode::Up)), Some(Action::Up));
        assert_eq!(map(key(KeyCode::Char('k'))), Some(Action::Up));
        assert_eq!(map(key(KeyCode::Down)), Some(Action::Down));
        assert_eq!(map(key(KeyCode::Char('j'))), Some(Action::Down));
        assert_eq!(map(key(KeyCode::Char('r'))), Some(Action::Refresh));
        assert_eq!(map(key(KeyCode::Char('?'))), Some(Action::Help));
        assert_eq!(map(key(KeyCode::Char('i'))), Some(Action::ToggleInactive));
        assert_eq!(map(key(KeyCode::Char('o'))), Some(Action::ToggleApi));
    }

    #[test]
    fn unknown_key_and_release_are_none() {
        assert_eq!(map(key(KeyCode::Char('z'))), None);
        let mut release = key(KeyCode::Char('q'));
        release.kind = KeyEventKind::Release;
        assert_eq!(map(release), None);
    }

    #[test]
    fn ctrl_c_and_ctrl_d_quit() {
        assert_eq!(
            map(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Some(Action::Quit)
        );
        assert_eq!(
            map(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL)),
            Some(Action::Quit)
        );
    }
}
