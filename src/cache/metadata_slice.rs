//! Same-entry archive ranges with complete backing and output commitments.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::{
    StoredFile,
    payload::{self, Commitment, Encoding},
};
use crate::{fsx, sys};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
// Backings cannot themselves reference another artifact.
pub enum SourceEncoding {
    Raw,
    Lz4BlocksV1,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
// Offsets address decoded backing bytes.
pub struct MetadataSlice {
    pub offset: u64,
    pub source_encoding: SourceEncoding,
    pub source_decoded_bytes: u64,
    pub source_content_hash: String,
    pub mode: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestoreGroup {
    pub base_index: usize,
    pub slice_indices: Vec<usize>,
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
fn hash_valid(hash: &str) -> bool {
    hash.len() == 64
        && hash
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn safe(name: &str) -> bool {
    !name.is_empty()
        && !name.contains(['/', '\\', ':'])
        && Path::new(name).components().count() == 1
        && matches!(
            Path::new(name).components().next(),
            Some(Component::Normal(_))
        )
}
fn mode_valid(mode: u32) -> bool {
    mode & !0o107777 == 0 && mode & 0o170000 == 0o100000
}
fn stem(name: &str, suffix: &str) -> Option<String> {
    name.strip_suffix(suffix).map(str::to_owned)
}

// Validate the whole entry before callers create any output.
pub fn validate_groups(files: &[StoredFile]) -> io::Result<Vec<RestoreGroup>> {
    let mut names = HashSet::new();
    let mut backing = HashMap::<&str, usize>::new();
    let mut groups = Vec::new();
    for (i, file) in files.iter().enumerate() {
        if !safe(&file.name) || !safe(&file.stored) || !names.insert(&file.name) {
            return Err(invalid("unsafe or duplicate output name"));
        }
        if file.encoding != Encoding::InlineMetadataSliceV1 {
            if file.metadata_slice.is_some() || backing.insert(&file.stored, i).is_some() {
                return Err(invalid("invalid or duplicate backing reference"));
            }
            if file.encoding == Encoding::Lz4BlocksV1
                && (file.dir
                    || file.text
                    || file.decoded_bytes.is_none()
                    || !file.content_hash.as_deref().is_some_and(hash_valid))
            {
                return Err(invalid("invalid compressed backing metadata"));
            }
            if file.encoding == Encoding::Raw
                && (file.decoded_bytes.is_some() || file.content_hash.is_some())
            {
                return Err(invalid("inconsistent raw backing metadata"));
            }
            groups.push(RestoreGroup {
                base_index: i,
                slice_indices: Vec::new(),
            });
        }
    }
    for (i, file) in files.iter().enumerate() {
        if file.encoding != Encoding::InlineMetadataSliceV1 {
            continue;
        }
        let range = file
            .metadata_slice
            .as_ref()
            .ok_or_else(|| invalid("missing metadata range"))?;
        // Resolve only declared ordinary backing records.
        let base_index = *backing
            .get(file.stored.as_str())
            .ok_or_else(|| invalid("missing range backing"))?;
        let base = &files[base_index];
        let len = file
            .decoded_bytes
            .ok_or_else(|| invalid("missing slice length"))?;
        let source_encoding = match base.encoding {
            Encoding::Raw => SourceEncoding::Raw,
            Encoding::Lz4BlocksV1 => SourceEncoding::Lz4BlocksV1,
            _ => return Err(invalid("chained metadata range")),
        };
        if base.dir
            || base.text
            || file.dir
            || file.text
            || base_index == i
            || stem(&file.name, ".rmeta").is_none()
            || stem(&file.name, ".rmeta") != stem(&base.name, ".rlib")
            || file.stored_bytes != Some(0)
            || len == 0
            || source_encoding != range.source_encoding
            || !mode_valid(range.mode)
            || !hash_valid(&range.source_content_hash)
            || !file.content_hash.as_deref().is_some_and(hash_valid)
            || range
                .offset
                .checked_add(len)
                .is_none_or(|end| end > range.source_decoded_bytes)
        {
            return Err(invalid("invalid metadata range"));
        }
        // Encoded records already commit the complete backing.
        if source_encoding == SourceEncoding::Lz4BlocksV1
            && (base.decoded_bytes != Some(range.source_decoded_bytes)
                || base.content_hash.as_ref() != Some(&range.source_content_hash))
        {
            return Err(invalid("inconsistent compressed backing commitment"));
        }
        let group = groups
            .iter_mut()
            .find(|g| g.base_index == base_index)
            .unwrap();
        if group.slice_indices.iter().any(|&j| {
            let previous = files[j].metadata_slice.as_ref().unwrap();
            previous.source_decoded_bytes != range.source_decoded_bytes
                || previous.source_content_hash != range.source_content_hash
        }) {
            return Err(invalid("conflicting backing commitments"));
        }
        group.slice_indices.push(i);
    }
    Ok(groups)
}

struct Temporary(PathBuf);
impl Drop for Temporary {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}
fn temporary(path: &Path) -> io::Result<(Temporary, File)> {
    let path = Temporary(fsx::temp_sibling(path));
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&path.0)?;
    Ok((path, file))
}
fn stream(source: &File, base: &StoredFile, out: &mut impl Write) -> io::Result<()> {
    match base.encoding {
        Encoding::Raw => {
            io::copy(
                &mut source.take(source.metadata()?.len().saturating_add(1)),
                out,
            )?;
            Ok(())
        }
        Encoding::Lz4BlocksV1 => payload::decode(source, base, out),
        _ => Err(invalid("invalid range source encoding")),
    }
}
fn decode_staged(
    source: &File,
    base: &StoredFile,
    decoded: &mut impl Write,
) -> io::Result<(u64, String)> {
    match base.encoding {
        Encoding::Lz4BlocksV1 => {
            // Successful decode already verifies the full backing digest, length, and terminator.
            payload::decode(source, base, decoded)?;
            decoded.flush()?;
            Ok((
                base.decoded_bytes
                    .ok_or_else(|| invalid("missing backing length"))?,
                base.content_hash
                    .clone()
                    .ok_or_else(|| invalid("missing backing hash"))?,
            ))
        }
        Encoding::Raw => {
            let mut output = hashing(decoded);
            stream(source, base, &mut output)?;
            output.flush()?;
            Ok((output.bytes, output.hash.finalize().to_hex().to_string()))
        }
        _ => Err(invalid("invalid staged backing encoding")),
    }
}

struct HashWriter<W> {
    output: W,
    hash: blake3::Hasher,
    bytes: u64,
}
impl<W: Write> Write for HashWriter<W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let n = self.output.write(bytes)?;
        self.hash.update(&bytes[..n]);
        self.bytes = self
            .bytes
            .checked_add(n as u64)
            .ok_or_else(|| invalid("payload too large"))?;
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.output.flush()
    }
}
fn hashing<W>(output: W) -> HashWriter<W> {
    HashWriter {
        output,
        hash: blake3::Hasher::new(),
        bytes: 0,
    }
}

struct MemberHeader<'a> {
    name: &'a str,
    size: u64,
    end: u64,
    next: u64,
}

fn member_header(header: &[u8; 60], at: u64, total: u64) -> Option<MemberHeader<'_>> {
    if &header[58..] != b"`\n" {
        return None;
    }
    let size = std::str::from_utf8(&header[48..58])
        .ok()?
        .trim()
        .parse()
        .ok()?;
    let end = at.checked_add(60)?.checked_add(size)?;
    let next = end.checked_add(size % 2)?;
    if next > total {
        return None;
    }
    Some(MemberHeader {
        name: std::str::from_utf8(&header[..16]).ok()?.trim_end(),
        size,
        end,
        next,
    })
}

// Both parsers resolve the same supported archive names and resource bounds.
enum MemberName {
    Table(usize),
    Bsd(usize),
    Resolved(bool),
}

fn member_name(name: &str, size: u64, names: &[u8]) -> Option<MemberName> {
    if name == "//" {
        return (size <= 1024 * 1024).then_some(MemberName::Table(size as usize));
    }
    if let Some(n) = name.strip_prefix("#1/") {
        let n = n.parse::<u64>().ok().filter(|n| *n <= size && *n <= 4096)?;
        return Some(MemberName::Bsd(n as usize));
    }
    let resolved = if name.starts_with('/') && name.len() > 1 && name != "/SYM64/" {
        let index = name[1..]
            .parse::<usize>()
            .ok()
            .filter(|n| *n < names.len())?;
        if index != 0 && names[index - 1] != b'\n' {
            return None;
        }
        let len = names[index..].iter().position(|b| *b == b'\n')?;
        std::str::from_utf8(&names[index..index + len])
            .ok()?
            .trim_end_matches('/')
    } else {
        name.trim_end_matches('/')
    };
    Some(MemberName::Resolved(resolved == "lib.rmeta"))
}

fn bsd_metadata(bytes: &[u8]) -> Option<bool> {
    Some(std::str::from_utf8(bytes).ok()?.trim_end_matches('\0') == "lib.rmeta")
}

// Unknown archive layouts are a normal storage fallback.
fn member_ranges(file: &mut File, total: u64) -> io::Result<Option<Vec<(u64, u64)>>> {
    let mut magic = [0; 8];
    if total < 8 {
        return Ok(None);
    }
    file.read_exact(&mut magic)?;
    if &magic != b"!<arch>\n" {
        return Ok(None);
    }
    let mut at = 8u64;
    let mut names = Vec::new();
    let mut found = Vec::new();
    while at < total {
        if total - at < 60 {
            return Ok(None);
        }
        file.seek(SeekFrom::Start(at))?;
        let mut bytes = [0; 60];
        file.read_exact(&mut bytes)?;
        let Some(header) = member_header(&bytes, at, total) else {
            return Ok(None);
        };
        let Some(name) = member_name(header.name, header.size, &names) else {
            return Ok(None);
        };
        let mut start = at + 60;
        let metadata = match name {
            MemberName::Table(size) => {
                names.resize(size, 0);
                file.read_exact(&mut names)?;
                false
            }
            MemberName::Bsd(size) => {
                let mut bytes = vec![0; size];
                file.read_exact(&mut bytes)?;
                let Some(metadata) = bsd_metadata(&bytes) else {
                    return Ok(None);
                };
                start += size as u64;
                metadata
            }
            MemberName::Resolved(metadata) => metadata,
        };
        if metadata {
            found.push((start, header.end - start));
        }
        at = header.next;
    }
    Ok(Some(found))
}

enum Frame {
    Magic,
    Header,
    Table { size: u64 },
    Bsd { size: u64 },
}

enum SpoolState {
    Frame {
        kind: Frame,
        bytes: Vec<u8>,
        len: usize,
    },
    Body {
        left: u64,
        keep: bool,
        pad: bool,
    },
    Padding,
    Fallback,
}

// The private file has one producer; sealing retains only a checked read descriptor.
pub(crate) struct EncoderCapture {
    temp: Temporary,
    file: File,
    sealed: Option<sys::Meta>,
}
impl EncoderCapture {
    pub(crate) fn new(staged: &Path) -> io::Result<Self> {
        let (temp, file) = temporary(staged)?;
        Ok(Self {
            temp,
            file,
            sealed: None,
        })
    }
    pub(crate) fn writer(&mut self) -> &mut File {
        &mut self.file
    }
    #[cfg(test)]
    pub(crate) fn readonly_for_control(&mut self) -> io::Result<()> {
        self.file = File::open(&self.temp.0)?;
        Ok(())
    }
    pub(crate) fn seal(&mut self, len: u64) -> io::Result<()> {
        self.file.flush()?;
        let before = sys::fstat(&self.file)?;
        if before.len() != len || !payload::stable(&before, &sys::lstat(&self.temp.0)?) {
            return Err(invalid("capture changed before seal"));
        }
        let readonly = File::open(&self.temp.0)?;
        let after = sys::fstat(&readonly)?;
        if !payload::stable(&before, &after) || !payload::stable(&after, &sys::lstat(&self.temp.0)?)
        {
            return Err(invalid("capture changed during seal"));
        }
        self.file = readonly;
        self.sealed = Some(after);
        Ok(())
    }
    pub(crate) fn stable(&self) -> bool {
        self.sealed.as_ref().is_some_and(|m| {
            sys::fstat(&self.file).is_ok_and(|now| payload::stable(m, &now))
                && sys::lstat(&self.temp.0).is_ok_and(|now| payload::stable(m, &now))
        })
    }
    pub(crate) fn reader(&self) -> io::Result<File> {
        self.file.try_clone()
    }
    #[cfg(all(test, unix))]
    pub(crate) fn path(&self) -> &Path {
        &self.temp.0
    }
}

// Private holes replace only bodies the authoritative parser never searches.
pub(crate) struct ArchiveSpool<'a> {
    file: &'a mut File,
    total: u64,
    consumed: u64,
    written: u64,
    names: Vec<u8>,
    state: SpoolState,
}

impl<'a> ArchiveSpool<'a> {
    pub(crate) fn new(file: &'a mut File, total: u64) -> Self {
        Self {
            file,
            total,
            consumed: 0,
            written: 0,
            names: Vec::new(),
            state: Self::frame(Frame::Magic, 8),
        }
    }

    fn frame(kind: Frame, len: usize) -> SpoolState {
        SpoolState::Frame {
            kind,
            bytes: Vec::new(),
            len,
        }
    }

    fn body(left: u64, keep: bool, size: u64) -> SpoolState {
        if left != 0 {
            SpoolState::Body {
                left,
                keep,
                pad: !size.is_multiple_of(2),
            }
        } else if !size.is_multiple_of(2) {
            SpoolState::Padding
        } else {
            Self::frame(Frame::Header, 60)
        }
    }

    fn complete_frame(&mut self, kind: Frame, bytes: Vec<u8>) -> SpoolState {
        match kind {
            Frame::Magic => {
                if bytes == b"!<arch>\n" {
                    Self::frame(Frame::Header, 60)
                } else {
                    SpoolState::Fallback
                }
            }
            Frame::Header => {
                let header_bytes: &[u8; 60] = bytes.as_slice().try_into().unwrap();
                let Some(header) = member_header(header_bytes, self.consumed - 60, self.total)
                else {
                    return SpoolState::Fallback;
                };
                match member_name(header.name, header.size, &self.names) {
                    Some(MemberName::Table(0)) => {
                        self.names.clear();
                        Self::body(0, false, 0)
                    }
                    Some(MemberName::Table(len)) => {
                        Self::frame(Frame::Table { size: header.size }, len)
                    }
                    Some(MemberName::Bsd(0)) => Self::body(header.size, false, header.size),
                    Some(MemberName::Bsd(len)) => {
                        Self::frame(Frame::Bsd { size: header.size }, len)
                    }
                    Some(MemberName::Resolved(keep)) => Self::body(header.size, keep, header.size),
                    None => SpoolState::Fallback,
                }
            }
            Frame::Table { size } => {
                self.names = bytes;
                Self::body(0, false, size)
            }
            Frame::Bsd { size } => match bsd_metadata(&bytes) {
                Some(keep) => Self::body(size - bytes.len() as u64, keep, size),
                None => SpoolState::Fallback,
            },
        }
    }

    // A final skipped body still contributes to the logical archive length.
    pub(crate) fn finish(&mut self, decoded_len: u64) -> io::Result<bool> {
        let complete = matches!(&self.state,
            SpoolState::Frame { kind: Frame::Header, bytes, .. } if bytes.is_empty());
        if self.consumed != decoded_len || decoded_len != self.total || !complete {
            return Ok(false);
        }
        self.file.set_len(decoded_len)?;
        Ok(true)
    }
}

impl Write for ArchiveSpool<'_> {
    fn write(&mut self, mut input: &[u8]) -> io::Result<usize> {
        let len = input.len();
        while !input.is_empty() {
            let state = std::mem::replace(&mut self.state, SpoolState::Fallback);
            let (take, keep) = match &state {
                SpoolState::Frame { bytes, len, .. } => {
                    ((len - bytes.len()).min(input.len()), true)
                }
                SpoolState::Body { left, keep, .. } => {
                    ((*left).min(input.len() as u64) as usize, *keep)
                }
                SpoolState::Padding => (1, true),
                SpoolState::Fallback => (input.len(), false),
            };
            self.consumed = self
                .consumed
                .checked_add(take as u64)
                .ok_or_else(|| invalid("archive too large"))?;
            if keep {
                self.file.write_all(&input[..take])?;
                self.written += take as u64;
            } else if !matches!(&state, SpoolState::Fallback) {
                self.file.seek(SeekFrom::Start(self.consumed))?;
            }
            self.state = match state {
                SpoolState::Frame {
                    kind,
                    mut bytes,
                    len,
                } => {
                    // Resource decline must not bypass the upstream integrity checks.
                    if bytes.try_reserve(take).is_err() {
                        SpoolState::Fallback
                    } else {
                        bytes.extend_from_slice(&input[..take]);
                        if bytes.len() == len {
                            self.complete_frame(kind, bytes)
                        } else {
                            SpoolState::Frame { kind, bytes, len }
                        }
                    }
                }
                SpoolState::Body { left, keep, pad } => {
                    if left == take as u64 {
                        if pad {
                            SpoolState::Padding
                        } else {
                            Self::frame(Frame::Header, 60)
                        }
                    } else {
                        SpoolState::Body {
                            left: left - take as u64,
                            keep,
                            pad,
                        }
                    }
                }
                SpoolState::Padding => Self::frame(Frame::Header, 60),
                SpoolState::Fallback => SpoolState::Fallback,
            };
            input = &input[take..];
        }
        Ok(len)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

#[derive(Default)]
struct SpoolStats {
    written: u64,
    #[cfg(test)]
    allocated: u64,
    #[cfg(test)]
    target_hashed_bytes: u64,
    fallback: bool,
}

fn staged_spool(
    source: &File,
    base: &StoredFile,
    path: &Path,
    sparse: bool,
    stats: &mut SpoolStats,
) -> io::Result<(Temporary, File, u64, String)> {
    let before = sys::fstat(source)?;
    let check_source = || -> io::Result<()> {
        if !payload::stable(&before, &sys::fstat(source)?)
            || !payload::stable(&before, &sys::lstat(path)?)
        {
            return Err(invalid("staged backing changed during spool"));
        }
        Ok(())
    };
    let (mut scratch, mut decoded) = temporary(path)?;
    let (proof, complete) = if sparse {
        let total = if base.encoding.is_raw() {
            before.len()
        } else {
            base.decoded_bytes
                .ok_or_else(|| invalid("missing backing length"))?
        };
        let mut spool = ArchiveSpool::new(&mut decoded, total);
        // Verification sees every original byte, including discarded payloads.
        let proof = decode_staged(source, base, &mut spool)?;
        let complete = spool.finish(proof.0)?;
        stats.written = spool.written;
        (proof, complete)
    } else {
        let proof = decode_staged(source, base, &mut decoded)?;
        stats.written = proof.0;
        (proof, true)
    };
    check_source()?;
    // Retry only a verified stream, through the same descriptor into fresh scratch.
    if !complete {
        stats.fallback = true;
        let mut source_cursor = source;
        source_cursor.seek(SeekFrom::Start(0))?;
        let (fresh_scratch, mut fresh_decoded) = temporary(path)?;
        let fresh_proof = decode_staged(source, base, &mut fresh_decoded)?;
        check_source()?;
        if fresh_proof != proof {
            return Err(invalid("staged backing changed during fallback"));
        }
        stats.written = stats.written.saturating_add(fresh_proof.0);
        scratch = fresh_scratch;
        decoded = fresh_decoded;
    }
    #[cfg(test)]
    {
        stats.allocated = sys::fstat(&decoded)?.allocated();
    }
    Ok((scratch, decoded, proof.0, proof.1))
}

// Compare the entire candidate and hash the bytes actually compared.
fn equal_range(
    base: &mut File,
    target: &mut File,
    offset: u64,
    len: u64,
    expected_hash: &str,
) -> io::Result<bool> {
    base.seek(SeekFrom::Start(offset))?;
    target.seek(SeekFrom::Start(0))?;
    let mut a = [0; 64 * 1024];
    let mut b = [0; 64 * 1024];
    let mut hash = blake3::Hasher::new();
    let mut left = len;
    while left != 0 {
        let n = left.min(a.len() as u64) as usize;
        base.read_exact(&mut a[..n])?;
        target.read_exact(&mut b[..n])?;
        if a[..n] != b[..n] {
            return Ok(false);
        }
        hash.update(&b[..n]);
        left -= n as u64;
    }
    Ok(hash.finalize().to_hex().as_str() == expected_hash)
}
// Prefix scanning keeps memory bounded even for large metadata files.
fn unique_range(
    base: &mut File,
    target: &mut File,
    ranges: &[(u64, u64)],
    len: u64,
    expected_hash: &str,
) -> io::Result<Option<u64>> {
    if len == 0 {
        return Ok(None);
    }
    let n = len.min(64) as usize;
    let mut prefix = [0; 64];
    target.read_exact(&mut prefix[..n])?;
    let mut buffer = vec![0; 64 * 1024 + n];
    let mut found = None;
    for &(start, size) in ranges {
        if size < len {
            continue;
        }
        let limit = start + size - len + 1;
        let mut at = start;
        while at < limit {
            let count = (limit - at).min(64 * 1024) as usize;
            base.seek(SeekFrom::Start(at))?;
            base.read_exact(&mut buffer[..count + n - 1])?;
            for i in 0..count {
                if buffer[i..i + n] == prefix[..n]
                    && equal_range(base, target, at + i as u64, len, expected_hash)?
                {
                    if found.is_some() {
                        return Ok(None);
                    }
                    found = Some(at + i as u64);
                }
            }
            at += count as u64;
        }
    }
    Ok(found)
}

pub fn build(
    staged_base: &Path,
    base: &StoredFile,
    original: &Path,
    target: &StoredFile,
    commitment: Option<&Commitment>,
) -> io::Result<Option<StoredFile>> {
    build_inner(staged_base, base, original, target, commitment, || {})
}

// Optional producer proofs leave the public build path and its staged verification intact.
pub(crate) fn build_proven(
    staged: &Path,
    base: &StoredFile,
    original: &Path,
    target: &StoredFile,
    commitment: Option<&Commitment>,
    proof: Option<&payload::StoreProof>,
) -> io::Result<(Option<StoredFile>, bool)> {
    build_proven_hook(staged, base, original, target, commitment, proof, || {})
}
pub(crate) fn build_proven_hook(
    staged: &Path,
    base: &StoredFile,
    original: &Path,
    target: &StoredFile,
    commitment: Option<&Commitment>,
    proof: Option<&payload::StoreProof>,
    after_parse: impl FnOnce(),
) -> io::Result<(Option<StoredFile>, bool)> {
    let candidate = (|| -> io::Result<Option<StoredFile>> {
        let Some(proof) = proof.filter(|p| p.usable(staged, base)) else {
            return Ok(None);
        };
        if !cfg!(unix)
            || base.dir
            || base.text
            || target.dir
            || target.text
            || base.metadata_slice.is_some()
            || stem(&target.name, ".rmeta").is_none()
            || stem(&target.name, ".rmeta") != stem(&base.name, ".rlib")
        {
            return Ok(None);
        }
        let Some(commitment) = commitment else {
            return Ok(None);
        };
        let target_path_before = sys::lstat(original)?;
        let mut target_file = File::open(original)?;
        let before = sys::fstat(&target_file)?;
        if !payload::stable(&target_path_before, &before)
            || !commitment.valid()
            || !commitment.matches(&before)
        {
            return Err(invalid("metadata changed before captured range"));
        }
        let mut captured = proof.reader()?;
        captured.seek(SeekFrom::Start(0))?;
        let len = base
            .decoded_bytes
            .ok_or_else(|| invalid("missing captured length"))?;
        let offset = match member_ranges(&mut captured, len)? {
            Some(ranges) => unique_range(
                &mut captured,
                &mut target_file,
                &ranges,
                before.len(),
                &commitment.hash,
            )?,
            None => None,
        };
        after_parse();
        if !proof.usable(staged, base) {
            return Ok(None);
        }
        let after = sys::fstat(&target_file)?;
        if !commitment.matches(&after) || !payload::stable(&after, &sys::lstat(original)?) {
            return Err(invalid("metadata changed during captured range"));
        }
        let Some(offset) = offset else {
            return Ok(None);
        };
        let mut result = target.clone();
        result.stored = base.stored.clone();
        result.bytes = before.allocated();
        result.encoding = Encoding::InlineMetadataSliceV1;
        result.stored_bytes = Some(0);
        result.decoded_bytes = Some(before.len());
        result.content_hash = Some(commitment.hash.clone());
        result.metadata_slice = Some(MetadataSlice {
            offset,
            source_encoding: SourceEncoding::Lz4BlocksV1,
            source_decoded_bytes: len,
            source_content_hash: base
                .content_hash
                .clone()
                .ok_or_else(|| invalid("missing captured hash"))?,
            mode: before.mode(),
        });
        validate_groups(&[base.clone(), result.clone()])?;
        Ok(Some(result))
    })()
    .unwrap_or(None);
    if candidate.is_some() {
        return Ok((candidate, true));
    }
    build(staged, base, original, target, commitment).map(|v| (v, false))
}

fn build_inner(
    staged_base: &Path,
    base: &StoredFile,
    original: &Path,
    target: &StoredFile,
    commitment: Option<&Commitment>,
    before_proof: impl FnOnce(),
) -> io::Result<Option<StoredFile>> {
    build_inner_spool(
        staged_base,
        base,
        original,
        target,
        commitment,
        before_proof,
        SpoolOptions {
            sparse: true,
            stats: &mut SpoolStats::default(),
            #[cfg(test)]
            standalone_target_hash: false,
        },
    )
}

struct SpoolOptions<'a> {
    sparse: bool,
    stats: &'a mut SpoolStats,
    #[cfg(test)]
    standalone_target_hash: bool,
}

fn target_digest(target: &mut File, len: u64) -> io::Result<String> {
    target.seek(SeekFrom::Start(0))?;
    let mut output = hashing(io::sink());
    io::copy(&mut target.take(len.saturating_add(1)), &mut output)?;
    if output.bytes != len {
        return Err(invalid("metadata differs from commitment"));
    }
    Ok(output.hash.finalize().to_hex().to_string())
}

fn build_inner_spool(
    staged_base: &Path,
    base: &StoredFile,
    original: &Path,
    target: &StoredFile,
    commitment: Option<&Commitment>,
    before_proof: impl FnOnce(),
    spool: SpoolOptions<'_>,
) -> io::Result<Option<StoredFile>> {
    if !cfg!(unix)
        || base.dir
        || base.text
        || target.dir
        || target.text
        || stem(&target.name, ".rmeta").is_none()
        || stem(&target.name, ".rmeta") != stem(&base.name, ".rlib")
        || !matches!(base.encoding, Encoding::Raw | Encoding::Lz4BlocksV1)
        || base.metadata_slice.is_some()
    {
        return Ok(None);
    }
    let path_before = sys::lstat(original)?;
    let mut target_file = File::open(original)?;
    let before = sys::fstat(&target_file)?;
    if !payload::stable(&path_before, &before)
        || !before.is_file()
        || commitment.is_some_and(|c| !c.valid() || !c.matches(&before))
    {
        return Err(invalid("metadata changed before range proof"));
    }
    let standalone = commitment.is_none();
    #[cfg(test)]
    let standalone = standalone || spool.standalone_target_hash;
    let digest = if standalone {
        let digest = target_digest(&mut target_file, before.len())?;
        #[cfg(test)]
        {
            spool.stats.target_hashed_bytes += before.len();
        }
        digest
    } else {
        commitment.expect("committed target").hash.clone()
    };
    if commitment.is_some_and(|c| c.hash != digest) {
        return Err(invalid("metadata differs from commitment"));
    }
    target_file.seek(SeekFrom::Start(0))?;
    before_proof();
    let path_meta = sys::lstat(staged_base)?;
    let source = File::open(staged_base)?;
    let source_before = sys::fstat(&source)?;
    if !payload::stable(&path_meta, &source_before) || !source_before.is_file() {
        return Err(invalid("invalid staged backing"));
    }
    // Private scratch makes archive parsing independent of live build outputs.
    let (_scratch, mut decoded, source_len, source_hash) =
        staged_spool(&source, base, staged_base, spool.sparse, spool.stats)?;
    if !payload::stable(&source_before, &sys::fstat(&source)?)
        || !payload::stable(&source_before, &sys::lstat(staged_base)?)
    {
        return Err(invalid("staged backing changed during proof"));
    }
    decoded.seek(SeekFrom::Start(0))?;
    let offset = match member_ranges(&mut decoded, source_len)? {
        Some(ranges) => unique_range(
            &mut decoded,
            &mut target_file,
            &ranges,
            before.len(),
            &digest,
        )?,
        None => None,
    };
    // A declined range must still reject changed committed bytes before falling back.
    if !standalone && offset.is_none() {
        let actual = target_digest(&mut target_file, before.len())?;
        #[cfg(test)]
        {
            spool.stats.target_hashed_bytes += before.len();
        }
        if actual != digest {
            return Err(invalid("metadata differs from commitment"));
        }
    }
    // Cargo may add a hardlink; committed bytes still must match exactly.
    let after = sys::fstat(&target_file)?;
    let path_after = sys::lstat(original)?;
    let unchanged =
        commitment.map_or_else(|| payload::stable(&before, &after), |c| c.matches(&after));
    if !unchanged || !payload::stable(&after, &path_after) {
        return Err(invalid("metadata changed during range proof"));
    }
    let Some(offset) = offset else {
        return Ok(None);
    };
    let mut result = target.clone();
    result.stored = base.stored.clone();
    result.bytes = before.allocated();
    result.encoding = Encoding::InlineMetadataSliceV1;
    result.stored_bytes = Some(0);
    result.decoded_bytes = Some(before.len());
    result.content_hash = Some(digest);
    result.metadata_slice = Some(MetadataSlice {
        offset,
        source_encoding: if base.encoding.is_raw() {
            SourceEncoding::Raw
        } else {
            SourceEncoding::Lz4BlocksV1
        },
        source_decoded_bytes: source_len,
        source_content_hash: source_hash,
        mode: before.mode(),
    });
    validate_groups(&[base.clone(), result.clone()])?;
    Ok(Some(result))
}

fn checked_group(files: &[StoredFile], group: &RestoreGroup) -> io::Result<()> {
    if !validate_groups(files)?.contains(group) {
        return Err(invalid("unvalidated restore group"));
    }
    Ok(())
}

pub fn cost(files: &[StoredFile], group: &RestoreGroup) -> io::Result<u64> {
    checked_group(files, group)?;
    let base = &files[group.base_index];
    Ok(group
        .slice_indices
        .first()
        .map(|i| {
            files[*i]
                .metadata_slice
                .as_ref()
                .unwrap()
                .source_decoded_bytes
        })
        .or(base.decoded_bytes)
        .unwrap_or(base.bytes))
}

pub fn proof_signature(files: &[StoredFile], group: &RestoreGroup) -> io::Result<String> {
    checked_group(files, group)?;
    let records: Vec<_> = std::iter::once(group.base_index)
        .chain(group.slice_indices.iter().copied())
        .map(|i| &files[i])
        .collect();
    let bytes = serde_json::to_vec(&records).map_err(io::Error::other)?;
    Ok(blake3::hash(&bytes).to_hex().to_string())
}

pub fn restored_mode(entry: &Path, files: &[StoredFile], index: usize) -> io::Result<u32> {
    validate_groups(files)?;
    let file = files
        .get(index)
        .ok_or_else(|| invalid("invalid output index"))?;
    if let Some(range) = &file.metadata_slice {
        return Ok(range.mode);
    }
    Ok(sys::lstat(&entry.join(&file.stored))?.mode())
}

fn apply_mode(path: &Path, mode: u32) -> io::Result<()> {
    if !mode_valid(mode) || !cfg!(unix) {
        return Err(invalid("metadata mode is unsupported on this platform"));
    }
    sys::set_path_mode(path, mode & 0o7777)?;
    if sys::lstat(path)?.mode() != mode {
        return Err(invalid("metadata mode could not be restored"));
    }
    Ok(())
}

struct SliceOutput<W> {
    offset: u64,
    len: u64,
    expected_hash: String,
    output: HashWriter<W>,
}
struct Tee<W> {
    base_output: W,
    base_bytes: u64,
    base_hash: Option<blake3::Hasher>,
    slices: Vec<SliceOutput<W>>,
}
impl<W: Write> Write for Tee<W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let start = self.base_bytes;
        let end = start
            .checked_add(bytes.len() as u64)
            .ok_or_else(|| invalid("backing length overflow"))?;
        self.base_output.write_all(bytes)?;
        if let Some(hash) = &mut self.base_hash {
            hash.update(bytes);
        }
        self.base_bytes = end;
        for slice in &mut self.slices {
            let lo = start.max(slice.offset);
            let hi = end.min(
                slice
                    .offset
                    .checked_add(slice.len)
                    .ok_or_else(|| invalid("slice length overflow"))?,
            );
            if lo < hi {
                slice
                    .output
                    .write_all(&bytes[(lo - start) as usize..(hi - start) as usize])?;
            }
        }
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        self.base_output.flush()?;
        for slice in &mut self.slices {
            slice.output.flush()?;
        }
        Ok(())
    }
}
fn decode_group<W: Write>(
    entry: &Path,
    files: &[StoredFile],
    group: &RestoreGroup,
    base_output: W,
    slice_outputs: Vec<W>,
) -> io::Result<u32> {
    checked_group(files, group)?;
    if slice_outputs.len() != group.slice_indices.len() {
        return Err(invalid("incorrect group output count"));
    }
    let base = &files[group.base_index];
    let source_path = entry.join(&base.stored);
    let path_meta = sys::lstat(&source_path)?;
    let source = File::open(&source_path)?;
    let before = sys::fstat(&source)?;
    if !before.is_file() || !payload::stable(&path_meta, &before) {
        return Err(invalid("invalid backing file"));
    }
    let mut tee = Tee {
        base_output,
        base_bytes: 0,
        base_hash: match base.encoding {
            Encoding::Raw => Some(blake3::Hasher::new()),
            Encoding::Lz4BlocksV1 => None,
            _ => return Err(invalid("invalid group backing encoding")),
        },
        slices: Vec::new(),
    };
    for (&index, output) in group.slice_indices.iter().zip(slice_outputs) {
        let file = &files[index];
        let range = file.metadata_slice.as_ref().unwrap();
        tee.slices.push(SliceOutput {
            offset: range.offset,
            len: file.decoded_bytes.unwrap(),
            expected_hash: file.content_hash.clone().unwrap(),
            output: hashing(output),
        });
    }
    stream(&source, base, &mut tee)?;
    tee.flush()?;
    if !payload::stable(&before, &sys::fstat(&source)?)
        || !payload::stable(&before, &sys::lstat(&source_path)?)
    {
        return Err(invalid("backing changed during grouped decode"));
    }
    // Validate all outputs before the first destination rename.
    // Compressed digests are reusable only after complete stream verification.
    let base_hash = match &tee.base_hash {
        Some(hash) => hash.finalize().to_hex().to_string(),
        None => base
            .content_hash
            .clone()
            .ok_or_else(|| invalid("missing verified backing hash"))?,
    };
    for (&index, slice) in group.slice_indices.iter().zip(&tee.slices) {
        let range = files[index].metadata_slice.as_ref().unwrap();
        if tee.base_bytes != range.source_decoded_bytes
            || base_hash != range.source_content_hash
            || slice.output.bytes != slice.len
            || slice.output.hash.finalize().to_hex().as_str() != slice.expected_hash
        {
            return Err(invalid("group payload length or hash mismatch"));
        }
    }
    Ok(before.mode())
}

pub fn verify_group(entry: &Path, files: &[StoredFile], group: &RestoreGroup) -> io::Result<()> {
    checked_group(files, group)?;
    if !cfg!(unix) && !group.slice_indices.is_empty() {
        return Err(invalid("metadata mode is unsupported on this platform"));
    }
    let base = &files[group.base_index];
    if base.dir || base.text {
        if group.slice_indices.is_empty() {
            return Ok(());
        }
        return Err(invalid("invalid grouped artifact"));
    }
    decode_group(
        entry,
        files,
        group,
        io::sink(),
        group.slice_indices.iter().map(|_| io::sink()).collect(),
    )
    .map(|_| ())
}

pub fn restore_group(
    entry: &Path,
    dest: &Path,
    files: &[StoredFile],
    group: &RestoreGroup,
) -> io::Result<()> {
    restore_group_inner(entry, dest, files, group, || {})
}

fn restore_group_inner(
    entry: &Path,
    dest: &Path,
    files: &[StoredFile],
    group: &RestoreGroup,
    after_decode: impl FnOnce(),
) -> io::Result<()> {
    checked_group(files, group)?;
    let base = &files[group.base_index];
    if group.slice_indices.is_empty() {
        return payload::restore(&entry.join(&base.stored), &dest.join(&base.name), base);
    }
    let indices: Vec<_> = std::iter::once(group.base_index)
        .chain(group.slice_indices.iter().copied())
        .collect();
    let mut temps = Vec::new();
    let mut outputs = Vec::new();
    for &index in &indices {
        let (temp, output) = temporary(&dest.join(&files[index].name))?;
        temps.push(temp);
        outputs.push(output);
    }
    let base_output = outputs.remove(0);
    let base_mode = decode_group(entry, files, group, base_output, outputs)?;
    after_decode();
    // Verify platform permissions while every output is still private.
    for (temp, &index) in temps.iter().zip(&indices) {
        let mode = if index == group.base_index {
            base_mode
        } else {
            files[index].metadata_slice.as_ref().unwrap().mode
        };
        apply_mode(&temp.0, mode)?;
        if fs::symlink_metadata(dest.join(&files[index].name)).is_ok_and(|m| m.is_dir()) {
            return Err(invalid("group destination is a directory"));
        }
    }
    for (temp, &index) in temps.iter().zip(&indices) {
        sys::rename(&temp.0, &dest.join(&files[index].name))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Scratch(PathBuf);
    impl Scratch {
        fn new() -> Self {
            let path = fsx::temp_sibling(&std::env::temp_dir().join("frust-slices"));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn record(name: &str, stored: &str) -> StoredFile {
        StoredFile {
            name: name.into(),
            stored: stored.into(),
            text: false,
            bytes: 0,
            encoding: Encoding::Raw,
            stored_bytes: None,
            decoded_bytes: None,
            content_hash: None,
            metadata_slice: None,
            dir: false,
        }
    }
    fn archive_member(out: &mut Vec<u8>, name: &str, content: &[u8]) {
        out.extend_from_slice(
            format!(
                "{name:<16}{:<12}{:<6}{:<6}{:<8}{:<10}`\n",
                0,
                0,
                0,
                "100644",
                content.len()
            )
            .as_bytes(),
        );
        out.extend_from_slice(content);
        if !content.len().is_multiple_of(2) {
            out.push(b'\n');
        }
    }
    // Compare parser decisions and every searchable byte, independently of chunking.
    fn sparse_equivalent(archive: &[u8], chunks: &[usize]) -> (bool, u64) {
        let scratch = Scratch::new();
        let path = scratch.0.join("archive");
        fs::write(&path, archive).unwrap();
        let mut full = File::open(&path).unwrap();
        let ranges = member_ranges(&mut full, archive.len() as u64).unwrap();
        let (_temp, mut sparse) = temporary(&path).unwrap();
        let mut spool = ArchiveSpool::new(&mut sparse, archive.len() as u64);
        let mut output = hashing(&mut spool);
        let mut at = 0usize;
        for &chunk in chunks {
            let end = at.saturating_add(chunk).min(archive.len());
            output.write_all(&archive[at..end]).unwrap();
            at = end;
        }
        output.write_all(&archive[at..]).unwrap();
        output.flush().unwrap();
        assert_eq!(output.bytes, archive.len() as u64);
        assert_eq!(output.hash.finalize(), blake3::hash(archive));
        let complete = spool.finish(archive.len() as u64).unwrap();
        let written = spool.written;
        assert_eq!(complete, ranges.is_some());
        if let Some(ranges) = ranges {
            assert_eq!(sparse.metadata().unwrap().len(), archive.len() as u64);
            sparse.seek(SeekFrom::Start(0)).unwrap();
            assert_eq!(
                member_ranges(&mut sparse, archive.len() as u64).unwrap(),
                Some(ranges.clone())
            );
            for (offset, len) in ranges {
                let expected = &archive[offset as usize..(offset + len) as usize];
                let mut actual = vec![0; len as usize];
                sparse.seek(SeekFrom::Start(offset)).unwrap();
                sparse.read_exact(&mut actual).unwrap();
                assert_eq!(actual, expected);
            }
        }
        (complete, written)
    }

    #[test]
    fn sparse_spool_preserves_all_archive_boundaries_and_names() {
        let mut archive = b"!<arch>\n".to_vec();
        archive_member(&mut archive, "/", b"symbols");
        archive_member(&mut archive, "/SYM64/", b"symbols64");
        archive_member(&mut archive, "empty.o/", b"");
        archive_member(&mut archive, "lib.rmeta/", b"first");
        archive_member(&mut archive, "//", b"code.o/\nlib.rmeta/\n");
        archive_member(&mut archive, "/8", b"second");
        archive_member(&mut archive, "/0", b"skipped");
        archive_member(&mut archive, "//", b"lib.rmeta/\n");
        archive_member(&mut archive, "/0", b"third");
        archive_member(&mut archive, "//", b"");
        archive_member(&mut archive, "#1/12", b"lib.rmeta\0\0\0fourth");
        archive_member(&mut archive, "#1/6", b"code.oignored");
        archive_member(&mut archive, "#1/0", b"");
        archive_member(&mut archive, "lib.rmeta/", b"");
        archive_member(&mut archive, "last.o/", &[0x5a; 256]);
        let (complete, written) = sparse_equivalent(&archive, &vec![1; archive.len()]);
        assert!(complete);
        assert!(written < archive.len() as u64 - 256);
        for split in 0..=archive.len() {
            assert!(sparse_equivalent(&archive, &[split]).0);
        }
        assert!(sparse_equivalent(b"!<arch>\n", &[1; 8]).0);
    }

    #[test]
    fn sparse_spool_malformed_layouts_match_full_parser() {
        let mut valid = b"!<arch>\n".to_vec();
        archive_member(&mut valid, "lib.rmeta/", b"body");
        let mut cases = vec![Vec::new(), b"!<thin>\n".to_vec()];
        for len in [1, 7, 9, 67, valid.len() - 1] {
            cases.push(valid[..len].to_vec());
        }
        for (offset, value) in [(8, 0xff), (56, b'x'), (66, b'x')] {
            let mut bad = valid.clone();
            bad[offset] = value;
            cases.push(bad);
        }
        for (name, body) in [
            ("#1/999", &b"body"[..]),
            ("#1/4097", &b"body"[..]),
            ("#1/1", &[0xff][..]),
            ("/0", &b"body"[..]),
            ("/999999999999999", &b"body"[..]),
        ] {
            let mut bad = b"!<arch>\n".to_vec();
            archive_member(&mut bad, name, body);
            cases.push(bad);
        }
        for (table, name) in [
            (&b"lib.rmeta/\n"[..], "/1"),
            (&b"lib.rmeta/"[..], "/0"),
            (&b"\xff/\n"[..], "/0"),
            (&b""[..], "/0"),
        ] {
            let mut bad = b"!<arch>\n".to_vec();
            archive_member(&mut bad, "//", table);
            archive_member(&mut bad, name, b"body");
            cases.push(bad);
        }
        let mut huge = b"!<arch>\n".to_vec();
        archive_member(&mut huge, "//", b"");
        huge[56..66].copy_from_slice(b"9999999999");
        cases.push(huge);
        let mut odd = b"!<arch>\n".to_vec();
        archive_member(&mut odd, "code.o/", b"x");
        odd.pop();
        cases.push(odd);
        for archive in cases {
            assert!(!sparse_equivalent(&archive, &vec![1; archive.len()]).0);
        }
        let header: [u8; 60] = valid[8..68].try_into().unwrap();
        assert!(member_header(&header, u64::MAX - 30, u64::MAX).is_none());
        assert!(member_name("//", 1024 * 1024 + 1, &[]).is_none());
        assert!(member_name("#1/4097", 4097, &[]).is_none());
    }

    #[test]
    fn sparse_spool_generated_archives_match_full_parser() {
        let mut seed = 0x28f4b16eu32;
        for case in 0..80usize {
            let mut archive = b"!<arch>\n".to_vec();
            for member in 0..1 + case % 11 {
                seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
                let body = vec![(seed >> 24) as u8; (seed as usize >> 8) % 127];
                match (case + member) % 4 {
                    0 => archive_member(&mut archive, "lib.rmeta/", &body),
                    1 => archive_member(&mut archive, "code.o/", &body),
                    2 => {
                        archive_member(&mut archive, "//", b"lib.rmeta/\n");
                        archive_member(&mut archive, "/0", &body);
                    }
                    _ => {
                        let mut named = b"lib.rmeta\0".to_vec();
                        named.extend_from_slice(&body);
                        archive_member(&mut archive, "#1/10", &named);
                    }
                }
            }
            if case % 3 == 0 {
                let index = seed as usize % archive.len();
                archive[index] ^= 0x80;
            }
            sparse_equivalent(
                &archive,
                &vec![1 + case % 71; archive.len() / (1 + case % 71)],
            );
        }
    }

    #[test]
    fn sparse_spool_keeps_duplicate_and_prefixed_range_matches() {
        let scratch = Scratch::new();
        let target = scratch.0.join("target");
        fs::write(&target, b"metadata").unwrap();
        for duplicate in [false, true] {
            let mut archive = b"!<arch>\n".to_vec();
            archive_member(&mut archive, "code.o/", b"metadata");
            archive_member(&mut archive, "lib.rmeta/", b"prefix metadata suffix");
            archive_member(
                &mut archive,
                "lib.rmeta/",
                if duplicate { b"metadata" } else { b"different" },
            );
            let path = scratch.0.join("archive");
            fs::write(&path, &archive).unwrap();
            let mut results = Vec::new();
            for sparse in [false, true] {
                let source = File::open(&path).unwrap();
                let (_temp, mut decoded, len, _) = staged_spool(
                    &source,
                    &record("libdemo.rlib", "f0"),
                    &path,
                    sparse,
                    &mut SpoolStats::default(),
                )
                .unwrap();
                decoded.seek(SeekFrom::Start(0)).unwrap();
                let ranges = member_ranges(&mut decoded, len).unwrap().unwrap();
                results.push(
                    unique_range(
                        &mut decoded,
                        &mut File::open(&target).unwrap(),
                        &ranges,
                        8,
                        blake3::hash(b"metadata").to_hex().as_str(),
                    )
                    .unwrap(),
                );
            }
            assert_eq!(results[0], results[1]);
            assert_eq!(results[0].is_none(), duplicate);
        }
    }

    #[test]
    fn sparse_spool_length_and_io_errors_do_not_become_success() {
        let scratch = Scratch::new();
        let path = scratch.0.join("archive");
        fs::write(&path, b"!<arch>\n").unwrap();
        let mut readonly = File::open(&path).unwrap();
        let mut spool = ArchiveSpool::new(&mut readonly, 8);
        assert!(spool.write_all(b"!<arch>\n").is_err());
        let (_temp, mut output) = temporary(&path).unwrap();
        let mut spool = ArchiveSpool::new(&mut output, 8);
        spool.write_all(b"!<arch>\n").unwrap();
        assert!(!spool.finish(9).unwrap());
    }

    #[test]
    fn sparse_fallback_requires_complete_backing_verification() {
        for compress in [false, true] {
            for corruption in ["none", "hash", "trailing", "truncated"] {
                if !compress && corruption != "none" {
                    continue;
                }
                let scratch = Scratch::new();
                let original = scratch.0.join("original");
                let content = vec![0x57; payload::MIN_BYTES as usize * 2];
                fs::write(&original, &content).unwrap();
                let path = scratch.0.join("f0");
                let stored = payload::store(&original, &path, compress).unwrap();
                let mut base = record("libdemo.rlib", "f0");
                base.encoding = stored.encoding;
                base.stored_bytes = stored.stored_bytes;
                base.decoded_bytes = stored.decoded_bytes;
                base.content_hash = stored.content_hash;
                if compress {
                    assert_eq!(base.encoding, Encoding::Lz4BlocksV1);
                }
                match corruption {
                    "hash" => base.content_hash = Some("0".repeat(64)),
                    "trailing" => OpenOptions::new()
                        .append(true)
                        .open(&path)
                        .unwrap()
                        .write_all(b"x")
                        .unwrap(),
                    "truncated" => {
                        let file = OpenOptions::new().write(true).open(&path).unwrap();
                        file.set_len(file.metadata().unwrap().len() - 1).unwrap();
                    }
                    _ => {}
                }
                let source = File::open(&path).unwrap();
                let mut stats = SpoolStats::default();
                let result = staged_spool(&source, &base, &path, true, &mut stats);
                if corruption == "none" {
                    let (_temp, mut decoded, len, hash) = result.unwrap();
                    assert!(stats.fallback);
                    assert_eq!(len, content.len() as u64);
                    assert_eq!(hash, blake3::hash(&content).to_hex().to_string());
                    decoded.seek(SeekFrom::Start(0)).unwrap();
                    let mut actual = Vec::new();
                    decoded.read_to_end(&mut actual).unwrap();
                    assert_eq!(actual, content);
                } else {
                    assert!(result.is_err(), "{corruption}");
                    assert!(!stats.fallback, "integrity error must not retry");
                }
            }
        }
    }

    #[cfg(unix)]
    fn fixture(scratch: &Scratch, compress: bool) -> (Vec<StoredFile>, Vec<u8>, Vec<u8>) {
        let metadata: Vec<_> = (0..1024 * 1024 + 211).map(|i| (i % 251) as u8).collect();
        let mut member = vec![0x7f; 37];
        member.extend_from_slice(&metadata);
        member.extend_from_slice(&[0x39; 113]);
        let mut archive = b"!<arch>\n".to_vec();
        archive_member(&mut archive, "lib.rmeta/", &member);
        archive_member(&mut archive, "code.o/", &[0x65; 71]);
        let source = scratch.0.join("source");
        fs::write(&source, &archive).unwrap();
        let target = scratch.0.join("original");
        fs::write(&target, &metadata).unwrap();
        #[cfg(unix)]
        {
            sys::set_path_mode(&source, 0o640).unwrap();
            sys::set_path_mode(&target, 0o751).unwrap();
        }
        let stored = payload::store(&source, &scratch.0.join("f0"), compress).unwrap();
        let mut base = record("libdemo-hash.rlib", "f0");
        base.bytes = stored.bytes;
        base.encoding = stored.encoding;
        base.stored_bytes = stored.stored_bytes;
        base.decoded_bytes = stored.decoded_bytes;
        base.content_hash = stored.content_hash;
        let target_meta = record("libdemo-hash.rmeta", "f1");
        let (_, commitment) = payload::capture(&target).unwrap();
        let range = build(
            &scratch.0.join("f0"),
            &base,
            &target,
            &target_meta,
            Some(&commitment),
        )
        .unwrap()
        .unwrap();
        (vec![base, range], archive, metadata)
    }

    #[cfg(unix)]
    struct ProducerFixture {
        base: StoredFile,
        target: StoredFile,
        proof: payload::StoreProof,
        commitment: Commitment,
        archive: Vec<u8>,
        metadata: Vec<u8>,
    }

    #[cfg(unix)]
    fn producer_fixture(scratch: &Scratch) -> ProducerFixture {
        let (files, archive, metadata) = fixture(scratch, true);
        let source = scratch.0.join("source");
        let (_, backing_commitment) = payload::capture(&source).unwrap();
        let (_, commitment) = payload::capture(&scratch.0.join("original")).unwrap();
        let (stored, proof) = payload::store_proven(
            &source,
            &scratch.0.join("producer"),
            true,
            Some(&backing_commitment),
            true,
        )
        .unwrap();
        let mut base = record(&files[0].name, "producer");
        base.bytes = stored.bytes;
        base.encoding = stored.encoding;
        base.stored_bytes = stored.stored_bytes;
        base.decoded_bytes = stored.decoded_bytes;
        base.content_hash = stored.content_hash;
        let target = record(&files[1].name, "f1");
        ProducerFixture {
            base,
            target,
            proof: proof.expect("supported producer archive"),
            commitment,
            archive,
            metadata,
        }
    }

    #[cfg(unix)]
    #[test]
    fn producer_ranges_match_staged_proofs_and_restore_bytes_hashes_and_modes() {
        let scratch = Scratch::new();
        let ProducerFixture {
            base,
            target,
            proof,
            commitment,
            archive,
            metadata,
        } = producer_fixture(&scratch);
        let staged = scratch.0.join("producer");
        let original = scratch.0.join("original");
        // A later raw rewrite must not redirect parsing away from the captured encoded stream.
        fs::write(
            scratch.0.join("source"),
            b"a later build replaced the archive",
        )
        .unwrap();
        let (range, used) = build_proven(
            &staged,
            &base,
            &original,
            &target,
            Some(&commitment),
            Some(&proof),
        )
        .unwrap();
        assert!(used);
        let range = range.unwrap();
        let reference = build(&staged, &base, &original, &target, Some(&commitment))
            .unwrap()
            .unwrap();
        assert_eq!(
            serde_json::to_value(&range).unwrap(),
            serde_json::to_value(reference).unwrap()
        );
        let files = vec![base, range];
        let groups = validate_groups(&files).unwrap();
        let destination = scratch.0.join("restored");
        fs::create_dir(&destination).unwrap();
        restore_group(&scratch.0, &destination, &files, &groups[0]).unwrap();
        assert_eq!(fs::read(destination.join(&files[0].name)).unwrap(), archive);
        assert_eq!(
            fs::read(destination.join(&files[1].name)).unwrap(),
            metadata
        );
        for file in &files {
            assert_eq!(
                crate::cache::hash_file(&destination.join(&file.name)).unwrap(),
                file.content_hash.clone().unwrap()
            );
        }
        assert_eq!(
            sys::lstat(&destination.join(&files[0].name))
                .unwrap()
                .mode()
                & 0o7777,
            0o640
        );
        assert_eq!(
            sys::lstat(&destination.join(&files[1].name))
                .unwrap()
                .mode()
                & 0o7777,
            0o751
        );
    }

    #[cfg(unix)]
    #[test]
    fn producer_ranges_recheck_private_bindings_and_keep_canonical_corruption_errors() {
        for case in [
            "staged-corrupt",
            "staged-replace",
            "capture-corrupt",
            "capture-replace",
            "capture-during-parse",
            "staged-during-parse",
            "staged-mode",
        ] {
            let scratch = Scratch::new();
            let ProducerFixture {
                base,
                target,
                proof,
                commitment,
                ..
            } = producer_fixture(&scratch);
            let staged = scratch.0.join("producer");
            let captured = proof.capture_path().to_path_buf();
            match case {
                "staged-corrupt" => fs::write(&staged, b"broken").unwrap(),
                "capture-corrupt" => fs::write(&captured, b"broken").unwrap(),
                "staged-replace" | "capture-replace" => {
                    let path = if case == "staged-replace" {
                        &staged
                    } else {
                        &captured
                    };
                    let replacement = scratch.0.join("replacement");
                    fs::copy(path, &replacement).unwrap();
                    sys::rename(&replacement, path).unwrap();
                }
                "staged-mode" => sys::set_path_mode(&staged, 0o600).unwrap(),
                _ => {}
            }
            let result = build_proven_hook(
                &staged,
                &base,
                &scratch.0.join("original"),
                &target,
                Some(&commitment),
                Some(&proof),
                || {
                    if case == "capture-during-parse" {
                        fs::write(&captured, b"broken").unwrap();
                    }
                    if case == "staged-during-parse" {
                        fs::write(&staged, b"broken").unwrap();
                    }
                },
            );
            if case == "staged-corrupt" || case == "staged-during-parse" {
                assert!(result.is_err(), "{case}");
            } else {
                let (range, used) = result.unwrap();
                assert!(!used && range.is_some(), "{case}");
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn producer_ranges_keep_missing_proof_and_invalid_target_commitment_behavior() {
        let scratch = Scratch::new();
        let ProducerFixture {
            base,
            target,
            proof,
            mut commitment,
            ..
        } = producer_fixture(&scratch);
        let staged = scratch.0.join("producer");
        let original = scratch.0.join("original");
        for (committed, captured) in [(Some(&commitment), None), (None, Some(&proof))] {
            let (range, used) =
                build_proven(&staged, &base, &original, &target, committed, captured).unwrap();
            assert!(!used && range.is_some());
        }
        for hash in ["0".repeat(64), "invalid".into()] {
            commitment.hash = hash;
            assert!(
                build_proven(
                    &staged,
                    &base,
                    &original,
                    &target,
                    Some(&commitment),
                    Some(&proof)
                )
                .is_err()
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn producer_ranges_decline_unknown_thin_and_ambiguous_archives_without_losing_storage() {
        let metadata = vec![0x37; 128 * 1024 + 17];
        for kind in ["unknown", "thin", "ambiguous"] {
            let scratch = Scratch::new();
            let mut archive = b"!<arch>\n".to_vec();
            match kind {
                "unknown" => archive = vec![b'?'; 200_000],
                "thin" => {
                    archive = b"!<thin>\n".to_vec();
                    archive.resize(200_000, b'?');
                }
                _ => {
                    archive_member(&mut archive, "lib.rmeta/", &metadata);
                    archive_member(&mut archive, "lib.rmeta/", &metadata);
                }
            }
            let source = scratch.0.join("source");
            let original = scratch.0.join("original");
            fs::write(&source, &archive).unwrap();
            fs::write(&original, &metadata).unwrap();
            let (_, commitment) = payload::capture(&original).unwrap();
            let (stored, proof) =
                payload::store_proven(&source, &scratch.0.join("f0"), true, None, true).unwrap();
            let mut base = record("libdemo.rlib", "f0");
            base.bytes = stored.bytes;
            base.encoding = stored.encoding;
            base.stored_bytes = stored.stored_bytes;
            base.decoded_bytes = stored.decoded_bytes;
            base.content_hash = stored.content_hash;
            let target = record("libdemo.rmeta", "f1");
            let (range, used) = build_proven(
                &scratch.0.join("f0"),
                &base,
                &original,
                &target,
                Some(&commitment),
                proof.as_ref(),
            )
            .unwrap();
            assert!(range.is_none() && !used, "{kind}");
            // Declining archive-specific optimization must leave ordinary backing storage usable.
            payload::restore(&scratch.0.join("f0"), &scratch.0.join("restored"), &base).unwrap();
            assert_eq!(fs::read(scratch.0.join("restored")).unwrap(), archive);
        }
    }

    #[cfg(unix)]
    #[test]
    #[ignore = "manual real-input paired release profile; set FRUST_METADATA_PROFILE_RLIB/RMETA/COMPRESS"]
    fn profile_sparse_archive_spool_against_full_reference() {
        profile_real_metadata(false);
    }

    #[cfg(unix)]
    #[test]
    #[ignore = "manual real-input paired release profile; set FRUST_METADATA_PROFILE_RLIB/RMETA/COMPRESS"]
    fn profile_committed_range_against_standalone_hash() {
        profile_real_metadata(true);
    }

    #[cfg(unix)]
    fn profile_real_metadata(compare_target_hash: bool) {
        if cfg!(debug_assertions) {
            panic!("run this profile with --release");
        }
        let rlib = PathBuf::from(
            std::env::var_os("FRUST_METADATA_PROFILE_RLIB").expect("set raw rlib path"),
        );
        let rmeta = PathBuf::from(
            std::env::var_os("FRUST_METADATA_PROFILE_RMETA").expect("set matching raw rmeta path"),
        );
        let compress = match std::env::var("FRUST_METADATA_PROFILE_COMPRESS").as_deref() {
            Ok("0") => false,
            Ok("1") => true,
            _ => panic!("set FRUST_METADATA_PROFILE_COMPRESS to 0 or 1"),
        };
        let scratch = Scratch::new();
        let path = scratch.0.join("f0");
        let (source_meta, source_commitment) = payload::capture(&rlib).unwrap();
        let (target_meta, commitment) = payload::capture(&rmeta).unwrap();
        let store_start = std::time::Instant::now();
        let stored =
            payload::store_committed(&rlib, &path, compress, Some(&source_commitment)).unwrap();
        let store_ms = store_start.elapsed().as_secs_f64() * 1000.0;
        let mut base = record("libprofile.rlib", "f0");
        base.bytes = stored.bytes;
        base.encoding = stored.encoding;
        base.stored_bytes = stored.stored_bytes;
        base.decoded_bytes = stored.decoded_bytes;
        base.content_hash = stored.content_hash;
        let target = record("libprofile.rmeta", "f1");
        let dest = scratch.0.join("restored");
        fs::create_dir(&dest).unwrap();
        let mut times = [Vec::new(), Vec::new()];
        let mut disk = [(0, 0); 2];
        let mut target_reads = [0; 2];
        let mut reference = None;
        // Alternate paired order; restoration and validation are outside the timer.
        for pair in 0..7usize {
            let order = if pair.is_multiple_of(2) {
                [0, 1]
            } else {
                [1, 0]
            };
            for variant in order {
                let mut stats = SpoolStats::default();
                let start = std::time::Instant::now();
                let slice = build_inner_spool(
                    &path,
                    &base,
                    &rmeta,
                    &target,
                    Some(&commitment),
                    || {},
                    SpoolOptions {
                        sparse: compare_target_hash || variant == 1,
                        stats: &mut stats,
                        standalone_target_hash: !compare_target_hash || variant == 0,
                    },
                )
                .unwrap()
                .expect("real inputs must support a unique metadata slice");
                let elapsed = start.elapsed().as_secs_f64() * 1000.0;
                assert!(
                    !stats.fallback,
                    "profile requires supported archive grammar"
                );
                let value = serde_json::to_value(&slice).unwrap();
                if let Some(expected) = &reference {
                    assert_eq!(&value, expected);
                } else {
                    reference = Some(value);
                }
                if pair != 0 {
                    times[variant].push(elapsed);
                }
                disk[variant] = (stats.written, stats.allocated);
                target_reads[variant] = stats.target_hashed_bytes;
                assert_eq!(stats.target_hashed_bytes, if compare_target_hash && variant == 1 { 0 } else { target_meta.len() });
                let files = vec![base.clone(), slice];
                let group = validate_groups(&files).unwrap().remove(0);
                verify_group(&scratch.0, &files, &group).unwrap();
                restore_group(&scratch.0, &dest, &files, &group).unwrap();
                for (original, name, proof, len) in [
                    (&rlib, &files[0].name, &source_commitment, source_meta.len()),
                    (&rmeta, &files[1].name, &commitment, target_meta.len()),
                ] {
                    let mut restored = File::open(dest.join(name)).unwrap();
                    assert_eq!(restored.metadata().unwrap().len(), len);
                    assert!(
                        equal_range(
                            &mut File::open(original).unwrap(),
                            &mut restored,
                            0,
                            len,
                            &proof.hash
                        )
                        .unwrap()
                    );
                    assert!(proof.matches(&sys::lstat(original).unwrap()));
                }
            }
        }
        if compare_target_hash {
            eprintln!("committed_range standalone_ms={:?} proved_ms={:?} standalone_target_hash_bytes={} proved_target_hash_bytes={}", times[0], times[1], target_reads[0], target_reads[1]);
            return;
        }
        eprintln!(
            "archive_spool rlib={} rmeta={} requested_compression={} encoding={:?} decoded_bytes={} store_ms={} full_ms={:?} sparse_ms={:?} full_written={} sparse_written={} full_allocated={} sparse_allocated={}",
            rlib.display(),
            rmeta.display(),
            compress,
            base.encoding,
            source_meta.len(),
            store_ms,
            times[0],
            times[1],
            disk[0].0,
            disk[1].0,
            disk[0].1,
            disk[1].1
        );
    }

    #[test]
    #[ignore = "manual warm-cache decode comparison using FRUST_METADATA_DECODE_ENTRY"]
    fn profile_verified_staged_decode_against_double_hash() {
        let entry = PathBuf::from(
            std::env::var_os("FRUST_METADATA_DECODE_ENTRY")
                .expect("set retained cache entry directory"),
        );
        let meta: super::super::EntryMeta =
            serde_json::from_slice(&fs::read(entry.join("meta.json")).unwrap()).unwrap();
        validate_groups(&meta.files).unwrap();
        let requested = std::env::var("FRUST_METADATA_DECODE_NAME").ok();
        let base = meta
            .files
            .iter()
            .find(|file| {
                file.encoding == Encoding::Lz4BlocksV1
                    && file.name.ends_with(".rlib")
                    && requested.as_ref().is_none_or(|name| &file.name == name)
            })
            .expect("entry must contain requested compressed rlib");
        let source_path = entry.join(&base.stored);
        let mut source = File::open(&source_path).unwrap();
        let before = sys::fstat(&source).unwrap();
        assert!(before.is_file());
        assert!(payload::stable(&before, &sys::lstat(&source_path).unwrap()));
        let scratch = Scratch::new();
        let (_old_temp, old_output) = temporary(&scratch.0.join("double-hash")).unwrap();
        let (_new_temp, new_output) = temporary(&scratch.0.join("verified-single-hash")).unwrap();
        let mut outputs = [old_output, new_output];
        let expected = (
            base.decoded_bytes.unwrap(),
            base.content_hash.clone().unwrap(),
        );
        let mut timings = [Vec::new(), Vec::new()];
        for pair in 0..9usize {
            let order = if pair.is_multiple_of(2) {
                [0, 1]
            } else {
                [1, 0]
            };
            for variant in order {
                source.seek(SeekFrom::Start(0)).unwrap();
                outputs[variant].set_len(0).unwrap();
                outputs[variant].seek(SeekFrom::Start(0)).unwrap();
                let start = std::time::Instant::now();
                let proof = if variant == 0 {
                    let mut output = hashing(&mut outputs[variant]);
                    stream(&source, base, &mut output).unwrap();
                    output.flush().unwrap();
                    (output.bytes, output.hash.finalize().to_hex().to_string())
                } else {
                    decode_staged(&source, base, &mut outputs[variant]).unwrap()
                };
                let elapsed = start.elapsed().as_secs_f64() * 1000.0;
                assert_eq!(proof, expected);
                if pair != 0 {
                    timings[variant].push(elapsed);
                }
            }
            let [old, new] = &mut outputs;
            assert!(equal_range(old, new, 0, expected.0, &expected.1).unwrap());
            assert_eq!(old.metadata().unwrap().len(), expected.0);
            assert_eq!(new.metadata().unwrap().len(), expected.0);
        }
        assert!(payload::stable(&before, &sys::fstat(&source).unwrap()));
        assert!(payload::stable(&before, &sys::lstat(&source_path).unwrap()));
        eprintln!(
            "staged_decode name={} decoded_bytes={} double_hash_ms={:?} verified_single_hash_ms={:?}",
            base.name, expected.0, timings[0], timings[1]
        );
    }

    #[test]
    fn staged_raw_decode_computes_actual_backing_digest_and_length() {
        let scratch = Scratch::new();
        let original: Vec<_> = (0..1024 * 1024 + 137).map(|i| (i % 251) as u8).collect();
        let source_path = scratch.0.join("f0");
        fs::write(&source_path, &original).unwrap();
        let source = File::open(&source_path).unwrap();
        let (_temp, mut decoded) = temporary(&source_path).unwrap();
        let actual = decode_staged(&source, &record("libdemo.rlib", "f0"), &mut decoded).unwrap();
        assert_eq!(
            actual,
            (
                original.len() as u64,
                blake3::hash(&original).to_hex().to_string()
            )
        );
        decoded.seek(SeekFrom::Start(0)).unwrap();
        let mut bytes = Vec::new();
        decoded.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, original);
    }

    #[cfg(unix)]
    #[test]
    fn staged_compressed_decode_reuses_only_fully_verified_commitments() {
        let scratch = Scratch::new();
        let (files, archive, _) = fixture(&scratch, true);
        let source_path = scratch.0.join("f0");
        let source = File::open(&source_path).unwrap();
        let (_temp, mut decoded) = temporary(&source_path).unwrap();
        let actual = decode_staged(&source, &files[0], &mut decoded).unwrap();
        assert_eq!(
            actual,
            (
                archive.len() as u64,
                blake3::hash(&archive).to_hex().to_string()
            )
        );
        decoded.seek(SeekFrom::Start(0)).unwrap();
        let mut bytes = Vec::new();
        decoded.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, archive);
    }

    #[cfg(unix)]
    #[test]
    fn corrupt_compressed_staging_aborts_before_range_publication() {
        for corruption in [
            "hash",
            "short-length",
            "long-length",
            "trailing",
            "truncated",
        ] {
            let scratch = Scratch::new();
            let (mut files, _, _) = fixture(&scratch, true);
            match corruption {
                "hash" => files[0].content_hash = Some("0".repeat(64)),
                "short-length" => *files[0].decoded_bytes.as_mut().unwrap() -= 1,
                "long-length" => *files[0].decoded_bytes.as_mut().unwrap() += 1,
                "trailing" => OpenOptions::new()
                    .append(true)
                    .open(scratch.0.join("f0"))
                    .unwrap()
                    .write_all(b"unexpected trailing bytes")
                    .unwrap(),
                "truncated" => {
                    let path = scratch.0.join("f0");
                    let len = fs::metadata(&path).unwrap().len();
                    OpenOptions::new()
                        .write(true)
                        .open(path)
                        .unwrap()
                        .set_len(len - 1)
                        .unwrap();
                }
                _ => unreachable!(),
            }
            let (_, commitment) = payload::capture(&scratch.0.join("original")).unwrap();
            assert!(
                build(
                    &scratch.0.join("f0"),
                    &files[0],
                    &scratch.0.join("original"),
                    &record("libdemo-hash.rmeta", "f1"),
                    Some(&commitment)
                )
                .is_err(),
                "{corruption}"
            );
            assert_eq!(
                fs::read_dir(&scratch.0).unwrap().count(),
                3,
                "failed proof leaves no scratch payload"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn raw_and_unaligned_multiblock_groups_restore_exact_bytes_and_modes() {
        for compress in [false, true] {
            let scratch = Scratch::new();
            let (files, archive, metadata) = fixture(&scratch, compress);
            assert_eq!(files[1].metadata_slice.as_ref().unwrap().offset, 105);
            assert_eq!(files[1].stored_bytes, Some(0));
            assert_eq!(fs::read_dir(&scratch.0).unwrap().count(), 3);
            let groups = validate_groups(&files).unwrap();
            assert_eq!(
                groups,
                vec![RestoreGroup {
                    base_index: 0,
                    slice_indices: vec![1]
                }]
            );
            assert_eq!(cost(&files, &groups[0]).unwrap(), archive.len() as u64);
            assert_eq!(
                files
                    .iter()
                    .map(|f| f.stored_bytes.unwrap_or(f.bytes))
                    .sum::<u64>(),
                fsx::allocated_size(&scratch.0.join("f0"))
            );
            verify_group(&scratch.0, &files, &groups[0]).unwrap();
            let dest = scratch.0.join("dest");
            fs::create_dir(&dest).unwrap();
            restore_group(&scratch.0, &dest, &files, &groups[0]).unwrap();
            assert_eq!(fs::read(dest.join(&files[0].name)).unwrap(), archive);
            assert_eq!(fs::read(dest.join(&files[1].name)).unwrap(), metadata);
            assert_eq!(
                sys::stat(&dest.join(&files[0].name)).unwrap().mode() & 0o7777,
                0o640
            );
            assert_eq!(
                sys::stat(&dest.join(&files[1].name)).unwrap().mode() & 0o7777,
                0o751
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn backing_replacement_after_decode_preserves_opened_source_mode() {
        for replace in [false, true] {
            let scratch = Scratch::new();
            let (files, archive, _) = fixture(&scratch, true);
            let group = validate_groups(&files).unwrap().remove(0);
            let dest = scratch.0.join("dest");
            fs::create_dir(&dest).unwrap();
            restore_group_inner(&scratch.0, &dest, &files, &group, || {
                let backing = scratch.0.join("f0");
                if replace {
                    let replacement = scratch.0.join("replacement");
                    fs::write(&replacement, b"new backing").unwrap();
                    sys::set_path_mode(&replacement, 0o777).unwrap();
                    sys::rename(&replacement, &backing).unwrap();
                } else {
                    sys::set_path_mode(&backing, 0o777).unwrap();
                }
            })
            .unwrap();
            let restored = dest.join(&files[0].name);
            assert_eq!(fs::read(&restored).unwrap(), archive);
            assert_eq!(sys::stat(&restored).unwrap().mode() & 0o7777, 0o640);
            assert_eq!(
                sys::stat(&dest.join(&files[1].name)).unwrap().mode() & 0o7777,
                0o751
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn corrupted_backing_or_slice_preserves_both_previous_outputs() {
        for compress in [false, true] {
            for corrupt_base in [false, true] {
                let scratch = Scratch::new();
                let (mut files, _, _) = fixture(&scratch, compress);
                let dest = scratch.0.join("dest");
                fs::create_dir(&dest).unwrap();
                for file in &files {
                    fs::write(dest.join(&file.name), b"previous output").unwrap();
                }
                if corrupt_base {
                    let mut data = fs::read(scratch.0.join("f0")).unwrap();
                    data[30] ^= 0x41;
                    fs::write(scratch.0.join("f0"), data).unwrap();
                } else {
                    files[1].content_hash = Some("0".repeat(64));
                }
                let groups = validate_groups(&files).unwrap();
                assert!(restore_group(&scratch.0, &dest, &files, &groups[0]).is_err());
                for file in &files {
                    assert_eq!(fs::read(dest.join(&file.name)).unwrap(), b"previous output");
                }
                assert_eq!(fs::read_dir(&dest).unwrap().count(), 2);
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn compressed_group_commitments_and_framing_must_verify_before_publication() {
        for corruption in ["hash", "length", "trailing", "truncated"] {
            let scratch = Scratch::new();
            let (mut files, _, _) = fixture(&scratch, true);
            match corruption {
                "hash" => {
                    files[0].content_hash = Some("0".repeat(64));
                    files[1]
                        .metadata_slice
                        .as_mut()
                        .unwrap()
                        .source_content_hash = "0".repeat(64);
                }
                "length" => {
                    *files[0].decoded_bytes.as_mut().unwrap() += 1;
                    files[1]
                        .metadata_slice
                        .as_mut()
                        .unwrap()
                        .source_decoded_bytes += 1;
                }
                "trailing" => OpenOptions::new()
                    .append(true)
                    .open(scratch.0.join("f0"))
                    .unwrap()
                    .write_all(b"unexpected trailing bytes")
                    .unwrap(),
                "truncated" => {
                    let path = scratch.0.join("f0");
                    let len = fs::metadata(&path).unwrap().len();
                    OpenOptions::new()
                        .write(true)
                        .open(path)
                        .unwrap()
                        .set_len(len - 1)
                        .unwrap();
                }
                _ => unreachable!(),
            }
            let group = validate_groups(&files).unwrap().remove(0);
            let dest = scratch.0.join("dest");
            fs::create_dir(&dest).unwrap();
            for file in &files {
                fs::write(dest.join(&file.name), b"previous output").unwrap();
            }
            assert!(
                verify_group(&scratch.0, &files, &group).is_err(),
                "{corruption}"
            );
            assert!(
                restore_group(&scratch.0, &dest, &files, &group).is_err(),
                "{corruption}"
            );
            for file in &files {
                assert_eq!(
                    fs::read(dest.join(&file.name)).unwrap(),
                    b"previous output",
                    "{corruption}"
                );
            }
            assert_eq!(fs::read_dir(&dest).unwrap().count(), 2);
        }
    }

    #[cfg(unix)]
    #[test]
    fn malformed_references_fail_before_any_destination_write() {
        let scratch = Scratch::new();
        let (files, _, _) = fixture(&scratch, false);
        for case in 0..11 {
            let mut changed = files.clone();
            match case {
                0 => changed[1].stored = "missing".into(),
                1 => changed[1].name = "other.rmeta".into(),
                2 => changed[1].metadata_slice.as_mut().unwrap().offset = u64::MAX,
                3 => changed[1].metadata_slice.as_mut().unwrap().mode = 0o120777,
                4 => {
                    changed[1]
                        .metadata_slice
                        .as_mut()
                        .unwrap()
                        .source_content_hash = "bad".into()
                }
                5 => changed[1].stored_bytes = Some(4096),
                6 => changed[0].metadata_slice = changed[1].metadata_slice.clone(),
                7 => changed[0].name = "../libdemo-hash.rlib".into(),
                8 => changed[0].encoding = Encoding::InlineMetadataSliceV1,
                9 => {
                    let mut extra = changed[0].clone();
                    extra.name = "duplicate.rlib".into();
                    changed.push(extra);
                }
                _ => {
                    changed[1].metadata_slice.as_mut().unwrap().source_encoding =
                        SourceEncoding::Lz4BlocksV1
                }
            }
            assert!(validate_groups(&changed).is_err(), "case {case}");
            let dest = scratch.0.join("never-created");
            assert!(
                restore_group(
                    &scratch.0,
                    &dest,
                    &changed,
                    &RestoreGroup {
                        base_index: 0,
                        slice_indices: vec![1]
                    }
                )
                .is_err()
            );
            assert!(!dest.exists());
        }
    }

    #[cfg(unix)]
    #[test]
    fn committed_range_declines_still_verify_the_complete_target() {
        let scratch = Scratch::new();
        let original = scratch.0.join("original");
        let target = record("libdemo.rmeta", "f1");
        let metadata = vec![0x37; 128 * 1024 + 17];
        for case in 0..9 {
            let bytes = if case == 8 { &[][..] } else { metadata.as_slice() };
            fs::write(&original, bytes).unwrap();
            let (_, commitment) = payload::capture(&original).unwrap();
            let mut archive = b"!<arch>\n".to_vec();
            match case {
                0 => archive = b"unknown archive".to_vec(),
                1 => archive = b"!<thin>\nexternal".to_vec(),
                2 => {}
                3 => archive_member(&mut archive, "code.o/", bytes),
                4 => archive_member(&mut archive, "lib.rmeta/", b"short"),
                5 | 6 => {
                    let mut different = bytes.to_vec();
                    let index = if case == 5 { 0 } else { different.len() - 1 };
                    different[index] ^= 1;
                    archive_member(&mut archive, "lib.rmeta/", &different);
                }
                7 => {
                    archive_member(&mut archive, "lib.rmeta/", bytes);
                    archive_member(&mut archive, "lib.rmeta/", bytes);
                }
                8 => archive_member(&mut archive, "lib.rmeta/", bytes),
                _ => unreachable!(),
            }
            for compress in [false, true] {
                let staged = scratch.0.join(format!("f0-{case}-{compress}"));
                let raw = scratch.0.join("raw");
                fs::write(&raw, &archive).unwrap();
                let stored = payload::store(&raw, &staged, compress).unwrap();
                let mut base = record("libdemo.rlib", "f0");
                base.encoding = stored.encoding;
                base.stored_bytes = stored.stored_bytes;
                base.decoded_bytes = stored.decoded_bytes;
                base.content_hash = stored.content_hash;
                for forged in [false, true] {
                    let mut proof = commitment.clone();
                    if forged { proof.hash = "0".repeat(64); }
                    let mut stats = SpoolStats::default();
                    let result = build_inner_spool(&staged, &base, &original, &target, Some(&proof), || {}, SpoolOptions {
                        sparse: true,
                        stats: &mut stats,
                        standalone_target_hash: false,
                    });
                    assert_eq!(stats.target_hashed_bytes, bytes.len() as u64, "case {case}");
                    if forged {
                        assert!(result.is_err(), "case {case}");
                    } else {
                        assert!(result.unwrap().is_none(), "case {case}");
                    }
                }
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn committed_range_success_checks_hash_and_allows_only_hardlink_drift() {
        for compress in [false, true] {
            let scratch = Scratch::new();
            let (files, archive, metadata) = fixture(&scratch, compress);
            let original = scratch.0.join("original");
            let (_, commitment) = payload::capture(&original).unwrap();
            let target = record("libdemo-hash.rmeta", "f1");
            let mut forged = commitment.clone();
            forged.hash = "0".repeat(64);
            assert!(build(&scratch.0.join("f0"), &files[0], &original, &target, Some(&forged)).is_err());
            let slice = build_inner(&scratch.0.join("f0"), &files[0], &original, &target, Some(&commitment), || {
                fs::hard_link(&original, scratch.0.join("uplift")).unwrap();
            }).unwrap().unwrap();
            assert_eq!(serde_json::to_value(&slice).unwrap(), serde_json::to_value(&files[1]).unwrap());
            let group_files = vec![files[0].clone(), slice];
            let group = validate_groups(&group_files).unwrap().remove(0);
            let dest = scratch.0.join("restored");
            fs::create_dir(&dest).unwrap();
            restore_group(&scratch.0, &dest, &group_files, &group).unwrap();
            assert_eq!(fs::read(dest.join(&files[0].name)).unwrap(), archive);
            assert_eq!(fs::read(dest.join(&target.name)).unwrap(), metadata);
            let modified = fs::metadata(&original).unwrap().modified().unwrap();
            let result = build_inner(&scratch.0.join("f0"), &files[0], &original, &target, Some(&commitment), || {
                let mut changed = metadata.clone();
                *changed.last_mut().unwrap() ^= 1;
                fs::write(&original, changed).unwrap();
                File::options().write(true).open(&original).unwrap()
                    .set_times(fs::FileTimes::new().set_modified(modified)).unwrap();
            });
            assert!(result.is_err());
        }
    }

    #[cfg(unix)]
    #[test]
    fn committed_target_replacement_and_rewrite_are_rejected() {
        let scratch = Scratch::new();
        let (files, _, metadata) = fixture(&scratch, false);
        let target = scratch.0.join("original");
        let (_, commitment) = payload::capture(&target).unwrap();
        let replacement = scratch.0.join("replacement");
        fs::write(&replacement, &metadata).unwrap();
        sys::set_path_mode(&replacement, 0o751).unwrap();
        sys::rename(&replacement, &target).unwrap();
        assert!(
            build(
                &scratch.0.join("f0"),
                &files[0],
                &target,
                &record("libdemo-hash.rmeta", "f1"),
                Some(&commitment)
            )
            .is_err()
        );
        let (_, commitment) = payload::capture(&target).unwrap();
        fs::write(&target, vec![0x37; metadata.len()]).unwrap();
        assert!(
            build(
                &scratch.0.join("f0"),
                &files[0],
                &target,
                &record("libdemo-hash.rmeta", "f1"),
                Some(&commitment)
            )
            .is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn thin_unknown_and_nonmatching_archives_fall_back() {
        let scratch = Scratch::new();
        let target = scratch.0.join("original");
        fs::write(&target, b"metadata bytes").unwrap();
        let base = record("libdemo.rlib", "f0");
        let output = record("libdemo.rmeta", "f1");
        for archive in [
            b"!<thin>\nexternal".as_slice(),
            b"unknown archive".as_slice(),
            b"!<arch>\n".as_slice(),
        ] {
            fs::write(scratch.0.join("f0"), archive).unwrap();
            assert!(
                build(&scratch.0.join("f0"), &base, &target, &output, None)
                    .unwrap()
                    .is_none()
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn replacement_during_range_proof_is_rejected() {
        let scratch = Scratch::new();
        let (files, _, metadata) = fixture(&scratch, false);
        let original = scratch.0.join("original");
        let (_, commitment) = payload::capture(&original).unwrap();
        let result = build_inner(
            &scratch.0.join("f0"),
            &files[0],
            &original,
            &record("libdemo-hash.rmeta", "f1"),
            Some(&commitment),
            || {
                let replacement = scratch.0.join("replacement");
                fs::write(&replacement, &metadata).unwrap();
                sys::set_path_mode(&replacement, 0o751).unwrap();
                sys::rename(&replacement, &original).unwrap();
            },
        );
        assert!(result.is_err());
    }

    #[test]
    fn gnu_and_bsd_member_names_are_checked() {
        let scratch = Scratch::new();
        for bsd in [false, true] {
            let mut archive = b"!<arch>\n".to_vec();
            if bsd {
                archive_member(&mut archive, "#1/12", b"lib.rmeta\0\0\0payload");
            } else {
                archive_member(&mut archive, "//", b"lib.rmeta/\n");
                archive_member(&mut archive, "/0", b"payload");
            }
            let path = scratch.0.join("archive");
            fs::write(&path, &archive).unwrap();
            let mut input = File::open(path).unwrap();
            let ranges = member_ranges(&mut input, archive.len() as u64)
                .unwrap()
                .unwrap();
            assert_eq!(ranges.len(), 1);
            assert_eq!(ranges[0].1, 7);
        }
    }

    #[test]
    fn legacy_records_default_and_old_encoding_enum_declines_ranges() {
        #[derive(Deserialize)]
        #[serde(rename_all = "snake_case")]
        enum OldEncoding {
            Raw,
            Lz4BlocksV1,
        }
        let old: StoredFile = serde_json::from_str(
            r#"{"name":"libdemo.rlib","stored":"f0","text":false,"bytes":32}"#,
        )
        .unwrap();
        assert!(old.metadata_slice.is_none());
        assert_eq!(validate_groups(&[old]).unwrap()[0].slice_indices.len(), 0);
        let encoded = serde_json::to_string(&Encoding::InlineMetadataSliceV1).unwrap();
        assert!(serde_json::from_str::<OldEncoding>(&encoded).is_err());
    }
}
