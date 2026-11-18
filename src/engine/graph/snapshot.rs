//! The in-memory graph saved to disk, so start-up reads one file instead of rebuilding the graph
//! row by row from SQLite.
//!
//! A snapshot records the store epoch and graph version it was taken at. On open, the engine
//! loads it and replays the store's delta log from that version onwards; when the epoch differs
//! (another database) or the log no longer reaches back that far, it rebuilds from SQLite
//! instead. A snapshot is therefore only ever a cache: deleting it is always safe.

use std::io::Write;
use std::path::Path;

use super::context::ContextGraph;
use crate::base::model::graph_codec::{self, EDGE_BYTES, NODE_BYTES, Reader};
use crate::base::{Error, Result};

const MAGIC: &[u8; 8] = b"ANCHORG1";

/// Writes `graph` to `path` atomically (a temporary file renamed into place). Returns the bytes
/// written.
pub fn save_snapshot(graph: &ContextGraph, path: &Path) -> Result<u64> {
    let (epoch, version) = graph.stamp();
    let mut out = Vec::with_capacity(
        64 + epoch.len() + graph.node_count() * NODE_BYTES + graph.edge_count() * EDGE_BYTES,
    );
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&(epoch.len() as u64).to_le_bytes());
    out.extend_from_slice(epoch.as_bytes());
    out.extend_from_slice(&version.to_le_bytes());
    out.extend_from_slice(&(graph.node_count() as u64).to_le_bytes());
    for node in graph.nodes() {
        graph_codec::put_node(&mut out, &node);
    }
    out.extend_from_slice(&(graph.edge_count() as u64).to_le_bytes());
    for edge in graph.edges() {
        graph_codec::put_edge(&mut out, &edge);
    }
    let checksum = blake3::hash(&out);
    out.extend_from_slice(&checksum.as_bytes()[..8]);

    let dir = path
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let mut file = tempfile::NamedTempFile::new_in(dir)?;
    file.write_all(&out)?;
    file.as_file().sync_all()?;
    file.persist(path).map_err(|err| Error::Io(err.error))?;
    Ok(out.len() as u64)
}

/// Reads a snapshot written by [`save_snapshot`]. `Ok(None)` when there is no file; an error
/// when it is corrupt or from another format.
pub fn load_snapshot(path: &Path) -> Result<Option<ContextGraph>> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err.into()),
    };
    if bytes.len() < MAGIC.len() + 8 || &bytes[..MAGIC.len()] != MAGIC {
        return Err(Error::other("not an Anchor graph snapshot"));
    }
    let (body, checksum) = bytes.split_at(bytes.len() - 8);
    if blake3::hash(body).as_bytes()[..8] != *checksum {
        return Err(Error::other("graph snapshot checksum mismatch"));
    }
    let mut reader = Reader::new(&body[MAGIC.len()..]);
    let epoch_len = reader.count(1)?;
    let epoch = String::from_utf8(reader.take(epoch_len)?.to_vec())
        .map_err(|_| Error::other("graph snapshot epoch is not UTF-8"))?;
    let version = reader.u64()?;
    let mut nodes = Vec::with_capacity(reader.count(NODE_BYTES)?.min(1 << 24));
    for _ in 0..nodes.capacity() {
        nodes.push(reader.node()?);
    }
    let edge_count = reader.count(EDGE_BYTES)?;
    let mut edges = Vec::with_capacity(edge_count);
    for _ in 0..edge_count {
        edges.push(reader.edge()?);
    }
    if !reader.is_empty() {
        return Err(Error::other("trailing bytes in graph snapshot"));
    }
    let mut graph = ContextGraph::from_parts(nodes, edges);
    graph.set_stamp(epoch, version);
    Ok(Some(graph))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::base::{EdgeId, EdgeKind, GraphEdge, GraphNode, NodeId, NodeKind, TrustLevel};

    fn sample() -> ContextGraph {
        let mut graph = ContextGraph::from_parts(
            (1..=3).map(|id| GraphNode {
                id: NodeId(id),
                kind: NodeKind::Service,
                trust: TrustLevel::SourceOfTruth,
            }),
            [GraphEdge {
                id: EdgeId(10),
                from: NodeId(1),
                to: NodeId(2),
                kind: EdgeKind::DependsOn,
                trust: TrustLevel::ReferenceOnly,
                weight: 0.5,
            }],
        );
        graph.set_stamp("epoch-a", 42);
        graph
    }

    fn sorted(graph: &ContextGraph) -> (Vec<GraphNode>, Vec<GraphEdge>) {
        let mut nodes: Vec<_> = graph.nodes().collect();
        let mut edges: Vec<_> = graph.edges().collect();
        nodes.sort_by_key(|n| n.id);
        edges.sort_by_key(|e| e.id);
        (nodes, edges)
    }

    #[test]
    fn snapshots_round_trip_with_their_stamp() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("graph.snapshot");
        assert!(load_snapshot(&path).expect("missing is fine").is_none());
        let graph = sample();
        assert!(save_snapshot(&graph, &path).expect("save") > 0);
        let loaded = load_snapshot(&path).expect("load").expect("present");
        assert_eq!(loaded.stamp(), ("epoch-a", 42));
        assert_eq!(sorted(&loaded), sorted(&graph));
    }

    #[test]
    fn corrupt_or_foreign_snapshots_are_rejected() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("graph.snapshot");
        save_snapshot(&sample(), &path).expect("save");
        let mut bytes = std::fs::read(&path).expect("read");
        let middle = bytes.len() / 2;
        bytes[middle] ^= 0xff;
        std::fs::write(&path, &bytes).expect("write");
        assert!(load_snapshot(&path).is_err());
        std::fs::write(&path, b"something else entirely").expect("write");
        assert!(load_snapshot(&path).is_err());
    }
}
