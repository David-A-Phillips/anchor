//! Depth as a materialisation view: units are stored and synced whole; a device materialises
//! only what its depth for the source allows.

use std::collections::HashSet;

use crate::base::{ChunkKind, EdgeKind, NodeKey, NodeKind, NodeRef, Unit, UnitKind, attrs};
use crate::config::Depth;

/// Whether units of `kind` are materialised at `depth`. Phone-book depth keeps the service, its
/// owners, the org model and API specs (they are the phone book); map and full keep everything.
pub fn unit_in_depth(kind: UnitKind, depth: Depth) -> bool {
    match depth {
        Depth::PhoneBook => matches!(
            kind,
            UnitKind::RepoMeta | UnitKind::Ownership | UnitKind::OrgModel | UnitKind::ApiSpec
        ),
        Depth::Map | Depth::Full => true,
    }
}

/// Thins `unit` to what `depth` materialises. Full leaves it unchanged. Below full, files keep
/// what exists and where (public types, no call graph, no code previews) and tickets lose their
/// discussion threads and comments; other kinds are unchanged.
pub fn thin_unit(unit: &mut Unit, depth: Depth) {
    if depth >= Depth::Full {
        return;
    }
    match unit.kind {
        UnitKind::File => thin_file(unit),
        UnitKind::Ticket => {
            let dropped: HashSet<NodeKey> = unit
                .nodes
                .iter()
                .filter(|node| matches!(node.kind, NodeKind::Thread | NodeKind::Comment))
                .map(|node| node.key.clone())
                .collect();
            drop_nodes(unit, &dropped);
        }
        _ => {}
    }
}

/// The view of a unit at `depth`, in the shape the store takes: `None` when the depth does not
/// materialise the unit at all.
pub fn view(depth: Depth) -> impl Fn(&Unit) -> Option<Unit> {
    move |unit| {
        unit_in_depth(unit.kind, depth).then(|| {
            let mut unit = unit.clone();
            thin_unit(&mut unit, depth);
            unit
        })
    }
}

fn drop_nodes(unit: &mut Unit, dropped: &HashSet<NodeKey>) {
    if dropped.is_empty() {
        return;
    }
    let gone =
        |reference: &NodeRef| matches!(reference, NodeRef::Key(key) if dropped.contains(key));
    unit.nodes.retain(|node| !dropped.contains(&node.key));
    unit.edges
        .retain(|edge| !gone(&edge.from) && !gone(&edge.to));
    unit.chunks.retain(|chunk| !dropped.contains(&chunk.node));
}

fn thin_file(unit: &mut Unit) {
    // Map depth: what exists and where, not how it is wired or implemented.
    unit.edges.retain(|edge| {
        !matches!(
            edge.kind,
            EdgeKind::Calls | EdgeKind::References | EdgeKind::Imports
        )
    });
    unit.chunks.retain(|chunk| chunk.kind != ChunkKind::Code);
    // Keep the outline someone outside the file would look for: public types. Functions,
    // methods and private items appear when the source is deepened.
    let dropped: HashSet<NodeKey> = unit
        .nodes
        .iter()
        .filter(|node| {
            let attr = |name: &str| node.attrs.get(name).and_then(|v| v.as_str());
            let outline = matches!(
                attr(attrs::SYMBOL_KIND),
                Some(
                    "struct"
                        | "enum"
                        | "trait"
                        | "interface"
                        | "class"
                        | "module"
                        | "type_alias"
                        | "union"
                )
            );
            node.kind == NodeKind::Symbol && !(outline && attr(attrs::VISIBILITY) == Some("public"))
        })
        .map(|node| node.key.clone())
        .collect();
    drop_nodes(unit, &dropped);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::base::{EdgeAssertion, NodeAssertion, SourceId, keys};

    fn node(kind: NodeKind, key: &str, symbol: Option<(&str, &str)>) -> NodeAssertion {
        let mut node = NodeAssertion::new(kind, NodeKey::new(key), key);
        if let Some((sk, vis)) = symbol {
            node.attrs.insert(attrs::SYMBOL_KIND.into(), sk.into());
            node.attrs.insert(attrs::VISIBILITY.into(), vis.into());
        }
        node
    }

    #[test]
    fn phone_book_keeps_only_directory_kinds() {
        for kind in [
            UnitKind::RepoMeta,
            UnitKind::Ownership,
            UnitKind::OrgModel,
            UnitKind::ApiSpec,
        ] {
            assert!(unit_in_depth(kind, Depth::PhoneBook), "{kind:?}");
        }
        for kind in [
            UnitKind::File,
            UnitKind::WikiPage,
            UnitKind::Ticket,
            UnitKind::PullRequest,
        ] {
            assert!(!unit_in_depth(kind, Depth::PhoneBook), "{kind:?}");
            assert!(unit_in_depth(kind, Depth::Map));
            assert!(unit_in_depth(kind, Depth::Full));
        }
    }

    #[test]
    fn thinning_files_and_tickets() {
        let source = SourceId::new("s");
        let mut file = Unit::new(
            keys::unit(&source, UnitKind::File, "a.rs"),
            source.clone(),
            UnitKind::File,
        );
        file.nodes.push(node(
            NodeKind::Symbol,
            "sym:pub_struct",
            Some(("struct", "public")),
        ));
        file.nodes.push(node(
            NodeKind::Symbol,
            "sym:func",
            Some(("function", "public")),
        ));
        file.add_edge(EdgeAssertion::new(
            NodeKey::new("sym:func"),
            EdgeKind::Calls,
            NodeKey::new("sym:pub_struct"),
        ));
        let mut thin = file.clone();
        thin_unit(&mut thin, Depth::Full);
        assert_eq!(thin, file);
        thin_unit(&mut thin, Depth::Map);
        let names: Vec<_> = thin.nodes.iter().map(|n| n.key.as_str()).collect();
        assert_eq!(names, ["sym:pub_struct"]);
        assert!(thin.edges.is_empty());

        let mut ticket = Unit::new(
            keys::unit(&source, UnitKind::Ticket, "T-1"),
            source.clone(),
            UnitKind::Ticket,
        );
        ticket
            .nodes
            .push(node(NodeKind::Ticket, "ticket:T-1", None));
        ticket.nodes.push(node(NodeKind::Thread, "thread:1", None));
        ticket
            .nodes
            .push(node(NodeKind::Comment, "comment:1", None));
        ticket.add_edge(EdgeAssertion::new(
            NodeKey::new("thread:1"),
            EdgeKind::Contains,
            NodeKey::new("comment:1"),
        ));
        thin_unit(&mut ticket, Depth::Map);
        assert_eq!(ticket.nodes.len(), 1);
        assert!(ticket.edges.is_empty());
    }
}
