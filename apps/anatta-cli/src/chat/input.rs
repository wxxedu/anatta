//! Line-editing input for the chat REPL.
//!
//! Thin wrapper over rustyline that:
//!   * persists history under `<anatta_home>/chat_history`
//!   * maps Ctrl-D (Eof) and Ctrl-C (Interrupted) to dedicated outcomes
//!     the chat loop reads to drive exit semantics
//!   * binds Shift+Tab to surface `CyclePermission` so the chat loop can
//!     advance the active permission level

use std::path::PathBuf;

use rustyline::error::ReadlineError;
use rustyline::{DefaultEditor, EventHandler, KeyCode, KeyEvent, Modifiers};

use super::ChatError;
use super::permission_hotkey::CyclePermissionFlag;

pub(crate) struct InputReader {
    editor: DefaultEditor,
    history_path: PathBuf,
    cycle_flag: CyclePermissionFlag,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ReadOutcome {
    Line(String),
    /// Ctrl-D on an empty prompt → graceful chat exit.
    Eof,
    /// Ctrl-C at the prompt (not during a turn) → also exit.
    Interrupted,
    /// User pressed Shift+Tab — cycle to the next permission level.
    CyclePermission,
}

impl InputReader {
    pub(crate) fn new(anatta_home: &std::path::Path) -> Result<Self, ChatError> {
        let mut editor = DefaultEditor::new().map_err(|e| ChatError::Readline(e.to_string()))?;
        let cycle_flag = CyclePermissionFlag::new();
        editor.bind_sequence(
            KeyEvent(KeyCode::BackTab, Modifiers::NONE),
            EventHandler::Conditional(Box::new(cycle_flag.handler())),
        );
        let history_path = anatta_home.join("chat_history");
        // Best-effort load; first-run is fine.
        let _ = editor.load_history(&history_path);
        Ok(Self {
            editor,
            history_path,
            cycle_flag,
        })
    }

    pub(crate) fn read_prompt(&mut self) -> ReadOutcome {
        match self.editor.readline("> ") {
            Ok(line) => {
                // Shift+Tab consumed the line — surface the cycle event.
                if self.cycle_flag.take() {
                    return ReadOutcome::CyclePermission;
                }
                let trimmed = line.trim().to_owned();
                if !trimmed.is_empty() {
                    let _ = self.editor.add_history_entry(&trimmed);
                }
                ReadOutcome::Line(trimmed)
            }
            Err(ReadlineError::Eof) => ReadOutcome::Eof,
            Err(ReadlineError::Interrupted) => ReadOutcome::Interrupted,
            Err(other) => {
                // Catch-all: treat as eof so we exit cleanly.
                eprintln!("[anatta] input error: {other}");
                ReadOutcome::Eof
            }
        }
    }

    /// Persist history. Best-effort; missing directory is ignored.
    pub(crate) fn save_history(&mut self) {
        let _ = self.editor.save_history(&self.history_path);
    }
}
