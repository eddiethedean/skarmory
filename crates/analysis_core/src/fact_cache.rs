//! Best-effort storage for owned, file-local analysis facts.

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use blake3::{Hash, Hasher};
use serde::{Serialize, de::DeserializeOwned};

const MAX_RECORD_BYTES: usize = 8 * 1024 * 1024;
const MAX_SCAN_IO_BYTES: usize = 64 * 1024 * 1024;
const HEADER_BYTES: usize = 2 * blake3::OUT_LEN;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
pub struct CacheStats {
    pub hits: usize,
    pub misses: usize,
    pub writes: usize,
    pub write_failures: usize,
}

pub(crate) struct FactCache {
    directory: PathBuf,
    namespace: &'static str,
    hits: AtomicUsize,
    misses: AtomicUsize,
    writes: AtomicUsize,
    write_failures: AtomicUsize,
    remaining_io: AtomicUsize,
}

impl FactCache {
    pub(crate) fn new(directory: &Path, namespace: &'static str) -> Self {
        Self {
            directory: directory.to_owned(),
            namespace,
            hits: AtomicUsize::new(0),
            misses: AtomicUsize::new(0),
            writes: AtomicUsize::new(0),
            write_failures: AtomicUsize::new(0),
            remaining_io: AtomicUsize::new(MAX_SCAN_IO_BYTES),
        }
    }

    pub(crate) fn for_project(
        directory: &Path,
        namespace: &'static str,
        input: &Path,
        roots: &[PathBuf],
    ) -> Self {
        // Creating a directory below a source root can change import lookup
        // and directory evidence. Only create missing out-of-tree directories.
        if !directory.is_dir()
            && let (Some(location), Ok(input)) =
                (resolve_allow_missing(directory), input.canonicalize())
        {
            let base = if input.is_file() {
                input.parent().unwrap_or(&input)
            } else {
                &input
            };
            let inside_source = location.starts_with(base)
                || roots.iter().any(|root| {
                    resolve_allow_missing(&base.join(root))
                        .is_none_or(|root| location.starts_with(root))
                });
            if !inside_source {
                let _ = fs::create_dir_all(directory);
            }
        }
        Self::new(directory, namespace)
    }

    pub(crate) fn get_or_extract<T: Serialize + DeserializeOwned, E>(
        &self,
        source: &str,
        extract: impl FnOnce(&str) -> Result<T, E>,
    ) -> Result<T, E> {
        self.get_or_extract_variant("", source, extract)
    }

    pub(crate) fn get_or_extract_variant<T: Serialize + DeserializeOwned, E>(
        &self,
        variant: &str,
        source: &str,
        extract: impl FnOnce(&str) -> Result<T, E>,
    ) -> Result<T, E> {
        let mut hasher = Hasher::new();
        hasher.update(b"skarmory-source-facts-v1\0");
        hasher.update(self.namespace.as_bytes());
        hasher.update(b"\0");
        if !variant.is_empty() {
            hasher.update(b"\0");
            hasher.update(variant.as_bytes());
            hasher.update(b"\0");
        }
        // Extraction consumes normalized UTF-8. Equivalent BOM and newline
        // representations have the same facts and normalized locations.
        hasher.update(source.as_bytes());
        let key = hasher.finalize();
        let path = self
            .directory
            .join(format!("skarmory-fact-{}.bin", key.to_hex()));
        if let Some(facts) = self.load(&path, key) {
            self.hits.fetch_add(1, Ordering::Relaxed);
            return Ok(facts);
        }
        self.misses.fetch_add(1, Ordering::Relaxed);
        let facts = extract(source)?;
        if self.store(&path, key, &facts).is_ok() {
            self.writes.fetch_add(1, Ordering::Relaxed);
        } else {
            self.write_failures.fetch_add(1, Ordering::Relaxed);
        }
        Ok(facts)
    }

    pub(crate) fn stats(&self) -> CacheStats {
        CacheStats {
            hits: self.hits.load(Ordering::Relaxed),
            misses: self.misses.load(Ordering::Relaxed),
            writes: self.writes.load(Ordering::Relaxed),
            write_failures: self.write_failures.load(Ordering::Relaxed),
        }
    }

    fn load<T: DeserializeOwned>(&self, path: &Path, key: Hash) -> Option<T> {
        let meta = fs::symlink_metadata(path).ok()?;
        let size = usize::try_from(meta.len()).ok()?;
        if !meta.is_file()
            || !(HEADER_BYTES..=MAX_RECORD_BYTES).contains(&size)
            || !self.reserve_io(size + 1)
        {
            return None;
        }
        let mut bytes = Vec::with_capacity(size + 1);
        File::open(path)
            .ok()?
            .take(size as u64 + 1)
            .read_to_end(&mut bytes)
            .ok()?;
        if bytes.len() != size || &bytes[..blake3::OUT_LEN] != key.as_bytes() {
            return None;
        }
        let payload = &bytes[HEADER_BYTES..];
        let digest = blake3::hash(payload);
        if &bytes[blake3::OUT_LEN..HEADER_BYTES] != digest.as_bytes() {
            return None;
        }
        serde_json::from_slice(payload).ok()
    }

    fn store<T: Serialize>(&self, path: &Path, key: Hash, facts: &T) -> io::Result<()> {
        let mut payload = BoundedPayload(Vec::new());
        serde_json::to_writer(&mut payload, facts)?;
        let BoundedPayload(payload) = payload;
        if !self.reserve_io(HEADER_BYTES + payload.len()) {
            return Err(io::Error::other("cache I/O budget exhausted"));
        }
        let mut pending = tempfile::Builder::new()
            .prefix(".skarmory-fact-")
            .suffix(".tmp")
            .tempfile_in(&self.directory)?;
        pending.write_all(key.as_bytes())?;
        pending.write_all(blake3::hash(&payload).as_bytes())?;
        pending.write_all(&payload)?;
        pending.flush()?;
        pending.persist(path)?;
        Ok(())
    }

    fn reserve_io(&self, size: usize) -> bool {
        self.remaining_io
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |remaining| {
                remaining.checked_sub(size)
            })
            .is_ok()
    }
}

fn resolve_allow_missing(path: &Path) -> Option<PathBuf> {
    let absolute = std::path::absolute(path).ok()?;
    for ancestor in absolute.ancestors() {
        if let Ok(resolved) = ancestor.canonicalize() {
            let suffix = absolute.strip_prefix(ancestor).ok()?;
            if suffix
                .components()
                .all(|component| matches!(component, Component::Normal(_)))
            {
                return Some(resolved.join(suffix));
            }
            return None;
        }
    }
    None
}

struct BoundedPayload(Vec<u8>);

impl Write for BoundedPayload {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > MAX_RECORD_BYTES - HEADER_BYTES - self.0.len() {
            return Err(io::Error::other("cache record exceeds size limit"));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Barrier;
    use std::thread;

    use super::*;

    fn letters(source: &str) -> Result<Vec<char>, ()> {
        Ok(source.chars().collect())
    }

    fn hit(_: &str) -> Result<Vec<char>, ()> {
        panic!("expected cached facts")
    }

    #[test]
    fn keyed_by_source_and_extractor() {
        let dir = tempfile::tempdir().unwrap();
        let cache = FactCache::new(dir.path(), "module-v1");
        assert_eq!(cache.get_or_extract("a", letters).unwrap(), ['a']);
        assert_eq!(cache.get_or_extract("b", letters).unwrap(), ['b']);
        let fresh = FactCache::new(dir.path(), "module-v1");
        assert_eq!(fresh.get_or_extract("a", hit).unwrap(), ['a']);
        assert_eq!(fresh.get_or_extract("b", hit).unwrap(), ['b']);
        assert_eq!(fresh.stats().hits, 2);
        let changed = FactCache::new(dir.path(), "module-v2");
        assert_eq!(changed.get_or_extract("a", letters).unwrap(), ['a']);
        assert_eq!(changed.stats().misses, 1);
    }

    #[test]
    fn corrupt_and_oversized_records_are_replaced_after_extraction() {
        let dir = tempfile::tempdir().unwrap();
        let cache = FactCache::new(dir.path(), "module-v1");
        cache.get_or_extract("a", letters).unwrap();
        let path = dir
            .path()
            .read_dir()
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let mut corrupt = fs::read(&path).unwrap();
        corrupt[HEADER_BYTES] ^= 1;
        fs::write(&path, corrupt).unwrap();
        assert_eq!(cache.get_or_extract("a", letters).unwrap(), ['a']);
        File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_len(MAX_RECORD_BYTES as u64 + 1)
            .unwrap();
        assert_eq!(cache.get_or_extract("a", letters).unwrap(), ['a']);
        assert_eq!(cache.get_or_extract("a", hit).unwrap(), ['a']);
        assert_eq!(cache.stats().misses, 3);
    }

    #[test]
    fn invalid_persisted_locations_are_rejected() {
        use skarmory_python::Location;

        let dir = tempfile::tempdir().unwrap();
        let cache = FactCache::new(dir.path(), "module-v1");
        cache
            .get_or_extract("a", |_| Ok::<_, ()>(Location { line: 0, column: 1 }))
            .unwrap();
        let location = cache
            .get_or_extract("a", |_| Ok::<_, ()>(Location { line: 1, column: 1 }))
            .unwrap();
        assert_eq!(location.line, 1);
        assert_eq!(cache.stats().misses, 2);
        assert_eq!(cache.stats().hits, 0);
    }

    #[test]
    fn concurrent_writes_publish_only_whole_records() {
        let dir = tempfile::tempdir().unwrap();
        let barrier = Barrier::new(4);
        thread::scope(|scope| {
            for _ in 0..4 {
                scope.spawn(|| {
                    let cache = FactCache::new(dir.path(), "module-v1");
                    let result = cache.get_or_extract("a", |text| {
                        barrier.wait();
                        letters(text)
                    });
                    assert_eq!(result.unwrap(), ['a']);
                });
            }
        });
        let cache = FactCache::new(dir.path(), "module-v1");
        assert_eq!(cache.get_or_extract("a", hit).unwrap(), ['a']);
        assert_eq!(dir.path().read_dir().unwrap().count(), 1);
    }

    #[test]
    fn creates_outside_source_roots_and_leaves_source_directories_intact() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        let external_root = dir.path().join("external");
        let in_tree = project.join("cache");
        let in_root = external_root.join("cache");
        let outside = dir.path().join("cache");
        fs::create_dir(&project).unwrap();
        fs::create_dir(&external_root).unwrap();
        let roots = vec![external_root];
        for path in [&in_tree, &in_root] {
            let cache = FactCache::for_project(path, "module-v1", &project, &roots);
            assert_eq!(cache.get_or_extract("a", letters).unwrap(), ['a']);
            assert_eq!(cache.stats().write_failures, 1);
            assert!(!path.exists());
        }
        let cache = FactCache::for_project(&outside, "module-v1", &project, &roots);
        assert_eq!(cache.get_or_extract("a", letters).unwrap(), ['a']);
        assert_eq!(cache.stats().writes, 1);
        assert!(outside.is_dir());
    }
}
