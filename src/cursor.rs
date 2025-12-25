//! Cursor persistence for crash-safe resume.
//!
//! Cursors track the last successfully forwarded entry per source.
//! - Stored as plain text files: `{cursor_dir}/{source_name}.cursor`
//! - Updated atomically (write to `.tmp`, rename)
//! - Only advanced after successful OTLP push

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use thiserror::Error;
use tracing::{debug, warn};

#[derive(Error, Debug)]
pub enum CursorError {
    #[error("Failed to create cursor directory: {0}")]
    CreateDir(io::Error),
    #[error("Failed to write cursor: {0}")]
    Write(io::Error),
    #[error("Failed to rename cursor file: {0}")]
    Rename(io::Error),
}

/// Cursor manager for a single source
#[derive(Debug, Clone)]
pub struct CursorManager {
    cursor_path: PathBuf,
    source_name: String,
}

impl CursorManager {
    /// Create a new cursor manager for a source
    pub fn new(cursor_dir: &Path, source_name: &str) -> Result<Self, CursorError> {
        // Ensure cursor directory exists
        if !cursor_dir.exists() {
            fs::create_dir_all(cursor_dir).map_err(CursorError::CreateDir)?;
        }

        // Sanitize source name for filesystem safety
        let safe_name = source_name
            .chars()
            .map(|c| {
                if c.is_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect::<String>();

        let cursor_path = cursor_dir.join(format!("{}.cursor", safe_name));

        Ok(Self {
            cursor_path,
            source_name: source_name.to_string(),
        })
    }

    /// Load the current cursor, if it exists
    pub fn load(&self) -> Option<String> {
        match fs::read_to_string(&self.cursor_path) {
            Ok(cursor) => {
                let cursor = cursor.trim().to_string();
                if cursor.is_empty() {
                    debug!(source = %self.source_name, "Cursor file is empty");
                    None
                } else {
                    debug!(source = %self.source_name, cursor = %cursor, "Loaded cursor");
                    Some(cursor)
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                debug!(source = %self.source_name, "No cursor file found, starting fresh");
                None
            }
            Err(e) => {
                warn!(source = %self.source_name, error = %e, "Failed to read cursor file, starting fresh");
                None
            }
        }
    }

    /// Save the cursor atomically
    pub fn save(&self, cursor: &str) -> Result<(), CursorError> {
        let tmp_path = self.cursor_path.with_extension("cursor.tmp");

        // Write to temp file
        fs::write(&tmp_path, cursor).map_err(CursorError::Write)?;

        // Atomic rename
        fs::rename(&tmp_path, &self.cursor_path).map_err(CursorError::Rename)?;

        debug!(source = %self.source_name, cursor = %cursor, "Saved cursor");
        Ok(())
    }

    /// Reset the cursor (delete file)
    pub fn reset(&self) -> Result<(), CursorError> {
        if self.cursor_path.exists() {
            fs::remove_file(&self.cursor_path).map_err(CursorError::Write)?;
            debug!(source = %self.source_name, "Reset cursor");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_cursor_roundtrip() {
        let dir = TempDir::new().unwrap();
        let cm = CursorManager::new(dir.path(), "test-source").unwrap();

        // Initially no cursor
        assert!(cm.load().is_none());

        // Save cursor
        cm.save("s=abc123;i=42").unwrap();

        // Load cursor
        assert_eq!(cm.load(), Some("s=abc123;i=42".to_string()));

        // Reset cursor
        cm.reset().unwrap();
        assert!(cm.load().is_none());
    }

    #[test]
    fn test_cursor_sanitizes_name() {
        let dir = TempDir::new().unwrap();
        let cm = CursorManager::new(dir.path(), "host/with:special<chars>").unwrap();
        assert!(cm
            .cursor_path
            .to_string_lossy()
            .contains("host_with_special_chars_"));
    }
}
