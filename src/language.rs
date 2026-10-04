use anyhow::{ensure, Result};
use clap::ValueEnum;
use serde::{Deserialize, Serialize};
use std::{fs, io::Write, os::unix::fs::OpenOptionsExt, path::Path};

#[derive(Clone, Copy, Debug, Default, ValueEnum, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Language {
    #[default]
    En,
    Cn,
}

impl Language {
    pub fn load(dir: &Path) -> Self {
        match fs::read_to_string(dir.join("ui-language")).as_deref() {
            Ok("cn\n") => Self::Cn,
            _ => Self::En,
        }
    }
    pub fn save(self, dir: &Path) -> Result<()> {
        if !dir.join(".mox-data").exists() {
            ensure!(
                fs::read_dir(dir)?.all(|entry| entry.is_ok_and(|e| e.file_name()
                    == "identity.lock"
                    || e.file_name() == "ui-language")),
                "DATA_DIR_NOT_EMPTY: Use a dedicated Mox data directory"
            );
        }
        let target = dir.join("ui-language");
        ensure!(
            !target
                .symlink_metadata()
                .is_ok_and(|m| m.file_type().is_symlink()),
            "INVALID_SETTINGS: Language settings must not be a symlink"
        );
        let temporary = dir.join(format!(".language-{}", crate::crypto::id()));
        let result = (|| {
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&temporary)?;
            file.write_all(match self {
                Self::En => b"en\n",
                Self::Cn => b"cn\n",
            })?;
            file.sync_all()?;
            fs::rename(&temporary, target)?;
            Ok(())
        })();
        let _ = fs::remove_file(temporary);
        result
    }
    pub fn text<'a>(self, en: &'a str, cn: &'a str) -> &'a str {
        match self {
            Self::En => en,
            Self::Cn => cn,
        }
    }
    pub fn error(self, error: &str) -> String {
        if self == Self::Cn
            || !error
                .chars()
                .any(|c| ('\u{3400}'..='\u{9fff}').contains(&c))
        {
            return error.to_owned();
        }
        let code = error
            .split(':')
            .next()
            .filter(|s| !s.is_empty() && s.bytes().all(|c| c.is_ascii_uppercase() || c == b'_'))
            .unwrap_or("MOX_ERROR");
        let message = match code {
            "CODEX_NOT_FOUND" | "CODEX_UNAVAILABLE" | "CODEX_EXITED" => {
                "Codex is unavailable. Install Codex and check its login and PATH."
            }
            "FILE_NOT_FOUND" => "File does not exist.",
            "FILE_NOT_READABLE" => "Cannot read the file.",
            "PATH_NOT_FILE" => "The path is not a file.",
            "MEDIA_TYPE_MISMATCH" => "The file does not match the selected media type.",
            "RUNTIME_NOT_RUNNING" => "Run mox start first.",
            "RUNTIME_RUNNING" => "This identity already has a running Mox service.",
            "RUNTIME_TIMEOUT" => "The Mox service did not respond in time.",
            "CODEX_TIMEOUT" => "Codex did not respond in time.",
            "TERMINAL_TOO_NARROW" => {
                "Widen the terminal to display the QR code, or use mox moxpub."
            }
            "CHAT_NOT_FOUND" => "Conversation not found.",
            "NOT_LOGGED_IN" => "Run mox login first.",
            "NOT_FRIEND" => "Accept the friend request before messaging.",
            "MESSAGE_NOT_FOUND" => "Message not found in this conversation.",
            _ => "Operation failed. Run the same command with --json for details.",
        };
        format!("{code}: {message}")
    }
}
