//! Filesystem discovery — walk the workspace with the `ignore` crate (respects
//! `.gitignore`, skips hidden dirs like `.git`/`.semnav`), keeping only files a
//! built-in adapter owns.

use std::path::{Path, PathBuf};

use anyhow::Result;
use ignore::WalkBuilder;

use crate::adapters::select_for_uri;

/// Walk `root` and return the `file://` URIs of source files a built-in adapter
/// owns, in walk order. Hidden entries (`.git`, `.semnav`, ...) and git-ignored
/// files are excluded by the walker (`docs/design/indexing-and-cache.md`).
pub fn discover_files(root: &Path) -> Result<Vec<String>> {
    let walker = WalkBuilder::new(root)
        .hidden(true)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .build();
    let mut uris = Vec::new();
    for entry in walker {
        let entry = entry?;
        let is_file = entry.file_type().map(|t| t.is_file()).unwrap_or(false);
        if !is_file {
            continue;
        }
        let uri = path_to_uri(entry.path());
        if select_for_uri(&uri).is_some() {
            uris.push(uri);
        }
    }
    Ok(uris)
}

/// Convert a filesystem path to a `file://` URI. No percent-encoding in 0.0.1
/// (paths with spaces/non-ASCII are not yet supported); `path` should be absolute.
pub fn path_to_uri(path: &Path) -> String {
    let s = path.to_string_lossy();
    if s.starts_with('/') {
        format!("file://{s}")
    } else {
        format!("file:///{s}")
    }
}

/// Inverse of [`path_to_uri`]: strip the `file://` scheme prefix.
pub fn uri_to_path(uri: &str) -> PathBuf {
    PathBuf::from(uri.strip_prefix("file://").unwrap_or(uri))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn discover_files_skips_hidden_and_non_source() {
        let dir = tempdir().expect("tempdir");
        fs::write(dir.path().join("a.py"), "").unwrap();
        fs::write(dir.path().join("b.ts"), "").unwrap();
        fs::write(dir.path().join("README"), "").unwrap();
        fs::create_dir(dir.path().join(".semnav")).unwrap();
        fs::write(dir.path().join(".semnav").join("graph.db"), "x").unwrap();

        let mut uris = discover_files(dir.path()).expect("discover");
        uris.sort();
        assert_eq!(
            uris.len(),
            2,
            "only a.py and b.ts; README and .semnav excluded"
        );
        assert!(
            uris.iter()
                .all(|u| u.ends_with(".py") || u.ends_with(".ts"))
        );
    }

    #[test]
    fn path_uri_roundtrip() {
        let p = Path::new("/repo/app/mod.py");
        let uri = path_to_uri(p);
        assert_eq!(uri, "file:///repo/app/mod.py");
        assert_eq!(uri_to_path(&uri), p);
    }
}
