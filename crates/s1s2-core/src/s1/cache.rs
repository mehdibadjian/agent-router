//! Generic `xxh64` mtime-backed state cache (spec §3.2.2).
//!
//! The registry is the expensive part of System 1: walking the tree and parsing every
//! frontmatter block. Fingerprinting `path + mtime + size` lets a warm run deserialize
//! the previous snapshot instead of re-reading the files.

use serde::de::DeserializeOwned;
use serde::Serialize;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;
use walkdir::WalkDir;
use xxhash_rust::xxh64::xxh64;

/// On-disk header key holding the fingerprint, alongside `payload_key`.
pub const HASH_KEY: &str = "hash";

/// Deterministic fingerprint over watched directories. Entries are sorted so the digest
/// does not depend on filesystem iteration order.
pub fn fingerprint(watched_dirs: &[PathBuf], extensions: &[&str]) -> (u64, Vec<PathBuf>) {
    let mut lines: Vec<String> = Vec::new();
    let mut files: Vec<PathBuf> = Vec::new();

    for dir in watched_dirs {
        if !dir.exists() {
            continue;
        }
        for entry in WalkDir::new(dir).follow_links(false).into_iter().filter_map(|e| e.ok()) {
            let path = entry.path();
            if !is_wanted(path, extensions) {
                continue;
            }
            let Ok(meta) = fs::metadata(path) else { continue };
            let nanos = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            lines.push(format!("{}:{}:{}|", path.display(), nanos, meta.len()));
            files.push(path.to_path_buf());
        }
    }

    lines.sort();
    files.sort();
    files.dedup();
    (xxh64(lines.join("").as_bytes(), 0), files)
}

fn is_wanted(path: &Path, extensions: &[&str]) -> bool {
    if !path.is_file() {
        return false;
    }
    // An empty filter means "any file", which is what non-file-backed domains want.
    if extensions.is_empty() {
        return true;
    }
    path.extension()
        .and_then(|s| s.to_str())
        .map(|ext| extensions.contains(&ext))
        .unwrap_or(false)
}

#[derive(Debug, Clone)]
pub struct MtimeCacheState<T> {
    pub payload: T,
    pub hash: u64,
    pub cache_hit: bool,
}

/// Reusable cache handle; `T` is a domain's `RegistryState`.
///
/// The file layout is `{"hash": <xxh64>, "<payload_key>": <T>}`. Extension #1 chooses
/// `payload_key = "entries"`, which reproduces the legacy `.router/registry.json` schema
/// exactly, so existing harnesses keep reading it unchanged.
pub struct MtimeCache<T: Serialize + DeserializeOwned> {
    cache_file: PathBuf,
    payload_key: String,
    extensions: Vec<String>,
    _marker: std::marker::PhantomData<T>,
}

impl<T: Serialize + DeserializeOwned> MtimeCache<T> {
    pub fn new(cache_file: PathBuf, payload_key: &str, extensions: &[&str]) -> Self {
        Self {
            cache_file,
            payload_key: payload_key.to_string(),
            extensions: extensions.iter().map(|s| s.to_string()).collect(),
            _marker: std::marker::PhantomData,
        }
    }

    pub fn cache_file(&self) -> &Path {
        &self.cache_file
    }

    /// Read the cached payload when the fingerprint still matches; otherwise rebuild,
    /// persist, and report `cache_hit == false`.
    pub fn load_or_build(
        &mut self,
        watched_dirs: &[PathBuf],
        build: impl FnOnce(&[PathBuf]) -> Result<T, String>,
    ) -> Result<MtimeCacheState<T>, String> {
        let exts: Vec<&str> = self.extensions.iter().map(|s| s.as_str()).collect();
        let (hash, files) = fingerprint(watched_dirs, &exts);

        if let Ok(raw) = fs::read_to_string(&self.cache_file) {
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(&raw) {
                if value.get(HASH_KEY).and_then(|h| h.as_u64()) == Some(hash) {
                    if let Some(payload) = value.get(&self.payload_key) {
                        if let Ok(typed) = serde_json::from_value::<T>(payload.clone()) {
                            return Ok(MtimeCacheState { payload: typed, hash, cache_hit: true });
                        }
                    }
                }
            }
        }

        let payload = build(&files)?;
        self.write(hash, &payload);
        Ok(MtimeCacheState { payload, hash, cache_hit: false })
    }

    /// Force the next `load_or_build` to rebuild.
    pub fn invalidate(&mut self) {
        let _ = fs::remove_file(&self.cache_file);
    }

    fn write(&self, hash: u64, payload: &T) {
        let Ok(body) = serde_json::to_value(payload) else { return };
        let mut document = serde_json::Map::new();
        document.insert(HASH_KEY.to_string(), serde_json::Value::Number(hash.into()));
        document.insert(self.payload_key.clone(), body);

        if let Some(parent) = self.cache_file.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let _ = fs::write(
            &self.cache_file,
            serde_json::to_string_pretty(&document).unwrap_or_default(),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::collections::BTreeMap;
    use std::time::Duration;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("s1s2-cache-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn fingerprint_is_order_independent_and_size_sensitive() {
        let dir = temp_dir("fp");
        fs::create_dir_all(dir.join("sub")).unwrap();
        fs::write(dir.join("a.md"), "one").unwrap();
        fs::write(dir.join("sub/b.md"), "two").unwrap();

        let dirs = vec![dir.clone()];
        let (h1, files) = fingerprint(&dirs, &["md"]);
        assert_eq!(files.len(), 2);
        let (h2, _) = fingerprint(&dirs, &["md"]);
        assert_eq!(h1, h2, "unchanged tree must fingerprint identically");

        fs::write(dir.join("sub/b.md"), "two-and-a-bit-longer").unwrap();
        let (h3, _) = fingerprint(&dirs, &["md"]);
        assert_ne!(h1, h3, "size change must invalidate");

        let (_h4, none) = fingerprint(&dirs, &["rs"]);
        assert!(none.is_empty(), "extension filter must apply");

        let (_h5, any) = fingerprint(&dirs, &[]);
        assert_eq!(any.len(), 2, "empty filter accepts any file");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn cache_round_trips_and_rebuilds_on_invalidate() {
        let dir = temp_dir("rt");
        fs::write(dir.join("x.md"), "hello").unwrap();
        let mut cache: MtimeCache<BTreeMap<String, String>> =
            MtimeCache::new(dir.join("state.json"), "payload", &["md"]);

        let builds = Cell::new(0usize);
        let mut build = |_: &[PathBuf]| {
            builds.set(builds.get() + 1);
            let mut m = BTreeMap::new();
            m.insert("x".to_string(), "hello".to_string());
            Ok(m)
        };

        let first = cache.load_or_build(&[dir.clone()], &mut build).unwrap();
        assert!(!first.cache_hit);
        let second = cache.load_or_build(&[dir.clone()], &mut build).unwrap();
        assert!(second.cache_hit);
        assert_eq!(builds.get(), 1, "warm run must not rebuild");

        cache.invalidate();
        let third = cache.load_or_build(&[dir.clone()], &mut build).unwrap();
        assert!(!third.cache_hit);
        assert_eq!(third.payload, second.payload);
        assert_eq!(builds.get(), 2);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn mtime_only_change_invalidates_without_size_change() {
        let dir = temp_dir("mtime");
        let file = dir.join("x.md");
        fs::write(&file, "hello").unwrap();
        let dirs = vec![dir.clone()];

        let (h1, _) = fingerprint(&dirs, &["md"]);
        let meta = fs::metadata(&file).unwrap();
        let bumped = meta.modified().unwrap() + Duration::from_secs(3);
        fs::File::open(&file).unwrap().set_modified(bumped).unwrap();
        let (h2, _) = fingerprint(&dirs, &["md"]);
        assert_ne!(h1, h2, "mtime bump alone must invalidate");
        let _ = fs::remove_dir_all(&dir);
    }
}
