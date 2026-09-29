//! The machine where work happens: its volume and its shell, together.
//!
//! A machine is one thing, not two. The files and the command runner live on
//! the same side of the wire, so a path a tool reads is the same path a command
//! sees. `Local` is the machine axe has always had, this process's filesystem
//! and its `bash`; a control plane can bring its own, backed by a sandbox
//! somewhere else, with the tools none the wiser.

use std::path::Path;

/// A relative path belongs to the machine, not to the process using it: the
/// working directory is the machine's, so that is where paths resolve.
pub fn resolve(dir: &str, path: &str) -> String {
    if path.starts_with('/') || dir.is_empty() {
        return path.to_string();
    }
    format!("{}/{}", dir.trim_end_matches('/'), path)
}

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
        std::fs::read(resolve(&self.dir, path)).map_err(|e| e.to_string())
    }

    fn write(&self, path: &str, bytes: &[u8]) -> Result<(), String> {
        let path = resolve(&self.dir, path);
        crate::atomic_write(Path::new(&path), bytes).map_err(|e| e.to_string())
    }

    fn list(&self, path: &str) -> Result<Vec<Entry>, String> {
        let path = resolve(&self.dir, path);
        let dir = if path.is_empty() { "." } else { path.as_str() };
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
        let path = resolve(&self.dir, path);
        match std::fs::symlink_metadata(&path) {
            Ok(meta) if meta.is_dir() => std::fs::remove_dir_all(&path).map_err(|e| e.to_string()),
            _ => std::fs::remove_file(&path).map_err(|e| e.to_string()),
        }
    }

    fn run(&self, command: &str, timeout: u64, progress: &mut dyn FnMut(&str)) -> String {
        crate::tools::run_shell(&self.dir, command, timeout, progress)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_relative_path_belongs_to_the_machine() {
        assert_eq!(resolve("/base", "a.txt"), "/base/a.txt");
        assert_eq!(resolve("/base/", "a.txt"), "/base/a.txt");
        assert_eq!(resolve("/base", "/otro/a.txt"), "/otro/a.txt");
        assert_eq!(resolve("", "a.txt"), "a.txt");
    }

    #[test]
    fn the_local_machine_resolves_against_its_directory() {
        let root = std::env::temp_dir().join(format!("axe-machine-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let machine = Local::new(&root.display().to_string());

        machine.write("adentro.txt", b"hola").unwrap();
        assert!(
            root.join("adentro.txt").is_file(),
            "wrote outside the machine directory"
        );
        assert_eq!(machine.read("adentro.txt").unwrap(), b"hola");
        assert_eq!(machine.list("").unwrap().len(), 1);
        machine.remove("adentro.txt").unwrap();
        assert!(machine.read("adentro.txt").is_err());

        std::fs::remove_dir_all(&root).ok();
    }
}
