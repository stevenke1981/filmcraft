//! Explicit, local filesystem scope for agent operations. No network or persistent permissions.

use std::path::{Component, Path, PathBuf};

use serde_json::Value;

#[derive(Debug)]
pub struct Workspace {
    pub root: PathBuf,
}

fn protected(path: &Path) -> bool {
    path.components().any(|component| {
        matches!(component, Component::Normal(name) if matches!(name.to_string_lossy().to_ascii_lowercase().as_str(), ".git" | ".codex" | ".agents" | ".aws"))
    })
}

impl Workspace {
    pub fn new(root: &Path) -> Result<Self, String> {
        let input = root.to_string_lossy();
        if input.starts_with("\\\\") || input.starts_with("//") || input.contains("://") {
            return Err("workspace must be a local directory, not a network or device path".into());
        }
        if !root.is_absolute() || !root.is_dir() {
            return Err("workspace must be an existing absolute directory".into());
        }
        let root = root.canonicalize().map_err(|e| format!("workspace: {e}"))?;
        if protected(&root) {
            return Err("protected configuration directories cannot be an agent workspace".into());
        }
        Ok(Self { root })
    }

    /// Existing ancestors are resolved too: a symlink/junction cannot redirect an output outside.
    /// Parents, UNC paths, device paths, alternate streams and Windows reserved names are rejected.
    pub fn path(&self, input: &str, must_exist: bool) -> Result<PathBuf, String> {
        if input.is_empty()
            || input.len() > 4096
            || input.contains(['\0', '\n', '\r'])
            || input.contains("://")
            || input.starts_with("\\\\")
            || input.starts_with("//")
        {
            return Err("invalid workspace path".into());
        }
        let p = Path::new(input);
        for component in p.components() {
            match component {
                Component::ParentDir => return Err("parent traversal is not allowed".into()),
                Component::Normal(name) => {
                    let name = name.to_string_lossy();
                    if matches!(name.to_ascii_lowercase().as_str(), ".git" | ".codex" | ".agents" | ".aws") {
                        return Err("protected configuration directories are outside the agent scope".into());
                    }
                    let stem = name.split('.').next().unwrap_or_default().to_ascii_uppercase();
                    if name.contains(':')
                        || name.ends_with(['.', ' '])
                        || matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
                        || (stem.len() == 4
                            && (stem.starts_with("COM") || stem.starts_with("LPT"))
                            && stem.ends_with(['1', '2', '3', '4', '5', '6', '7', '8', '9']))
                    {
                        return Err("device names and alternate streams are not allowed".into());
                    }
                }
                _ => {}
            }
        }
        let target = if p.is_absolute() { p.to_path_buf() } else { self.root.join(p) };
        let mut existing = target.as_path();
        let mut missing = Vec::new();
        while !existing.exists() {
            missing.push(existing.file_name().ok_or("invalid workspace path")?.to_os_string());
            existing = existing.parent().ok_or("invalid workspace path")?;
        }
        if must_exist && !missing.is_empty() {
            return Err("path does not exist".into());
        }
        let mut resolved = existing.canonicalize().map_err(|e| format!("path: {e}"))?;
        if !resolved.starts_with(&self.root) {
            return Err("path is outside the authorized workspace".into());
        }
        if protected(&resolved) {
            return Err("protected configuration directories are outside the agent scope".into());
        }
        for part in missing.into_iter().rev() {
            resolved.push(part);
        }
        // Do not create arbitrary parent directories implicitly, or overwrite the workspace itself.
        if !must_exist && (resolved == self.root || !resolved.parent().is_some_and(Path::is_dir)) {
            return Err("output needs an existing parent directory inside the workspace".into());
        }
        Ok(resolved)
    }

    /// Validate referenced media/proxies/LUTs before the engine opens a project or renders it.
    pub fn document(&self, value: &Value) -> Result<(), String> {
        fn visit(w: &Workspace, value: &Value, key: &str, depth: usize, budget: &mut usize) -> Result<(), String> {
            if depth > 64 || *budget == 0 {
                return Err("project exceeds agent inspection limits".into());
            }
            *budget -= 1;
            match value {
                Value::Object(map) => {
                    for (key, value) in map {
                        visit(w, value, key, depth + 1, budget)?;
                    }
                }
                Value::Array(array) => {
                    for value in array {
                        visit(w, value, key, depth + 1, budget)?;
                    }
                }
                Value::String(text)
                    if !text.is_empty()
                        && (matches!(
                            key,
                            "path" | "file" | "sourcePath" | "source_path" | "captured" | "videoPreviews" | "audioPreviews" | "autoSave" | "destination"
                        ) || Path::new(text).is_absolute()) =>
                {
                    // The engine uses process-relative references as written, not relative to
                    // --workspace. Imports normalize paths; older relative references need relinking.
                    if !Path::new(text).is_absolute() {
                        return Err("project file references must be absolute paths inside the workspace".into());
                    }
                    w.path(text, false)?;
                }
                _ => {}
            }
            Ok(())
        }
        visit(self, value, "", 0, &mut 100_000)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn paths_and_project_references_stay_in_the_workspace() {
        let root = std::env::temp_dir().join(format!("filmcraft-scope-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let w = Workspace::new(&root).unwrap();
        assert!(w.path("movie.mp4", false).unwrap().starts_with(&w.root));
        for bad in [
            "../escape.mp4",
            "https://example.com/x",
            "\\\\server\\share\\x",
            "NUL.mp4",
            "movie.mp4:stream",
            "trailing.",
            "missing/parent/out.mp4",
            ".git/config",
            ".codex/config.toml",
            ".agents/skills/test",
            ".aws/credentials",
        ] {
            assert!(w.path(bad, false).is_err(), "{bad}");
        }
        assert!(w.path("missing.mp4", true).is_err());
        assert!(w.document(&json!({"media":{"path":"../secret.mp4"}})).is_err());
        assert!(w.document(&json!({"media":{"path":"movie.mp4"}})).is_err());
        assert!(w.document(&json!({"settings":{"scratch":{"autoSave":"relative"}}})).is_err());
        assert!(w.document(&json!({"name":"日本語 {name}","media":{"path":root.join("movie.mp4")}})).is_ok());
        let config = root.join(".codex");
        std::fs::create_dir(&config).unwrap();
        assert!(Workspace::new(&config).is_err());
        std::fs::remove_dir(config).unwrap();
        let mut deep = json!(0);
        for _ in 0..70 {
            deep = json!([deep]);
        }
        assert!(w.document(&deep).is_err());
        std::fs::remove_dir(&root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn symlink_outputs_cannot_escape() {
        let root = std::env::temp_dir().join(format!("filmcraft-symlink-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        std::os::unix::fs::symlink(std::env::temp_dir(), root.join("outside")).unwrap();
        let w = Workspace::new(&root).unwrap();
        assert!(w.path("outside/escape.mp4", false).is_err());
        std::fs::remove_file(root.join("outside")).unwrap();
        std::fs::remove_dir(root).unwrap();
    }
}
