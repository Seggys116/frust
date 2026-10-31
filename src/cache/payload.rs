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
const MIN_BYTES: u64 = 64 * 1024;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Encoding {
    #[default]
    Raw,
    Lz4BlocksV1,
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

pub fn compression_supported(staged: &Path) -> bool {
    if !cfg!(target_os = "linux") {
        return false;
    }
    static DEVICES: OnceLock<Mutex<HashMap<u64, bool>>> = OnceLock::new();
    let Ok(meta) = sys::stat(staged) else { return false };
    let mut devices = DEVICES.get_or_init(Mutex::default).lock().unwrap_or_else(|e| e.into_inner());
    *devices.entry(meta.dev()).or_insert_with(|| !fsx::reflink_supported(staged))
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
    let meta = fs::symlink_metadata(src)?;
    let bytes = fsx::allocated_size(src);
    if compress && meta.is_file() && meta.len() >= MIN_BYTES {
        let tmp = Temporary(fsx::temp_sibling(dst));
        let source = File::open(src)?;
        let before = sys::fstat(&source)?;
        let mut input = BufReader::new((&source).take(before.len().saturating_add(1)));
        let output = OpenOptions::new().write(true).create_new(true).open(&tmp.0)?;
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
            let packed = lz4_flex::block::compress_into(&block[..len], &mut encoded).map_err(io::Error::other)?;
            output.write_all(&(len as u32).to_le_bytes())?;
            output.write_all(&(packed as u32).to_le_bytes())?;
            output.write_all(&encoded[..packed])?;
        }
        output.write_all(&[0; 8])?;
        output.flush()?;
        let after = sys::fstat(&source)?;
        if decoded_bytes != before.len() || before.len() != after.len() || before.mtime() != after.mtime() || before.ctime() != after.ctime() {
            return Err(invalid("artifact changed while compressing"));
        }
        let packed_bytes = output.get_ref().metadata()?.len();
        if packed_bytes.saturating_add(4096) <= decoded_bytes && packed_bytes <= decoded_bytes - decoded_bytes / 8 {
            fs::set_permissions(&tmp.0, before.permissions())?;
            drop(output);
            sys::rename(&tmp.0, dst)?;
            return Ok(StoredPayload {
                bytes,
                encoding: Encoding::Lz4BlocksV1,
                stored_bytes: Some(fsx::allocated_size(dst)),
                decoded_bytes: Some(decoded_bytes),
                content_hash: Some(hash.finalize().to_hex().to_string()),
            });
        }
    }
    fsx::clone_new(src, dst)?;
    Ok(StoredPayload {
        bytes,
        encoding: Encoding::Raw,
        stored_bytes: Some(fsx::allocated_size(dst)),
        decoded_bytes: None,
        content_hash: None,
    })
}

/// A failed decode leaves the previous destination intact; raw entries retain their original clone path.
pub fn restore(src: &Path, dst: &Path, meta: &StoredFile) -> io::Result<()> {
    if meta.encoding.is_raw() {
        return fsx::clone_replace(src, dst);
    }
    let source = File::open(src)?;
    let source_meta = source.metadata()?;
    let tmp = Temporary(fsx::temp_sibling(dst));
    let output = OpenOptions::new().write(true).create_new(true).open(&tmp.0)?;
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
    if before.len() != after.len() || before.mtime() != after.mtime() || before.ctime() != after.ctime() {
        return Err(invalid("payload changed while verifying"));
    }
    Ok(())
}

fn decode(source: &File, meta: &StoredFile, output: &mut impl Write) -> io::Result<()> {
    if meta.encoding.is_raw() || meta.dir || meta.text {
        return Err(invalid("encoded payload is not a regular artifact"));
    }
    let expected_len = meta.decoded_bytes.ok_or_else(|| invalid("missing decoded payload length"))?;
    let expected_hash = meta.content_hash.as_deref().ok_or_else(|| invalid("missing payload hash"))?;
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
        if len == 0 || len > BLOCK_BYTES || packed == 0 || packed > encoded.len() || len as u64 > expected_len.saturating_sub(decoded_bytes) {
            return Err(invalid("invalid payload block length"));
        }
        input.read_exact(&mut encoded[..packed])?;
        let actual = lz4_flex::block::decompress_into(&encoded[..packed], &mut block[..len]).map_err(io::Error::other)?;
        if actual != len {
            return Err(invalid("incorrect decoded block length"));
        }
        decoded_bytes += len as u64;
        hash.update(&block[..len]);
        output.write_all(&block[..len])?;
    }
    let mut trailing = [0];
    if decoded_bytes != expected_len || hash.finalize().to_hex().as_str() != expected_hash || input.read(&mut trailing)? != 0 {
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
        }
    }

    fn compressed(scratch: &Scratch) -> (PathBuf, StoredFile, Vec<u8>) {
        let source = scratch.0.join("source");
        let encoded = scratch.0.join("f0");
        let content: Vec<_> = (0..BLOCK_BYTES * 2 + 12345).map(|i| (i % 251) as u8).collect();
        fs::write(&source, &content).unwrap();
        let meta = metadata(store(&source, &encoded, true).unwrap());
        assert_eq!(meta.encoding, Encoding::Lz4BlocksV1);
        assert!(fs::metadata(&encoded).unwrap().len() < content.len() as u64 / 4);
        (encoded, meta, content)
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
        let meta: StoredFile = serde_json::from_str(r#"{"name":"artifact","stored":"f0","text":false,"bytes":123}"#).unwrap();
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
        assert_eq!(fs::read(scratch.0.join("restored-bundle/member")).unwrap(), b"bundle bytes");
    }

    #[test]
    fn small_disabled_and_incompressible_files_stay_raw() {
        let scratch = Scratch::new();
        let src = scratch.0.join("source");
        fs::write(&src, vec![7; 100]).unwrap();
        assert!(store(&src, &scratch.0.join("small"), true).unwrap().encoding.is_raw());
        fs::write(&src, vec![7; MIN_BYTES as usize]).unwrap();
        assert!(store(&src, &scratch.0.join("disabled"), false).unwrap().encoding.is_raw());
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
        for bad in [original[..original.len() - 1].to_vec(), original[..20].to_vec(), corrupt, oversize, trailing] {
            fs::write(&encoded, bad).unwrap();
            fs::write(&restored, b"existing destination").unwrap();
            assert!(restore(&encoded, &restored, &meta).is_err());
            assert_eq!(fs::read(&restored).unwrap(), b"existing destination");
            assert_eq!(fs::read_dir(&scratch.0).unwrap().count(), 3, "failed decode cleans its temporary file");
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
        assert_eq!(fs::read(restored.join("member")).unwrap(), b"original directory");
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
}
