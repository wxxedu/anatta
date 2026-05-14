//! Line-editing input for the chat REPL.
//!
//! Thin wrapper over rustyline that:
//!   * persists history under `<anatta_home>/chat_history`
//!   * maps Ctrl-D (Eof) and Ctrl-C (Interrupted) to dedicated outcomes
//!     the chat loop reads to drive exit semantics

use std::path::PathBuf;

use rustyline::DefaultEditor;
use rustyline::error::ReadlineError;

use super::ChatError;

pub(crate) struct InputReader {
    editor: DefaultEditor,
    history_path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ReadOutcome {
    Line(String),
    /// Ctrl-D on an empty prompt → graceful chat exit.
    Eof,
    /// Ctrl-C at the prompt (not during a turn) → also exit.
    Interrupted,
}

impl InputReader {
    pub(crate) fn new(anatta_home: &std::path::Path) -> Result<Self, ChatError> {
        let mut editor = DefaultEditor::new().map_err(|e| ChatError::Readline(e.to_string()))?;
        let history_path = anatta_home.join("chat_history");
        // Best-effort load; first-run is fine.
        let _ = editor.load_history(&history_path);
        Ok(Self {
            editor,
            history_path,
        })
    }

    pub(crate) fn read_prompt(&mut self) -> ReadOutcome {
        match self.editor.readline("> ") {
            Ok(line) => {
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
