//! Change impact and hotspots: two questions about the shape of the graph rather than about one
//! entity.
//!
//! * [`Engine::impact`] takes many changed things at once (files, symbols, services, APIs: a
//!   whole diff) and returns everything that depends on any of them, with the path that makes it
//!   depend, and the teams or people who own what is affected. One multi-source search serves
//!   every target.
//! * [`Engine::hotspots`] ranks entities by how many others depend on them and reports who owns
//!   them, flagging the risky ones: depended on by many and owned by nobody, by one person, or
//!   only according to reference-only sources. It also ranks owners by how much they own.
//!
//! Both read the in-memory graph and the store at one version ([`GraphStore::consistent`]) and
//! respect the traversal budgets ([`Limits::max_visits`], [`QueryConfig::timeout_ms`]); a report
//! cut short says so in `truncated`.
//!
//! [`GraphStore::consistent`]: super::store::GraphStore::consistent
//! [`QueryConfig::timeout_ms`]: super::query::QueryConfig::timeout_ms

use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use super::core::Engine;
use super::graph::{
    ContextGraph, Direction, Goal, Limits, Path, Pathfinder, Seed, TraversalProfile,
};
use crate::base::{EdgeKind, Error, Lookup, NodeId, NodeKey, NodeKind, Result, TrustLevel};
use crate::engine::store::Resolution;

/// Edges meaning "`from` uses `to`".
const DEPENDENCY_EDGES: &[EdgeKind] = &[
    EdgeKind::Calls,
    EdgeKind::References,
    EdgeKind::Imports,
    EdgeKind::DependsOn,
    EdgeKind::Consumes,
];

/// Kinds a target named by text may resolve to, most likely first.
const TARGET_KINDS: &[NodeKind] = &[
    NodeKind::Service,
    NodeKind::ApiSpec,
    NodeKind::ApiEndpoint,
    NodeKind::Component,
    NodeKind::Domain,
    NodeKind::Portfolio,
    NodeKind::File,
    NodeKind::Symbol,
    NodeKind::Team,
    NodeKind::Person,
];

/// Kinds a target stands for by what it owns or contains rather than by itself: a team, a
/// person, or a grouping of services. Such a target is replaced by the services, APIs and files
/// under it.
const GROUP_KINDS: &[NodeKind] = &[
    NodeKind::Team,
    NodeKind::Person,
    NodeKind::Portfolio,
    NodeKind::Domain,
    NodeKind::Component,
];

/// Kinds a group target expands to.
const MEMBER_KINDS: &[NodeKind] = &[NodeKind::Service, NodeKind::ApiSpec, NodeKind::File];

/// Entities a group target may stand for.
const MAX_MEMBERS: usize = 500;

/// Kinds whose owner an impact report looks up.
const OWNED_KINDS: &[NodeKind] = &[
    NodeKind::Service,
    NodeKind::Component,
    NodeKind::ApiSpec,
    NodeKind::File,
];

/// Granularities hotspots can be counted at.
pub const HOTSPOT_KINDS: &[NodeKind] = &[
    NodeKind::Service,
    NodeKind::Component,
    NodeKind::ApiSpec,
    NodeKind::ApiEndpoint,
    NodeKind::File,
    NodeKind::Symbol,
];

/// Owner lookups per report, so a very wide impact stays fast.
const MAX_OWNER_LOOKUPS: usize = 200;
/// Containment levels climbed to find an enclosing file or service.
const MAX_CLIMB: usize = 8;
/// Dependents counted per report (those listed are capped by [`ImpactOptions::limit`]).
const MAX_COUNTED: usize = 5_000;

/// An entity as it appears in these reports.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entity {
    /// Natural key.
    pub key: String,
    /// Kind.
    pub kind: NodeKind,
    /// Display name.
    pub name: String,
    /// Trust of the sources asserting it.
    pub trust: TrustLevel,
}

/// One thing asked about.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ImpactTarget {
    /// As given.
    pub input: String,
    /// What it resolved to; `None` when it did not.
    pub entity: Option<Entity>,
    /// Why it did not resolve.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub problem: Option<String>,
    /// For a team, person, component, domain or portfolio: keys of the services, APIs and
    /// files it owns or contains, which are what was searched from.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub covers: Vec<String>,
}

/// Something that depends, directly or transitively, on a target.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Dependent {
    /// The dependent entity.
    pub entity: Entity,
    /// Edges between it and the nearest target.
    pub hops: u32,
    /// The target it depends on.
    pub target: String,
    /// The chain from the target to it, e.g. `file:a:src/x.rs <-calls- symbol:b:...`.
    pub via: String,
    /// `true` when every element of that chain comes from a source of truth.
    pub authoritative: bool,
}

/// An owner and what they own among the targets and their dependents.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct OwnerImpact {
    /// The team or person.
    pub owner: Entity,
    /// Keys of the affected entities they own.
    pub owns: Vec<String>,
    /// `true` when every ownership claim behind this is authoritative.
    pub authoritative: bool,
}

/// What a set of changes may affect.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ImpactReport {
    /// The targets, resolved.
    pub targets: Vec<ImpactTarget>,
    /// Number of dependents of each kind, including those beyond `limit`.
    pub summary: BTreeMap<String, usize>,
    /// Number of dependents found, including those beyond `limit`.
    pub total: usize,
    /// Dependents, fewest hops first, at most `limit` of them.
    pub dependents: Vec<Dependent>,
    /// Who to tell: owners of the targets and of the listed services, components, APIs and
    /// files, most affected first.
    pub owners: Vec<OwnerImpact>,
    /// `true` when the search stopped at its time or visit budget: there may be more
    /// dependents than `total`.
    pub truncated: bool,
    /// What could not be determined.
    pub gaps: Vec<String>,
}

/// Options of [`Engine::impact`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ImpactOptions {
    /// Maximum dependents returned.
    pub limit: usize,
    /// Maximum edges between a target and a dependent.
    pub max_hops: u32,
    /// Only report dependents of these kinds (empty: all).
    pub kinds: Vec<NodeKind>,
}

impl Default for ImpactOptions {
    fn default() -> Self {
        Self {
            limit: 200,
            max_hops: 6,
            kinds: Vec::new(),
        }
    }
}

/// The owner found for a hotspot.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Owner {
    /// The team or person.
    pub owner: Entity,
    /// `true` when the ownership claim comes from sources of truth only.
    pub authoritative: bool,
}

/// An entity many others depend on.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Hotspot {
    /// The entity.
    pub entity: Entity,
    /// Distinct entities of the same granularity that depend on it.
    pub dependents: usize,
    /// Distinct entities of the same granularity it depends on.
    pub depends_on: usize,
    /// Its owner, if any.
    pub owner: Option<Owner>,
    /// Who changed it, from the ingested git history: empty when there is none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub committers: Vec<Committer>,
    /// Ingested commits that changed it.
    #[serde(default)]
    pub commits: usize,
    /// Why it is risky: `no_owner`, `single_person_owner`, `reference_only_owner`,
    /// `single_committer` (every ingested commit to it is by one person).
    pub risks: Vec<String>,
}

/// A person who changed a hotspot, from git history.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Committer {
    /// The person.
    pub person: Entity,
    /// Their commits to it.
    pub commits: usize,
}

/// An owner ranked by how much they own.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct OwnerLoad {
    /// The team or person.
    pub owner: Entity,
    /// Distinct entities they own directly.
    pub owns: usize,
    /// The same, by kind.
    pub by_kind: BTreeMap<String, usize>,
    /// How many of the reported hotspots they own.
    pub hotspots: usize,
}

/// Options of [`Engine::hotspots`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct HotspotOptions {
    /// Granularity: dependencies are counted between entities of this kind (`service`, `file`,
    /// `symbol`, `component`, `api_spec`, `api_endpoint`).
    pub kind: NodeKind,
    /// Maximum hotspots and owners returned.
    pub limit: usize,
    /// Only report entities with at least this many dependents.
    pub min_dependents: usize,
}

impl Default for HotspotOptions {
    fn default() -> Self {
        Self {
            kind: NodeKind::Service,
            limit: 20,
            min_dependents: 1,
        }
    }
}

/// The most depended-on entities and the most loaded owners.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct HotspotReport {
    /// Granularity used.
    pub kind: Option<NodeKind>,
    /// Most depended-on first.
    pub hotspots: Vec<Hotspot>,
    /// Owners by number of entities owned, most first.
    pub owners: Vec<OwnerLoad>,
    /// Hotspots reported with no owner at all.
    pub unowned: usize,
    /// Hotspots whose nearest owner is a single person rather than a team.
    pub single_person: usize,
    /// Hotspots that only one person has ever changed, according to the ingested history.
    #[serde(default)]
    pub single_committer: usize,
    /// `true` when git history was ingested for any hotspot, so the committer figures mean
    /// something.
    #[serde(default)]
    pub history: bool,
    /// `true` when counting stopped at the time budget.
    pub truncated: bool,
}

/// The services, APIs and files a group target stands for: what it owns (a team or person) or
/// contains (a portfolio, domain or component), descending through owned or contained groups,
/// nearest first, at most [`MAX_MEMBERS`].
fn members(graph: &ContextGraph, group: NodeId) -> Vec<NodeId> {
    let mut found = Vec::new();
    let mut seen = HashSet::from([group]);
    let mut queue = std::collections::VecDeque::from([group]);
    while let Some(node) = queue.pop_front() {
        for neighbor in graph.neighbors(node) {
            if neighbor.direction != Direction::Forward
                || !matches!(neighbor.edge.kind, EdgeKind::Owns | EdgeKind::Contains)
                || !seen.insert(neighbor.node)
            {
                continue;
            }
            let Some(data) = graph.node(neighbor.node) else {
                continue;
            };
            if MEMBER_KINDS.contains(&data.kind) {
                found.push(neighbor.node);
                if found.len() >= MAX_MEMBERS {
                    return found;
                }
            } else if matches!(
                data.kind,
                NodeKind::Portfolio | NodeKind::Domain | NodeKind::Component
            ) {
                queue.push_back(neighbor.node);
            }
        }
    }
    found
}

/// Formats a path as `a <-calls- b -contains-> c` using node keys.
fn path_text(path: &Path, keys: &HashMap<NodeId, (NodeKey, String)>) -> String {
    let key = |id: &NodeId| {
        keys.get(id)
            .map_or_else(|| format!("#{}", id.0), |(key, _)| key.to_string())
    };
    let mut out = path.nodes.first().map(key).unwrap_or_default();
    for (step, node) in path.steps.iter().zip(path.nodes.iter().skip(1)) {
        match step.direction {
            Direction::Forward => out.push_str(&format!(" -{}-> ", step.kind)),
            Direction::Reverse => out.push_str(&format!(" <-{}- ", step.kind)),
        }
        out.push_str(&key(node));
    }
    out
}

fn entity(
    graph: &ContextGraph,
    names: &HashMap<NodeId, (NodeKey, String)>,
    id: NodeId,
) -> Option<Entity> {
    let node = graph.node(id)?;
    let (key, name) = names.get(&id)?;
    Some(Entity {
        key: key.to_string(),
        kind: node.kind,
        name: name.clone(),
        trust: node.trust,
    })
}

/// The nearest enclosing node of `kind` (the node itself when it has that kind), climbing
/// containment; for services, an API or endpoint leads to the service implementing it.
fn lift(
    graph: &ContextGraph,
    memo: &mut HashMap<NodeId, Option<NodeId>>,
    node: NodeId,
    kind: NodeKind,
) -> Option<NodeId> {
    if let Some(known) = memo.get(&node) {
        return *known;
    }
    let mut current = node;
    let mut found = None;
    for _ in 0..MAX_CLIMB {
        let Some(data) = graph.node(current) else {
            break;
        };
        if data.kind == kind {
            found = Some(current);
            break;
        }
        let mut parent = None;
        for neighbor in graph.neighbors(current) {
            if neighbor.direction != Direction::Reverse {
                continue;
            }
            match neighbor.edge.kind {
                EdgeKind::Contains => {
                    parent = Some(neighbor.node);
                    break;
                }
                EdgeKind::Implements
                    if kind == NodeKind::Service
                        && matches!(data.kind, NodeKind::ApiSpec | NodeKind::ApiEndpoint) =>
                {
                    parent = parent.or(Some(neighbor.node));
                }
                _ => {}
            }
        }
        match parent {
            Some(next) if next != node => current = next,
            _ => break,
        }
    }
    memo.insert(node, found);
    found
}

impl Engine {
    fn traversal_deadline(&self) -> Option<Instant> {
        let millis = self.config().query.timeout_ms;
        (millis > 0).then(|| Instant::now() + Duration::from_millis(millis))
    }

    /// Resolves one target: a node key, a file path (as for `why`), an entity name (a service,
    /// API, component, team or person; when several share the name, the first of those kinds
    /// wins), or a symbol.
    fn resolve_target(&self, input: &str) -> Result<NodeId> {
        let store = self.store();
        let text = input.trim();
        if let Some(node) = store.node_by_key(&NodeKey::new(text))? {
            return Ok(node.id);
        }
        let looks_like_path = text.contains('/') || text.contains('.');
        let mut path_problem = None;
        if looks_like_path {
            match self.locate_target(text) {
                Ok(located) => {
                    return located.node.ok_or_else(|| {
                        Error::not_found(format!(
                            "`{}` (in {}) is not in the graph: it is not ingested yet (`anchor ingest`), or it is not a kind of file Anchor indexes",
                            located.path, located.source
                        ))
                    });
                }
                Err(err) => path_problem = Some(err),
            }
        }
        match store.resolve(&Lookup::new(TARGET_KINDS, text))? {
            Resolution::Resolved(node) => return Ok(node),
            Resolution::Ambiguous(nodes) => {
                let records = store.nodes(&nodes)?;
                for kind in TARGET_KINDS {
                    let of_kind: Vec<_> = records.iter().filter(|n| n.kind == *kind).collect();
                    match of_kind.as_slice() {
                        [only] => return Ok(only.id),
                        [] => {}
                        _ => break,
                    }
                }
                let keys: Vec<String> = records.iter().take(6).map(|n| n.key.to_string()).collect();
                return Err(Error::invalid(format!(
                    "several entities are called `{text}`; use a key: {}",
                    keys.join(", ")
                )));
            }
            Resolution::NotFound => {}
        }
        if let Some(err) = path_problem {
            return Err(err);
        }
        if let Ok(located) = self.locate_target(text)
            && let Some(node) = located.node
        {
            return Ok(node);
        }
        store
            .search_nodes(text, TARGET_KINDS, 1)?
            .into_iter()
            .find(|hit| hit.score >= 0.9)
            .map(|hit| hit.node)
            .ok_or_else(|| {
                Error::not_found(format!(
                    "`{text}` is not a known file, symbol, service, API, team, person or key"
                ))
            })
    }

    /// Everything that depends on any of `targets` (blocking). Targets that do not resolve are
    /// reported, not fatal; an error is returned only when none resolves.
    pub fn impact(&self, targets: &[String], options: &ImpactOptions) -> Result<ImpactReport> {
        if targets.is_empty() {
            return Err(Error::invalid("name at least one file, symbol or entity"));
        }
        let mut report = ImpactReport::default();
        let mut resolved: Vec<(String, NodeId)> = Vec::new();
        for input in targets {
            match self.resolve_target(input) {
                Ok(node) => resolved.push((input.clone(), node)),
                Err(err) => report.targets.push(ImpactTarget {
                    input: input.clone(),
                    entity: None,
                    problem: Some(err.detail()),
                    covers: Vec::new(),
                }),
            }
        }
        if resolved.is_empty() {
            let problems: Vec<String> = report
                .targets
                .iter()
                .filter_map(|t| t.problem.clone())
                .collect();
            return Err(Error::not_found(problems.join("; ")));
        }
        let deadline = self.traversal_deadline();
        let config = self.config().graph.clone();
        self.store().consistent(|graph| {
            let finder = Pathfinder::new(graph, &config);
            let limits = Limits {
                max_hops: options.max_hops,
                max_cost: f32::MAX,
                ..Limits::from(&config)
            }
            .until(deadline);

            // Seeds: the targets, plus the symbols a target file or symbol contains (callers use
            // those, not the file). Each seed remembers the target it stands for.
            let mut seeds: Vec<Seed> = Vec::new();
            let mut origin: HashMap<NodeId, NodeId> = HashMap::new();
            let mut covers: HashMap<NodeId, Vec<NodeId>> = HashMap::new();
            for (_, target) in &resolved {
                if origin.insert(*target, *target).is_none() {
                    seeds.push(Seed::new(*target));
                }
                let mut starts = vec![*target];
                if graph.node(*target).is_some_and(|n| GROUP_KINDS.contains(&n.kind)) {
                    let members = members(graph, *target);
                    if members.len() >= MAX_MEMBERS {
                        report.gaps.push(format!(
                            "a group target was searched from its first {MAX_MEMBERS} services, APIs and files only"
                        ));
                    }
                    for member in &members {
                        if !origin.contains_key(member) {
                            origin.insert(*member, *target);
                            seeds.push(Seed::new(*member));
                        }
                    }
                    starts.extend(members.iter().copied());
                    covers.insert(*target, members);
                }
                let mut stack: Vec<(NodeId, usize)> = starts.into_iter().map(|node| (node, 0)).collect();
                while let Some((node, depth)) = stack.pop() {
                    if depth >= MAX_CLIMB {
                        continue;
                    }
                    for neighbor in graph.neighbors(node) {
                        let is_child = neighbor.edge.kind == EdgeKind::Contains
                            && neighbor.direction == Direction::Forward
                            && graph
                                .node(neighbor.node)
                                .is_some_and(|n| n.kind == NodeKind::Symbol);
                        if is_child && !origin.contains_key(&neighbor.node) {
                            origin.insert(neighbor.node, *target);
                            seeds.push(Seed::new(neighbor.node));
                            stack.push((neighbor.node, depth + 1));
                        }
                    }
                }
            }

            // One search from every seed; each reached node keeps its cheapest origin.
            let paths = finder.reach(&seeds, &TraversalProfile::impact(), limits, MAX_COUNTED);
            report.truncated = finder.truncated() || paths.len() >= MAX_COUNTED;
            let wanted = |kind: NodeKind| options.kinds.is_empty() || options.kinds.contains(&kind);
            let mut paths: Vec<Path> = paths
                .into_iter()
                .filter(|path| {
                    path.target().is_some_and(|node| {
                        !origin.contains_key(&node)
                            && graph.node(node).is_some_and(|n| wanted(n.kind))
                    })
                })
                .collect();
            // Fewest hops first; the search order (cheapest, most trusted) breaks ties.
            paths.sort_by_key(|path| path.steps.len());
            report.total = paths.len();
            for path in &paths {
                if let Some(node) = path.target().and_then(|id| graph.node(id)) {
                    *report.summary.entry(node.kind.as_str().to_owned()).or_default() += 1;
                }
            }
            paths.truncate(options.limit);

            let mut ids: Vec<NodeId> = resolved.iter().map(|(_, id)| *id).collect();
            ids.extend(covers.values().flatten().copied());
            for path in &paths {
                ids.extend(path.nodes.iter().copied());
            }
            ids.sort_unstable();
            ids.dedup();
            let mut names = self.store().names(&ids)?;

            for (input, id) in &resolved {
                report.targets.push(ImpactTarget {
                    input: input.clone(),
                    entity: entity(graph, &names, *id),
                    problem: None,
                    covers: covers
                        .get(id)
                        .map(|members| {
                            members
                                .iter()
                                .filter_map(|member| names.get(member).map(|(key, _)| key.to_string()))
                                .collect()
                        })
                        .unwrap_or_default(),
                });
            }
            for path in &paths {
                let Some(node) = path.target() else { continue };
                let Some(found) = entity(graph, &names, node) else {
                    continue;
                };
                let target = path
                    .nodes
                    .first()
                    .and_then(|seed| origin.get(seed))
                    .and_then(|target| names.get(target))
                    .map(|(key, _)| key.to_string())
                    .unwrap_or_default();
                report.dependents.push(Dependent {
                    entity: found,
                    hops: path.steps.len() as u32,
                    target,
                    via: path_text(path, &names),
                    authoritative: path.cost.is_authoritative(),
                });
            }

            // Owners of the targets (or, for a group, of its members) and of the affected
            // entities that have owners.
            let mut owned: Vec<NodeId> = Vec::new();
            for (_, id) in &resolved {
                match covers.get(id) {
                    Some(members) => owned.extend(members.iter().copied()),
                    None => owned.push(*id),
                }
            }
            owned.extend(paths.iter().filter_map(Path::target).filter(|node| {
                graph
                    .node(*node)
                    .is_some_and(|n| OWNED_KINDS.contains(&n.kind))
            }));
            let mut seen = HashSet::new();
            owned.retain(|node| seen.insert(*node));
            if owned.len() > MAX_OWNER_LOOKUPS {
                owned.truncate(MAX_OWNER_LOOKUPS);
                report.gaps.push(format!(
                    "owners were looked up for the first {MAX_OWNER_LOOKUPS} affected entities only"
                ));
            }
            let mut by_owner: BTreeMap<NodeId, (Vec<NodeId>, bool)> = BTreeMap::new();
            let mut unowned = Vec::new();
            let owner_goal = Goal::kinds(&[NodeKind::Team, NodeKind::Person]);
            let ownership = TraversalProfile::ownership();
            let owner_limits = Limits::from(&config).until(deadline);
            for node in &owned {
                let found = finder.nearest(&[Seed::new(*node)], &owner_goal, 1, &ownership, owner_limits);
                match found.first().and_then(|path| Some((path.target()?, path.cost.is_authoritative()))) {
                    Some((owner, authoritative)) => {
                        let entry = by_owner.entry(owner).or_insert((Vec::new(), true));
                        entry.0.push(*node);
                        entry.1 &= authoritative;
                    }
                    None => unowned.push(*node),
                }
            }
            report.truncated |= finder.truncated();
            let extra: Vec<NodeId> = by_owner
                .keys()
                .copied()
                .chain(owned.iter().copied())
                .filter(|id| !names.contains_key(id))
                .collect();
            names.extend(self.store().names(&extra)?);
            let mut owners: Vec<OwnerImpact> = by_owner
                .into_iter()
                .filter_map(|(owner, (nodes, authoritative))| {
                    Some(OwnerImpact {
                        owner: entity(graph, &names, owner)?,
                        owns: nodes
                            .iter()
                            .filter_map(|id| names.get(id).map(|(key, _)| key.to_string()))
                            .collect(),
                        authoritative,
                    })
                })
                .collect();
            owners.sort_by(|a, b| b.owns.len().cmp(&a.owns.len()).then(a.owner.key.cmp(&b.owner.key)));
            report.owners = owners;
            let unowned: Vec<String> = unowned
                .iter()
                .filter(|id| {
                    graph
                        .node(**id)
                        .is_some_and(|n| matches!(n.kind, NodeKind::Service | NodeKind::Component))
                })
                .filter_map(|id| names.get(id).map(|(key, _)| key.to_string()))
                .collect();
            if !unowned.is_empty() {
                report.gaps.push(format!("no owner in the graph for: {}", unowned.join(", ")));
            }
            if report.truncated {
                report.gaps.push(
                    "the search stopped at its time or visit budget, so there may be more dependents; narrow the targets".into(),
                );
            }
            Ok(report)
        })
    }

    /// The most depended-on entities of one kind, with their owners, and the owners who own
    /// the most (blocking).
    pub fn hotspots(&self, options: &HotspotOptions) -> Result<HotspotReport> {
        if !HOTSPOT_KINDS.contains(&options.kind) {
            let names: Vec<&str> = HOTSPOT_KINDS.iter().map(|kind| kind.as_str()).collect();
            return Err(Error::invalid(format!(
                "hotspots are counted per {}; `{}` is not one of them",
                names.join(", "),
                options.kind
            )));
        }
        let deadline = self.traversal_deadline();
        let config = self.config().graph.clone();
        self.store().consistent(|graph| {
            let mut report = HotspotReport {
                kind: Some(options.kind),
                ..HotspotReport::default()
            };
            let mut memo = HashMap::new();
            let mut dependents: HashMap<NodeId, HashSet<NodeId>> = HashMap::new();
            let mut depends_on: HashMap<NodeId, HashSet<NodeId>> = HashMap::new();
            let mut loads: HashMap<NodeId, HashMap<NodeKind, HashSet<NodeId>>> = HashMap::new();
            let mut changes: HashMap<NodeId, HashSet<NodeId>> = HashMap::new();
            for (index, edge) in graph.edges().enumerate() {
                if index % 4096 == 4095 && deadline.is_some_and(|d| Instant::now() >= d) {
                    report.truncated = true;
                    break;
                }
                if edge.kind == EdgeKind::Owns {
                    if let Some(target) = graph.node(edge.to) {
                        loads
                            .entry(edge.from)
                            .or_default()
                            .entry(target.kind)
                            .or_default()
                            .insert(edge.to);
                    }
                    continue;
                }
                if edge.kind == EdgeKind::Modifies {
                    if let Some(changed) = lift(graph, &mut memo, edge.to, options.kind) {
                        changes.entry(changed).or_default().insert(edge.from);
                    }
                    continue;
                }
                if !DEPENDENCY_EDGES.contains(&edge.kind) {
                    continue;
                }
                let Some(used) = lift(graph, &mut memo, edge.to, options.kind) else {
                    continue;
                };
                let Some(user) = lift(graph, &mut memo, edge.from, options.kind) else {
                    continue;
                };
                if user != used {
                    dependents.entry(used).or_default().insert(user);
                    depends_on.entry(user).or_default().insert(used);
                }
            }

            let mut ranked: Vec<(NodeId, usize)> = dependents
                .iter()
                .map(|(node, users)| (*node, users.len()))
                .filter(|(_, count)| *count >= options.min_dependents.max(1))
                .collect();
            ranked.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
            ranked.truncate(options.limit);

            let finder = Pathfinder::new(graph, &config);
            let owner_goal = Goal::kinds(&[NodeKind::Team, NodeKind::Person]);
            let ownership = TraversalProfile::ownership();
            let limits = Limits::from(&config).until(deadline);
            let mut owners_of: Vec<Option<(NodeId, bool)>> = Vec::new();
            for (node, _) in &ranked {
                let found = finder.nearest(&[Seed::new(*node)], &owner_goal, 1, &ownership, limits);
                owners_of.push(
                    found
                        .first()
                        .and_then(|path| Some((path.target()?, path.cost.is_authoritative()))),
                );
            }
            report.truncated |= finder.truncated();

            // Committers of each hotspot: the authors of the commits that changed it.
            let committers_of: Vec<Vec<(NodeId, usize)>> = ranked
                .iter()
                .map(|(node, _)| {
                    let mut counts: HashMap<NodeId, usize> = HashMap::new();
                    for commit in changes.get(node).into_iter().flatten() {
                        for neighbor in graph.neighbors(*commit) {
                            if neighbor.direction == Direction::Forward
                                && neighbor.edge.kind == EdgeKind::AuthoredBy
                            {
                                *counts.entry(neighbor.node).or_default() += 1;
                            }
                        }
                    }
                    let mut counts: Vec<(NodeId, usize)> = counts.into_iter().collect();
                    counts.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
                    counts
                })
                .collect();

            let mut owner_rank: Vec<(NodeId, usize)> = loads
                .iter()
                .map(|(owner, kinds)| (*owner, kinds.values().map(HashSet::len).sum()))
                .collect();
            owner_rank.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
            owner_rank.truncate(options.limit);

            let mut ids: Vec<NodeId> = ranked.iter().map(|(id, _)| *id).collect();
            ids.extend(owners_of.iter().flatten().map(|(id, _)| *id));
            ids.extend(owner_rank.iter().map(|(id, _)| *id));
            ids.extend(committers_of.iter().flatten().map(|(id, _)| *id));
            ids.sort_unstable();
            ids.dedup();
            let names = self.store().names(&ids)?;

            let mut owned_hotspots: HashMap<NodeId, usize> = HashMap::new();
            for (((node, count), owner), committers) in
                ranked.iter().zip(owners_of).zip(committers_of)
            {
                let Some(found) = entity(graph, &names, *node) else {
                    continue;
                };
                let owner = owner.and_then(|(id, authoritative)| {
                    *owned_hotspots.entry(id).or_default() += 1;
                    Some(Owner {
                        owner: entity(graph, &names, id)?,
                        authoritative,
                    })
                });
                let mut risks = Vec::new();
                match &owner {
                    None => {
                        risks.push("no_owner".to_owned());
                        report.unowned += 1;
                    }
                    Some(owner) => {
                        if owner.owner.kind == NodeKind::Person {
                            risks.push("single_person_owner".to_owned());
                            report.single_person += 1;
                        }
                        if !owner.authoritative {
                            risks.push("reference_only_owner".to_owned());
                        }
                    }
                }
                let commits = changes.get(node).map_or(0, HashSet::len);
                let committers: Vec<Committer> = committers
                    .into_iter()
                    .filter_map(|(id, commits)| {
                        Some(Committer {
                            person: entity(graph, &names, id)?,
                            commits,
                        })
                    })
                    .collect();
                if commits > 0 {
                    report.history = true;
                    if committers.len() == 1 {
                        risks.push("single_committer".to_owned());
                        report.single_committer += 1;
                    }
                }
                report.hotspots.push(Hotspot {
                    entity: found,
                    dependents: *count,
                    depends_on: depends_on.get(node).map_or(0, HashSet::len),
                    owner,
                    committers,
                    commits,
                    risks,
                });
            }
            for (owner, owns) in owner_rank {
                let Some(found) = entity(graph, &names, owner) else {
                    continue;
                };
                let by_kind = loads
                    .get(&owner)
                    .map(|kinds| {
                        kinds
                            .iter()
                            .map(|(kind, nodes)| (kind.as_str().to_owned(), nodes.len()))
                            .collect()
                    })
                    .unwrap_or_default();
                report.owners.push(OwnerLoad {
                    owner: found,
                    owns,
                    by_kind,
                    hotspots: owned_hotspots.get(&owner).copied().unwrap_or(0),
                });
            }
            Ok(report)
        })
    }
}

/// "1 service", "3 files", "2 API specs".
fn counted(kind: &str, count: usize) -> String {
    let (one, many) = match kind {
        "api_spec" => ("API spec", "API specs"),
        "api_endpoint" => ("API endpoint", "API endpoints"),
        "wiki_page" => ("wiki page", "wiki pages"),
        "pull_request" => ("pull request", "pull requests"),
        other => (other, ""),
    };
    if count == 1 {
        format!("1 {}", one.replace('_', " "))
    } else if many.is_empty() {
        format!("{count} {}s", one.replace('_', " "))
    } else {
        format!("{count} {many}")
    }
}

/// How an entity is named in a ranking: files and symbols by key (names repeat across
/// repositories), everything else by name.
fn label(entity: &Entity) -> &str {
    match entity.kind {
        NodeKind::File | NodeKind::Symbol | NodeKind::ApiEndpoint => entity
            .key
            .split_once(':')
            .map_or(entity.key.as_str(), |(_, rest)| rest),
        _ => &entity.name,
    }
}

/// The first `max` items joined, and how many more there are.
fn abbreviate(items: &[String], max: usize) -> String {
    let mut text = items
        .iter()
        .take(max)
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    if items.len() > max {
        text.push_str(&format!(" and {} more", items.len() - max));
    }
    text
}

/// Markdown rendering of an impact report, for a pull-request comment: a one-line summary, the
/// owners to notify, and the nearest dependents in a collapsed table.
pub fn render_impact_markdown(report: &ImpactReport) -> String {
    let mut out = String::from("### Change impact\n\n");
    let resolved: Vec<&ImpactTarget> = report
        .targets
        .iter()
        .filter(|target| target.entity.is_some())
        .collect();
    if report.dependents.is_empty() {
        out.push_str(&format!(
            "Nothing in the graph depends on the {} changed.\n",
            counted("target", resolved.len())
        ));
    } else {
        let summary: Vec<String> = report
            .summary
            .iter()
            .map(|(kind, count)| counted(kind, *count))
            .collect();
        out.push_str(&format!(
            "{} changed; affected, directly or through what depends on them: **{}**{}.\n",
            counted("target", resolved.len()),
            summary.join(", "),
            if report.truncated {
                " (search cut short; there may be more)"
            } else {
                ""
            }
        ));
    }
    if !report.owners.is_empty() {
        out.push_str("\n**Owners to notify**\n\n");
        for owner in &report.owners {
            let owns: Vec<String> = owner.owns.iter().map(|key| format!("`{key}`")).collect();
            out.push_str(&format!(
                "- {} ({}){}: {}\n",
                owner.owner.name,
                owner.owner.kind,
                if owner.authoritative {
                    ""
                } else {
                    ", reference-only claim"
                },
                abbreviate(&owns, 6)
            ));
        }
    }
    if !report.dependents.is_empty() {
        out.push_str(&format!(
            "\n<details><summary>Nearest {} of {} dependents</summary>\n\n| Hops | Dependent | Kind | Via |\n|---|---|---|---|\n",
            report.dependents.len(),
            report.total
        ));
        for dependent in &report.dependents {
            out.push_str(&format!(
                "| {} | `{}` | {} | {} |\n",
                dependent.hops,
                dependent.entity.key,
                dependent.entity.kind,
                dependent.via.replace('|', "\\|")
            ));
        }
        out.push_str("\n</details>\n");
    }
    let unresolved: Vec<String> = report
        .targets
        .iter()
        .filter(|target| target.entity.is_none())
        .map(|target| format!("`{}`", target.input))
        .collect();
    if !unresolved.is_empty() {
        out.push_str(&format!(
            "\nNot in the graph (nothing to follow): {}\n",
            abbreviate(&unresolved, 10)
        ));
    }
    for gap in &report.gaps {
        out.push_str(&format!("\n_Note: {gap}_\n"));
    }
    out
}

/// Plain-text rendering of an impact report.
pub fn render_impact(report: &ImpactReport) -> String {
    let mut out = String::new();
    for target in &report.targets {
        match (&target.entity, &target.problem) {
            (Some(entity), _) => {
                out.push_str(&format!(
                    "target {} ({}) [{}]\n",
                    entity.key,
                    entity.kind,
                    entity.trust.as_str()
                ));
                if !target.covers.is_empty() {
                    out.push_str(&format!(
                        "  searched from what it {}: {}\n",
                        if matches!(entity.kind, NodeKind::Team | NodeKind::Person) {
                            "owns"
                        } else {
                            "contains"
                        },
                        abbreviate(&target.covers, 8)
                    ));
                }
            }
            (None, Some(problem)) => out.push_str(&format!("target {}: {problem}\n", target.input)),
            (None, None) => {}
        }
    }
    if report.dependents.is_empty() {
        out.push_str("\nNothing in the graph depends on these targets.\n");
    } else {
        let summary: Vec<String> = report
            .summary
            .iter()
            .map(|(kind, count)| counted(kind, *count))
            .collect();
        let shown = if report.dependents.len() < report.total {
            format!(
                " (showing the nearest {}; raise --limit for more)",
                report.dependents.len()
            )
        } else {
            String::new()
        };
        out.push_str(&format!(
            "\nAffected, directly or through what depends on them: {}{shown}\n",
            summary.join(", ")
        ));
        for dependent in &report.dependents {
            out.push_str(&format!(
                "  {:>2} hop{} {} ({}){}\n       via {}\n",
                dependent.hops,
                if dependent.hops == 1 { " " } else { "s" },
                dependent.entity.key,
                dependent.entity.kind,
                if dependent.authoritative {
                    ""
                } else {
                    " [partly reference-only]"
                },
                dependent.via
            ));
        }
    }
    if !report.owners.is_empty() {
        out.push_str("\nOwners to notify:\n");
        for owner in &report.owners {
            out.push_str(&format!(
                "  {} ({}){}: {}\n",
                owner.owner.name,
                owner.owner.kind,
                if owner.authoritative {
                    ""
                } else {
                    " [reference-only claim]"
                },
                abbreviate(&owner.owns, 6)
            ));
        }
    }
    for gap in &report.gaps {
        out.push_str(&format!("\nnote: {gap}\n"));
    }
    out
}

/// Plain-text rendering of a hotspot report.
pub fn render_hotspots(report: &HotspotReport) -> String {
    let mut out = String::new();
    let kind = report.kind.map_or("entity", NodeKind::as_str);
    if report.hotspots.is_empty() {
        out.push_str(&format!(
            "No {} has dependents in the graph (try --kind file or --kind symbol).\n",
            kind.replace('_', " ")
        ));
    } else {
        out.push_str(&format!(
            "Most depended-on {}s (direct dependents at this level; `anchor impact <name>` follows them further):\n",
            kind.replace('_', " ")
        ));
        for (rank, hotspot) in report.hotspots.iter().enumerate() {
            let owner = hotspot.owner.as_ref().map_or_else(
                || "no owner".to_owned(),
                |owner| format!("owner {} ({})", owner.owner.name, owner.owner.kind),
            );
            let risks = if hotspot.risks.is_empty() {
                String::new()
            } else {
                format!("  ! {}", hotspot.risks.join(", ").replace('_', " "))
            };
            out.push_str(&format!(
                "  {:>2}. {}  used by {}, uses {}  {owner}{risks}\n",
                rank + 1,
                label(&hotspot.entity),
                hotspot.dependents,
                hotspot.depends_on,
            ));
            if !hotspot.committers.is_empty() {
                let names: Vec<String> = hotspot
                    .committers
                    .iter()
                    .take(3)
                    .map(|c| format!("{} {}", c.person.name, c.commits))
                    .collect();
                let more = hotspot.committers.len().saturating_sub(3);
                out.push_str(&format!(
                    "      changed in {} by {}{}\n",
                    counted("commit", hotspot.commits),
                    names.join(", "),
                    if more > 0 {
                        format!(" and {more} more")
                    } else {
                        String::new()
                    }
                ));
            }
        }
        out.push_str(&format!(
            "\n{} of these have no owner; {} are owned by a single person",
            report.unowned, report.single_person
        ));
        if report.history {
            out.push_str(&format!(
                "; {} were only ever changed by one person",
                report.single_committer
            ));
        }
        out.push_str(".\n");
    }
    if !report.owners.is_empty() {
        out.push_str("\nOwners by entities owned directly:\n");
        for owner in &report.owners {
            let kinds: Vec<String> = owner
                .by_kind
                .iter()
                .map(|(kind, count)| counted(kind, *count))
                .collect();
            let share = if report.hotspots.is_empty() {
                String::new()
            } else {
                format!("; {} of the {kind}s above", owner.hotspots)
            };
            out.push_str(&format!(
                "  {} ({}): {} ({}){share}\n",
                owner.owner.name,
                owner.owner.kind,
                owner.owns,
                kinds.join(", "),
            ));
        }
    }
    if report.truncated {
        out.push_str(
            "\nnote: counting stopped at the time budget; the ranking may be incomplete.\n",
        );
    }
    out
}
