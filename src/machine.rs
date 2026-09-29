//! The machine where work happens: its volume and its shell, together.
//!
//! A machine is one thing, not two. The files and the command runner live on
//! the same side of the wire, so a path a tool reads is the same path a command
//! sees. `Local` is the machine axe has always had, this process's filesystem
//! and its `bash`; a control plane can bring its own, backed by a sandbox
//! somewhere else, with the tools none the wiser.

use std::path::Path;

/// One entry of a directory listing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
}

/// A working directory, its volume and a shell to run commands in it.
pub trait Machine: Send + Sync {
    /// The whole file. Ranges are not part of the contract: a caller that
    /// wants a piece asks for the file and cuts it, or runs a command that
    /// does.
    fn read(&self, path: &str) -> Result<Vec<u8>, String>;

    fn write(&self, path: &str, bytes: &[u8]) -> Result<(), String>;

    fn list(&self, path: &str) -> Result<Vec<Entry>, String>;

    /// A file, or a directory and everything under it.
    fn remove(&self, path: &str) -> Result<(), String>;

    /// Run a command and answer what the model should read. `progress` gets
    /// the output as it arrives, and the answer is the final output with the
    /// exit status and the timeout already folded in.
    fn run(&self, command: &str, timeout: u64, progress: &mut dyn FnMut(&str)) -> String;
}

/// The machine axe has always had: this process's filesystem and its children.
pub struct Local {
    dir: String,
}

impl Local {
    pub fn new(dir: &str) -> Local {
        Local {
            dir: dir.to_string(),
        }
    }
}

impl Machine for Local {
    fn read(&self, path: &str) -> Result<Vec<u8>, String> {
        std::fs::read(path).map_err(|e| e.to_string())
    }

    fn write(&self, path: &str, bytes: &[u8]) -> Result<(), String> {
        crate::atomic_write(Path::new(path), bytes).map_err(|e| e.to_string())
    }

    fn list(&self, path: &str) -> Result<Vec<Entry>, String> {
        let dir = if path.is_empty() { "." } else { path };
        let listing = std::fs::read_dir(dir).map_err(|e| e.to_string())?;
        let mut entries = Vec::new();
        for entry in listing {
            let entry = entry.map_err(|e| e.to_string())?;
            let meta = entry.metadata().map_err(|e| e.to_string())?;
            entries.push(Entry {
                name: entry.file_name().to_string_lossy().into_owned(),
                is_dir: meta.is_dir(),
                size: meta.len(),
            });
        }
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(entries)
    }

    fn remove(&self, path: &str) -> Result<(), String> {
        match std::fs::symlink_metadata(path) {
            Ok(meta) if meta.is_dir() => std::fs::remove_dir_all(path).map_err(|e| e.to_string()),
            _ => std::fs::remove_file(path).map_err(|e| e.to_string()),
        }
    }

    fn run(&self, command: &str, timeout: u64, progress: &mut dyn FnMut(&str)) -> String {
        crate::tools::run_shell(&self.dir, command, timeout, progress)
    }
}
