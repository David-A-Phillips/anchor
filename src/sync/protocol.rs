//! Wire messages of the sync protocol and the store-side operations both peers share.
//!
//! # Exchange (identical over WebSocket and gRPC)
//!
//! 1. **Hello**: the client sends its protocol version, device id and the Merkle root of each of
//!    its namespaces (the bearer token travels in the WebSocket hello, in gRPC metadata
//!    otherwise); the server answers with its own roots.
//! 2. **Metas**: for every namespace whose roots differ and that is in scope (published or
//!    subscribed), the client asks for the server's unit metas. *Design note:* the Merkle diff
//!    API needs one round trip per tree level; this protocol instead ships the full meta list of
//!    each differing namespace (about 100 bytes per unit) and compares locally. Namespaces whose
//!    roots match cost nothing.
//! 3. **Push**: the client sends delta packs holding the published units the server lacks or has
//!    older versions of.
//! 4. **Pull**: the client asks for the subscribed units it lacks, by key, and receives delta
//!    packs. Every pack is verified ([`PackReader`]: signature, signer trust, hashes) completely
//!    before the first unit is applied, then merged through [`UnitStore::apply`].
//! 5. **Watch**: a second, long-lived channel on which the server announces changed namespaces.
//!
//! On WebSockets control messages are JSON text frames ([`Request`], [`Response`]); a pack is a
//! sequence of binary frames of at most [`CHUNK_BYTES`] announced by its total size.

use std::collections::BTreeMap;
use std::io::Cursor;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::base::{ContentHash, Error, Result, SourceDescriptor, SourceId, UnitKey, UnitMeta};

use super::identity::{DeviceIdentity, TrustPolicy};
use super::merkle::MerkleTree;
use super::pack::{PackEntry, PackKind, PackLimits, PackReader, PackWriter};
use super::store::{ApplyReport, Rejection, UnitStore, should_apply};

/// Version of this protocol; peers with different versions refuse each other.
pub const PROTOCOL_VERSION: u32 = 1;
/// Size of one binary frame / gRPC chunk.
pub const CHUNK_BYTES: usize = 256 * 1024;
/// Units per pack and keys per pull request (bounds memory and message sizes).
pub const BATCH_UNITS: usize = 2000;
/// Units handed to one [`UnitStore::apply`] call.
const APPLY_BATCH: usize = 256;

/// Purpose of a connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// Request/response synchronisation.
    Sync,
    /// Server pushes [`Response::Changed`] notifications.
    Watch,
}

/// Client greeting.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Hello {
    /// Protocol version of the client.
    pub version: u32,
    /// Device id of the client.
    pub device_id: String,
    /// Bearer token, if the client has one.
    pub token: Option<String>,
    /// Connection purpose.
    pub mode: Mode,
    /// Merkle root of each client namespace.
    pub roots: BTreeMap<SourceId, ContentHash>,
}

/// Server greeting.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ServerHello {
    /// Protocol version of the server.
    pub version: u32,
    /// Device id of the server.
    pub device_id: String,
    /// Merkle root of each server namespace.
    pub roots: BTreeMap<SourceId, ContentHash>,
}

/// Client-to-server control message (WebSocket).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request {
    /// First message.
    Hello(Hello),
    /// Ask for the metas of these namespaces.
    Metas {
        /// Namespaces.
        sources: Vec<SourceId>,
    },
    /// `size` bytes of pack follow as binary frames.
    Push {
        /// Total pack size.
        size: u64,
    },
    /// Ask for a pack with these unit keys.
    Pull {
        /// Wanted units.
        keys: Vec<UnitKey>,
        /// The requester's roots (delta base roots).
        base_roots: BTreeMap<SourceId, ContentHash>,
    },
}

/// Outcome of a push.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PushResult {
    /// Versions applied.
    pub applied: u64,
    /// Versions the receiver already had.
    pub stale: u64,
    /// Refused versions.
    pub rejected: Vec<Rejection>,
}

/// Server-to-client control message (WebSocket).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Response {
    /// Answer to hello.
    Hello(ServerHello),
    /// Answer to [`Request::Metas`].
    Metas {
        /// Metas per namespace.
        metas: BTreeMap<SourceId, Vec<UnitMeta>>,
    },
    /// Answer to a push.
    Pushed(PushResult),
    /// `size` bytes of pack follow as binary frames.
    Pack {
        /// Total pack size.
        size: u64,
    },
    /// A namespace changed (watch mode).
    Changed {
        /// Namespace.
        source: SourceId,
        /// Device that caused the change.
        origin: String,
    },
    /// The request failed; the connection ends.
    Error {
        /// Reason.
        message: String,
    },
}

/// One side's store, identity and verification rules: the operations behind every message.
/// All methods block; call them from a blocking thread.
#[derive(Clone)]
pub struct Local {
    /// The system of record.
    pub store: Arc<dyn UnitStore>,
    /// Signs outgoing packs.
    pub identity: Arc<DeviceIdentity>,
    /// Accepted signers of incoming packs.
    pub trust: TrustPolicy,
    /// Bounds for incoming packs.
    pub limits: PackLimits,
}

impl Local {
    /// Merkle root of every namespace.
    pub fn roots(&self) -> Result<BTreeMap<SourceId, ContentHash>> {
        let mut roots = BTreeMap::new();
        for source in self.store.sources()? {
            let root = MerkleTree::from_metas(self.store.metas(&source.id)?).root();
            roots.insert(source.id, root);
        }
        Ok(roots)
    }

    /// Metas of the given namespaces.
    pub fn metas(&self, sources: &[SourceId]) -> Result<BTreeMap<SourceId, Vec<UnitMeta>>> {
        sources
            .iter()
            .map(|source| Ok((source.clone(), self.store.metas(source)?)))
            .collect()
    }

    /// A signed delta pack with the current version of each key (unknown keys are skipped).
    /// `base_roots` are the receiver's roots, recorded as delta bases.
    pub fn build_pack(
        &self,
        keys: &[UnitKey],
        base_roots: &BTreeMap<SourceId, ContentHash>,
    ) -> Result<Vec<u8>> {
        let mut entries = Vec::with_capacity(keys.len());
        for key in keys {
            if let Some((meta, unit)) = self.store.get(key)? {
                entries.push(PackEntry { meta, unit });
            }
        }
        let descriptors: BTreeMap<SourceId, SourceDescriptor> = self
            .store
            .sources()?
            .into_iter()
            .map(|source| (source.id.clone(), source))
            .collect();
        let mut writer = PackWriter::new(&self.identity, PackKind::Delta)?;
        let mut declared = std::collections::BTreeSet::new();
        for entry in &entries {
            let id = &entry.meta.source;
            if !declared.insert(id.clone()) {
                continue;
            }
            let descriptor = descriptors
                .get(id)
                .ok_or_else(|| Error::sync(format!("unit of unknown source `{id}`")))?;
            let root = MerkleTree::from_metas(self.store.metas(id)?).root();
            writer.namespace(descriptor.clone(), base_roots.get(id).copied(), root)?;
        }
        for entry in &entries {
            writer.write(entry)?;
        }
        let mut out = Vec::new();
        writer.finish(&mut out)?;
        Ok(out)
    }

    /// Verifies a received pack completely, then merges it. Nothing is applied when the pack
    /// fails verification (signature, signer trust, any hash, sizes, counts).
    pub fn apply_pack(&self, bytes: &[u8]) -> Result<ApplyReport> {
        let open = || PackReader::open(Cursor::new(bytes), &self.trust, self.limits);
        open()?.verify_all()?;
        let mut reader = open()?;
        let descriptors: Vec<SourceDescriptor> = reader
            .manifest()
            .namespaces
            .iter()
            .map(|ns| ns.source.clone())
            .collect();
        let mut report = ApplyReport::default();
        let mut batch = Vec::with_capacity(APPLY_BATCH);
        while let Some(entry) = reader.next_entry()? {
            batch.push(entry);
            if batch.len() == APPLY_BATCH {
                report.merge(self.store.apply(&descriptors, std::mem::take(&mut batch))?);
            }
        }
        if !batch.is_empty() {
            report.merge(self.store.apply(&descriptors, batch)?);
        }
        Ok(report)
    }
}

/// Keys of the `from` versions that should replace `to`'s (`from` is the sender).
pub fn missing_keys(from: &[UnitMeta], to: &[UnitMeta]) -> Vec<UnitKey> {
    let theirs: BTreeMap<&UnitKey, &UnitMeta> = to.iter().map(|m| (&m.key, m)).collect();
    from.iter()
        .filter(|meta| should_apply(theirs.get(&meta.key).copied(), meta))
        .map(|meta| meta.key.clone())
        .collect()
}

impl From<&ApplyReport> for PushResult {
    fn from(report: &ApplyReport) -> Self {
        Self {
            applied: report.applied.len() as u64,
            stale: report.stale,
            rejected: report.rejected.clone(),
        }
    }
}
