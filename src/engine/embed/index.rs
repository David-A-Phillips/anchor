//! The persistent vector index.
//!
//! Vectors are stored as signed bytes: each (unit) vector is divided by its largest absolute
//! component and scaled to `-127..=127`, and that per-vector factor is kept next to it. Adapting
//! the scale to every vector keeps precision for dense model embeddings (components around
//! `1/sqrt(d)`) as well as for sparse hash embeddings (a few large components). A search quantises
//! the query to 12 bits and computes, for every slot, an integer dot product over a contiguous
//! `i8` row, the cheapest loop there is, then rescales it into an approximate cosine similarity.
//! The result is the exact top-k of those quantised scores.
//!
//! # Memory
//!
//! `dimensions + 28` bytes per vector plus the id map (about 16 bytes per entry): roughly 430 MB
//! for one million 384-dimensional vectors, a quarter of the `f32` representation.
//!
//! # Locking
//!
//! The in-memory state sits behind a `parking_lot::RwLock`: any number of searches run
//! concurrently under read locks. Mutations are serialised by a separate writer mutex, write to
//! SQLite first *without* holding the state lock (searches keep running during disk I/O) and then
//! take the write lock only for the short in-memory copy. A reader therefore sees either the state
//! before or after a mutation, never a partial one, and the database is never behind memory.
//!
//! All methods block (SQLite, CPU-bound scans): call them from `spawn_blocking` or use the
//! `*_async` wrappers.

use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap};
use std::sync::Arc;

use parking_lot::{Mutex, RwLock};
use rayon::prelude::*;
use rusqlite::params;

use crate::base::{ChunkId, ContentHash, Db, Error, Result};

/// Largest supported vector length. Bounds the integer dot product: `127 * 2047 * 8192 < 2^31`.
pub const MAX_DIMENSIONS: usize = 8192;

/// Query components are quantised to this magnitude (12 bits), see [`MAX_DIMENSIONS`].
const QUERY_LEVELS: f32 = 2047.0;
/// Stored components are quantised to this magnitude.
const VECTOR_LEVELS: f32 = 127.0;
/// Rows scanned per rayon task.
const BLOCK_ROWS: usize = 4096;
/// Below this many matrix cells a scan stays on the calling thread: thread hand-off would cost
/// more than the scan itself.
const PARALLEL_CELLS: usize = 2_000_000;

/// A search result.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SearchHit {
    /// The chunk.
    pub id: ChunkId,
    /// Approximate dot product of the query and the stored vector (cosine similarity for unit
    /// vectors, off by the quantisation error, typically below 0.005).
    pub score: f32,
}

/// Predicate restricting a search to some chunks. Only called for candidates that would enter the
/// current top-k, so it may be moderately expensive, but it runs on rayon threads.
pub type VectorFilter<'a> = &'a (dyn Fn(ChunkId) -> bool + Sync);

/// Int8-quantised vectors keyed by [`ChunkId`]; see the module documentation.
///
/// Cloning is cheap and yields a handle to the same index.
#[derive(Clone)]
pub struct VectorIndex {
    shared: Arc<Shared>,
}

struct Shared {
    db: Db,
    model_id: String,
    dimensions: usize,
    /// Serialises mutations (database write followed by memory update).
    writer: Mutex<()>,
    state: RwLock<State>,
}

/// Slot-addressed storage. A free slot has `scales[slot] == 0.0` and an all-zero row, so the scan
/// needs no liveness branch: it scores zero and is rejected with the candidate check.
#[derive(Default)]
struct State {
    /// `slots * dimensions` quantised components, row-major.
    data: Vec<i8>,
    ids: Vec<ChunkId>,
    /// First 16 bytes of each content hash: enough to detect changed content, half the memory.
    hashes: Vec<[u8; 16]>,
    scales: Vec<f32>,
    free: Vec<u32>,
    slots: HashMap<ChunkId, u32>,
}

impl std::fmt::Debug for VectorIndex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VectorIndex")
            .field("model_id", &self.shared.model_id)
            .field("dimensions", &self.shared.dimensions)
            .field("len", &self.len())
            .finish()
    }
}

/// Candidate ordered by "better": higher score first, lower chunk id on ties (deterministic).
#[derive(Clone, Copy, PartialEq)]
struct Candidate {
    score: f32,
    id: ChunkId,
}

impl Eq for Candidate {}

impl Ord for Candidate {
    fn cmp(&self, other: &Self) -> Ordering {
        self.score
            .total_cmp(&other.score)
            .then_with(|| other.id.cmp(&self.id))
    }
}

impl PartialOrd for Candidate {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Keeps the `k` best candidates in a min-heap (worst on top).
struct TopK {
    k: usize,
    heap: BinaryHeap<std::cmp::Reverse<Candidate>>,
}

impl TopK {
    fn new(k: usize) -> Self {
        Self {
            k,
            heap: BinaryHeap::with_capacity(k.min(1024) + 1),
        }
    }

    fn admits(&self, candidate: &Candidate) -> bool {
        self.heap.len() < self.k || self.heap.peek().is_some_and(|worst| *candidate > worst.0)
    }

    fn push(&mut self, candidate: Candidate) {
        self.heap.push(std::cmp::Reverse(candidate));
        if self.heap.len() > self.k {
            self.heap.pop();
        }
    }

    fn merge(mut self, other: Self) -> Self {
        for candidate in other.heap {
            if self.admits(&candidate.0) {
                self.push(candidate.0);
            }
        }
        self
    }
}

fn hash_prefix(hash: &ContentHash) -> [u8; 16] {
    let mut prefix = [0_u8; 16];
    prefix.copy_from_slice(&hash.as_bytes()[..16]);
    prefix
}

/// Quantises `vector` into signed bytes and returns the factor that maps them back.
fn quantize(vector: &[f32], dimensions: usize) -> Result<(Vec<i8>, f32)> {
    if vector.len() != dimensions {
        return Err(Error::invalid(format!(
            "vector has {} dimensions, the index has {dimensions}",
            vector.len()
        )));
    }
    let mut max = 0.0_f32;
    for value in vector {
        if !value.is_finite() {
            return Err(Error::invalid("vector contains a non-finite component"));
        }
        max = max.max(value.abs());
    }
    if max <= 0.0 {
        return Err(Error::invalid("the zero vector cannot be indexed"));
    }
    let factor = VECTOR_LEVELS / max;
    let quantized = vector
        .iter()
        .map(|value| (value * factor).round() as i8)
        .collect();
    Ok((quantized, max / VECTOR_LEVELS))
}

/// Integer dot product of one stored row with the quantised query. Written as a plain zip-sum over
/// equal-length slices so LLVM turns it into widening multiply-add SIMD.
#[inline]
fn dot(row: &[i8], query: &[i16]) -> i32 {
    row.iter()
        .zip(query)
        .map(|(a, b)| i32::from(*a) * i32::from(*b))
        .sum()
}

impl State {
    fn scan(
        &self,
        dimensions: usize,
        query: &[i16],
        first_slot: usize,
        rows: &[i8],
        k: usize,
        filter: Option<VectorFilter<'_>>,
    ) -> TopK {
        let mut top = TopK::new(k);
        for (offset, row) in rows.chunks_exact(dimensions).enumerate() {
            let slot = first_slot + offset;
            let scale = self.scales[slot];
            let candidate = Candidate {
                score: dot(row, query) as f32 * scale,
                id: self.ids[slot],
            };
            if scale > 0.0 && top.admits(&candidate) && filter.is_none_or(|f| f(candidate.id)) {
                top.push(candidate);
            }
        }
        top
    }

    fn release(&mut self, slot: u32, dimensions: usize) {
        let start = slot as usize * dimensions;
        self.data[start..start + dimensions].fill(0);
        self.scales[slot as usize] = 0.0;
        self.free.push(slot);
    }

    fn store(&mut self, id: ChunkId, hash: [u8; 16], scale: f32, row: &[i8]) {
        let dimensions = row.len();
        let slot = match self.slots.get(&id) {
            Some(slot) => *slot,
            None => {
                let slot = self.free.pop().unwrap_or_else(|| {
                    self.data.resize(self.data.len() + dimensions, 0);
                    self.ids.push(id);
                    self.hashes.push(hash);
                    self.scales.push(0.0);
                    (self.ids.len() - 1) as u32
                });
                self.slots.insert(id, slot);
                slot
            }
        };
        let index = slot as usize;
        self.data[index * dimensions..(index + 1) * dimensions].copy_from_slice(row);
        self.ids[index] = id;
        self.hashes[index] = hash;
        self.scales[index] = scale;
    }
}

impl VectorIndex {
    /// Opens the index of `model_id`, reading its vectors of length `dimensions` from `db`.
    ///
    /// Rows written under another model id or another vector length are left on disk but not
    /// loaded: they are *stale* (see [`Self::stale_count`], [`Self::purge_stale`]) and are replaced
    /// one by one as their chunks are re-embedded.
    ///
    /// Fails with [`Error::Invalid`] when `dimensions` is `0` or above [`MAX_DIMENSIONS`].
    pub fn load(db: &Db, model_id: &str, dimensions: usize) -> Result<Self> {
        if dimensions == 0 || dimensions > MAX_DIMENSIONS {
            return Err(Error::invalid(format!(
                "vector index dimensions must be between 1 and {MAX_DIMENSIONS}, got {dimensions}"
            )));
        }
        let state = db.read(|conn| {
            let count: i64 = conn.query_row(
                "SELECT COUNT(*) FROM embed_vectors WHERE model_id = ?1 AND dimensions = ?2",
                params![model_id, dimensions as i64],
                |row| row.get(0),
            )?;
            let count = usize::try_from(count).unwrap_or(0);
            let mut state = State::default();
            // Exact reservations: doubling growth would waste hundreds of megabytes at scale.
            state.data.reserve_exact(count * dimensions);
            state.ids.reserve_exact(count);
            state.hashes.reserve_exact(count);
            state.scales.reserve_exact(count);
            state.slots.reserve(count);
            let mut statement = conn.prepare(
                "SELECT chunk_id, content_hash, scale, vector FROM embed_vectors
                 WHERE model_id = ?1 AND dimensions = ?2",
            )?;
            let mut rows = statement.query(params![model_id, dimensions as i64])?;
            while let Some(row) = rows.next()? {
                let id = ChunkId(row.get(0)?);
                let scale: f64 = row.get(2)?;
                let scale = scale as f32;
                let hash = row.get_ref(1)?.as_blob().ok();
                let vector = row.get_ref(3)?.as_blob().ok();
                // A damaged row is treated like a missing one: the chunk is simply re-embedded.
                let (Some(hash), Some(vector)) = (hash, vector) else {
                    continue;
                };
                if hash.len() != 32
                    || vector.len() != dimensions
                    || !scale.is_finite()
                    || scale <= 0.0
                {
                    continue;
                }
                let mut prefix = [0_u8; 16];
                prefix.copy_from_slice(&hash[..16]);
                state.slots.insert(id, state.ids.len() as u32);
                state.ids.push(id);
                state.hashes.push(prefix);
                state.scales.push(scale);
                state.data.extend(vector.iter().map(|byte| *byte as i8));
            }
            Ok(state)
        })?;
        Ok(Self {
            shared: Arc::new(Shared {
                db: db.clone(),
                model_id: model_id.to_owned(),
                dimensions,
                writer: Mutex::new(()),
                state: RwLock::new(state),
            }),
        })
    }

    /// [`Self::load`] on Tokio's blocking pool.
    pub async fn load_async(db: &Db, model_id: &str, dimensions: usize) -> Result<Self> {
        let db = db.clone();
        let model_id = model_id.to_owned();
        tokio::task::spawn_blocking(move || Self::load(&db, &model_id, dimensions)).await?
    }

    /// Model whose vectors this index holds.
    pub fn model_id(&self) -> &str {
        &self.shared.model_id
    }

    /// Length of the indexed vectors.
    pub fn dimensions(&self) -> usize {
        self.shared.dimensions
    }

    /// Number of vectors in the index.
    pub fn len(&self) -> usize {
        self.shared.state.read().slots.len()
    }

    /// `true` when the index holds no vector.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Approximate heap memory held by the in-memory index, in bytes.
    pub fn memory_bytes(&self) -> usize {
        let state = self.shared.state.read();
        state.data.capacity()
            + state.ids.capacity() * size_of::<ChunkId>()
            + state.hashes.capacity() * 16
            + state.scales.capacity() * size_of::<f32>()
            + state.free.capacity() * size_of::<u32>()
            // hashbrown: one (key, value) pair plus one control byte per bucket.
            + state.slots.capacity() * (size_of::<(ChunkId, u32)>() + 1)
    }

    /// `true` when `id` is indexed under the current model with exactly this content hash, i.e.
    /// when it does not need to be embedded again.
    pub fn contains(&self, id: ChunkId, content_hash: &ContentHash) -> bool {
        let state = self.shared.state.read();
        state
            .slots
            .get(&id)
            .is_some_and(|slot| state.hashes[*slot as usize] == hash_prefix(content_hash))
    }

    /// Inserts or replaces the vector of one chunk, in the database and in memory.
    ///
    /// `vector` should be L2-normalised (scores are dot products). Fails with [`Error::Invalid`],
    /// changing nothing, when its length differs from [`Self::dimensions`], when it contains a
    /// non-finite value or when it is all zeros.
    pub fn upsert(&self, id: ChunkId, content_hash: ContentHash, vector: &[f32]) -> Result<()> {
        self.upsert_rows(vec![self.prepare(id, &content_hash, vector)?])
    }

    /// [`Self::upsert`] for a batch, in one transaction. All vectors are validated first: on
    /// error nothing is written. A chunk listed twice keeps its last vector.
    pub fn upsert_many(&self, items: &[(ChunkId, ContentHash, Vec<f32>)]) -> Result<()> {
        let rows = items
            .iter()
            .map(|(id, hash, vector)| self.prepare(*id, hash, vector))
            .collect::<Result<Vec<_>>>()?;
        self.upsert_rows(rows)
    }

    /// [`Self::upsert_many`] on Tokio's blocking pool.
    pub async fn upsert_many_async(
        &self,
        items: Vec<(ChunkId, ContentHash, Vec<f32>)>,
    ) -> Result<()> {
        let index = self.clone();
        tokio::task::spawn_blocking(move || index.upsert_many(&items)).await?
    }

    fn prepare(&self, id: ChunkId, hash: &ContentHash, vector: &[f32]) -> Result<Row> {
        let (quantized, scale) = quantize(vector, self.shared.dimensions)?;
        Ok(Row {
            id,
            hash: *hash.as_bytes(),
            scale,
            quantized,
        })
    }

    fn upsert_rows(&self, rows: Vec<Row>) -> Result<()> {
        if rows.is_empty() {
            return Ok(());
        }
        let shared = &self.shared;
        let _writer = shared.writer.lock();
        shared.db.transaction(|tx| {
            let mut statement = tx.prepare_cached(
                "INSERT INTO embed_vectors
                     (chunk_id, model_id, content_hash, dimensions, scale, vector)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT (chunk_id) DO UPDATE SET
                     model_id = excluded.model_id, content_hash = excluded.content_hash,
                     dimensions = excluded.dimensions, scale = excluded.scale,
                     vector = excluded.vector",
            )?;
            let mut blob = Vec::with_capacity(shared.dimensions);
            for row in &rows {
                blob.clear();
                blob.extend(row.quantized.iter().map(|value| *value as u8));
                statement.execute(params![
                    row.id.get(),
                    shared.model_id,
                    &row.hash[..],
                    shared.dimensions as i64,
                    f64::from(row.scale),
                    blob,
                ])?;
            }
            Ok(())
        })?;
        let mut state = shared.state.write();
        for row in &rows {
            let mut prefix = [0_u8; 16];
            prefix.copy_from_slice(&row.hash[..16]);
            state.store(row.id, prefix, row.scale, &row.quantized);
        }
        Ok(())
    }

    /// Removes chunks from the database (whatever model their vector belongs to) and from memory.
    /// Unknown ids are ignored. Returns how many vectors left the in-memory index; their slots are
    /// reused by later insertions.
    pub fn remove(&self, ids: &[ChunkId]) -> Result<usize> {
        if ids.is_empty() {
            return Ok(0);
        }
        let shared = &self.shared;
        let _writer = shared.writer.lock();
        shared.db.transaction(|tx| {
            let mut statement =
                tx.prepare_cached("DELETE FROM embed_vectors WHERE chunk_id = ?1")?;
            for id in ids {
                statement.execute(params![id.get()])?;
            }
            Ok(())
        })?;
        let mut state = shared.state.write();
        let mut removed = 0;
        for id in ids {
            if let Some(slot) = state.slots.remove(id) {
                state.release(slot, shared.dimensions);
                removed += 1;
            }
        }
        Ok(removed)
    }

    /// Number of stored vectors that do not belong to the current model (or have another length).
    pub fn stale_count(&self) -> Result<u64> {
        let shared = &self.shared;
        shared.db.read(|conn| {
            let count: i64 = conn.query_row(
                "SELECT COUNT(*) FROM embed_vectors WHERE model_id <> ?1 OR dimensions <> ?2",
                params![shared.model_id, shared.dimensions as i64],
                |row| row.get(0),
            )?;
            Ok(u64::try_from(count).unwrap_or(0))
        })
    }

    /// Deletes every stale vector from the database and returns how many were deleted.
    pub fn purge_stale(&self) -> Result<u64> {
        let shared = &self.shared;
        let _writer = shared.writer.lock();
        shared.db.transaction(|tx| {
            let deleted = tx.execute(
                "DELETE FROM embed_vectors WHERE model_id <> ?1 OR dimensions <> ?2",
                params![shared.model_id, shared.dimensions as i64],
            )?;
            Ok(deleted as u64)
        })
    }

    /// The `k` vectors with the highest dot product with `query`, best first (ties broken by
    /// ascending chunk id), restricted to chunks accepted by `filter` when one is given.
    ///
    /// The scan is exact (every vector is scored) over the quantised representation. Returns fewer
    /// than `k` hits when fewer vectors qualify, and nothing for `k == 0`, an empty index or an
    /// all-zero query. Fails with [`Error::Invalid`] when the query length differs from
    /// [`Self::dimensions`] or a component is not finite.
    ///
    /// Large indexes are scanned in parallel on the rayon pool; call from a blocking context.
    pub fn search(
        &self,
        query: &[f32],
        k: usize,
        filter: Option<VectorFilter<'_>>,
    ) -> Result<Vec<SearchHit>> {
        let dimensions = self.shared.dimensions;
        if query.len() != dimensions {
            return Err(Error::invalid(format!(
                "query has {} dimensions, the index has {dimensions}",
                query.len()
            )));
        }
        let mut max = 0.0_f32;
        for value in query {
            if !value.is_finite() {
                return Err(Error::invalid("query contains a non-finite component"));
            }
            max = max.max(value.abs());
        }
        if k == 0 || max <= 0.0 {
            return Ok(Vec::new());
        }
        let factor = QUERY_LEVELS / max;
        let quantized: Vec<i16> = query
            .iter()
            .map(|value| (value * factor).round() as i16)
            .collect();
        let query_scale = max / QUERY_LEVELS;

        let state = self.shared.state.read();
        let top = if state.data.len() < PARALLEL_CELLS {
            state.scan(dimensions, &quantized, 0, &state.data, k, filter)
        } else {
            state
                .data
                .par_chunks(BLOCK_ROWS * dimensions)
                .enumerate()
                .map(|(block, rows)| {
                    state.scan(dimensions, &quantized, block * BLOCK_ROWS, rows, k, filter)
                })
                .reduce(|| TopK::new(k), TopK::merge)
        };
        drop(state);
        Ok(top
            .heap
            .into_sorted_vec()
            .into_iter()
            .map(|candidate| SearchHit {
                id: candidate.0.id,
                score: candidate.0.score * query_scale,
            })
            .collect())
    }

    /// [`Self::search`] on Tokio's blocking pool.
    pub async fn search_async(
        &self,
        query: Vec<f32>,
        k: usize,
        filter: Option<Arc<dyn Fn(ChunkId) -> bool + Send + Sync>>,
    ) -> Result<Vec<SearchHit>> {
        let index = self.clone();
        tokio::task::spawn_blocking(move || match filter {
            Some(filter) => index.search(&query, k, Some(&*filter)),
            None => index.search(&query, k, None),
        })
        .await?
    }
}

/// A validated, quantised vector ready to be written.
struct Row {
    id: ChunkId,
    hash: [u8; 32],
    scale: f32,
    quantized: Vec<i8>,
}

#[cfg(test)]
pub(crate) mod tests {
    use std::time::Instant;

    use super::*;
    use crate::base::MigrationSet;

    pub(crate) const SCHEMA: &[MigrationSet] = &[MigrationSet {
        module: "embed",
        migrations: crate::engine::embed::MIGRATIONS,
    }];

    /// SplitMix64: tiny deterministic generator for test vectors.
    pub(crate) struct Rng(pub u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            z ^ (z >> 31)
        }

        pub(crate) fn unit(&mut self, dimensions: usize) -> Vec<f32> {
            let mut v: Vec<f32> = (0..dimensions)
                .map(|_| (self.next() >> 40) as f32 / (1u64 << 23) as f32 - 1.0)
                .collect();
            crate::engine::embed::l2_normalize(&mut v);
            v
        }
    }

    fn hash(n: u64) -> ContentHash {
        ContentHash::of(&n.to_le_bytes())
    }

    fn memory_index(dimensions: usize) -> (Db, VectorIndex) {
        let db = Db::open_in_memory_with(SCHEMA).unwrap();
        let index = VectorIndex::load(&db, "test-model", dimensions).unwrap();
        (db, index)
    }

    fn fill(index: &VectorIndex, rng: &mut Rng, count: usize) -> Vec<Vec<f32>> {
        let vectors: Vec<Vec<f32>> = (0..count).map(|_| rng.unit(index.dimensions())).collect();
        let items: Vec<_> = vectors
            .iter()
            .enumerate()
            .map(|(i, v)| (ChunkId(i as i64), hash(i as u64), v.clone()))
            .collect();
        for batch in items.chunks(10_000) {
            index.upsert_many(batch).unwrap();
        }
        vectors
    }

    fn exact_top(vectors: &[Vec<f32>], query: &[f32], k: usize) -> Vec<ChunkId> {
        let mut scored: Vec<(f32, i64)> = vectors
            .iter()
            .enumerate()
            .map(|(i, v)| (v.iter().zip(query).map(|(a, b)| a * b).sum(), i as i64))
            .collect();
        scored.sort_by(|a, b| b.0.total_cmp(&a.0));
        scored.iter().take(k).map(|(_, i)| ChunkId(*i)).collect()
    }

    #[test]
    fn empty_index_and_oversized_k() {
        let (_db, index) = memory_index(8);
        assert!(index.is_empty());
        let query = Rng(1).unit(8);
        assert!(index.search(&query, 5, None).unwrap().is_empty());

        let mut rng = Rng(2);
        fill(&index, &mut rng, 3);
        let hits = index.search(&query, 50, None).unwrap();
        assert_eq!(hits.len(), 3);
        assert!(hits.windows(2).all(|w| w[0].score >= w[1].score));
        assert!(index.search(&query, 0, None).unwrap().is_empty());
        assert!(index.search(&[0.0; 8], 5, None).unwrap().is_empty());
    }

    #[test]
    fn dimension_mismatch_and_bad_vectors_are_errors() {
        let (db, index) = memory_index(8);
        let err = index.upsert(ChunkId(1), hash(1), &[1.0; 7]).unwrap_err();
        assert!(matches!(err, Error::Invalid(_)), "{err}");
        assert!(index.upsert(ChunkId(1), hash(1), &[0.0; 8]).is_err());
        assert!(index.upsert(ChunkId(1), hash(1), &[f32::NAN; 8]).is_err());
        assert!(matches!(
            index.search(&[1.0; 9], 3, None),
            Err(Error::Invalid(_))
        ));
        assert!(index.search(&[f32::INFINITY; 8], 3, None).is_err());
        // A batch with one bad vector writes nothing.
        let batch = vec![
            (ChunkId(1), hash(1), vec![1.0; 8]),
            (ChunkId(2), hash(2), vec![1.0; 3]),
        ];
        assert!(index.upsert_many(&batch).is_err());
        assert!(index.is_empty());
        assert!(VectorIndex::load(&db, "m", 0).is_err());
        assert!(VectorIndex::load(&db, "m", MAX_DIMENSIONS + 1).is_err());
    }

    #[test]
    fn scores_approximate_cosine_and_self_is_nearest() {
        let (_db, index) = memory_index(384);
        let mut rng = Rng(3);
        let vectors = fill(&index, &mut rng, 200);
        let hits = index.search(&vectors[17], 3, None).unwrap();
        assert_eq!(hits[0].id, ChunkId(17));
        assert!((hits[0].score - 1.0).abs() < 0.01, "{}", hits[0].score);
        let exact: f32 = vectors[17]
            .iter()
            .zip(&vectors[hits[1].id.get() as usize])
            .map(|(a, b)| a * b)
            .sum();
        assert!((hits[1].score - exact).abs() < 0.005);
    }

    #[test]
    fn quantised_top10_matches_exact_search() {
        let (_db, index) = memory_index(384);
        let mut rng = Rng(4);
        // Above the parallel threshold, so the rayon path and its merge are what is checked.
        let vectors = fill(&index, &mut rng, 12_000);
        for _ in 0..5 {
            let query = rng.unit(384);
            let expected = exact_top(&vectors, &query, 10);
            let hits = index.search(&query, 10, None).unwrap();
            let shared = hits.iter().filter(|h| expected.contains(&h.id)).count();
            assert!(shared >= 9, "only {shared} of the exact top-10 found");
        }
    }

    #[test]
    fn filter_restricts_results() {
        let (_db, index) = memory_index(32);
        let mut rng = Rng(5);
        let vectors = fill(&index, &mut rng, 300);
        let query = rng.unit(32);
        let even = |id: ChunkId| id.get() % 2 == 0;
        let hits = index.search(&query, 10, Some(&even)).unwrap();
        assert_eq!(hits.len(), 10);
        assert!(hits.iter().all(|hit| hit.id.get() % 2 == 0));
        let even_vectors: Vec<Vec<f32>> = vectors.iter().step_by(2).cloned().collect();
        let best = exact_top(&even_vectors, &query, 1)[0];
        assert_eq!(hits[0].id, ChunkId(best.get() * 2));
        let none = |_: ChunkId| false;
        assert!(index.search(&query, 10, Some(&none)).unwrap().is_empty());
    }

    #[test]
    fn removal_frees_slots_for_reuse() {
        let (_db, index) = memory_index(16);
        let mut rng = Rng(6);
        let vectors = fill(&index, &mut rng, 100);
        let gone: Vec<ChunkId> = (0..50).map(ChunkId).collect();
        assert_eq!(index.remove(&gone).unwrap(), 50);
        assert_eq!(index.remove(&gone).unwrap(), 0);
        assert_eq!(index.len(), 50);
        assert!(!index.contains(ChunkId(10), &hash(10)));
        let hits = index.search(&vectors[10], 100, None).unwrap();
        assert_eq!(hits.len(), 50);
        assert!(hits.iter().all(|hit| hit.id.get() >= 50));

        for i in 1000..1050 {
            index
                .upsert(ChunkId(i), hash(i as u64), &rng.unit(16))
                .unwrap();
        }
        assert_eq!(index.len(), 100);
        let state = index.shared.state.read();
        assert_eq!(state.ids.len(), 100, "freed slots must be reused");
        assert_eq!(state.data.len(), 100 * 16);
        assert!(state.free.is_empty());
    }

    #[test]
    fn upsert_replaces_and_contains_tracks_the_hash() {
        let (_db, index) = memory_index(16);
        let mut rng = Rng(7);
        let (a, b) = (rng.unit(16), rng.unit(16));
        index.upsert(ChunkId(1), hash(1), &a).unwrap();
        assert!(index.contains(ChunkId(1), &hash(1)));
        assert!(!index.contains(ChunkId(1), &hash(2)));
        assert!(!index.contains(ChunkId(2), &hash(1)));
        index.upsert(ChunkId(1), hash(2), &b).unwrap();
        assert_eq!(index.len(), 1);
        assert!(index.contains(ChunkId(1), &hash(2)));
        let hit = index.search(&b, 1, None).unwrap()[0];
        assert!((hit.score - 1.0).abs() < 0.01);
    }

    #[tokio::test]
    async fn persistence_round_trip_and_stale_models() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("anchor.db");
        let mut rng = Rng(8);
        let query = rng.unit(64);
        let expected = {
            let db = Db::open_with(&path, SCHEMA).unwrap();
            let index = VectorIndex::load(&db, "model-a", 64).unwrap();
            fill(&index, &mut rng, 500);
            index.remove(&[ChunkId(3)]).unwrap();
            index.search(&query, 10, None).unwrap()
        };

        let db = Db::open_with(&path, SCHEMA).unwrap();
        let reloaded = VectorIndex::load_async(&db, "model-a", 64).await.unwrap();
        assert_eq!(reloaded.len(), 499);
        assert!(reloaded.contains(ChunkId(4), &hash(4)));
        assert_eq!(reloaded.stale_count().unwrap(), 0);
        let hits = reloaded
            .search_async(query.clone(), 10, None)
            .await
            .unwrap();
        assert_eq!(hits, expected);

        // Another model sees none of these vectors: they are stale until re-embedded or purged.
        let other = VectorIndex::load(&db, "model-b", 64).unwrap();
        assert!(other.is_empty());
        assert_eq!(other.stale_count().unwrap(), 499);
        assert!(!other.contains(ChunkId(4), &hash(4)));
        other.upsert(ChunkId(4), hash(4), &rng.unit(64)).unwrap();
        assert_eq!(other.stale_count().unwrap(), 498);
        assert_eq!(other.purge_stale().unwrap(), 498);
        assert_eq!(other.stale_count().unwrap(), 0);
        assert_eq!(VectorIndex::load(&db, "model-a", 64).unwrap().len(), 0);
        assert_eq!(VectorIndex::load(&db, "model-b", 64).unwrap().len(), 1);
        // Same model, different length: also stale.
        assert_eq!(
            VectorIndex::load(&db, "model-b", 32)
                .unwrap()
                .stale_count()
                .unwrap(),
            1
        );
    }

    #[test]
    fn concurrent_readers_with_one_writer() {
        let (_db, index) = memory_index(32);
        let mut rng = Rng(9);
        fill(&index, &mut rng, 200);
        let query = rng.unit(32);
        std::thread::scope(|scope| {
            for _ in 0..4 {
                scope.spawn(|| {
                    for _ in 0..200 {
                        let hits = index.search(&query, 5, None).unwrap();
                        assert_eq!(hits.len(), 5);
                    }
                });
            }
            let mut writer_rng = Rng(10);
            for i in 200..400 {
                index
                    .upsert(ChunkId(i), hash(i as u64), &writer_rng.unit(32))
                    .unwrap();
                index.remove(&[ChunkId(i - 200)]).unwrap();
            }
        });
        assert_eq!(index.len(), 200);
    }

    /// Fills the in-memory state directly: going through SQLite would dominate a speed test.
    fn synthetic(count: usize, dimensions: usize) -> VectorIndex {
        let (_db, index) = memory_index(dimensions);
        let mut rng = Rng(11);
        let mut state = index.shared.state.write();
        for i in 0..count {
            let row: Vec<i8> = (0..dimensions).map(|_| (rng.next() >> 56) as i8).collect();
            state.store(ChunkId(i as i64), [0; 16], 1.0 / 127.0, &row);
        }
        drop(state);
        index
    }

    fn best_of(index: &VectorIndex, runs: usize) -> std::time::Duration {
        let query = Rng(12).unit(index.dimensions());
        (0..runs)
            .map(|_| {
                let started = Instant::now();
                let hits = index.search(&query, 10, None).unwrap();
                assert_eq!(hits.len(), 10);
                started.elapsed()
            })
            .min()
            .unwrap()
    }

    #[test]
    fn search_over_100k_vectors_is_fast() {
        let index = synthetic(100_000, 384);
        let elapsed = best_of(&index, 5);
        println!("100k x 384 search: {elapsed:?}");
        // Target is 30 ms in release builds; the bound is generous because the test profile
        // compiles this crate at opt-level 1 (no auto-vectorisation) and CI machines are shared.
        assert!(elapsed.as_millis() < 300, "search took {elapsed:?}");
    }

    /// `cargo test --release -- --ignored million` measures the headline target (about 400 MB).
    #[test]
    #[ignore = "allocates ~400 MB; run explicitly, ideally with --release"]
    fn search_over_one_million_vectors() {
        let index = synthetic(1_000_000, 384);
        let elapsed = best_of(&index, 5);
        println!(
            "1M x 384 search: {elapsed:?}, memory {} MB",
            index.memory_bytes() >> 20
        );
        assert!(elapsed.as_millis() < 600, "search took {elapsed:?}");
    }
}
