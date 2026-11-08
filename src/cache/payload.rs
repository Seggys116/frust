//! Bounded LZ4 blocks for cache files on filesystems without copy-on-write clones.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use serde::{Deserialize, Serialize};

use super::StoredFile;
use crate::{fsx, sys};

/// Outputs smaller than this keep ordinary payloads; a link saves too little to be worth the bookkeeping.
pub const LINKED_MIN_BYTES: u64 = 4096;

/// `FRUST_CACHE_BACKED`: `force` treats the cache volume as one without clones, for tests on copy-on-write volumes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackedMode {
    Off,
    #[default]
    Auto,
    Force,
}

impl BackedMode {
    pub fn parse(value: Option<&str>) -> BackedMode {
        match value {
            Some("off" | "0" | "false" | "no") => BackedMode::Off,
            Some("force") => BackedMode::Force,
            _ => BackedMode::Auto,
        }
    }

    pub fn from_env() -> BackedMode {
        BackedMode::parse(std::env::var("FRUST_CACHE_BACKED").ok().as_deref())
    }
}

const MAGIC: &[u8; 9] = b"FRUSTLZ4\x01";
// Blocks use bounded little-endian decoded/encoded u32 lengths; a zero pair ends the stream.

const BLOCK_BYTES: usize = 1024 * 1024;
pub const MIN_BYTES: u64 = 64 * 1024;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Encoding {
    #[default]
    Raw,
    Lz4BlocksV1,
    InlineMetadataSliceV1,
    /// The stored file is a second name of the target output, so the bytes exist once; every restore re-hashes it.
    Linked,
}

impl Encoding {
    pub fn is_raw(&self) -> bool {
        *self == Self::Raw
    }
}

pub struct StoredPayload {
    pub bytes: u64,
    pub encoding: Encoding,
    pub stored_bytes: Option<u64>,
    pub decoded_bytes: Option<u64>,
    pub content_hash: Option<String>,
    pub mode: Option<u32>,
}

/// Captured before Cargo can uplift an output; the full hash permits later ctime-only drift.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Commitment {
    pub(super) hash: String,
    pub(super) mode: u32,
    dev: u64,
    pub(super) ino: u64,
    pub(super) len: u64,
    mtime: (i64, i64),
}

impl Commitment {
    pub fn valid(&self) -> bool {
        self.hash.len() == 64
            && self
                .hash
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
            && self.mode & 0o170000 == 0o100000
            && (0..1_000_000_000).contains(&self.mtime.1)
    }

    pub fn hash(&self) -> &str {
        &self.hash
    }

    pub fn matches_identity(&self, dev: u64, ino: u64, len: u64, mtime_ns: i128) -> bool {
        self.dev == dev
            && self.ino == ino
            && self.len == len
            && self.mtime.0 as i128 * 1_000_000_000 + self.mtime.1 as i128 == mtime_ns
    }

    pub(super) fn matches(&self, meta: &sys::Meta) -> bool {
        meta.is_file()
            && self.dev == meta.dev()
            && self.ino == meta.ino()
            && self.len == meta.len()
            && self.mtime == meta.mtime()
            && self.mode == meta.mode()
    }
}

/// Uncommitted reads require unchanged identity, content stamps, and permissions.
pub(super) fn stable(before: &sys::Meta, after: &sys::Meta) -> bool {
    before.is_file()
        && after.is_file()
        && before.dev() == after.dev()
        && before.ino() == after.ino()
        && before.len() == after.len()
        && before.mtime() == after.mtime()
        && before.ctime() == after.ctime()
        && before.mode() == after.mode()
}

pub(super) fn hash_open(source: &File, len: u64) -> io::Result<(u64, String)> {
    let limit = len.saturating_add(1);
    let mut input = source.take(limit);
    let hash = super::hashing::hash_reader(&mut input)?;
    Ok((limit - input.limit(), hash))
}

pub fn capture(src: &Path) -> io::Result<(sys::Meta, Commitment)> {
    if !fs::symlink_metadata(src)?.is_file() {
        return Err(invalid("artifact is not a regular file"));
    }
    let source = File::open(src)?;
    let before = sys::fstat(&source)?;
    let (len, hash) = hash_open(&source, before.len())?;
    let after = sys::fstat(&source)?;
    if len != before.len() || !stable(&before, &after) {
        return Err(invalid("artifact changed while committing"));
    }
    let commitment = Commitment {
        hash,
        mode: before.mode(),
        dev: before.dev(),
        ino: before.ino(),
        len,
        mtime: before.mtime(),
    };
    Ok((before, commitment))
}

pub fn compression_supported(staged: &Path) -> bool {
    cfg!(any(target_os = "linux", windows)) && clones_unavailable(staged)
}

/// A volume without clones and with a steady change time, probed once per device; the condition for compressed and backed storage.
pub fn clones_unavailable(staged: &Path) -> bool {
    static DEVICES: OnceLock<Mutex<HashMap<u64, bool>>> = OnceLock::new();
    let Ok(meta) = sys::stat(staged) else {
        return false;
    };
    let mut devices = DEVICES
        .get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    *devices
        .entry(meta.dev())
        .or_insert_with(|| probe_allows_compression(staged, fsx::reflink_supported))
}

// Volumes without a change time (FAT-style) would fail every stability check, so they keep raw payloads.
fn probe_allows_compression(staged: &Path, reflinks: impl FnOnce(&Path) -> bool) -> bool {
    let steady_ctime = || -> io::Result<bool> {
        let first = sys::stat(staged)?;
        Ok(first.ctime() == sys::stat(staged)?.ctime())
    };
    !reflinks(staged) && steady_ctime().unwrap_or(false)
}

struct Temporary(PathBuf);

impl Drop for Temporary {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

/// The destination is private staging; directories and small or incompressible files retain raw storage.
pub fn store(src: &Path, dst: &Path, compress: bool) -> io::Result<StoredPayload> {
    store_committed(src, dst, compress, None)
}

/// Verify the private stored bytes against any commitment captured before Cargo handoff.
pub fn store_committed(
    src: &Path,
    dst: &Path,
    compress: bool,
    commitment: Option<&Commitment>,
) -> io::Result<StoredPayload> {
    store_inner(src, dst, compress, commitment, || {}, || {})
}

fn store_inner(
    src: &Path,
    dst: &Path,
    compress: bool,
    commitment: Option<&Commitment>,
    before_read: impl FnOnce(),
    before_clone: impl FnOnce(),
) -> io::Result<StoredPayload> {
    store_inner_captured(
        src,
        dst,
        compress,
        commitment,
        StoreHooks {
            before_read,
            before_clone,
            after_read: || {},
        },
        CaptureOptions::default(),
    )
    .map(|v| v.0)
}

// An opaque owned capture represents the actual stream, never a reread of the live source.
pub(crate) struct StoreProof {
    capture: super::metadata_slice::EncoderCapture,
    staged: File,
    staged_path: PathBuf,
    staged_meta: sys::Meta,
    hash: String,
    len: u64,
}
impl StoreProof {
    pub(crate) fn usable(&self, path: &Path, base: &StoredFile) -> bool {
        let check = || -> io::Result<bool> {
            Ok(path == self.staged_path
                && base.encoding == Encoding::Lz4BlocksV1
                && base.decoded_bytes == Some(self.len)
                && base.content_hash.as_ref() == Some(&self.hash)
                && stable(&self.staged_meta, &sys::fstat(&self.staged)?)
                && stable(&self.staged_meta, &sys::lstat(path)?)
                && self.capture.stable())
        };
        check().unwrap_or(false)
    }
    pub(crate) fn reader(&self) -> io::Result<File> {
        self.capture.reader()
    }
    #[cfg(all(test, unix))]
    pub(crate) fn capture_path(&self) -> &Path {
        self.capture.path()
    }
}
pub(crate) fn store_proven(
    src: &Path,
    dst: &Path,
    compress: bool,
    commitment: Option<&Commitment>,
    capture: bool,
) -> io::Result<(StoredPayload, Option<StoreProof>)> {
    store_inner_captured(
        src,
        dst,
        compress,
        commitment,
        StoreHooks {
            before_read: || {},
            before_clone: || {},
            after_read: || {},
        },
        CaptureOptions {
            enabled: capture,
            fail_after: None,
        },
    )
}
#[cfg(all(test, unix))]
pub(crate) fn store_proven_hook(
    src: &Path,
    dst: &Path,
    commitment: Option<&Commitment>,
    hook: impl FnOnce(),
    fail_after: Option<u64>,
) -> io::Result<(StoredPayload, Option<StoreProof>)> {
    store_inner_captured(
        src,
        dst,
        true,
        commitment,
        StoreHooks {
            before_read: || {},
            before_clone: || {},
            after_read: hook,
        },
        CaptureOptions {
            enabled: true,
            fail_after,
        },
    )
}
struct StoreHooks<B, C, A> {
    before_read: B,
    before_clone: C,
    after_read: A,
}

#[derive(Default)]
struct CaptureOptions {
    enabled: bool,
    fail_after: Option<u64>,
}

fn store_inner_captured<B: FnOnce(), C: FnOnce(), A: FnOnce()>(
    src: &Path,
    dst: &Path,
    compress: bool,
    commitment: Option<&Commitment>,
    hooks: StoreHooks<B, C, A>,
    options: CaptureOptions,
) -> io::Result<(StoredPayload, Option<StoreProof>)> {
    let StoreHooks {
        before_read,
        before_clone,
        after_read,
    } = hooks;
    let meta = fs::symlink_metadata(src)?;
    if commitment.is_some_and(|c| !c.valid() || !meta.is_file()) {
        return Err(invalid("invalid artifact commitment"));
    }
    let bytes = fsx::allocated_size(src);
    if compress && meta.is_file() && meta.len() >= MIN_BYTES {
        let tmp = Temporary(fsx::temp_sibling(dst));
        let source = File::open(src)?;
        let before = sys::fstat(&source)?;
        if commitment.is_some_and(|c| !c.matches(&before)) {
            return Err(invalid("artifact changed after commitment"));
        }
        let mut capture = if options.enabled && cfg!(unix) && before.len() <= 256 * 1024 * 1024 {
            super::metadata_slice::EncoderCapture::new(dst).ok()
        } else {
            None
        };
        #[cfg(test)]
        if options.fail_after == Some(0)
            && let Some(capture) = capture.as_mut()
        {
            capture.readonly_for_control()?;
        }
        let mut spool = capture
            .as_mut()
            .map(|c| super::metadata_slice::ArchiveSpool::new(c.writer(), before.len()));
        let mut capture_failed = false;
        before_read();
        let mut input = BufReader::new((&source).take(before.len().saturating_add(1)));
        let output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp.0)?;
        let mut output = BufWriter::new(output);
        output.write_all(MAGIC)?;
        // Keep the growth sentinel in the same frame even for an initially empty descriptor.
        let block_bytes = before.len().saturating_add(1).min(BLOCK_BYTES as u64) as usize;
        let mut block = vec![0; block_bytes];
        let mut encoded = vec![0; lz4_flex::block::get_maximum_output_size(block_bytes)];
        let mut hash = blake3::Hasher::new();
        let mut decoded_bytes = 0u64;
        loop {
            let mut len = 0;
            while len < block.len() {
                match input.read(&mut block[len..]) {
                    Ok(0) => break,
                    Ok(n) => len += n,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(e) => return Err(e),
                }
            }
            if len == 0 {
                break;
            }
            hash.update(&block[..len]);
            decoded_bytes += len as u64;
            // The same immutable block is observed and encoded; capture failure only removes the hint.
            if !capture_failed && let Some(spool) = spool.as_mut() {
                capture_failed = options
                    .fail_after
                    .is_some_and(|limit| limit > 0 && decoded_bytes > limit)
                    || spool.write_all(&block[..len]).is_err();
            }
            let packed = lz4_flex::block::compress_into(&block[..len], &mut encoded)
                .map_err(io::Error::other)?;
            output.write_all(&(len as u32).to_le_bytes())?;
            output.write_all(&(packed as u32).to_le_bytes())?;
            output.write_all(&encoded[..packed])?;
        }
        output.write_all(&[0; 8])?;
        output.flush()?;
        after_read();
        let after = sys::fstat(&source)?;
        let content_hash = hash.finalize().to_hex().to_string();
        let unchanged = commitment.map_or_else(
            || stable(&before, &after),
            |c| c.matches(&after) && c.hash == content_hash,
        );
        if decoded_bytes != before.len() || !unchanged {
            return Err(invalid("artifact changed while compressing"));
        }
        let capture_complete = !capture_failed
            && spool
                .as_mut()
                .is_some_and(|s| s.finish(decoded_bytes).unwrap_or(false));
        drop(spool);
        let packed_bytes = output.get_ref().metadata()?.len();
        if packed_bytes.saturating_add(4096) <= decoded_bytes
            && packed_bytes <= decoded_bytes - decoded_bytes / 8
        {
            fs::set_permissions(&tmp.0, before.permissions())?;
            let staged_before = capture_complete
                .then(|| sys::fstat(output.get_ref()).ok())
                .flatten();
            let staged_writer = capture_complete
                .then(|| output.get_ref().try_clone().ok())
                .flatten();
            drop(output);
            sys::rename(&tmp.0, dst)?;
            // Only our rename may move staged ctime; the writer FD binds identity across it.
            let proof = (|| -> Option<StoreProof> {
                if !capture_complete {
                    return None;
                }
                let mut capture = capture?;
                capture.seal(decoded_bytes).ok()?;
                let writer = staged_writer?;
                let prior = staged_before?;
                let after = sys::fstat(&writer).ok()?;
                if prior.dev() != after.dev()
                    || prior.ino() != after.ino()
                    || prior.len() != after.len()
                    || prior.mode() != after.mode()
                    || prior.mtime() != after.mtime()
                {
                    return None;
                }
                let staged = File::open(dst).ok()?;
                let staged_meta = sys::fstat(&staged).ok()?;
                if !stable(&after, &staged_meta) || !stable(&staged_meta, &sys::lstat(dst).ok()?) {
                    return None;
                }
                Some(StoreProof {
                    capture,
                    staged,
                    staged_path: dst.to_path_buf(),
                    staged_meta,
                    hash: content_hash.clone(),
                    len: decoded_bytes,
                })
            })();
            return Ok((
                StoredPayload {
                    bytes,
                    encoding: Encoding::Lz4BlocksV1,
                    stored_bytes: Some(fsx::allocated_size(dst)),
                    decoded_bytes: Some(decoded_bytes),
                    content_hash: Some(content_hash),
                    mode: None,
                },
                proof,
            ));
        }
    }
    before_clone();
    fsx::clone_new(src, dst)?;
    if let Some(expected) = commitment {
        // Incompressible inputs are reopened for cloning, so verify the actual private copy.
        let path_before = sys::lstat(dst)?;
        if !path_before.is_file() {
            return Err(invalid("staged artifact is not a regular file"));
        }
        let source = File::open(dst)?;
        let before = sys::fstat(&source)?;
        if !stable(&path_before, &before) {
            return Err(invalid("staged artifact changed before verification"));
        }
        let (len, hash) = hash_open(&source, expected.len)?;
        let after = sys::fstat(&source)?;
        let path_after = sys::lstat(dst)?;
        if !stable(&before, &after)
            || !stable(&after, &path_after)
            || before.mode() != expected.mode
            || len != expected.len
            || hash != expected.hash
        {
            return Err(invalid("staged artifact differs from commitment"));
        }
    }
    Ok((
        StoredPayload {
            bytes,
            encoding: Encoding::Raw,
            stored_bytes: Some(fsx::allocated_size(dst)),
            decoded_bytes: None,
            content_hash: None,
            mode: None,
        },
        None,
    ))
}

/// A failed decode leaves the previous destination intact; raw entries retain their original clone path.
pub fn restore(src: &Path, dst: &Path, meta: &StoredFile) -> io::Result<()> {
    if meta.metadata_slice.is_some() || meta.encoding == Encoding::InlineMetadataSliceV1 {
        return Err(invalid("inline metadata requires grouped restore"));
    }
    if meta.encoding == Encoding::Linked {
        return Err(invalid("linked entries restore through a verified copy"));
    }
    if meta.encoding.is_raw() {
        return fsx::clone_replace(src, dst);
    }
    let source = File::open(src)?;
    let source_meta = source.metadata()?;
    let tmp = Temporary(fsx::temp_sibling(dst));
    let output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp.0)?;
    let mut output = BufWriter::new(output);
    decode(&source, meta, &mut output)?;
    output.flush()?;
    fs::set_permissions(&tmp.0, source_meta.permissions())?;
    drop(output);
    if fs::symlink_metadata(dst).is_ok_and(|m| m.is_dir()) {
        fs::remove_dir_all(dst)?;
    }
    sys::rename(&tmp.0, dst)
}

/// Verifies encoded bytes without creating a restored artifact.
pub fn verify(src: &Path, meta: &StoredFile) -> io::Result<()> {
    let source = File::open(src)?;
    let before = sys::fstat(&source)?;
    if !before.is_file() {
        return Err(invalid("payload is not a regular file"));
    }
    decode(&source, meta, &mut io::sink())?;
    let after = sys::fstat(&source)?;
    if before.len() != after.len()
        || before.mtime() != after.mtime()
        || before.ctime() != after.ctime()
    {
        return Err(invalid("payload changed while verifying"));
    }
    Ok(())
}

/// Decode bounded blocks and validate the complete source before returning success.
pub(super) fn decode(source: &File, meta: &StoredFile, output: &mut impl Write) -> io::Result<()> {
    if meta.encoding != Encoding::Lz4BlocksV1
        || meta.dir
        || meta.text
        || meta.metadata_slice.is_some()
    {
        return Err(invalid("encoded payload is not a regular artifact"));
    }
    let expected_len = meta
        .decoded_bytes
        .ok_or_else(|| invalid("missing decoded payload length"))?;
    let expected_hash = meta
        .content_hash
        .as_deref()
        .ok_or_else(|| invalid("missing payload hash"))?;
    let mut input = BufReader::new(source);
    let mut magic = [0; MAGIC.len()];
    input.read_exact(&mut magic)?;
    if magic != *MAGIC {
        return Err(invalid("unknown payload header"));
    }
    let mut encoded = vec![0; lz4_flex::block::get_maximum_output_size(BLOCK_BYTES)];
    let mut block = vec![0; BLOCK_BYTES];
    let mut decoded_bytes = 0u64;
    let mut hash = blake3::Hasher::new();
    loop {
        let mut header = [0; 8];
        input.read_exact(&mut header)?;
        let len = u32::from_le_bytes(header[..4].try_into().unwrap()) as usize;
        let packed = u32::from_le_bytes(header[4..].try_into().unwrap()) as usize;
        if len == 0 && packed == 0 {
            break;
        }
        if len == 0
            || len > BLOCK_BYTES
            || packed == 0
            || packed > encoded.len()
            || len as u64 > expected_len.saturating_sub(decoded_bytes)
        {
            return Err(invalid("invalid payload block length"));
        }
        input.read_exact(&mut encoded[..packed])?;
        let actual = lz4_flex::block::decompress_into(&encoded[..packed], &mut block[..len])
            .map_err(io::Error::other)?;
        if actual != len {
            return Err(invalid("incorrect decoded block length"));
        }
        decoded_bytes += len as u64;
        hash.update(&block[..len]);
        output.write_all(&block[..len])?;
    }
    let mut trailing = [0];
    if decoded_bytes != expected_len
        || hash.finalize().to_hex().as_str() != expected_hash
        || input.read(&mut trailing)? != 0
    {
        return Err(invalid("payload length or hash mismatch"));
    }
    Ok(())
}

/// A holder or destination that cannot supply an entry's bytes; any other error is a local failure to write.
#[derive(Debug)]
pub struct Unbacked(&'static str);

impl std::fmt::Display for Unbacked {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "linked file unusable: {}", self.0)
    }
}

impl std::error::Error for Unbacked {}

pub fn unbacked(reason: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, Unbacked(reason))
}

pub fn is_unbacked(error: &io::Error) -> bool {
    error.get_ref().is_some_and(|inner| inner.is::<Unbacked>())
}

/// A copy whose source kept changing underneath it; the entry may be fine, so the candidate is skipped and nothing is removed.
#[derive(Debug)]
pub struct Unstable(&'static str);

impl std::fmt::Display for Unstable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "linked file unstable: {}", self.0)
    }
}

impl std::error::Error for Unstable {}

pub fn is_unstable(error: &io::Error) -> bool {
    error.get_ref().is_some_and(|inner| inner.is::<Unstable>())
}

// Only a missing file means the store file is gone; any other failure is transient and leaves the entry alone.
fn lost<T>(result: io::Result<T>) -> io::Result<T> {
    result.map_err(|error| if error.kind() == io::ErrorKind::NotFound { unbacked("stored file is gone") } else { error })
}

fn linked_expectation(file: &StoredFile) -> io::Result<(u64, &str, u32)> {
    match (file.encoding, file.decoded_bytes, file.content_hash.as_deref(), file.mode) {
        (Encoding::Linked, Some(len), Some(hash), Some(mode)) if !file.dir && !file.text => Ok((len, hash, mode)),
        _ => Err(invalid("linked metadata is incomplete")),
    }
}

/// Content identity only: a link added or removed by Cargo's uplift moves ctime without touching the bytes.
fn same_content(a: &sys::Meta, b: &sys::Meta) -> bool {
    a.is_file()
        && b.is_file()
        && a.dev() == b.dev()
        && a.ino() == b.ino()
        && a.len() == b.len()
        && a.mtime() == b.mtime()
        && a.mode() == b.mode()
}

/// Like `capture`, but ctime is ignored: Cargo's uplift adds names to the inode, which moves ctime and never the bytes.
fn capture_content(src: &Path) -> io::Result<Commitment> {
    let source = File::open(src)?;
    let before = sys::fstat(&source)?;
    if !before.is_file() {
        return Err(invalid("artifact is not a regular file"));
    }
    let (len, hash) = hash_open(&source, before.len())?;
    let after = sys::fstat(&source)?;
    if len != before.len() || !same_content(&before, &after) {
        return Err(invalid("artifact changed while committing"));
    }
    Ok(Commitment { hash, mode: before.mode(), dev: before.dev(), ino: before.ino(), len, mtime: before.mtime() })
}

fn link_commitment(src: &Path, dst: &Path, commitment: Option<&Commitment>) -> io::Result<StoredPayload> {
    let meta = sys::lstat(src)?;
    if !meta.is_file() || meta.len() < LINKED_MIN_BYTES {
        return Err(invalid("output is not linkable"));
    }
    let captured;
    let commitment = match commitment {
        Some(commitment) => commitment,
        None => {
            captured = capture_content(src)?;
            &captured
        }
    };
    if !commitment.valid() || !commitment.matches(&meta) || !commitment.matches(&sys::lstat(src)?) {
        return Err(invalid("artifact changed after commitment"));
    }
    // The link adds a name to the inode just hashed and checked, and Cargo's uplift only adds names, so the bytes it exposes are the hashed ones.
    link_retrying(src, dst)?;
    if !commitment.matches(&sys::lstat(dst)?) {
        return Err(invalid("linked artifact differs from commitment"));
    }
    Ok(StoredPayload {
        bytes: fsx::allocated_size(src),
        encoding: Encoding::Linked,
        stored_bytes: Some(commitment.len),
        decoded_bytes: Some(commitment.len),
        content_hash: Some(commitment.hash.clone()),
        mode: Some(commitment.mode),
    })
}

#[cfg(test)]
thread_local! {
    static LINK_FAULTS: std::cell::RefCell<std::collections::VecDeque<io::Error>> = const { std::cell::RefCell::new(std::collections::VecDeque::new()) };
}

/// Makes the next hard links on this thread fail with `errors`, in order, as another process's handle would.
#[cfg(test)]
pub(crate) fn inject_link_faults(errors: impl IntoIterator<Item = io::Error>) {
    LINK_FAULTS.with(|faults| faults.borrow_mut().extend(errors));
}

fn hard_link(src: &Path, dst: &Path) -> io::Result<()> {
    #[cfg(test)]
    if let Some(error) = LINK_FAULTS.with(|faults| faults.borrow_mut().pop_front()) {
        return Err(error);
    }
    fs::hard_link(src, dst)
}

// About 155 ms in all: a scanner or indexer that opened the file on Windows usually lets go within that.
const LINK_RETRY_MS: [u64; 5] = [5, 10, 20, 40, 80];

fn link_retrying(src: &Path, dst: &Path) -> io::Result<()> {
    let mut delays = LINK_RETRY_MS.iter();
    loop {
        match hard_link(src, dst) {
            Err(error) if sys::transient_share_error(&error) => match delays.next() {
                Some(&ms) => std::thread::sleep(std::time::Duration::from_millis(ms)),
                None => return Err(error),
            },
            other => return other,
        }
    }
}

/// A short, stable label for timing records: the OS error code when there is one, else the error kind.
pub fn error_label(error: &io::Error) -> String {
    match error.raw_os_error() {
        Some(code) => format!("os{code}"),
        None => format!("{:?}", error.kind()),
    }
}

/// How `restore_linked` placed a file: as the store file's own inode, or as a verified copy, with why it was not linked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Placement {
    Linked,
    Copied(String),
}

/// Stores a large output as a second name of the target file; any link failure leaves no staged name and falls back to a copy.
pub(crate) fn store_linked(
    src: &Path,
    dst: &Path,
    compress: bool,
    commitment: Option<&Commitment>,
    capture: bool,
) -> io::Result<(StoredPayload, Option<StoreProof>)> {
    match link_commitment(src, dst, commitment) {
        Ok(payload) => Ok((payload, None)),
        Err(_) => {
            let _ = fs::remove_file(dst);
            store_proven(src, dst, compress, commitment, capture)
        }
    }
}

fn copy_hashed(source: &File, len: u64, out: &mut impl Write) -> io::Result<(u64, String)> {
    let mut input = source.take(len.saturating_add(1));
    let mut hash = blake3::Hasher::new();
    let mut buffer = vec![0; 256 * 1024];
    let mut total = 0u64;
    loop {
        let n = match input.read(&mut buffer) {
            Ok(0) => break,
            Ok(n) => n,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        hash.update(&buffer[..n]);
        out.write_all(&buffer[..n])?;
        total += n as u64;
    }
    Ok((total, hash.finalize().to_hex().to_string()))
}

/// Succeeds only when `path` already holds exactly the entry's bytes and did not change while being read.
pub fn verify_in_place(path: &Path, file: &StoredFile) -> io::Result<()> {
    let (len, expected, mode) = linked_expectation(file)?;
    let path_meta = lost(sys::lstat(path))?;
    if !path_meta.is_file() || path_meta.len() != len || path_meta.mode() != mode {
        return Err(unbacked("destination differs from the entry"));
    }
    let source = lost(File::open(path))?;
    let before = lost(sys::fstat(&source))?;
    if !same_content(&path_meta, &before) {
        return Err(unbacked("destination changed before reading"));
    }
    let (read, hash) = lost(hash_open(&source, len))?;
    let after = lost(sys::fstat(&source))?;
    let again = lost(sys::lstat(path))?;
    if read != len || hash != expected || !same_content(&before, &after) || !same_content(&after, &again) {
        return Err(unbacked("destination bytes differ from the entry"));
    }
    Ok(())
}

/// Links a store file no target shares into `dst`, else copies it and publishes the copy only if the bytes copied hash to the recorded content hash; the mode is the recorded one, since a chmod through any name changes them all.
pub fn restore_linked(store_file: &Path, dst: &Path, file: &StoredFile) -> io::Result<Placement> {
    let (len, expected, mode) = linked_expectation(file)?;
    let refused = match restore_by_link(store_file, dst, len, expected, mode)? {
        Ok(()) => return Ok(Placement::Linked),
        Err(reason) => reason,
    };
    // Only the bytes copied count: a touch or an adopt during the copy may move mtime or the name, so a copy that hashes right is published, and one that does not is retried once before it is called unstable.
    for _ in 0..2 {
        let source = lost(File::open(store_file))?;
        let before = lost(sys::fstat(&source))?;
        if !before.is_file() {
            return Err(unbacked("stored file is not a regular file"));
        }
        let tmp = Temporary(fsx::temp_sibling(dst));
        let output = OpenOptions::new().write(true).create_new(true).open(&tmp.0)?;
        let mut output = BufWriter::new(output);
        let (copied, hash) = copy_hashed(&source, len, &mut output)?;
        output.flush()?;
        if copied == len && hash == expected {
            sys::set_path_mode(&tmp.0, mode & 0o7777)?;
            drop(output);
            if fs::symlink_metadata(dst).is_ok_and(|m| m.is_dir()) {
                fs::remove_dir_all(dst)?;
            }
            return sys::rename(&tmp.0, dst).map(|()| Placement::Copied(refused));
        }
        let after = lost(sys::fstat(&source))?;
        // The open handle kept its inode, so a length or mtime change means a writer was active during the read.
        if after.len() == before.len() && after.mtime() == before.mtime() {
            return Err(unbacked("stored bytes differ from the record"));
        }
    }
    Err(io::Error::other(Unstable("the store file kept changing during the copy")))
}

/// Moves the store file's only name-free inode into the target through a private temporary link, so nothing is copied; an inner `Err` is why the caller must copy instead.
fn restore_by_link(store_file: &Path, dst: &Path, len: u64, expected: &str, mode: u32) -> io::Result<Result<(), String>> {
    let source = lost(File::open(store_file))?;
    let before = lost(sys::fstat(&source))?;
    if !before.is_file() || before.len() != len {
        return Ok(Err("store-not-entry".into()));
    }
    if before.nlink() != 1 {
        return Ok(Err(format!("store-nlink={}", before.nlink())));
    }
    // The bytes are read once and must match the record: an in-place writer may have changed them.
    let (read, hash) = hash_open(&source, len)?;
    let after = lost(sys::fstat(&source))?;
    if read != len || hash != expected {
        if after.len() == before.len() && after.mtime() == before.mtime() {
            return Err(unbacked("stored bytes differ from the record"));
        }
        return Ok(Err("store-changed".into()));
    }
    let tmp = Temporary(fsx::temp_sibling(dst));
    if let Err(error) = link_retrying(store_file, &tmp.0) {
        return Ok(Err(format!("link:{}", error_label(&error))));
    }
    // Exactly the store name and our temporary: any other restorer or target that holds a name makes this a copy case, so no two targets share an inode.
    let now = match sys::lstat(store_file) {
        Ok(now) => now,
        Err(error) => return Ok(Err(format!("relstat:{}", error_label(&error)))),
    };
    if now.nlink() != 2 {
        return Ok(Err(format!("linked-nlink={}", now.nlink())));
    }
    if !now.is_file() || now.ino() != before.ino() || now.dev() != before.dev() || now.len() != len || now.mtime() != before.mtime() {
        return Ok(Err("store-moved".into()));
    }
    if !sys::lstat(&tmp.0).is_ok_and(|held| held.ino() == before.ino() && held.dev() == before.dev()) {
        return Ok(Err("temp-moved".into()));
    }
    if before.mode() != mode {
        sys::set_path_mode(&tmp.0, mode & 0o7777)?;
    }
    if fs::symlink_metadata(dst).is_ok_and(|m| m.is_dir()) {
        fs::remove_dir_all(dst)?;
    }
    Ok(sys::rename(&tmp.0, dst).map_err(|error| format!("rename:{}", error_label(&error))))
}

/// Best effort: when no target name shares the store file any more, the freshly verified destination replaces it, so the store and that one target keep a single copy; `Ok(false)` means they already share it and `Err` says why it did not.
pub fn adopt(store_file: &Path, dst: &Path) -> Result<bool, String> {
    let stored = sys::lstat(store_file).map_err(|error| format!("store:{}", error_label(&error)))?;
    let placed = sys::lstat(dst).map_err(|error| format!("target:{}", error_label(&error)))?;
    if stored.is_file() && placed.is_file() && stored.dev() == placed.dev() && stored.ino() == placed.ino() {
        return Ok(false);
    }
    if stored.nlink() != 1 {
        return Err(format!("store-nlink={}", stored.nlink()));
    }
    if !stored.is_file() || !placed.is_file() || stored.dev() != placed.dev() || stored.len() != placed.len() {
        return Err("target-differs".into());
    }
    let tmp = Temporary(fsx::temp_sibling(store_file));
    link_retrying(dst, &tmp.0).map_err(|error| format!("link:{}", error_label(&error)))?;
    if !sys::lstat(&tmp.0).is_ok_and(|linked| same_content(&linked, &placed)) {
        return Err("target-moved".into());
    }
    // A sharing violation on Windows (a scanner holding the store file) usually passes within moments; giving up only leaves a copy.
    let mut last = None;
    for attempt in 0..3 {
        match sys::rename(&tmp.0, store_file) {
            Ok(()) => return Ok(true),
            Err(error) => last = Some(error),
        }
        if attempt < 2 {
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
    }
    Err(format!("rename:{}", last.map_or_else(String::new, |error| error_label(&error))))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Scratch(PathBuf);

    impl Scratch {
        fn new() -> Self {
            let path = fsx::temp_sibling(&std::env::temp_dir().join("frust-payload"));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn bounded_output_hash_checks_length_sentinel_and_buffer_boundaries() {
        let scratch = Scratch::new();
        let source = scratch.0.join("bounded-hash");
        for (actual, declared, consumed) in [
            (0, 0, 0),
            (0, u64::MAX, 0),
            (1, 0, 1),
            (9, 3, 4),
            (9, 9, 9),
            (9, 20, 9),
            (9, u64::MAX, 9),
            (262_145, 262_142, 262_143),
            (262_145, 262_143, 262_144),
            (262_145, 262_144, 262_145),
            (1_048_577, 1_048_574, 1_048_575),
            (1_048_577, 1_048_575, 1_048_576),
            (1_048_577, 1_048_576, 1_048_577),
        ] {
            let content: Vec<u8> = (0..actual).map(|i| (i % 251) as u8).collect();
            fs::write(&source, &content).unwrap();
            let mut file = File::open(&source).unwrap();
            let (bytes, hash) = hash_open(&file, declared).unwrap();
            assert_eq!(bytes, consumed as u64);
            assert_eq!(hash, blake3::hash(&content[..consumed]).to_hex().as_str());
            let mut next = [0];
            assert_eq!(file.read(&mut next).unwrap(), usize::from(consumed < actual));
            if consumed < actual {
                assert_eq!(next[0], content[consumed]);
            }
        }
    }

    fn metadata(payload: StoredPayload) -> StoredFile {
        StoredFile {
            name: "artifact".into(),
            stored: "f0".into(),
            text: false,
            bytes: payload.bytes,
            dir: false,
            encoding: payload.encoding,
            stored_bytes: payload.stored_bytes,
            decoded_bytes: payload.decoded_bytes,
            content_hash: payload.content_hash,
            metadata_slice: None,
            mode: payload.mode,
            inline: None,
        }
    }

    // Links `source` into a staged name and returns the record a store would write for it.
    fn linked(scratch: &Path, source: &Path) -> (PathBuf, StoredFile) {
        let (_, commitment) = capture(source).unwrap();
        let staged = scratch.join("f0");
        let (payload, proof) = store_linked(source, &staged, false, Some(&commitment), false).unwrap();
        assert!(proof.is_none());
        (staged, metadata(payload))
    }

    #[test]
    fn a_linked_store_shares_the_inode_and_falls_back_to_a_copy_when_it_cannot_link() {
        let scratch = Scratch::new();
        let big = scratch.0.join("big");
        fs::write(&big, vec![b'b'; 8192]).unwrap();
        let (staged, file) = linked(&scratch.0, &big);
        assert_eq!(file.encoding, Encoding::Linked);
        assert_eq!((file.decoded_bytes, file.mode), (Some(8192), Some(sys::lstat(&big).unwrap().mode())));
        assert_eq!(file.content_hash.as_deref(), Some(blake3::hash(&vec![b'b'; 8192]).to_hex().as_str()));
        assert_eq!(sys::lstat(&staged).unwrap().ino(), sys::lstat(&big).unwrap().ino());
        assert_eq!(sys::lstat(&big).unwrap().nlink(), 2);

        let (_, commitment) = capture(&big).unwrap();
        let taken = scratch.0.join("taken");
        fs::write(&taken, b"already here").unwrap();
        let (payload, _) = store_linked(&big, &taken, false, Some(&commitment), false).unwrap();
        assert_eq!(payload.encoding, Encoding::Raw, "a link that fails leaves an ordinary copy");
        assert_eq!(fs::read(&taken).unwrap(), vec![b'b'; 8192]);
        assert_ne!(sys::lstat(&taken).unwrap().ino(), sys::lstat(&big).unwrap().ino());

        let small = scratch.0.join("small");
        fs::write(&small, vec![b's'; (LINKED_MIN_BYTES - 1) as usize]).unwrap();
        let (payload, _) = store_linked(&small, &scratch.0.join("g0"), false, None, false).unwrap();
        assert_eq!(payload.encoding, Encoding::Raw, "small outputs are copied");

        fs::write(&big, vec![b'c'; 8193]).unwrap();
        let stale = store_linked(&big, &scratch.0.join("g1"), false, Some(&commitment), false);
        assert!(stale.is_err(), "a rewritten output no longer matches its commitment");
        let (hashed, _) = store_linked(&big, &scratch.0.join("g2"), false, None, false).unwrap();
        assert_eq!(hashed.encoding, Encoding::Linked, "the store step hashes an uncommitted output itself");
        assert_eq!(hashed.content_hash.as_deref(), Some(blake3::hash(&vec![b'c'; 8193]).to_hex().as_str()));
    }

    #[test]
    fn restores_copy_with_the_recorded_mode_and_adopt_shares_only_with_an_orphaned_store_file() {
        let scratch = Scratch::new();
        let source = scratch.0.join("source");
        let bytes: Vec<u8> = (0..70_000u32).map(|i| (i % 251) as u8).collect();
        fs::write(&source, &bytes).unwrap();
        let (staged, file) = linked(&scratch.0, &source);
        let recorded = file.mode.unwrap();
        let first = scratch.0.join("first");
        fs::write(&first, b"stale").unwrap();
        restore_linked(&staged, &first, &file).unwrap();
        assert_eq!(fs::read(&first).unwrap(), bytes);
        assert_ne!(sys::lstat(&first).unwrap().ino(), sys::lstat(&staged).unwrap().ino(), "a restore always copies");
        assert!(fs::read_dir(&scratch.0).unwrap().flatten().all(|e| !e.file_name().to_string_lossy().contains(fsx::TEMP_MARKER)), "no temporary is left behind");

        assert_eq!(adopt(&staged, &first), Err("store-nlink=2".to_string()));
        assert_ne!(sys::lstat(&staged).unwrap().ino(), sys::lstat(&first).unwrap().ino(), "the store is still shared with the source name");
        fs::remove_file(source).unwrap();
        assert_eq!(adopt(&staged, &first), Ok(true));
        assert_eq!(sys::lstat(&staged).unwrap().ino(), sys::lstat(&first).unwrap().ino(), "an orphaned store file is replaced by the verified destination");
        assert_eq!(adopt(&staged, &first), Ok(false), "an already shared destination needs nothing");
        let second = scratch.0.join("second");
        assert_eq!(restore_linked(&staged, &second, &file).unwrap(), Placement::Copied("store-nlink=2".into()));
        assert_eq!(adopt(&staged, &second), Err("store-nlink=2".to_string()));
        assert_ne!(sys::lstat(&second).unwrap().ino(), sys::lstat(&first).unwrap().ino(), "no two destinations ever share an inode");
        assert_ne!(sys::lstat(&second).unwrap().ino(), sys::lstat(&staged).unwrap().ino());

        #[cfg(unix)]
        {
            sys::set_path_mode(&first, 0o700).unwrap();
            let third = scratch.0.join("third");
            restore_linked(&staged, &third, &file).unwrap();
            assert_eq!(sys::lstat(&third).unwrap().mode(), recorded, "the mode comes from the record, not from a chmod through another name");
        }
        let _ = recorded;

        let mut mutated = bytes.clone();
        mutated[30_000] ^= 0x40;
        fs::write(&first, &mutated).unwrap();
        let other = scratch.0.join("other");
        let denied = restore_linked(&staged, &other, &file).unwrap_err();
        assert!(is_unbacked(&denied), "{denied}");
        assert!(!other.exists(), "an in-place rewrite through a target name publishes nothing");

        fs::remove_file(&staged).unwrap();
        assert!(is_unbacked(&restore_linked(&staged, &other, &file).unwrap_err()));
        let plain = StoredFile { encoding: Encoding::Raw, ..file };
        assert!(!is_unbacked(&restore_linked(&staged, &other, &plain).unwrap_err()));
    }

    // A linked store file whose target name is gone, so the store holds the only name.
    fn exclusive(scratch: &Path, bytes: &[u8]) -> (PathBuf, StoredFile) {
        let source = scratch.join("source");
        fs::write(&source, bytes).unwrap();
        let (staged, file) = linked(scratch, &source);
        fs::remove_file(source).unwrap();
        assert_eq!(sys::lstat(&staged).unwrap().nlink(), 1);
        (staged, file)
    }

    #[test]
    fn an_exclusive_store_file_is_restored_by_link_without_copying() {
        let scratch = Scratch::new();
        let bytes = pattern(300_000);
        let (staged, file) = exclusive(&scratch.0, &bytes);
        let before = sys::lstat(&staged).unwrap();
        let dst = scratch.0.join("dst");
        assert_eq!(restore_linked(&staged, &dst, &file).unwrap(), Placement::Linked);
        let after = sys::lstat(&staged).unwrap();
        assert_eq!(sys::lstat(&dst).unwrap().ino(), before.ino(), "the destination is the store inode");
        assert_eq!((after.len(), after.mtime(), after.nlink()), (before.len(), before.mtime(), 2), "no data was written");
        assert_eq!(fs::read(&dst).unwrap(), bytes);
        let other = scratch.0.join("other");
        restore_linked(&staged, &other, &file).unwrap();
        assert_ne!(sys::lstat(&other).unwrap().ino(), before.ino(), "a store file that already has a target name is copied");
        assert!(fs::read_dir(&scratch.0).unwrap().flatten().all(|e| !e.file_name().to_string_lossy().contains(fsx::TEMP_MARKER)));
    }

    fn busy() -> io::Error {
        io::Error::from(io::ErrorKind::ResourceBusy)
    }

    #[test]
    fn a_link_refused_while_another_handle_holds_the_file_is_retried_before_copying() {
        let scratch = Scratch::new();
        let bytes = pattern(70_000);
        let (staged, file) = exclusive(&scratch.0, &bytes);
        let dst = scratch.0.join("dst");
        inject_link_faults([busy(), busy()]);
        assert_eq!(restore_linked(&staged, &dst, &file).unwrap(), Placement::Linked, "a passing sharing violation still links");
        assert_eq!(sys::lstat(&dst).unwrap().ino(), sys::lstat(&staged).unwrap().ino());
        assert!(LINK_FAULTS.with(|faults| faults.borrow().is_empty()));
    }

    #[test]
    fn a_copy_fallback_names_its_reason_and_a_later_adopt_collapses_it() {
        let scratch = Scratch::new();
        let bytes = pattern(70_000);
        let (staged, file) = exclusive(&scratch.0, &bytes);
        let dst = scratch.0.join("dst");
        inject_link_faults((0..=LINK_RETRY_MS.len()).map(|_| busy()));
        assert_eq!(restore_linked(&staged, &dst, &file).unwrap(), Placement::Copied("link:ResourceBusy".into()));
        assert_eq!(fs::read(&dst).unwrap(), bytes);
        assert_ne!(sys::lstat(&dst).unwrap().ino(), sys::lstat(&staged).unwrap().ino(), "the store file is held, so the target is a copy");
        inject_link_faults([io::Error::from(io::ErrorKind::PermissionDenied)]);
        assert_eq!(adopt(&staged, &dst), Err("link:PermissionDenied".to_string()), "a lasting refusal is reported, not retried");
        assert!(LINK_FAULTS.with(|faults| faults.borrow().is_empty()));
        verify_in_place(&dst, &file).unwrap();
        assert_eq!(adopt(&staged, &dst), Ok(true));
        assert_eq!(sys::lstat(&dst).unwrap().ino(), sys::lstat(&staged).unwrap().ino(), "the next opportunity leaves one copy");
        assert_eq!(sys::lstat(&staged).unwrap().nlink(), 2);
        assert!(fs::read_dir(&scratch.0).unwrap().flatten().all(|e| !e.file_name().to_string_lossy().contains(fsx::TEMP_MARKER)));
    }

    #[test]
    fn a_tampered_exclusive_store_file_is_rejected_before_any_link() {
        let scratch = Scratch::new();
        let bytes = pattern(70_000);
        let (staged, file) = exclusive(&scratch.0, &bytes);
        let mut changed = bytes.clone();
        changed[100] ^= 1;
        fs::write(&staged, &changed).unwrap();
        let dst = scratch.0.join("dst");
        assert!(is_unbacked(&restore_linked(&staged, &dst, &file).unwrap_err()));
        assert!(!dst.exists());
        assert_eq!(sys::lstat(&staged).unwrap().nlink(), 1, "no link was left behind");
    }

    #[test]
    fn two_concurrent_restorers_never_end_up_sharing_an_inode() {
        let bytes = pattern(200_000);
        for round in 0..20 {
            let scratch = Scratch::new();
            let (staged, file) = exclusive(&scratch.0, &bytes);
            let first = scratch.0.join("first");
            let second = scratch.0.join("second");
            std::thread::scope(|scope| {
                scope.spawn(|| restore_linked(&staged, &first, &file).unwrap());
                restore_linked(&staged, &second, &file).unwrap();
            });
            assert_eq!(fs::read(&first).unwrap(), bytes, "round {round}");
            assert_eq!(fs::read(&second).unwrap(), bytes, "round {round}");
            assert_ne!(sys::lstat(&first).unwrap().ino(), sys::lstat(&second).unwrap().ino(), "round {round}");
        }
    }

    fn pattern(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i % 253) as u8).collect()
    }

    // Exact bytes, or a miss that leaves the entry alone: never a verdict that the entry is corrupt.
    fn assert_sound(result: io::Result<Placement>, dst: &Path, bytes: &[u8]) {
        match result {
            Ok(_) => assert_eq!(fs::read(dst).unwrap(), bytes),
            Err(error) => assert!(!is_unbacked(&error), "{error}"),
        }
    }

    #[test]
    fn a_concurrent_touch_of_the_shared_inode_never_makes_the_entry_look_corrupt() {
        let scratch = Scratch::new();
        let source = scratch.0.join("source");
        let bytes = pattern(16 * 1024 * 1024);
        fs::write(&source, &bytes).unwrap();
        let (staged, file) = linked(&scratch.0, &source);
        let stop = std::sync::atomic::AtomicBool::new(false);
        std::thread::scope(|scope| {
            scope.spawn(|| {
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    fsx::touch(&source);
                }
            });
            for n in 0..4 {
                let dst = scratch.0.join(format!("dst-{n}"));
                assert_sound(restore_linked(&staged, &dst, &file), &dst, &bytes);
            }
            stop.store(true, std::sync::atomic::Ordering::Relaxed);
        });
    }

    #[cfg(unix)]
    #[test]
    fn an_adopt_replacing_the_store_file_during_a_copy_never_removes_the_entry() {
        let scratch = Scratch::new();
        let source = scratch.0.join("source");
        let bytes = pattern(16 * 1024 * 1024);
        fs::write(&source, &bytes).unwrap();
        let (staged, file) = linked(&scratch.0, &source);
        fs::remove_file(source).unwrap();
        let stop = std::sync::atomic::AtomicBool::new(false);
        std::thread::scope(|scope| {
            scope.spawn(|| {
                let mut previous: Option<PathBuf> = None;
                for n in 0..200 {
                    if stop.load(std::sync::atomic::Ordering::Relaxed) {
                        break;
                    }
                    let placed = scratch.0.join(format!("placed-{n}"));
                    fs::write(&placed, &bytes).unwrap();
                    sys::set_path_mode(&placed, file.mode.unwrap() & 0o7777).unwrap();
                    if let Some(old) = previous.take() {
                        fs::remove_file(old).unwrap();
                    }
                    let _ = adopt(&staged, &placed);
                    previous = Some(placed);
                }
            });
            for n in 0..4 {
                let dst = scratch.0.join(format!("dst-{n}"));
                assert_sound(restore_linked(&staged, &dst, &file), &dst, &bytes);
            }
            stop.store(true, std::sync::atomic::Ordering::Relaxed);
        });
    }

    #[cfg(unix)]
    #[test]
    fn an_unreadable_store_file_is_a_plain_error_that_removes_nothing() {
        let scratch = Scratch::new();
        let source = scratch.0.join("source");
        fs::write(&source, pattern(9000)).unwrap();
        let (staged, file) = linked(&scratch.0, &source);
        let mode = sys::lstat(&staged).unwrap().mode() & 0o7777;
        sys::set_path_mode(&staged, 0).unwrap();
        if File::open(&staged).is_ok() {
            // Running with privileges that ignore permissions.
            return;
        }
        let dst = scratch.0.join("dst");
        let error = restore_linked(&staged, &dst, &file).unwrap_err();
        sys::set_path_mode(&staged, mode).unwrap();
        assert!(!is_unbacked(&error) && !is_unstable(&error), "{error}");
        assert!(!dst.exists());
    }

    #[test]
    fn verifying_in_place_requires_the_exact_bytes_and_leaves_the_file_alone() {
        let scratch = Scratch::new();
        let path = scratch.0.join("artifact");
        fs::write(&path, vec![b'v'; 9000]).unwrap();
        let (_, file) = linked(&scratch.0, &path);
        verify_in_place(&path, &file).unwrap();
        assert_eq!(fs::read(&path).unwrap(), vec![b'v'; 9000]);
        fs::write(&path, vec![b'w'; 9000]).unwrap();
        assert!(is_unbacked(&verify_in_place(&path, &file).unwrap_err()));
        fs::write(&path, vec![b'v'; 8999]).unwrap();
        assert!(is_unbacked(&verify_in_place(&path, &file).unwrap_err()));
        fs::remove_file(&path).unwrap();
        assert!(is_unbacked(&verify_in_place(&path, &file).unwrap_err()));
    }

    #[test]
    fn compression_follows_the_reflink_probe_on_every_platform() {
        let scratch = Scratch::new();
        assert!(probe_allows_compression(&scratch.0, |_| false));
        assert!(!probe_allows_compression(&scratch.0, |_| true));
        assert!(!probe_allows_compression(&scratch.0.join("missing"), |_| false));
    }

    #[test]
    fn compression_support_is_enabled_only_where_implemented() {
        let scratch = Scratch::new();
        if !cfg!(any(target_os = "linux", windows)) {
            assert!(!compression_supported(&scratch.0));
        } else if !fsx::reflink_supported(&scratch.0) {
            assert!(compression_supported(&scratch.0));
        }
    }

    fn compressed(scratch: &Scratch) -> (PathBuf, StoredFile, Vec<u8>) {
        let source = scratch.0.join("source");
        let encoded = scratch.0.join("f0");
        let content: Vec<_> = (0..BLOCK_BYTES * 2 + 12345)
            .map(|i| (i % 251) as u8)
            .collect();
        fs::write(&source, &content).unwrap();
        let meta = metadata(store(&source, &encoded, true).unwrap());
        assert_eq!(meta.encoding, Encoding::Lz4BlocksV1);
        assert!(fs::metadata(&encoded).unwrap().len() < content.len() as u64 / 4);
        (encoded, meta, content)
    }

    #[cfg(unix)]
    fn producer_archive(scratch: &Scratch) -> (PathBuf, Vec<u8>, Commitment) {
        producer_archive_with_code_size(scratch, 2 * BLOCK_BYTES + 1)
    }

    #[cfg(unix)]
    fn producer_archive_with_code_size(
        scratch: &Scratch,
        code_bytes: usize,
    ) -> (PathBuf, Vec<u8>, Commitment) {
        let mut archive = b"!<arch>\n".to_vec();
        for (name, bytes) in [
            ("lib.rmeta/", vec![0x5a; 128 * 1024 + 17]),
            ("code.o/", vec![0x65; code_bytes]),
        ] {
            archive.extend_from_slice(
                format!(
                    "{name:<16}{:<12}{:<6}{:<6}{:<8}{:<10}`\n",
                    0,
                    0,
                    0,
                    "644",
                    bytes.len()
                )
                .as_bytes(),
            );
            archive.extend_from_slice(&bytes);
            if bytes.len() % 2 != 0 {
                archive.push(b'\n');
            }
        }
        let source = scratch.0.join("source");
        fs::write(&source, &archive).unwrap();
        sys::set_path_mode(&source, 0o640).unwrap();
        let (_, commitment) = capture(&source).unwrap();
        (source, archive, commitment)
    }

    #[cfg(unix)]
    #[test]
    fn producer_capture_preserves_fresh_consumed_bytes_after_a_late_header_rewrite() {
        let scratch = Scratch::new();
        let (source, archive, commitment) = producer_archive(&scratch);
        let modified = fs::metadata(&source).unwrap().modified().unwrap();
        let staged = scratch.0.join("f0");
        let (stored, proof) = store_proven_hook(
            &source,
            &staged,
            Some(&commitment),
            || {
                use std::io::{Seek, SeekFrom};
                let mut file = File::options().write(true).open(&source).unwrap();
                file.seek(SeekFrom::Start(0)).unwrap();
                file.write_all(b"?").unwrap();
                file.set_times(fs::FileTimes::new().set_modified(modified))
                    .unwrap();
            },
            None,
        )
        .unwrap();
        let meta = metadata(stored);
        let proof = proof.expect("a captured stream needs no source settling delay");
        assert!(proof.usable(&staged, &meta));
        let captured_path = proof.capture_path().to_path_buf();
        let restored = scratch.0.join("restored");
        restore(&staged, &restored, &meta).unwrap();
        assert_eq!(fs::read(&restored).unwrap(), archive);
        assert_eq!(
            sys::lstat(&restored).unwrap().mode(),
            sys::lstat(&source).unwrap().mode()
        );
        assert_ne!(fs::read(&source).unwrap(), archive);
        drop(proof);
        assert!(!captured_path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn producer_capture_io_and_incomplete_failures_keep_verified_ordinary_storage() {
        for fail_after in [0, BLOCK_BYTES as u64] {
            let scratch = Scratch::new();
            let (source, archive, commitment) = producer_archive(&scratch);
            let staged = scratch.0.join("f0");
            let (stored, proof) =
                store_proven_hook(&source, &staged, Some(&commitment), || {}, Some(fail_after))
                    .unwrap();
            assert!(proof.is_none());
            let meta = metadata(stored);
            assert_eq!(meta.encoding, Encoding::Lz4BlocksV1);
            let restored = scratch.0.join("restored");
            restore(&staged, &restored, &meta).unwrap();
            assert_eq!(fs::read(&restored).unwrap(), archive);
            let mut names: Vec<_> = fs::read_dir(&scratch.0)
                .unwrap()
                .map(|e| e.unwrap().file_name())
                .collect();
            names.sort();
            assert_eq!(
                names,
                ["f0", "restored", "source"].map(std::ffi::OsString::from)
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn producer_capture_never_survives_failed_ordinary_commitment_verification() {
        let scratch = Scratch::new();
        let (source, _, mut commitment) = producer_archive(&scratch);
        commitment.hash = "0".repeat(64);
        assert!(
            store_proven(
                &source,
                &scratch.0.join("f0"),
                true,
                Some(&commitment),
                true
            )
            .is_err()
        );
        let names: Vec<_> = fs::read_dir(&scratch.0)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, [std::ffi::OsString::from("source")]);
    }

    #[cfg(unix)]
    #[test]
    fn producer_capture_is_optional_for_raw_small_and_incompressible_outputs() {
        for (name, bytes, compress, capture_enabled) in [
            ("small", vec![7; 100], true, true),
            ("raw", vec![7; MIN_BYTES as usize], false, true),
            ("unpaired", vec![7; MIN_BYTES as usize], true, false),
        ] {
            let scratch = Scratch::new();
            let source = scratch.0.join("source");
            fs::write(&source, &bytes).unwrap();
            let (stored, proof) = store_proven(
                &source,
                &scratch.0.join("f0"),
                compress,
                None,
                capture_enabled,
            )
            .unwrap();
            assert!(proof.is_none(), "{name}");
            let meta = metadata(stored);
            restore(&scratch.0.join("f0"), &scratch.0.join("restored"), &meta).unwrap();
            assert_eq!(fs::read(scratch.0.join("restored")).unwrap(), bytes);
        }
        let scratch = Scratch::new();
        let (source, _, _) = producer_archive(&scratch);
        let mut state = 0x1234_5678_u32;
        let noise: Vec<_> = (0..MIN_BYTES)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                state as u8
            })
            .collect();
        fs::write(&source, &noise).unwrap();
        let (stored, proof) =
            store_proven(&source, &scratch.0.join("f0"), true, None, true).unwrap();
        assert!(stored.encoding.is_raw() && proof.is_none());
        assert_eq!(fs::read(scratch.0.join("f0")).unwrap(), noise);
    }

    #[test]
    fn compressed_multiblock_payload_roundtrips_without_shared_mutations() {
        let scratch = Scratch::new();
        let (encoded, meta, content) = compressed(&scratch);
        let restored = scratch.0.join("restored");
        restore(&encoded, &restored, &meta).unwrap();
        assert_eq!(fs::read(&restored).unwrap(), content);
        fs::write(&restored, b"rewritten by rustc").unwrap();
        fs::write(scratch.0.join("source"), b"source rewritten").unwrap();
        restore(&encoded, &restored, &meta).unwrap();
        assert_eq!(fs::read(&restored).unwrap(), content);
        assert_eq!(meta.stored_bytes, Some(fsx::allocated_size(&encoded)));
    }

    #[test]
    fn compression_boundaries_preserve_frames_and_roundtrip() {
        let minimum = MIN_BYTES as usize;
        for (size, expected_frames) in [
            (minimum - 1, vec![]),
            (minimum, vec![minimum]),
            (minimum + 1, vec![minimum + 1]),
            (BLOCK_BYTES - 1, vec![BLOCK_BYTES - 1]),
            (BLOCK_BYTES, vec![BLOCK_BYTES]),
            (BLOCK_BYTES + 1, vec![BLOCK_BYTES, 1]),
            (2 * BLOCK_BYTES + 17, vec![BLOCK_BYTES, BLOCK_BYTES, 17]),
        ] {
            let scratch = Scratch::new();
            let source = scratch.0.join("source");
            let staged = scratch.0.join("f0");
            let content: Vec<_> = (0..size).map(|i| (i % 251) as u8).collect();
            fs::write(&source, &content).unwrap();
            let (_, commitment) = capture(&source).unwrap();
            let meta =
                metadata(store_committed(&source, &staged, true, Some(&commitment)).unwrap());
            let bytes = fs::read(&staged).unwrap();
            if expected_frames.is_empty() {
                assert!(meta.encoding.is_raw());
                assert_eq!(bytes, content);
            } else {
                assert_eq!(meta.encoding, Encoding::Lz4BlocksV1);
                assert_eq!(meta.decoded_bytes, Some(size as u64));
                assert_eq!(meta.content_hash.as_deref(), Some(commitment.hash.as_str()));
                assert!(bytes.starts_with(MAGIC));
                let mut at = MAGIC.len();
                let mut frames = Vec::new();
                loop {
                    let decoded =
                        u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()) as usize;
                    let encoded =
                        u32::from_le_bytes(bytes[at + 4..at + 8].try_into().unwrap()) as usize;
                    at += 8;
                    if decoded == 0 {
                        assert_eq!(encoded, 0);
                        assert_eq!(at, bytes.len());
                        break;
                    }
                    assert!(encoded > 0 && encoded <= bytes.len() - at);
                    frames.push(decoded);
                    at += encoded;
                }
                assert_eq!(frames, expected_frames, "artifact size {size}");
            }
            let restored = scratch.0.join("restored");
            restore(&staged, &restored, &meta).unwrap();
            assert_eq!(fs::read(restored).unwrap(), content);
        }
    }

    #[test]
    fn compression_rejects_observed_growth_and_shrink_after_source_is_restored() {
        for size in [MIN_BYTES as usize, BLOCK_BYTES, BLOCK_BYTES + 1] {
            for changed_size in [0, size - 1, size + 1] {
                for committed in [false, true] {
                    let scratch = Scratch::new();
                    let source = scratch.0.join("source");
                    let staged = scratch.0.join("f0");
                    let content = vec![0x57; size];
                    fs::write(&source, &content).unwrap();
                    let (_, commitment) = capture(&source).unwrap();
                    let modified = fs::metadata(&source).unwrap().modified().unwrap();
                    let read_finished = std::cell::Cell::new(false);
                    let cloned = std::cell::Cell::new(false);
                    let result = store_inner_captured(
                        &source,
                        &staged,
                        true,
                        committed.then_some(&commitment),
                        StoreHooks {
                            before_read: || {
                                File::options()
                                    .write(true)
                                    .open(&source)
                                    .unwrap()
                                    .set_len(changed_size as u64)
                                    .unwrap();
                            },
                            before_clone: || cloned.set(true),
                            after_read: || {
                                // Restored identity must not hide the length change consumed by the encoder.
                                fs::write(&source, &content).unwrap();
                                File::options()
                                    .write(true)
                                    .open(&source)
                                    .unwrap()
                                    .set_times(fs::FileTimes::new().set_modified(modified))
                                    .unwrap();
                                assert!(commitment.matches(&sys::stat(&source).unwrap()));
                                read_finished.set(true);
                            },
                        },
                        CaptureOptions {
                            enabled: true,
                            fail_after: None,
                        },
                    );
                    assert!(read_finished.get());
                    assert!(
                        result.is_err(),
                        "size {size}, changed {changed_size}, committed {committed}"
                    );
                    assert!(!cloned.get() && !staged.exists());
                    assert_eq!(fs::read(&source).unwrap(), content);
                    assert_eq!(fs::read_dir(&scratch.0).unwrap().count(), 1);
                }
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn small_compressed_archive_preserves_sparse_producer_capture() {
        let scratch = Scratch::new();
        let code_bytes = 32 * 1024;
        let (source, archive, commitment) = producer_archive_with_code_size(&scratch, code_bytes);
        assert!(archive.len() < BLOCK_BYTES);
        let staged = scratch.0.join("f0");
        let (stored, proof) =
            store_proven(&source, &staged, true, Some(&commitment), true).unwrap();
        let meta = metadata(stored);
        assert_eq!(meta.encoding, Encoding::Lz4BlocksV1);
        let proof = proof.expect("small archives retain producer capture");
        assert!(proof.usable(&staged, &meta));
        let mut captured = Vec::new();
        proof.reader().unwrap().read_to_end(&mut captured).unwrap();
        let code_start = archive.len() - code_bytes;
        assert_eq!(captured.len(), archive.len());
        assert_eq!(captured[..code_start], archive[..code_start]);
        assert!(captured[code_start..].iter().all(|byte| *byte == 0));
        let restored = scratch.0.join("restored");
        restore(&staged, &restored, &meta).unwrap();
        assert_eq!(fs::read(restored).unwrap(), archive);
    }

    #[test]
    fn legacy_raw_metadata_and_directories_restore() {
        let scratch = Scratch::new();
        let meta: StoredFile =
            serde_json::from_str(r#"{"name":"artifact","stored":"f0","text":false,"bytes":123}"#)
                .unwrap();
        assert!(meta.encoding.is_raw());
        assert!(meta.stored_bytes.is_none());
        let raw = scratch.0.join("f0");
        let restored = scratch.0.join("restored");
        fs::write(&raw, b"old raw entry").unwrap();
        restore(&raw, &restored, &meta).unwrap();
        fs::write(&restored, b"changed").unwrap();
        assert_eq!(fs::read(&raw).unwrap(), b"old raw entry");
        let bundle = scratch.0.join("bundle");
        fs::create_dir(&bundle).unwrap();
        fs::write(bundle.join("member"), b"bundle bytes").unwrap();
        let stored = scratch.0.join("stored-bundle");
        let payload = store(&bundle, &stored, true).unwrap();
        assert!(payload.encoding.is_raw());
        let mut meta = metadata(payload);
        meta.dir = true;
        restore(&stored, &scratch.0.join("restored-bundle"), &meta).unwrap();
        assert_eq!(
            fs::read(scratch.0.join("restored-bundle/member")).unwrap(),
            b"bundle bytes"
        );
    }

    #[test]
    fn small_disabled_and_incompressible_files_stay_raw() {
        let scratch = Scratch::new();
        let src = scratch.0.join("source");
        fs::write(&src, vec![7; 100]).unwrap();
        assert!(
            store(&src, &scratch.0.join("small"), true)
                .unwrap()
                .encoding
                .is_raw()
        );
        fs::write(&src, vec![7; MIN_BYTES as usize]).unwrap();
        assert!(
            store(&src, &scratch.0.join("disabled"), false)
                .unwrap()
                .encoding
                .is_raw()
        );
        let mut state = 0x1234_5678_u32;
        let noise: Vec<_> = (0..MIN_BYTES)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                state as u8
            })
            .collect();
        fs::write(&src, &noise).unwrap();
        let dst = scratch.0.join("noise");
        assert!(store(&src, &dst, true).unwrap().encoding.is_raw());
        assert_eq!(fs::read(dst).unwrap(), noise);
    }

    #[cfg(unix)]
    #[test]
    fn committed_hardlink_during_compression_roundtrips_but_uncommitted_rejects_it() {
        let scratch = Scratch::new();
        let source = scratch.0.join("source");
        let content = vec![0x57; MIN_BYTES as usize * 2];
        fs::write(&source, &content).unwrap();
        sys::set_path_mode(&source, 0o751).unwrap();
        let (_, commitment) = capture(&source).unwrap();
        let encoded = scratch.0.join("encoded");
        let payload = store_inner(
            &source,
            &encoded,
            true,
            Some(&commitment),
            || {
                fs::hard_link(&source, scratch.0.join("uplift")).unwrap();
            },
            || {},
        )
        .unwrap();
        assert_eq!(payload.encoding, Encoding::Lz4BlocksV1);
        let restored = scratch.0.join("restored");
        restore(&encoded, &restored, &metadata(payload)).unwrap();
        assert_eq!(fs::read(&restored).unwrap(), content);
        assert_eq!(sys::stat(&restored).unwrap().mode() & 0o777, 0o751);
        assert!(
            store_inner(
                &source,
                &scratch.0.join("strict"),
                true,
                None,
                || {
                    fs::hard_link(&source, scratch.0.join("second-uplift")).unwrap();
                },
                || {}
            )
            .is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn commitment_rejects_rewrite_with_restored_mtime_even_when_hardlinked() {
        let scratch = Scratch::new();
        let source = scratch.0.join("source");
        fs::write(&source, vec![0x57; MIN_BYTES as usize * 2]).unwrap();
        let (_, commitment) = capture(&source).unwrap();
        let modified = fs::metadata(&source).unwrap().modified().unwrap();
        assert!(
            store_inner(
                &source,
                &scratch.0.join("encoded"),
                true,
                Some(&commitment),
                || {
                    fs::write(&source, vec![0x58; MIN_BYTES as usize * 2]).unwrap();
                    File::options()
                        .write(true)
                        .open(&source)
                        .unwrap()
                        .set_times(fs::FileTimes::new().set_modified(modified))
                        .unwrap();
                    fs::hard_link(&source, scratch.0.join("uplift")).unwrap();
                },
                || {}
            )
            .is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn commitment_rejects_permission_changes_during_compression() {
        let scratch = Scratch::new();
        let source = scratch.0.join("source");
        fs::write(&source, vec![7; MIN_BYTES as usize * 2]).unwrap();
        sys::set_path_mode(&source, 0o751).unwrap();
        let (_, commitment) = capture(&source).unwrap();
        assert!(
            store_inner(
                &source,
                &scratch.0.join("encoded"),
                true,
                Some(&commitment),
                || {
                    sys::set_path_mode(&source, 0o644).unwrap();
                },
                || {}
            )
            .is_err()
        );
    }

    #[test]
    fn committed_raw_fallback_verifies_private_bytes_after_clone() {
        let scratch = Scratch::new();
        let source = scratch.0.join("source");
        let mut state = 0x1234_5678_u32;
        let noise: Vec<_> = (0..MIN_BYTES)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                state as u8
            })
            .collect();
        fs::write(&source, &noise).unwrap();
        let (_, commitment) = capture(&source).unwrap();
        let raw = scratch.0.join("raw");
        assert!(
            store_committed(&source, &raw, true, Some(&commitment))
                .unwrap()
                .encoding
                .is_raw()
        );
        assert_eq!(fs::read(raw).unwrap(), noise);
        let modified = fs::metadata(&source).unwrap().modified().unwrap();
        assert!(
            store_inner(
                &source,
                &scratch.0.join("changed"),
                true,
                Some(&commitment),
                || {},
                || {
                    fs::write(&source, vec![9; MIN_BYTES as usize]).unwrap();
                    File::options()
                        .write(true)
                        .open(&source)
                        .unwrap()
                        .set_times(fs::FileTimes::new().set_modified(modified))
                        .unwrap();
                }
            )
            .is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn committed_raw_fallback_rejects_symlink_to_original_inode() {
        let scratch = Scratch::new();
        if fsx::reflink_supported(&scratch.0) {
            return;
        }
        let source = scratch.0.join("source");
        let alias = scratch.0.join("alias");
        let mut state = 0x1234_5678_u32;
        let noise: Vec<_> = (0..MIN_BYTES)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                state as u8
            })
            .collect();
        fs::write(&source, &noise).unwrap();
        let (original, commitment) = capture(&source).unwrap();
        let staged = scratch.0.join("staged");
        assert!(
            store_inner(
                &source,
                &staged,
                true,
                Some(&commitment),
                || {},
                || {
                    fs::hard_link(&source, &alias).unwrap();
                    sys::rename(&source, &scratch.0.join("moved")).unwrap();
                    sys::link_tool(&alias, &source).unwrap();
                }
            )
            .is_err()
        );
        let followed = sys::stat(&source).unwrap();
        assert_eq!(
            (
                original.dev(),
                original.ino(),
                original.len(),
                original.mtime()
            ),
            (
                followed.dev(),
                followed.ino(),
                followed.len(),
                followed.mtime()
            )
        );
        let generic = scratch.0.join("generic");
        assert!(store(&source, &generic, false).unwrap().encoding.is_raw());
        assert!(
            fs::symlink_metadata(&generic)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(fs::read(&generic).unwrap(), noise);
    }

    #[test]
    fn commitment_rejects_changes_before_store_and_invalid_hashes() {
        let scratch = Scratch::new();
        let source = scratch.0.join("source");
        fs::write(&source, vec![7; MIN_BYTES as usize]).unwrap();
        let (_, mut commitment) = capture(&source).unwrap();
        commitment.hash = "z".repeat(64);
        assert!(
            store_committed(&source, &scratch.0.join("invalid"), true, Some(&commitment)).is_err()
        );
        let (_, commitment) = capture(&source).unwrap();
        fs::write(&source, vec![8; MIN_BYTES as usize]).unwrap();
        assert!(
            store_committed(&source, &scratch.0.join("changed"), true, Some(&commitment)).is_err()
        );
    }

    #[test]
    fn corrupt_truncated_and_trailing_payloads_preserve_destination() {
        let scratch = Scratch::new();
        let (encoded, meta, _) = compressed(&scratch);
        let original = fs::read(&encoded).unwrap();
        let restored = scratch.0.join("restored");
        let mut corrupt = original.clone();
        corrupt[20] ^= 0x55;
        let mut oversize = original.clone();
        oversize[MAGIC.len()..MAGIC.len() + 4].copy_from_slice(&u32::MAX.to_le_bytes());
        let mut trailing = original.clone();
        trailing.push(1);
        for bad in [
            original[..original.len() - 1].to_vec(),
            original[..20].to_vec(),
            corrupt,
            oversize,
            trailing,
        ] {
            fs::write(&encoded, bad).unwrap();
            fs::write(&restored, b"existing destination").unwrap();
            assert!(restore(&encoded, &restored, &meta).is_err());
            assert_eq!(fs::read(&restored).unwrap(), b"existing destination");
            assert_eq!(
                fs::read_dir(&scratch.0).unwrap().count(),
                3,
                "failed decode cleans its temporary file"
            );
        }
    }

    #[test]
    fn metadata_hash_and_length_are_verified() {
        let scratch = Scratch::new();
        let (encoded, mut meta, _) = compressed(&scratch);
        let restored = scratch.0.join("restored");
        meta.decoded_bytes = meta.decoded_bytes.map(|n| n + 1);
        assert!(restore(&encoded, &restored, &meta).is_err());
        meta.decoded_bytes = meta.decoded_bytes.map(|n| n - 1);
        meta.content_hash = Some("0".repeat(64));
        assert!(restore(&encoded, &restored, &meta).is_err());
        meta.content_hash = None;
        assert!(restore(&encoded, &restored, &meta).is_err());
        assert!(!restored.exists());
    }

    #[test]
    fn valid_decode_replaces_directories_and_failed_decode_preserves_them() {
        let scratch = Scratch::new();
        let (encoded, meta, content) = compressed(&scratch);
        let restored = scratch.0.join("restored");
        fs::create_dir(&restored).unwrap();
        fs::write(restored.join("member"), b"original directory").unwrap();
        let mut bad_meta = meta.clone();
        bad_meta.content_hash = Some("0".repeat(64));
        assert!(restore(&encoded, &restored, &bad_meta).is_err());
        assert_eq!(
            fs::read(restored.join("member")).unwrap(),
            b"original directory"
        );
        restore(&encoded, &restored, &meta).unwrap();
        assert_eq!(fs::read(restored).unwrap(), content);
    }

    #[cfg(unix)]
    #[test]
    fn compressed_executables_keep_original_permissions() {
        let scratch = Scratch::new();
        let source = scratch.0.join("source");
        fs::write(&source, vec![0x7f; MIN_BYTES as usize * 2]).unwrap();
        sys::set_path_mode(&source, 0o751).unwrap();
        let encoded = scratch.0.join("f0");
        let meta = metadata(store(&source, &encoded, true).unwrap());
        assert_eq!(meta.encoding, Encoding::Lz4BlocksV1);
        let restored = scratch.0.join("restored");
        restore(&encoded, &restored, &meta).unwrap();
        assert_eq!(sys::stat(&restored).unwrap().mode() & 0o777, 0o751);
    }

    #[test]
    #[ignore = "requires FRUST_PAYLOAD_BENCH_SOURCES with explicit real artifact paths"]
    fn profile_artifact_storage() {
        const PAIRS: usize = 8;
        let supplied = std::env::var_os("FRUST_PAYLOAD_BENCH_SOURCES")
            .expect("set FRUST_PAYLOAD_BENCH_SOURCES to a platform-separated list of existing regular artifact files");
        let sources: Vec<_> = std::env::split_paths(&supplied).collect();
        assert!(
            !sources.is_empty(),
            "FRUST_PAYLOAD_BENCH_SOURCES must contain real artifact paths"
        );
        let mut inputs = Vec::new();
        for path in sources {
            assert!(
                !path.as_os_str().is_empty(),
                "FRUST_PAYLOAD_BENCH_SOURCES contains an empty path"
            );
            assert!(
                path.to_str().is_some(),
                "the JSON report requires UTF-8 source paths: {}",
                path.display()
            );
            assert!(
                fs::symlink_metadata(&path).is_ok_and(|meta| meta.is_file()),
                "artifact must be an existing regular file: {}",
                path.display()
            );
            inputs.push((path, "provided_artifact"));
        }
        let scratch = Scratch::new();
        let reflinks = fsx::reflink_supported(&scratch.0);
        for kind in ["compressible", "incompressible", "mixed"] {
            let path = scratch.0.join(kind);
            let mut state = 0x1234_5678_u32;
            let content: Vec<_> = (0..BLOCK_BYTES * 4 + 12345)
                .map(|index| {
                    state ^= state << 13;
                    state ^= state >> 17;
                    state ^= state << 5;
                    if kind == "compressible"
                        || (kind == "mixed" && (index / (64 * 1024)).is_multiple_of(2))
                    {
                        (index % 251) as u8
                    } else {
                        state as u8
                    }
                })
                .collect();
            fs::write(&path, content).unwrap();
            inputs.push((path, kind));
        }
        let mut rows = Vec::new();
        for (source, kind) in inputs {
            let before = sys::stat(&source).unwrap();
            let expected_hash =
                crate::cache::hashing::hash_reader(&mut File::open(&source).unwrap()).unwrap();
            let mut samples = Vec::new();
            // Round zero warms both paths; restoration and complete verification are outside each store timing.
            for round in 0..=PAIRS {
                let pair_dir = scratch.0.join("pair");
                fs::create_dir(&pair_dir).unwrap();
                let mut measurements = Vec::new();
                let order = if round.is_multiple_of(2) {
                    [false, true]
                } else {
                    [true, false]
                };
                for compress in order {
                    let destination = pair_dir.join(if compress {
                        "compression-requested"
                    } else {
                        "raw"
                    });
                    let start = (round > 0).then(std::time::Instant::now);
                    let stored = store(&source, &destination, compress);
                    let store_micros = start.map(|start| start.elapsed().as_micros());
                    let meta = metadata(
                        stored
                            .unwrap_or_else(|error| panic!("store {}: {error}", source.display())),
                    );
                    let restored = pair_dir.join("restored");
                    restore(&destination, &restored, &meta)
                        .unwrap_or_else(|error| panic!("restore {}: {error}", source.display()));
                    let restored_hash =
                        crate::cache::hashing::hash_reader(&mut File::open(&restored).unwrap())
                            .unwrap();
                    let restored_meta = sys::stat(&restored).unwrap();
                    assert_eq!(
                        restored_meta.len(),
                        before.len(),
                        "restored length: {}",
                        source.display()
                    );
                    assert_eq!(
                        restored_hash,
                        expected_hash,
                        "restored content: {}",
                        source.display()
                    );
                    assert_eq!(
                        restored_meta.mode() & 0o7777,
                        before.mode() & 0o7777,
                        "restored mode: {}",
                        source.display()
                    );
                    let after = sys::stat(&source).unwrap();
                    assert_eq!(
                        (
                            after.dev(),
                            after.ino(),
                            after.len(),
                            after.mtime(),
                            after.ctime(),
                            after.mode()
                        ),
                        (
                            before.dev(),
                            before.ino(),
                            before.len(),
                            before.mtime(),
                            before.ctime(),
                            before.mode()
                        ),
                        "input changed during profiling: {}",
                        source.display()
                    );
                    if let Some(store_micros) = store_micros {
                        measurements.push(serde_json::json!({
                            "compression_requested": compress,
                            "compression_accepted": !meta.encoding.is_raw(),
                            "encoding": meta.encoding,
                            "store_micros": store_micros,
                            "stored_file_bytes": fs::metadata(&destination).unwrap().len(),
                            "stored_allocated_bytes": fsx::allocated_size(&destination),
                            "stored_physical_bytes": fsx::physical_size(&[&destination]),
                            "source_plus_stored_physical_bytes": fsx::physical_size(&[&source, &destination]),
                            "restored_hash_and_mode_verified": true,
                        }));
                    }
                    fs::remove_file(&restored).unwrap();
                }
                fs::remove_dir_all(&pair_dir).unwrap();
                if round > 0 {
                    samples.push(serde_json::json!({ "pair": round - 1, "measurements_in_order": measurements }));
                }
            }
            rows.push(serde_json::json!({
                "source_path": source,
                "source_kind": kind,
                "source_bytes": before.len(),
                "source_allocated_bytes": before.allocated(),
                "source_mode": before.mode() & 0o7777,
                "samples": samples,
            }));
        }
        println!(
            "{}",
            serde_json::json!({
                "schema_version": 1,
                "timing_unit": "microseconds",
                "timing_scope": "store_only",
                "os": std::env::consts::OS,
                "arch": std::env::consts::ARCH,
                "debug_assertions": cfg!(debug_assertions),
                "filesystem_cache_policy": "one warmup pair; caches are not flushed",
                "pairs_per_source": PAIRS,
                "untimed_warmup_pairs_per_source": 1,
                "reflink_probe_succeeded": reflinks,
                "physical_size_method": "fsx physical_size; whole-file shared extents and inodes counted once within each root set",
                "sources": rows,
            })
        );
    }
}
