//! `solx-files` — on-disk byte store for files attached to documents/actions.
//!
//! There is no database here. The bytes live under a configured files root; the
//! owning entity's database holds the relative-path references. Files written
//! without any matching entity row are simply loose/shared, which is allowed.
//!
//! Conventional relative layout (all under the root):
//! * `files/docs/<doc_id>/<file_name>`
//! * `files/docs/shared/<file_name>`
//! * `files/actions/<action_id>/<file_name>`
//! * `files/actions/shared/<file_name>`

use std::fs;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use solx_config::ConfigService;
use solx_surface::error::{Result, SolxError};
use solx_surface::managers::FileStore;

/// Filesystem-backed [`FileStore`] rooted at a directory.
pub struct LocalFileStore {
    root: PathBuf,
}

impl LocalFileStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        LocalFileStore { root: root.into() }
    }

    /// Build a store rooted at the config's files directory.
    pub fn from_config(cfg: &ConfigService) -> Self {
        LocalFileStore::new(cfg.files_dir())
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Normalize a relative path into forward-slash segments, rejecting
    /// absolute paths and `..` traversal.
    fn normalize_rel(rel: &str) -> Result<Vec<String>> {
        let mut segs = Vec::new();
        for seg in rel.split(['/', '\\']) {
            let seg = seg.trim();
            if seg.is_empty() || seg == "." {
                continue;
            }
            if seg == ".." {
                return Err(SolxError::Invalid(format!(
                    "path traversal not allowed in '{rel}'"
                )));
            }
            if seg.contains(':') || seg.chars().any(|c| c.is_control()) {
                return Err(SolxError::Invalid(format!(
                    "invalid file path segment '{seg}'"
                )));
            }
            segs.push(sanitize_segment(seg));
        }
        if segs.is_empty() {
            return Err(SolxError::Invalid("empty file path".into()));
        }
        Ok(segs)
    }

    fn resolve(&self, rel: &str) -> Result<(PathBuf, String)> {
        let segs = Self::normalize_rel(rel)?;
        let mut abs = self.root.clone();
        for s in &segs {
            abs.push(s);
        }
        Ok((abs, segs.join("/")))
    }
}

// These are `async fn`s reached from action execution, so they must not do
// blocking file I/O inline — `std::fs` here would park a tokio worker on
// every read/write. `get` in particular is on the WASM hot path: it reads
// the whole `.wasm` artifact before every WASM action runs.
#[async_trait]
impl FileStore for LocalFileStore {
    async fn put(&self, rel_path: &str, bytes: Vec<u8>) -> Result<String> {
        let (abs, normalized) = self.resolve(rel_path)?;
        if let Some(parent) = abs.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(&abs, &bytes).await?;
        Ok(normalized)
    }

    async fn get(&self, rel_path: &str) -> Result<Vec<u8>> {
        let (abs, normalized) = self.resolve(rel_path)?;
        match tokio::fs::read(&abs).await {
            Ok(b) => Ok(b),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Err(SolxError::NotFound(format!("file '{normalized}'")))
            }
            Err(e) => Err(e.into()),
        }
    }

    async fn delete(&self, rel_path: &str) -> Result<()> {
        let (abs, _) = self.resolve(rel_path)?;
        match tokio::fs::remove_file(&abs).await {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        // Recursive directory pruning — cheaper to offload wholesale than to
        // make the loop async.
        let root = self.root.clone();
        let parent = abs.parent().map(Path::to_path_buf);
        tokio::task::spawn_blocking(move || cleanup_empty_parents(&root, parent.as_deref()))
            .await
            .map_err(|e| SolxError::Other(format!("file cleanup task panicked: {e}")))?;
        Ok(())
    }

    async fn list(&self, prefix: &str) -> Result<Vec<String>> {
        let base = if prefix.trim().is_empty() {
            self.root.clone()
        } else {
            let segs = Self::normalize_rel(prefix)?;
            let mut p = self.root.clone();
            for s in &segs {
                p.push(s);
            }
            p
        };
        // `walk` recurses; a recursive async fn would need boxing at every
        // level for no benefit, so the whole traversal goes to the blocking
        // pool instead.
        let root = self.root.clone();
        tokio::task::spawn_blocking(move || {
            let mut out = Vec::new();
            walk(&root, &base, &mut out)?;
            out.sort();
            Ok(out)
        })
        .await
        .map_err(|e| SolxError::Other(format!("file list task panicked: {e}")))?
    }
}

/// Build the conventional relative path for a document's file.
pub fn doc_file_path(doc_id: &str, file_name: &str) -> String {
    format!(
        "files/docs/{}/{}",
        sanitize_segment(doc_id),
        sanitize_segment(file_name)
    )
}

/// Build the conventional relative path for a shared (unowned) document file.
pub fn shared_doc_file_path(file_name: &str) -> String {
    format!("files/docs/shared/{}", sanitize_segment(file_name))
}

/// Build the conventional relative path for an action's file/artifact.
pub fn action_file_path(action_id: &str, file_name: &str) -> String {
    format!(
        "files/actions/{}/{}",
        sanitize_segment(action_id),
        sanitize_segment(file_name)
    )
}

/// Build the conventional relative path for a shared action file.
pub fn shared_action_file_path(file_name: &str) -> String {
    format!("files/actions/shared/{}", sanitize_segment(file_name))
}

/// Replace filesystem-hostile characters in a single segment with `_`.
pub fn sanitize_segment(seg: &str) -> String {
    seg.chars()
        .map(|c| {
            if c.is_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn walk(root: &Path, dir: &Path, out: &mut Vec<String>) -> Result<()> {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            walk(root, &path, out)?;
        } else if let Ok(rel) = path.strip_prefix(root) {
            out.push(rel.to_string_lossy().replace('\\', "/"));
        }
    }
    Ok(())
}

fn cleanup_empty_parents(root: &Path, mut dir: Option<&Path>) {
    while let Some(d) = dir {
        if d == root || !d.starts_with(root) {
            break;
        }
        let empty = fs::read_dir(d).map(|mut it| it.next().is_none()).unwrap_or(false);
        if !empty {
            break;
        }
        let _ = fs::remove_dir(d);
        dir = d.parent();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn put_get_delete_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let store = LocalFileStore::new(dir.path());
        let rel = doc_file_path("doc-1", "note.txt");
        let stored = store.put(&rel, b"hello".to_vec()).await.unwrap();
        assert_eq!(stored, "files/docs/doc-1/note.txt");
        assert_eq!(store.get(&rel).await.unwrap(), b"hello");

        let listed = store.list("files/docs").await.unwrap();
        assert_eq!(listed, vec!["files/docs/doc-1/note.txt".to_string()]);

        store.delete(&rel).await.unwrap();
        assert!(matches!(store.get(&rel).await, Err(SolxError::NotFound(_))));
    }

    #[tokio::test]
    async fn rejects_traversal() {
        let dir = tempfile::tempdir().unwrap();
        let store = LocalFileStore::new(dir.path());
        assert!(store.put("../evil.txt", b"x".to_vec()).await.is_err());
    }
}
