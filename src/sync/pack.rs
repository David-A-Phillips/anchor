//! The `.contextpack` container: a signed manifest followed by a compressed stream of unit
//! versions.
//!
//! # File layout (format 1.0)
//!
//! All integers are little-endian. There is no padding and nothing after the payload.
//!
//! | Offset   | Size | Field |
//! |----------|------|-------|
//! | 0        | 8    | magic, the ASCII bytes `ANCHORPK` |
//! | 8        | 2    | format major version (`u16`), currently 1 |
//! | 10       | 2    | format minor version (`u16`), currently 0 |
//! | 12       | 4    | manifest length `M` in bytes (`u32`) |
//! | 16       | `M`  | manifest: canonical JSON of [`PackManifest`] |
//! | 16 + `M` | 64   | Ed25519 signature |
//! | 80 + `M` | `C`  | payload: one zstd stream, `C` = `manifest.payload.compressed_size` |
//!
//! **Signature.** Made with the origin device's key over the 31 bytes
//! `anchor.contextpack.manifest.v1\0` followed by bytes `0 .. 16 + M` of the file (header and
//! manifest). The manifest names the signing key (`public_key`) and the device id derived from it
//! (`origin`).
//!
//! **Payload.** `manifest.payload.hash` is the BLAKE3 hash of the `C` compressed payload bytes, so
//! the payload can be authenticated without decompressing it. Decompressed, the payload is
//! `manifest.payload.raw_size` bytes: `manifest.entries` records, each
//!
//! | Size | Field |
//! |------|-------|
//! | 4    | record length `L` (`u32`), greater than zero |
//! | `L`  | canonical JSON of [`PackEntry`]: `{"meta": UnitMeta, "unit": Unit}`, `unit` absent for a tombstone |
//!
//! Records are ordered by namespace and unit key by the exporter, but readers do not rely on it.
//!
//! **Versioning.** A reader rejects any major version other than its own. A minor version bump
//! only adds manifest or entry fields that older readers may ignore; unknown JSON fields are
//! skipped (they are still covered by the signature and the payload hash).
//!
//! **Snapshot or delta.** `manifest.kind` is `snapshot` when the pack holds every unit version of
//! its namespaces (tombstones included) and `delta` when it holds a subset. A namespace of a delta
//! may state the `base_root` it was computed against; `root` is always the root of the exporter's
//! Merkle tree for the namespace at export time, which is what the importer converges towards.
//!
//! # What a reader verifies
//!
//! Before yielding anything ([`PackReader::open`]): magic, major version, manifest size limit,
//! signature and signer trust ([`TrustPolicy`]), consistency of the manifest (origin matches the
//! key, counts add up, codec known, decompressed size within limits). While streaming: record and
//! total size limits (the decoder is never asked for more than the manifest declares, so a
//! decompression bomb cannot expand), each unit's content hash after canonicalisation against
//! `meta.hash`, key/source/kind agreement between meta and unit, namespace membership. At the end:
//! compressed size, payload hash, absence of trailing bytes, per-namespace and total counts.
//! [`PackReader::next_entry`] returns `Ok(None)` only when all of that held.
//!
//! Entries are yielded before the payload hash can be checked. Importers that must not act on
//! unauthenticated entries call [`PackReader::verify_payload`] first (it needs `Seek`), as
//! `Engine::import_pack` does.
//!
//! Writer and reader hold one record in memory at a time. The writer spools the compressed payload
//! to an anonymous temporary file, because the manifest that precedes it in the file is only known
//! once the payload is complete.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{self, BufReader, Read, Seek, SeekFrom, Write};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::base::{
    ContentHash, DeviceId, Error, Result, SourceDescriptor, SourceId, Unit, UnitKey, UnitKind,
    UnitMeta, UnitVersion, canonical_json, now_ms,
};

use super::identity::{DeviceIdentity, DeviceSignature, PublicKey, TrustPolicy};

/// First eight bytes of every pack.
pub const PACK_MAGIC: &[u8; 8] = b"ANCHORPK";
/// Major format version written and accepted by this build.
pub const PACK_FORMAT_MAJOR: u16 = 1;
/// Minor format version written by this build.
pub const PACK_FORMAT_MINOR: u16 = 0;
/// Conventional file extension.
pub const PACK_EXTENSION: &str = "contextpack";
/// The only payload codec of format 1.
pub const PACK_CODEC: &str = "zstd";

const SIGN_DOMAIN: &[u8] = b"anchor.contextpack.manifest.v1\0";
const HEADER_LEN: usize = 16;

/// Whether a pack is a full snapshot of its namespaces or a subset of their unit versions.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PackKind {
    /// Every unit version of every namespace in the pack.
    Snapshot,
    /// Some unit versions (changes since a point in time, or the answer to a unit request).
    Delta,
}

/// One namespace of a pack.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NamespaceManifest {
    /// The source the namespace belongs to.
    pub source: SourceDescriptor,
    /// Root of the tree the delta was computed against, when known. Never set in a snapshot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_root: Option<ContentHash>,
    /// Root of the exporter's tree for this namespace at export time.
    pub root: ContentHash,
    /// Number of live unit versions of this namespace in the payload.
    pub units: u64,
    /// Number of tombstones of this namespace in the payload.
    pub tombstones: u64,
}

/// Description of the payload.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PayloadInfo {
    /// Compression codec; `"zstd"`.
    pub codec: String,
    /// Decompressed size in bytes (records with their length prefixes).
    pub raw_size: u64,
    /// Compressed size in bytes, as stored in the file.
    pub compressed_size: u64,
    /// BLAKE3 hash of the compressed bytes.
    pub hash: ContentHash,
}

/// The signed description of a pack.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackManifest {
    /// Format major version; equals the header field.
    pub format_major: u16,
    /// Format minor version; equals the header field.
    pub format_minor: u16,
    /// Unique id of the pack (a UUID v7).
    pub pack_id: String,
    /// Creation time, milliseconds since the Unix epoch.
    pub created_ms: i64,
    /// Device that produced and signed the pack.
    pub origin: DeviceId,
    /// Key the manifest is signed with; `origin` is derived from it.
    pub public_key: PublicKey,
    /// Snapshot or delta.
    pub kind: PackKind,
    /// Namespaces, ordered by source id.
    pub namespaces: Vec<NamespaceManifest>,
    /// Total number of payload records.
    pub entries: u64,
    /// Codec, sizes and hash of the payload.
    pub payload: PayloadInfo,
}

impl PackManifest {
    /// The manifest entry of `source`, if the pack contains that namespace.
    pub fn namespace(&self, source: &SourceId) -> Option<&NamespaceManifest> {
        self.namespaces.iter().find(|ns| &ns.source.id == source)
    }

    fn check_consistency(&self) -> Result<()> {
        if self.origin != self.public_key.device_id() {
            return Err(Error::integrity(
                "pack origin does not match its public key",
            ));
        }
        if self.payload.codec != PACK_CODEC {
            return Err(Error::sync(format!(
                "unsupported pack payload codec `{}`",
                self.payload.codec
            )));
        }
        let mut total = 0u64;
        let mut previous: Option<&SourceId> = None;
        for ns in &self.namespaces {
            if previous.is_some_and(|p| p >= &ns.source.id) {
                return Err(Error::sync(
                    "pack namespaces are not unique and ordered by source id",
                ));
            }
            previous = Some(&ns.source.id);
            if self.kind == PackKind::Snapshot && ns.base_root.is_some() {
                return Err(Error::sync("a snapshot pack cannot have a base root"));
            }
            total = total
                .checked_add(ns.units)
                .and_then(|t| t.checked_add(ns.tombstones))
                .ok_or_else(|| Error::sync("pack entry counts overflow"))?;
        }
        if total != self.entries {
            return Err(Error::sync(
                "pack entry total does not match its namespaces",
            ));
        }
        Ok(())
    }
}

/// One payload record: a unit version, or a tombstone when `unit` is `None`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PackEntry {
    /// Key, source, kind, content hash and version.
    pub meta: UnitMeta,
    /// The canonical unit; `None` for a tombstone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit: Option<Unit>,
}

impl PackEntry {
    /// A live version of `unit`: canonicalises it and computes its content hash.
    pub fn live(mut unit: Unit, version: UnitVersion) -> Result<Self> {
        unit.canonicalize();
        let meta = UnitMeta {
            key: unit.key.clone(),
            source: unit.source.clone(),
            kind: unit.kind,
            hash: unit.content_hash()?,
            hlc: version.hlc,
            origin: version.origin,
            deleted: false,
        };
        Ok(Self {
            meta,
            unit: Some(unit),
        })
    }

    /// A tombstone recording the deletion of `key` at `version`.
    pub fn tombstone(key: UnitKey, source: SourceId, kind: UnitKind, version: UnitVersion) -> Self {
        Self {
            meta: UnitMeta {
                key,
                source,
                kind,
                hash: ContentHash::ZERO,
                hlc: version.hlc,
                origin: version.origin,
                deleted: true,
            },
            unit: None,
        }
    }

    /// Canonicalises the unit in place and checks that meta and unit agree: a tombstone has no
    /// unit and the zero hash; a live version has a unit with the same key, source and kind whose
    /// canonical content hash is `meta.hash`. Fails with [`Error::Integrity`].
    pub fn validate(&mut self) -> Result<()> {
        let key = &self.meta.key;
        match (&mut self.unit, self.meta.deleted) {
            (None, true) => {
                if !self.meta.hash.is_zero() {
                    return Err(Error::integrity(format!(
                        "tombstone `{key}` has a content hash"
                    )));
                }
            }
            (Some(_), true) => {
                return Err(Error::integrity(format!(
                    "tombstone `{key}` carries a unit"
                )));
            }
            (None, false) => {
                return Err(Error::integrity(format!("unit `{key}` has no content")));
            }
            (Some(unit), false) => {
                if unit.key != self.meta.key
                    || unit.source != self.meta.source
                    || unit.kind != self.meta.kind
                {
                    return Err(Error::integrity(format!(
                        "unit `{key}` does not match its metadata"
                    )));
                }
                unit.canonicalize();
                if unit.content_hash()? != self.meta.hash {
                    return Err(Error::integrity(format!(
                        "unit `{key}` does not match its content hash"
                    )));
                }
            }
        }
        Ok(())
    }
}

/// Bounds a reader enforces on untrusted packs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PackLimits {
    /// Largest accepted manifest, in bytes.
    pub max_manifest_bytes: u32,
    /// Largest accepted payload record, in bytes.
    pub max_entry_bytes: u32,
    /// Largest accepted decompressed payload, in bytes.
    pub max_raw_bytes: u64,
}

impl Default for PackLimits {
    fn default() -> Self {
        Self {
            max_manifest_bytes: 8 << 20,
            max_entry_bytes: 64 << 20,
            max_raw_bytes: 64 << 30,
        }
    }
}

/// Options of a [`PackWriter`].
#[derive(Clone, Debug)]
pub struct PackWriterOptions {
    /// zstd compression level.
    pub level: i32,
    /// Directory for the payload spool file; the system temporary directory when `None`.
    pub spool_dir: Option<PathBuf>,
    /// Largest record the writer accepts; keep it at or below what readers allow.
    pub max_entry_bytes: u32,
}

impl Default for PackWriterOptions {
    fn default() -> Self {
        Self {
            level: 3,
            spool_dir: None,
            max_entry_bytes: PackLimits::default().max_entry_bytes,
        }
    }
}

/// What [`PackWriter::finish`] produced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackSummary {
    /// Id of the pack.
    pub pack_id: String,
    /// Total size of the pack in bytes.
    pub bytes: u64,
    /// Number of payload records.
    pub entries: u64,
    /// The signed manifest.
    pub manifest: PackManifest,
}

struct HashingWriter<W> {
    inner: W,
    hasher: blake3::Hasher,
    written: u64,
}

impl<W: Write> Write for HashingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let written = self.inner.write(buf)?;
        self.hasher.update(&buf[..written]);
        self.written += written as u64;
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

struct HashingReader<R> {
    inner: R,
    hasher: blake3::Hasher,
    read: u64,
}

impl<R: Read> Read for HashingReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let read = self.inner.read(buf)?;
        self.hasher.update(&buf[..read]);
        self.read += read as u64;
        Ok(read)
    }
}

fn hash_of(hasher: &blake3::Hasher) -> ContentHash {
    ContentHash::from_bytes(*hasher.finalize().as_bytes())
}

/// Streaming writer of a `.contextpack`. Blocking: use it from a blocking thread.
///
/// Declare every namespace with [`Self::namespace`], add records with [`Self::write`], then
/// [`Self::finish`] into the destination.
pub struct PackWriter<'a> {
    identity: &'a DeviceIdentity,
    kind: PackKind,
    namespaces: BTreeMap<SourceId, NamespaceManifest>,
    encoder: zstd::stream::write::Encoder<'static, HashingWriter<File>>,
    raw_size: u64,
    entries: u64,
    max_entry_bytes: u32,
}

impl<'a> PackWriter<'a> {
    /// Starts a pack signed by `identity`, with default options.
    pub fn new(identity: &'a DeviceIdentity, kind: PackKind) -> Result<Self> {
        Self::with_options(identity, kind, &PackWriterOptions::default())
    }

    /// Starts a pack signed by `identity`.
    pub fn with_options(
        identity: &'a DeviceIdentity,
        kind: PackKind,
        options: &PackWriterOptions,
    ) -> Result<Self> {
        let spool = match &options.spool_dir {
            Some(dir) => tempfile::tempfile_in(dir)?,
            None => tempfile::tempfile()?,
        };
        let spool = HashingWriter {
            inner: spool,
            hasher: blake3::Hasher::new(),
            written: 0,
        };
        Ok(Self {
            identity,
            kind,
            namespaces: BTreeMap::new(),
            encoder: zstd::stream::write::Encoder::new(spool, options.level)?,
            raw_size: 0,
            entries: 0,
            max_entry_bytes: options.max_entry_bytes,
        })
    }

    /// Declares a namespace: its descriptor, the base root of a delta when known, and the root of
    /// the exporter's tree. Must precede the namespace's records.
    pub fn namespace(
        &mut self,
        source: SourceDescriptor,
        base_root: Option<ContentHash>,
        root: ContentHash,
    ) -> Result<()> {
        if self.kind == PackKind::Snapshot && base_root.is_some() {
            return Err(Error::invalid("a snapshot pack cannot have a base root"));
        }
        if self.namespaces.contains_key(&source.id) {
            return Err(Error::invalid(format!(
                "namespace `{}` declared twice",
                source.id
            )));
        }
        self.namespaces.insert(
            source.id.clone(),
            NamespaceManifest {
                source,
                base_root,
                root,
                units: 0,
                tombstones: 0,
            },
        );
        Ok(())
    }

    /// Appends a record. The entry is validated first (see [`PackEntry::validate`]), so a writer
    /// never produces a pack its own reader would reject.
    pub fn write(&mut self, entry: &PackEntry) -> Result<()> {
        let mut entry = entry.clone();
        entry.validate()?;
        let namespace = self.namespaces.get_mut(&entry.meta.source).ok_or_else(|| {
            Error::invalid(format!(
                "unit `{}` belongs to undeclared namespace `{}`",
                entry.meta.key, entry.meta.source
            ))
        })?;
        let bytes = canonical_json(&entry)?;
        let len = u32::try_from(bytes.len())
            .ok()
            .filter(|len| *len <= self.max_entry_bytes)
            .ok_or_else(|| {
                Error::invalid(format!(
                    "unit `{}` is too large for a pack ({} bytes)",
                    entry.meta.key,
                    bytes.len()
                ))
            })?;
        self.encoder.write_all(&len.to_le_bytes())?;
        self.encoder.write_all(&bytes)?;
        if entry.meta.deleted {
            namespace.tombstones += 1;
        } else {
            namespace.units += 1;
        }
        self.raw_size += 4 + u64::from(len);
        self.entries += 1;
        Ok(())
    }

    /// Number of records written so far.
    pub fn entries(&self) -> u64 {
        self.entries
    }

    /// Completes the payload, signs the manifest and writes the whole pack to `out`.
    pub fn finish(self, mut out: impl Write) -> Result<PackSummary> {
        let spool = self.encoder.finish()?;
        let payload = PayloadInfo {
            codec: PACK_CODEC.to_owned(),
            raw_size: self.raw_size,
            compressed_size: spool.written,
            hash: hash_of(&spool.hasher),
        };
        let manifest = PackManifest {
            format_major: PACK_FORMAT_MAJOR,
            format_minor: PACK_FORMAT_MINOR,
            pack_id: uuid::Uuid::now_v7().to_string(),
            created_ms: now_ms(),
            origin: self.identity.device_id().clone(),
            public_key: self.identity.public_key(),
            kind: self.kind,
            namespaces: self.namespaces.into_values().collect(),
            entries: self.entries,
            payload,
        };
        let manifest_bytes = canonical_json(&manifest)?;
        let manifest_len = u32::try_from(manifest_bytes.len())
            .map_err(|_| Error::invalid("pack manifest is too large"))?;

        let mut signed = Vec::with_capacity(SIGN_DOMAIN.len() + HEADER_LEN + manifest_bytes.len());
        signed.extend_from_slice(SIGN_DOMAIN);
        signed.extend_from_slice(PACK_MAGIC);
        signed.extend_from_slice(&PACK_FORMAT_MAJOR.to_le_bytes());
        signed.extend_from_slice(&PACK_FORMAT_MINOR.to_le_bytes());
        signed.extend_from_slice(&manifest_len.to_le_bytes());
        signed.extend_from_slice(&manifest_bytes);
        let signature = self.identity.sign(&signed);

        out.write_all(&signed[SIGN_DOMAIN.len()..])?;
        out.write_all(signature.as_bytes())?;
        let mut spool = spool.inner;
        spool.seek(SeekFrom::Start(0))?;
        let copied = io::copy(&mut spool, &mut out)?;
        if copied != manifest.payload.compressed_size {
            return Err(Error::other("pack spool file changed size while copying"));
        }
        out.flush()?;
        let bytes = (HEADER_LEN + manifest_bytes.len() + DeviceSignature::LEN) as u64 + copied;
        Ok(PackSummary {
            pack_id: manifest.pack_id.clone(),
            bytes,
            entries: manifest.entries,
            manifest,
        })
    }
}

type PayloadDecoder<R> =
    zstd::stream::read::Decoder<'static, BufReader<HashingReader<io::Take<R>>>>;

enum ReaderState<R: Read> {
    /// Manifest verified, payload untouched.
    Fresh(R),
    Streaming(Box<PayloadDecoder<R>>),
    Done,
    Failed,
}

/// Streaming, verifying reader of a `.contextpack`. Blocking: use it from a blocking thread.
///
/// See the [module docs](self) for what is verified and when. After an error the reader is
/// poisoned and keeps returning errors.
pub struct PackReader<R: Read> {
    manifest: PackManifest,
    limits: PackLimits,
    state: ReaderState<R>,
    raw_read: u64,
    seen: BTreeMap<SourceId, (u64, u64)>,
    seen_total: u64,
    buf: Vec<u8>,
}

fn truncated(err: io::Error, what: &str) -> Error {
    if err.kind() == io::ErrorKind::UnexpectedEof {
        Error::integrity(format!("pack is truncated ({what})"))
    } else {
        Error::Io(err)
    }
}

impl<R: Read> std::fmt::Debug for PackReader<R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PackReader")
            .field("manifest", &self.manifest)
            .field("raw_read", &self.raw_read)
            .finish_non_exhaustive()
    }
}

impl<R: Read> PackReader<R> {
    /// Reads and verifies the header, manifest and signature. Nothing of the payload is read yet.
    ///
    /// Errors: [`Error::Sync`] for a file that is not a pack, an unsupported version or codec, or
    /// a manifest over the limits; [`Error::Integrity`] for a truncated file, a bad signature or an
    /// untrusted signer.
    pub fn open(mut reader: R, policy: &TrustPolicy, limits: PackLimits) -> Result<Self> {
        let mut header = [0u8; HEADER_LEN];
        reader
            .read_exact(&mut header)
            .map_err(|err| truncated(err, "header"))?;
        if &header[..8] != PACK_MAGIC {
            return Err(Error::sync("not a .contextpack file (bad magic)"));
        }
        let major = u16::from_le_bytes([header[8], header[9]]);
        let minor = u16::from_le_bytes([header[10], header[11]]);
        if major != PACK_FORMAT_MAJOR {
            return Err(Error::sync(format!(
                "unsupported .contextpack format version {major}.{minor} (this build reads \
                 {PACK_FORMAT_MAJOR}.x)"
            )));
        }
        let manifest_len = u32::from_le_bytes([header[12], header[13], header[14], header[15]]);
        if manifest_len > limits.max_manifest_bytes {
            return Err(Error::sync(format!(
                "pack manifest of {manifest_len} bytes exceeds the limit of {} bytes",
                limits.max_manifest_bytes
            )));
        }
        let mut signed = Vec::with_capacity(SIGN_DOMAIN.len() + HEADER_LEN);
        signed.extend_from_slice(SIGN_DOMAIN);
        signed.extend_from_slice(&header);
        // Grows as bytes arrive, so a lying length cannot force a large allocation.
        let read = (&mut reader)
            .take(u64::from(manifest_len))
            .read_to_end(&mut signed)?;
        if read != manifest_len as usize {
            return Err(Error::integrity("pack is truncated (manifest)"));
        }
        let mut signature = [0u8; DeviceSignature::LEN];
        reader
            .read_exact(&mut signature)
            .map_err(|err| truncated(err, "signature"))?;

        let manifest: PackManifest =
            serde_json::from_slice(&signed[SIGN_DOMAIN.len() + HEADER_LEN..])
                .map_err(|err| Error::sync(format!("pack manifest is malformed: {err}")))?;
        policy.verify(
            &manifest.public_key,
            &signed,
            &DeviceSignature::from_bytes(signature),
        )?;
        if manifest.format_major != major || manifest.format_minor != minor {
            return Err(Error::sync("pack manifest and header versions disagree"));
        }
        manifest.check_consistency()?;
        if manifest.payload.raw_size > limits.max_raw_bytes {
            return Err(Error::sync(format!(
                "pack payload of {} bytes exceeds the limit of {} bytes",
                manifest.payload.raw_size, limits.max_raw_bytes
            )));
        }
        Ok(Self {
            manifest,
            limits,
            state: ReaderState::Fresh(reader),
            raw_read: 0,
            seen: BTreeMap::new(),
            seen_total: 0,
            buf: Vec::new(),
        })
    }

    /// The verified manifest.
    pub fn manifest(&self) -> &PackManifest {
        &self.manifest
    }

    /// The next verified record, or `Ok(None)` once the whole payload has been read and its hash,
    /// sizes and counts have been verified.
    pub fn next_entry(&mut self) -> Result<Option<PackEntry>> {
        let result = self.advance();
        if result.is_err() {
            self.state = ReaderState::Failed;
        }
        result
    }

    fn advance(&mut self) -> Result<Option<PackEntry>> {
        if matches!(self.state, ReaderState::Fresh(_)) {
            let ReaderState::Fresh(reader) =
                std::mem::replace(&mut self.state, ReaderState::Failed)
            else {
                unreachable!("state was just matched as Fresh");
            };
            let source = HashingReader {
                inner: reader.take(self.manifest.payload.compressed_size),
                hasher: blake3::Hasher::new(),
                read: 0,
            };
            let decoder = zstd::stream::read::Decoder::new(source)
                .map_err(|err| Error::integrity(format!("pack payload is corrupt: {err}")))?;
            self.state = ReaderState::Streaming(Box::new(decoder));
        }
        let decoder = match &mut self.state {
            ReaderState::Streaming(decoder) => decoder,
            ReaderState::Done => return Ok(None),
            ReaderState::Fresh(_) | ReaderState::Failed => {
                return Err(Error::sync("pack reader failed earlier"));
            }
        };
        let remaining = self.manifest.payload.raw_size - self.raw_read;
        if remaining == 0 {
            return self.finalize().map(|()| None);
        }
        let corrupt = |err: io::Error| Error::integrity(format!("pack payload is corrupt: {err}"));
        let mut prefix = [0u8; 4];
        if remaining < 4 {
            return Err(Error::integrity("pack payload ends inside a record"));
        }
        decoder.read_exact(&mut prefix).map_err(corrupt)?;
        let len = u32::from_le_bytes(prefix);
        if len > self.limits.max_entry_bytes {
            return Err(Error::sync(format!(
                "pack record of {len} bytes exceeds the limit of {} bytes",
                self.limits.max_entry_bytes
            )));
        }
        if len == 0 || u64::from(len) > remaining - 4 {
            return Err(Error::integrity(
                "pack record length contradicts the declared payload size",
            ));
        }
        self.buf.clear();
        let read = decoder
            .take(u64::from(len))
            .read_to_end(&mut self.buf)
            .map_err(corrupt)?;
        if read != len as usize {
            return Err(Error::integrity("pack is truncated (payload)"));
        }
        self.raw_read += 4 + u64::from(len);

        let mut entry: PackEntry = serde_json::from_slice(&self.buf)
            .map_err(|err| Error::integrity(format!("pack record is malformed: {err}")))?;
        entry.validate()?;
        let Some(namespace) = self.manifest.namespace(&entry.meta.source) else {
            return Err(Error::integrity(format!(
                "unit `{}` belongs to namespace `{}`, which the manifest does not declare",
                entry.meta.key, entry.meta.source
            )));
        };
        let seen = self.seen.entry(entry.meta.source.clone()).or_default();
        if entry.meta.deleted {
            seen.1 += 1;
        } else {
            seen.0 += 1;
        }
        self.seen_total += 1;
        if seen.0 > namespace.units || seen.1 > namespace.tombstones {
            return Err(Error::integrity(format!(
                "namespace `{}` holds more records than the manifest declares",
                entry.meta.source
            )));
        }
        // One oversized record should not pin its buffer for the rest of the pack.
        if self.buf.capacity() > (1 << 20) {
            self.buf = Vec::new();
        }
        Ok(Some(entry))
    }

    /// End-of-payload checks: nothing left to decompress, compressed size and hash as declared,
    /// nothing after the payload, counts as declared.
    fn finalize(&mut self) -> Result<()> {
        let ReaderState::Streaming(mut decoder) =
            std::mem::replace(&mut self.state, ReaderState::Failed)
        else {
            return Err(Error::sync("pack reader failed earlier"));
        };
        let mut probe = [0u8; 1];
        let extra = decoder
            .read(&mut probe)
            .map_err(|err| Error::integrity(format!("pack payload is corrupt: {err}")))?;
        if extra != 0 {
            return Err(Error::integrity(
                "pack payload is larger than the manifest declares",
            ));
        }
        let hashing = decoder.finish().into_inner();
        if hashing.read != self.manifest.payload.compressed_size {
            return Err(Error::integrity("pack is truncated (payload)"));
        }
        if hash_of(&hashing.hasher) != self.manifest.payload.hash {
            return Err(Error::integrity(
                "pack payload does not match the hash in its manifest",
            ));
        }
        let mut rest = hashing.inner.into_inner();
        if rest.read(&mut probe)? != 0 {
            return Err(Error::integrity("unexpected data after the pack payload"));
        }
        for namespace in &self.manifest.namespaces {
            let seen = self
                .seen
                .get(&namespace.source.id)
                .copied()
                .unwrap_or_default();
            if seen != (namespace.units, namespace.tombstones) {
                return Err(Error::integrity(format!(
                    "namespace `{}` holds fewer records than the manifest declares",
                    namespace.source.id
                )));
            }
        }
        if self.seen_total != self.manifest.entries {
            return Err(Error::integrity(
                "pack record count does not match its manifest",
            ));
        }
        self.state = ReaderState::Done;
        Ok(())
    }

    /// Reads and verifies every remaining record, discarding them. Returns how many were read.
    pub fn verify_all(&mut self) -> Result<u64> {
        let mut count = 0;
        while self.next_entry()?.is_some() {
            count += 1;
        }
        Ok(count)
    }
}

impl<R: Read + Seek> PackReader<R> {
    /// Authenticates the compressed payload (size, BLAKE3 hash, nothing after it) without
    /// decompressing it, then rewinds so that records can be streamed. Must be called before the
    /// first [`Self::next_entry`]; afterwards every record yielded is known to come from the
    /// signer.
    pub fn verify_payload(&mut self) -> Result<()> {
        let result = self.verify_payload_inner();
        if result.is_err() {
            self.state = ReaderState::Failed;
        }
        result
    }

    fn verify_payload_inner(&mut self) -> Result<()> {
        let ReaderState::Fresh(reader) = &mut self.state else {
            return Err(Error::invalid(
                "the pack payload can only be verified before it is read",
            ));
        };
        let start = reader.stream_position()?;
        let mut hasher = blake3::Hasher::new();
        let mut chunk = vec![0u8; 64 * 1024];
        let mut total = 0u64;
        loop {
            let read = reader.read(&mut chunk)?;
            if read == 0 {
                break;
            }
            total += read as u64;
            if total > self.manifest.payload.compressed_size {
                return Err(Error::integrity("unexpected data after the pack payload"));
            }
            hasher.update(&chunk[..read]);
        }
        if total != self.manifest.payload.compressed_size {
            return Err(Error::integrity("pack is truncated (payload)"));
        }
        if hash_of(&hasher) != self.manifest.payload.hash {
            return Err(Error::integrity(
                "pack payload does not match the hash in its manifest",
            ));
        }
        reader.seek(SeekFrom::Start(start))?;
        Ok(())
    }
}

impl<R: Read> Iterator for PackReader<R> {
    type Item = Result<PackEntry>;

    fn next(&mut self) -> Option<Self::Item> {
        match self.state {
            // Report a failure once, then end the iteration.
            ReaderState::Failed => None,
            _ => self.next_entry().transpose(),
        }
    }
}

crate::base::opaque_debug!(@'a; PackWriter<'a>);

#[cfg(test)]
pub(crate) mod tests {
    use std::io::Cursor;

    use proptest::prelude::*;

    use super::*;
    use crate::base::{Hlc, NodeAssertion, NodeKind, SourceKind, TrustLevel, keys};

    pub(crate) fn source(id: &str) -> SourceDescriptor {
        SourceDescriptor {
            id: SourceId::new(id),
            kind: SourceKind::Repo,
            name: id.to_owned(),
            trust: TrustLevel::default(),
        }
    }

    pub(crate) fn unit(source: &str, name: &str, content: &str) -> Unit {
        let source = SourceId::new(source);
        let mut unit = Unit::new(
            keys::unit(&source, UnitKind::File, name),
            source,
            UnitKind::File,
        );
        unit.add_node(NodeAssertion::new(
            NodeKind::Team,
            keys::team(content),
            content,
        ));
        unit
    }

    pub(crate) fn live(
        source: &str,
        name: &str,
        content: &str,
        hlc: u64,
        origin: &str,
    ) -> PackEntry {
        PackEntry::live(
            unit(source, name, content),
            UnitVersion::new(Hlc::new(hlc, 0), origin),
        )
        .expect("entry")
    }

    pub(crate) fn dead(source: &str, name: &str, hlc: u64, origin: &str) -> PackEntry {
        let id = SourceId::new(source);
        PackEntry::tombstone(
            keys::unit(&id, UnitKind::File, name),
            id,
            UnitKind::File,
            UnitVersion::new(Hlc::new(hlc, 0), origin),
        )
    }

    fn sample_entries() -> Vec<PackEntry> {
        vec![
            live("alpha", "a.rs", "one", 1, "d1"),
            live("alpha", "b.rs", "two", 2, "d1"),
            dead("alpha", "c.rs", 3, "d2"),
            live("beta", "x.md", "three", 4, "d2"),
        ]
    }

    fn write_pack(identity: &DeviceIdentity, kind: PackKind, entries: &[PackEntry]) -> Vec<u8> {
        let mut writer = PackWriter::new(identity, kind).expect("writer");
        let base = (kind == PackKind::Delta).then(|| ContentHash::of(b"base"));
        writer
            .namespace(source("alpha"), base, ContentHash::of(b"alpha-root"))
            .expect("namespace");
        writer
            .namespace(source("beta"), None, ContentHash::of(b"beta-root"))
            .expect("namespace");
        for entry in entries {
            writer.write(entry).expect("write");
        }
        let mut out = Vec::new();
        let summary = writer.finish(&mut out).expect("finish");
        assert_eq!(summary.bytes, out.len() as u64);
        assert_eq!(summary.entries, entries.len() as u64);
        out
    }

    fn read_pack(bytes: &[u8], policy: &TrustPolicy) -> Result<(PackManifest, Vec<PackEntry>)> {
        let mut reader = PackReader::open(Cursor::new(bytes), policy, PackLimits::default())?;
        let mut entries = Vec::new();
        while let Some(entry) = reader.next_entry()? {
            entries.push(entry);
        }
        Ok((reader.manifest().clone(), entries))
    }

    #[test]
    fn snapshot_and_delta_round_trip_with_tombstones() {
        let identity = DeviceIdentity::generate();
        for kind in [PackKind::Snapshot, PackKind::Delta] {
            let entries = sample_entries();
            let bytes = write_pack(&identity, kind, &entries);
            assert_eq!(&bytes[..8], PACK_MAGIC);
            let (manifest, read) = read_pack(&bytes, &TrustPolicy::AnyValid).expect("read");
            assert_eq!(read, entries);
            assert_eq!(manifest.kind, kind);
            assert_eq!(&manifest.origin, identity.device_id());
            assert_eq!(manifest.entries, 4);
            let alpha = manifest.namespace(&SourceId::new("alpha")).expect("alpha");
            assert_eq!((alpha.units, alpha.tombstones), (2, 1));
            assert_eq!(alpha.base_root.is_some(), kind == PackKind::Delta);
            assert_eq!(alpha.root, ContentHash::of(b"alpha-root"));
            assert_eq!(manifest.payload.codec, PACK_CODEC);

            let mut reader = PackReader::open(
                Cursor::new(&bytes),
                &TrustPolicy::AnyValid,
                PackLimits::default(),
            )
            .expect("open");
            reader.verify_payload().expect("payload");
            assert_eq!(reader.verify_all().expect("verify"), 4);
        }
        // An empty pack is valid too.
        let bytes = write_pack(&identity, PackKind::Snapshot, &[]);
        let (manifest, read) = read_pack(&bytes, &TrustPolicy::AnyValid).expect("read");
        assert!(read.is_empty());
        assert_eq!(manifest.payload.raw_size, 0);
    }

    #[test]
    fn writer_rejects_invalid_entries() {
        let identity = DeviceIdentity::generate();
        let mut writer = PackWriter::new(&identity, PackKind::Snapshot).expect("writer");
        assert!(
            writer
                .namespace(source("alpha"), Some(ContentHash::ZERO), ContentHash::ZERO)
                .is_err()
        );
        writer
            .namespace(source("alpha"), None, ContentHash::ZERO)
            .expect("namespace");
        assert!(writer.write(&live("gamma", "a", "x", 1, "d")).is_err());
        let mut wrong_hash = live("alpha", "a", "x", 1, "d");
        wrong_hash.meta.hash = ContentHash::of(b"other");
        assert!(writer.write(&wrong_hash).is_err());
        let mut zombie = live("alpha", "a", "x", 1, "d");
        zombie.meta.deleted = true;
        assert!(writer.write(&zombie).is_err());
        assert_eq!(writer.entries(), 0);
    }

    #[test]
    fn signer_trust_is_enforced() {
        let alice = DeviceIdentity::generate();
        let mallory = DeviceIdentity::generate();
        let bytes = write_pack(&alice, PackKind::Snapshot, &sample_entries());
        read_pack(&bytes, &TrustPolicy::allowlist([alice.public_key()])).expect("trusted");
        let err = read_pack(&bytes, &TrustPolicy::allowlist([mallory.public_key()]))
            .expect_err("untrusted signer");
        assert!(err.to_string().contains("not a trusted device"), "{err}");

        // A manifest re-signed by someone else while still naming Alice's key is a wrong signer.
        let manifest_len = u32::from_le_bytes(bytes[12..16].try_into().expect("4 bytes")) as usize;
        let mut signed = SIGN_DOMAIN.to_vec();
        signed.extend_from_slice(&bytes[..HEADER_LEN + manifest_len]);
        let mut forged = bytes.clone();
        forged[HEADER_LEN + manifest_len..HEADER_LEN + manifest_len + 64]
            .copy_from_slice(mallory.sign(&signed).as_bytes());
        let err = read_pack(&forged, &TrustPolicy::AnyValid).expect_err("wrong signer");
        assert!(matches!(err, Error::Integrity(_)), "{err}");
    }

    #[test]
    fn truncated_and_extended_files_are_rejected() {
        let identity = DeviceIdentity::generate();
        let bytes = write_pack(&identity, PackKind::Snapshot, &sample_entries());
        for len in [0, 7, 15, 40, bytes.len() - 70, bytes.len() - 1] {
            assert!(
                read_pack(&bytes[..len], &TrustPolicy::AnyValid).is_err(),
                "length {len}"
            );
            if let Ok(mut reader) = PackReader::open(
                Cursor::new(&bytes[..len]),
                &TrustPolicy::AnyValid,
                PackLimits::default(),
            ) {
                assert!(reader.verify_payload().is_err(), "length {len}");
            }
        }
        let mut longer = bytes.clone();
        longer.push(0);
        assert!(read_pack(&longer, &TrustPolicy::AnyValid).is_err());
        let mut reader = PackReader::open(
            Cursor::new(&longer),
            &TrustPolicy::AnyValid,
            PackLimits::default(),
        )
        .expect("open");
        assert!(reader.verify_payload().is_err());
        // A failed reader stays failed and its iterator ends.
        assert!(reader.next_entry().is_err());
        assert!(reader.next().is_none());
    }

    #[test]
    fn unknown_major_version_is_rejected_clearly() {
        let identity = DeviceIdentity::generate();
        let mut bytes = write_pack(&identity, PackKind::Snapshot, &sample_entries());
        bytes[8] = 2;
        let err = read_pack(&bytes, &TrustPolicy::AnyValid).expect_err("version");
        assert!(
            err.to_string()
                .contains("unsupported .contextpack format version 2.0"),
            "{err}"
        );
        assert!(read_pack(b"definitely not a pack", &TrustPolicy::AnyValid).is_err());
    }

    #[test]
    fn limits_are_enforced() {
        let identity = DeviceIdentity::generate();
        let big = "x".repeat(4_000);
        let entries = vec![live("alpha", "a.rs", &big, 1, "d1")];
        let bytes = write_pack(&identity, PackKind::Snapshot, &entries);
        let open = |limits: PackLimits| {
            PackReader::open(Cursor::new(&bytes), &TrustPolicy::AnyValid, limits)
        };
        let defaults = PackLimits::default();
        assert!(
            open(PackLimits {
                max_manifest_bytes: 64,
                ..defaults
            })
            .is_err()
        );
        // The decompressed size is refused up front, before any decompression.
        assert!(
            open(PackLimits {
                max_raw_bytes: 1_000,
                ..defaults
            })
            .is_err()
        );
        let mut reader = open(PackLimits {
            max_entry_bytes: 1_000,
            ..defaults
        })
        .expect("open");
        let err = reader.next_entry().expect_err("entry limit");
        assert!(err.to_string().contains("exceeds the limit"), "{err}");

        let mut writer = PackWriter::with_options(
            &identity,
            PackKind::Snapshot,
            &PackWriterOptions {
                max_entry_bytes: 1_000,
                ..PackWriterOptions::default()
            },
        )
        .expect("writer");
        writer
            .namespace(source("alpha"), None, ContentHash::ZERO)
            .expect("namespace");
        assert!(writer.write(&entries[0]).is_err());
    }

    /// A signed pack whose manifest understates the payload: the reader must stop at the declared
    /// size instead of inflating whatever the stream contains.
    #[test]
    fn payload_larger_than_declared_is_rejected() {
        let identity = DeviceIdentity::generate();
        let entries = sample_entries();
        let honest = write_pack(&identity, PackKind::Snapshot, &entries);
        let (manifest, _) = read_pack(&honest, &TrustPolicy::AnyValid).expect("read");
        let manifest_len = u32::from_le_bytes(honest[12..16].try_into().expect("4 bytes")) as usize;
        let payload = &honest[HEADER_LEN + manifest_len + 64..];

        let resign = |manifest: &PackManifest| {
            let body = canonical_json(manifest).expect("json");
            let mut signed = SIGN_DOMAIN.to_vec();
            signed.extend_from_slice(PACK_MAGIC);
            signed.extend_from_slice(&PACK_FORMAT_MAJOR.to_le_bytes());
            signed.extend_from_slice(&PACK_FORMAT_MINOR.to_le_bytes());
            signed.extend_from_slice(&(body.len() as u32).to_le_bytes());
            signed.extend_from_slice(&body);
            let mut out = signed[SIGN_DOMAIN.len()..].to_vec();
            out.extend_from_slice(identity.sign(&signed).as_bytes());
            out.extend_from_slice(payload);
            out
        };
        // Declares fewer entries and bytes than the payload holds.
        let mut lying = manifest.clone();
        lying.entries = 3;
        lying.namespaces[1].units = 0;
        let first_three: u64 = entries[..3]
            .iter()
            .map(|e| 4 + canonical_json(e).expect("json").len() as u64)
            .sum();
        lying.payload.raw_size = first_three;
        let err = read_pack(&resign(&lying), &TrustPolicy::AnyValid).expect_err("bomb");
        assert!(
            err.to_string().contains("larger than the manifest"),
            "{err}"
        );
        // Declares a count the payload does not deliver.
        let mut lying = manifest;
        lying.entries = 5;
        lying.namespaces[1].units = 2;
        assert!(read_pack(&resign(&lying), &TrustPolicy::AnyValid).is_err());
    }

    #[test]
    fn large_packs_stream_in_bounded_memory() {
        let identity = DeviceIdentity::generate();
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("big.contextpack");
        let mut writer = PackWriter::with_options(
            &identity,
            PackKind::Snapshot,
            &PackWriterOptions {
                level: 1,
                spool_dir: Some(dir.path().to_path_buf()),
                ..PackWriterOptions::default()
            },
        )
        .expect("writer");
        writer
            .namespace(source("alpha"), None, ContentHash::ZERO)
            .expect("namespace");
        let filler = "lorem ipsum ".repeat(40);
        const COUNT: u64 = 20_000;
        for index in 0..COUNT {
            let content = format!("{index} {filler}");
            writer
                .write(&live(
                    "alpha",
                    &format!("f{index}"),
                    &content,
                    index + 1,
                    "d1",
                ))
                .expect("write");
        }
        let summary = writer
            .finish(io::BufWriter::new(File::create(&path).expect("create")))
            .expect("finish");
        assert_eq!(summary.entries, COUNT);
        assert!(summary.manifest.payload.raw_size > 15_000_000);

        let file = io::BufReader::new(File::open(&path).expect("open"));
        let mut reader =
            PackReader::open(file, &TrustPolicy::AnyValid, PackLimits::default()).expect("open");
        reader.verify_payload().expect("payload");
        let mut count = 0;
        while let Some(entry) = reader.next_entry().expect("entry") {
            assert!(!entry.meta.deleted);
            count += 1;
            // The reader's only growing state is its record buffer: one record, not the pack.
            assert!(reader.buf.capacity() < 64 * 1024);
        }
        assert_eq!(count, COUNT);
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(400))]

        /// Changing any single byte of a pack makes reading it fail, both when streaming and when
        /// authenticating the payload up front.
        #[test]
        fn tampering_with_any_byte_is_detected(position in 0usize..10_000, flip in 1u8..=255) {
            static PACK: std::sync::OnceLock<Vec<u8>> = std::sync::OnceLock::new();
            let original = PACK.get_or_init(|| {
                write_pack(&DeviceIdentity::generate(), PackKind::Delta, &sample_entries())
            });
            let mut bytes = original.clone();
            let position = position % bytes.len();
            bytes[position] ^= flip;
            prop_assert!(read_pack(&bytes, &TrustPolicy::AnyValid).is_err(), "byte {}", position);
            if let Ok(mut reader) =
                PackReader::open(Cursor::new(&bytes), &TrustPolicy::AnyValid, PackLimits::default())
            {
                prop_assert!(reader.verify_payload().is_err(), "byte {}", position);
            }
        }
    }

    /// Exhaustive companion of the property test: every byte, one bit each.
    #[test]
    fn every_byte_is_covered() {
        let bytes = write_pack(
            &DeviceIdentity::generate(),
            PackKind::Snapshot,
            &sample_entries(),
        );
        for position in 0..bytes.len() {
            let mut tampered = bytes.clone();
            tampered[position] ^= 0x01;
            assert!(
                read_pack(&tampered, &TrustPolicy::AnyValid).is_err(),
                "byte {position} of {}",
                bytes.len()
            );
        }
    }
}
