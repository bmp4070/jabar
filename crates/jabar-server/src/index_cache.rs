//! A versioned, atomic snapshot of the already-built symbol index.
//!
//! The cache is keyed by the workspace, Bazel output tree, configuration and
//! HEAD. A hit avoids both the output-tree walk and SCIP protobuf decoding.
//! Shard freshness is checked separately after startup; HEAD alone cannot see
//! an external rebuild on the same commit.

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use symbol_index::{ShardMetadata, SymbolIndex};

use crate::config::Config;

const VERSION: u32 = 2;
const MAGIC: &[u8; 8] = b"JABARIDX";
static NEXT_GENERATION: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheKey {
    workspace: PathBuf,
    output_tree: PathBuf,
    head: Option<String>,
    output_base: Option<String>,
    bazel: Option<String>,
    targets: Vec<String>,
    scip_java: Option<String>,
}

impl CacheKey {
    pub fn new(root: &Path, dir: &Path, config: &Config) -> io::Result<Self> {
        Ok(Self {
            workspace: fs::canonicalize(root)?,
            output_tree: fs::canonicalize(dir)?,
            head: git_head(root),
            output_base: config.output_base.as_ref().map(|p| p.as_str().to_owned()),
            bazel: config.bazel.clone(),
            targets: config.index.targets.clone(),
            scip_java: config.index.scip_java.as_ref().map(|p| p.as_str().to_owned()),
        })
    }

    pub fn still_current(&self, root: &Path, dir: &Path) -> bool {
        self.head == git_head(root)
            && fs::canonicalize(root).is_ok_and(|workspace| workspace == self.workspace)
            && fs::canonicalize(dir).is_ok_and(|output| output == self.output_tree)
    }
}

fn git_head(root: &Path) -> Option<String> {
    let output = Command::new("git").args(["rev-parse", "HEAD"]).current_dir(root).output().ok()?;
    output.status.success().then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

#[derive(Serialize, Deserialize)]
struct Manifest {
    version: u32,
    key: CacheKey,
    shards: Vec<ShardMetadata>,
    bytes: u64,
    checksum: u64,
}

pub struct CacheHit {
    pub index: SymbolIndex,
    pub shards: Vec<ShardMetadata>,
    pub bytes: u64,
}

fn cache_dir(root: &Path) -> PathBuf {
    root.join(".jabar/index/cache")
}

/// Removes the published pointer after a confirmed empty shard set. Old
/// generations remain as a fallback for manual inspection and are pruned by a
/// later successful store.
pub fn invalidate(root: &Path) {
    let _ = fs::remove_file(cache_dir(root).join("CURRENT"));
}

/// Returns a validated snapshot, or a miss. Corruption never prevents a shard
/// fallback, and no recursive filesystem walk occurs on this path.
pub fn load(root: &Path, key: &CacheKey) -> io::Result<Option<CacheHit>> {
    let cache = cache_dir(root);
    let generation = match fs::read_to_string(cache.join("CURRENT")) {
        Ok(generation) => generation,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err),
    };
    let generation = generation.trim();
    if !generation.starts_with("g-")
        || !generation.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '-')
    {
        return Ok(None);
    }
    let dir = cache.join(generation);
    let manifest: Manifest = serde_json::from_reader(File::open(dir.join("manifest.json"))?)?;
    if manifest.version != VERSION || manifest.key != *key {
        return Ok(None);
    }

    let file = File::open(dir.join("index.bin"))?;
    if file.metadata()?.len() != manifest.bytes {
        return Ok(None);
    }
    // Buffer outside the digest wrapper so MessagePack's many small reads do
    // not run the checksum loop one field at a time.
    let mut reader = BufReader::new(DigestReader::new(file));
    let mut header = [0; 12];
    reader.read_exact(&mut header)?;
    if &header[..8] != MAGIC || u32::from_le_bytes(header[8..].try_into().unwrap()) != VERSION {
        return Ok(None);
    }
    let index = SymbolIndex::read_snapshot(&mut reader)?;
    let mut trailing = [0];
    if reader.read(&mut trailing)? != 0 || reader.get_ref().checksum() != manifest.checksum {
        return Ok(None);
    }
    Ok(Some(CacheHit { index, shards: manifest.shards, bytes: manifest.bytes }))
}

/// Publishes a generation only when the shard list still matches the index.
/// This runs off the LSP loop because serialization and verification can be
/// expensive on a large repository.
pub fn store(
    root: &Path,
    dir: &Path,
    key: &CacheKey,
    shards: &[ShardMetadata],
    index: &SymbolIndex,
    is_current: impl Fn() -> bool,
) -> io::Result<()> {
    if !is_current() {
        return Err(io::Error::other("cache generation was superseded before serialization"));
    }
    let cache = cache_dir(root);
    fs::create_dir_all(&cache)?;
    let generation = format!(
        "g-{}-{}-{}",
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos(),
        std::process::id(),
        NEXT_GENERATION.fetch_add(1, Ordering::Relaxed)
    );
    let staging = cache.join(format!("{generation}-tmp"));
    fs::create_dir(&staging)?;
    let result = (|| {
        let file = File::create(staging.join("index.bin"))?;
        // Buffer outside the digest wrapper for the same reason as the reader.
        let mut writer = BufWriter::new(DigestWriter::new(file));
        writer.write_all(MAGIC)?;
        writer.write_all(&VERSION.to_le_bytes())?;
        {
            let mut checked = CurrentWriter::new(&mut writer, &is_current);
            index.write_snapshot(&mut checked)?;
        }
        if !is_current() {
            return Err(io::Error::other("cache generation was superseded while serializing"));
        }
        writer.flush()?;
        writer.get_ref().inner.sync_all()?;
        let checksum = writer.get_ref().checksum();
        let bytes = fs::metadata(staging.join("index.bin"))?.len();

        // A build that rewrote shards while we serialized must not publish
        // this snapshot as if it represented the completed output tree.
        if SymbolIndex::scan_shards(dir)? != shards
            || !key.still_current(root, dir)
            || !is_current()
        {
            return Err(io::Error::other("shards or HEAD changed while caching"));
        }
        let manifest = Manifest {
            version: VERSION,
            key: key.clone(),
            shards: shards.to_vec(),
            bytes,
            checksum,
        };
        let mut stamp = BufWriter::new(File::create(staging.join("manifest.json"))?);
        serde_json::to_writer(&mut stamp, &manifest)?;
        stamp.flush()?;
        stamp.get_ref().sync_all()?;

        // Coordinate publication and pruning across server processes. No
        // cleanup can see this generation before its pointer is installed.
        let lock = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(cache.join("PUBLISH.lock"))?;
        lock.lock()?;
        if !is_current() || !key.still_current(root, dir) {
            return Err(io::Error::other("cache generation superseded before publication"));
        }
        fs::rename(&staging, cache.join(&generation))?;

        let pointer = cache.join(format!("CURRENT-{generation}-tmp"));
        let mut current = File::create(&pointer)?;
        current.write_all(generation.as_bytes())?;
        current.sync_all()?;
        if !is_current() || !key.still_current(root, dir) {
            let _ = fs::remove_file(&pointer);
            return Err(io::Error::other("cache generation superseded before publication"));
        }
        fs::rename(pointer, cache.join("CURRENT"))?;
        cleanup(&cache);
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(&staging);
        let _ = fs::remove_dir_all(cache.join(&generation));
    }
    result
}

/// Checks cancellation periodically while a large snapshot is serialized.
struct CurrentWriter<'a, W, F> {
    inner: &'a mut W,
    is_current: &'a F,
    until_check: usize,
}

impl<'a, W, F> CurrentWriter<'a, W, F> {
    fn new(inner: &'a mut W, is_current: &'a F) -> Self {
        Self { inner, is_current, until_check: 0 }
    }
}

impl<W: Write, F: Fn() -> bool> Write for CurrentWriter<'_, W, F> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        const CHECK_BYTES: usize = 1024 * 1024;
        if self.until_check == 0 {
            if !(self.is_current)() {
                return Err(io::Error::other("cache generation was superseded while serializing"));
            }
            self.until_check = CHECK_BYTES;
        }
        let chunk = &buf[..buf.len().min(self.until_check)];
        let written = self.inner.write(chunk)?;
        self.until_check = self.until_check.saturating_sub(written);
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

fn cleanup(cache: &Path) {
    let current = fs::read_to_string(cache.join("CURRENT")).unwrap_or_default();
    let Ok(entries) = fs::read_dir(cache) else { return };
    let mut generations: Vec<_> = entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_str()?.to_owned();
            (entry.file_type().ok()?.is_dir() && name.starts_with("g-") && !name.ends_with("-tmp"))
                .then_some((name, entry.path()))
        })
        .collect();
    generations.sort_by(|a, b| b.0.cmp(&a.0));
    let mut kept_fallback = false;
    for (name, path) in generations {
        if name == current {
            continue;
        }
        if !kept_fallback {
            kept_fallback = true;
            continue;
        }
        let _ = fs::remove_dir_all(path);
    }
}

struct DigestWriter<W> {
    inner: W,
    checksum: u64,
}

impl<W> DigestWriter<W> {
    fn new(inner: W) -> Self {
        Self { inner, checksum: FNV_OFFSET }
    }

    fn checksum(&self) -> u64 {
        self.checksum
    }
}

impl<W: Write> Write for DigestWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let written = self.inner.write(buf)?;
        update_checksum(&mut self.checksum, &buf[..written]);
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

struct DigestReader<R> {
    inner: R,
    checksum: u64,
}

impl<R> DigestReader<R> {
    fn new(inner: R) -> Self {
        Self { inner, checksum: FNV_OFFSET }
    }

    fn checksum(&self) -> u64 {
        self.checksum
    }
}

impl<R: Read> Read for DigestReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let read = self.inner.read(buf)?;
        update_checksum(&mut self.checksum, &buf[..read]);
        Ok(read)
    }
}

const FNV_OFFSET: u64 = 0xcbf29ce484222325;
const FNV_PRIME: u64 = 0x100000001b3;

fn update_checksum(checksum: &mut u64, bytes: &[u8]) {
    for byte in bytes {
        *checksum ^= u64::from(*byte);
        *checksum = checksum.wrapping_mul(FNV_PRIME);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use symbol_index::{Definition, PositionEncoding, Range, SymbolKind};

    fn fixture() -> (tempfile::TempDir, PathBuf, CacheKey, SymbolIndex) {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let dir = root.join("bazel-bin");
        fs::create_dir(&dir).unwrap();
        let key = CacheKey::new(root, &dir, &Config::default()).unwrap();
        let mut index = SymbolIndex::default();
        index.insert(Definition {
            symbol: "java Foo#".into(),
            name: "Foo".into(),
            kind: SymbolKind::Class,
            path: "src/Foo.java".into(),
            range: Range { start_line: 0, start_col: 6, end_line: 0, end_col: 9 },
            encoding: PositionEncoding::Utf16,
            implements: Vec::new(),
            documentation: vec!["A class".into()],
            signature: "class Foo".into(),
            enclosing: None,
        });
        (temp, dir, key, index)
    }

    #[test]
    fn snapshot_round_trips_and_rejects_a_changed_key() {
        let (temp, dir, key, index) = fixture();
        store(temp.path(), &dir, &key, &[], &index, || true).unwrap();
        let hit = load(temp.path(), &key).unwrap().unwrap();
        assert_eq!(hit.index.definition("java Foo#").unwrap().signature, "class Foo");
        assert_eq!(hit.index.search("foo").len(), 1);

        let mut changed = key.clone();
        changed.targets = vec!["//other/...".into()];
        assert!(load(temp.path(), &changed).unwrap().is_none());
    }

    #[test]
    fn old_format_is_rejected_before_its_body_is_decoded() {
        let (temp, dir, key, index) = fixture();
        store(temp.path(), &dir, &key, &[], &index, || true).unwrap();
        let cache = cache_dir(temp.path());
        let current = fs::read_to_string(cache.join("CURRENT")).unwrap();
        let generation = cache.join(current);
        let manifest_path = generation.join("manifest.json");
        let body = b"legacy body is deliberately unreadable";
        fs::write(generation.join("index.bin"), body).unwrap();
        let mut manifest: Manifest =
            serde_json::from_reader(File::open(&manifest_path).unwrap()).unwrap();
        manifest.version = VERSION - 1;
        manifest.bytes = body.len() as u64;
        serde_json::to_writer(File::create(manifest_path).unwrap(), &manifest).unwrap();

        assert!(load(temp.path(), &key).unwrap().is_none());
    }

    #[test]
    fn checksum_rejects_a_same_length_body_mutation() {
        let (temp, dir, key, index) = fixture();
        store(temp.path(), &dir, &key, &[], &index, || true).unwrap();
        let cache = cache_dir(temp.path());
        let current = fs::read_to_string(cache.join("CURRENT")).unwrap();
        let body = cache.join(current).join("index.bin");
        let mut bytes = fs::read(&body).unwrap();
        let offset = bytes
            .windows(3)
            .position(|window| window == b"Foo")
            .expect("fixture symbol is present in the snapshot");
        bytes[offset] = b'G';
        fs::write(body, bytes).unwrap();

        assert!(load(temp.path(), &key).unwrap().is_none());
    }

    #[test]
    fn trailing_snapshot_data_is_rejected() {
        let (temp, dir, key, index) = fixture();
        store(temp.path(), &dir, &key, &[], &index, || true).unwrap();
        let cache = cache_dir(temp.path());
        let current = fs::read_to_string(cache.join("CURRENT")).unwrap();
        let generation = cache.join(current);
        let body = generation.join("index.bin");
        OpenOptions::new().append(true).open(&body).unwrap().write_all(b"trailing").unwrap();

        let manifest_path = generation.join("manifest.json");
        let mut manifest: Manifest =
            serde_json::from_reader(File::open(&manifest_path).unwrap()).unwrap();
        let bytes = fs::read(body).unwrap();
        manifest.bytes = bytes.len() as u64;
        manifest.checksum = FNV_OFFSET;
        update_checksum(&mut manifest.checksum, &bytes);
        serde_json::to_writer(File::create(manifest_path).unwrap(), &manifest).unwrap();

        assert!(load(temp.path(), &key).unwrap().is_none());
    }

    #[test]
    fn snapshot_round_trip_crosses_buffer_boundaries() {
        let (temp, dir, key, mut index) = fixture();
        for number in 0..1_000 {
            let name = format!("Type{number}");
            index.insert(Definition {
                symbol: format!("java {name}#"),
                name,
                kind: SymbolKind::Class,
                path: format!("src/Type{number}.java"),
                range: Range { start_line: 0, start_col: 6, end_line: 0, end_col: 10 },
                encoding: PositionEncoding::Utf16,
                implements: Vec::new(),
                documentation: Vec::new(),
                signature: String::new(),
                enclosing: None,
            });
        }
        store(temp.path(), &dir, &key, &[], &index, || true).unwrap();

        let hit = load(temp.path(), &key).unwrap().unwrap();
        assert!(hit.bytes > 8 * 1024);
        assert_eq!(hit.index.definition_count(), 1_001);
    }

    #[test]
    fn truncated_snapshot_falls_back_and_old_generations_are_pruned() {
        let (temp, dir, key, index) = fixture();
        for _ in 0..3 {
            store(temp.path(), &dir, &key, &[], &index, || true).unwrap();
        }
        let cache = cache_dir(temp.path());
        let generations = fs::read_dir(&cache)
            .unwrap()
            .flatten()
            .filter(|entry| entry.file_type().unwrap().is_dir())
            .count();
        assert_eq!(generations, 2);

        let current = fs::read_to_string(cache.join("CURRENT")).unwrap();
        fs::write(cache.join(current).join("index.bin"), b"broken").unwrap();
        assert!(load(temp.path(), &key).unwrap().is_none());
    }

    #[test]
    fn concurrent_publishers_leave_a_loadable_current_generation() {
        let (temp, dir, key, index) = fixture();
        std::thread::scope(|scope| {
            for _ in 0..3 {
                scope.spawn(|| store(temp.path(), &dir, &key, &[], &index, || true).unwrap());
            }
        });
        let hit = load(temp.path(), &key).unwrap().unwrap();
        assert_eq!(hit.index.definition_count(), 1);
        let generations = fs::read_dir(cache_dir(temp.path()))
            .unwrap()
            .flatten()
            .filter(|entry| entry.file_type().unwrap().is_dir())
            .count();
        assert!(generations <= 2, "old cache generations should be bounded");
    }

    #[test]
    fn a_superseded_writer_stops_during_serialization() {
        let (temp, dir, key, index) = fixture();
        let checks = std::cell::Cell::new(0);

        let error = store(temp.path(), &dir, &key, &[], &index, || {
            let check = checks.get();
            checks.set(check + 1);
            check == 0
        })
        .expect_err("the second cancellation check should stop serialization");

        assert!(!error.to_string().is_empty());
        assert!(checks.get() >= 2, "serialization performed a cancellation check");
        assert!(!cache_dir(temp.path()).join("CURRENT").exists());
    }
}
