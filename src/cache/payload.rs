//! Bounded LZ4 blocks for cache files on filesystems without copy-on-write clones.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use serde::{Deserialize, Serialize};

use super::StoredFile;
use crate::{fsx, sys};

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
}

/// Captured before Cargo can uplift an output; the full hash permits later ctime-only drift.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Commitment {
    pub(super) hash: String,
    mode: u32,
    dev: u64,
    ino: u64,
    len: u64,
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

fn hash_open(source: &File, len: u64) -> io::Result<(u64, String)> {
    let mut input = BufReader::new(source.take(len.saturating_add(1)));
    let mut buffer = vec![0; BLOCK_BYTES];
    let mut hash = blake3::Hasher::new();
    let mut bytes = 0;
    loop {
        let n = input.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hash.update(&buffer[..n]);
        bytes += n as u64;
    }
    Ok((bytes, hash.finalize().to_hex().to_string()))
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
    if !cfg!(target_os = "linux") {
        return false;
    }
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
        .or_insert_with(|| !fsx::reflink_supported(staged))
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
        let mut block = vec![0; BLOCK_BYTES];
        let mut encoded = vec![0; lz4_flex::block::get_maximum_output_size(BLOCK_BYTES)];
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
            if !capture_failed
                && let Some(spool) = spool.as_mut()
            {
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
        },
        None,
    ))
}

/// A failed decode leaves the previous destination intact; raw entries retain their original clone path.
pub fn restore(src: &Path, dst: &Path, meta: &StoredFile) -> io::Result<()> {
    if meta.metadata_slice.is_some() || meta.encoding == Encoding::InlineMetadataSliceV1 {
        return Err(invalid("inline metadata requires grouped restore"));
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
        let mut archive = b"!<arch>\n".to_vec();
        for (name, bytes) in [
            ("lib.rmeta/", vec![0x5a; 128 * 1024 + 17]),
            ("code.o/", vec![0x65; 2 * BLOCK_BYTES + 1]),
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
