//! The plan: a pure function of the spec that decides every entity and every interesting case
//! before any file is written. Emitters stream files from it; ground truth is derived from it.

use std::collections::BTreeSet;

use super::rng::Rng;
use super::spec::{SynthLanguage, SynthSpec};
use super::words;
use crate::base::keys;

/// Days since the Unix epoch of 2025-01-01, the origin of generated timestamps.
const EPOCH_2025: i64 = 20_089;

/// `YYYY-MM-DDTHH:MM:SSZ` for `day` (days after 2025-01-01) and `second` of that day.
pub(crate) fn timestamp(day: i64, second: u32) -> String {
    let z = day + EPOCH_2025 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { year + 1 } else { year };
    let day_of_month = doy - (153 * mp + 2) / 5 + 1;
    let second = second % 86_400;
    format!(
        "{year:04}-{month:02}-{day_of_month:02}T{:02}:{:02}:{:02}Z",
        second / 3600,
        second % 3600 / 60,
        second % 60
    )
}

pub(crate) struct Person {
    pub name: String,
    pub handle: String,
    pub email: String,
    pub role: String,
    pub reports_to: Option<usize>,
    pub teams: Vec<usize>,
}

pub(crate) struct Team {
    pub name: String,
    pub slug: String,
    pub email: String,
    pub lead: usize,
    pub members: Vec<usize>,
}

pub(crate) struct Portfolio {
    pub name: String,
    pub lead: usize,
}

pub(crate) struct Domain {
    pub name: String,
    pub portfolio: usize,
    pub lead: usize,
}

pub(crate) struct Component {
    pub name: String,
    pub domain: usize,
    pub owner: usize,
    pub services: Vec<usize>,
}

/// Which ownership file a repository carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OwnershipStyle {
    CodeOwners,
    Maintainers,
    Owners,
    None,
}

/// The direct, source-of-truth owner a repository's ownership file names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Owner {
    Team(usize),
    Person(usize),
}

pub(crate) struct Operation {
    pub method: &'static str,
    pub route: String,
    pub id: String,
    pub summary: String,
}

/// One approval record of an API.
pub(crate) struct Approval {
    pub id: String,
    pub approver: String,
    pub decision: &'static str,
    pub blocking: bool,
    pub reason: String,
    pub scope: Option<String>,
    pub day: i64,
}

pub(crate) struct ApiPlan {
    pub portal_id: String,
    pub title: String,
    pub status: &'static str,
    pub operations: Vec<Operation>,
    pub approvals: Vec<Approval>,
}

pub(crate) struct Service {
    pub slug: String,
    pub ident: String,
    pub language: SynthLanguage,
    /// `None` for services outside the org model (only a reference source names an owner).
    pub component: Option<usize>,
    pub style: OwnershipStyle,
    pub owner: Option<Owner>,
    /// `(source file index, person)`: a CODEOWNERS rule giving one file to a person.
    pub special_file: Option<(usize, usize)>,
    pub stale_team: Option<usize>,
    pub reference_team: Option<usize>,
    pub calls: Vec<usize>,
    pub api: Option<ApiPlan>,
    pub modules: Vec<String>,
}

pub(crate) struct Ticket {
    pub key: String,
    pub summary: String,
    pub kind: &'static str,
    pub status: &'static str,
    pub component: Option<usize>,
    pub service: usize,
    pub assignee: usize,
    pub reporter: usize,
    pub parent: Option<usize>,
    pub blocked_by: Option<usize>,
    pub reason: Option<String>,
    pub day: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReviewScenario {
    Approved,
    Blocking,
    Dismissed,
}

pub(crate) struct PullRequest {
    pub number: u64,
    pub service: usize,
    pub title: String,
    pub ticket: Option<usize>,
    pub author: usize,
    pub reviewer: usize,
    pub scenario: ReviewScenario,
    pub reason: String,
    pub day: i64,
}

pub(crate) struct Plan {
    pub people: Vec<Person>,
    pub teams: Vec<Team>,
    pub portfolios: Vec<Portfolio>,
    pub domains: Vec<Domain>,
    pub components: Vec<Component>,
    pub services: Vec<Service>,
    pub tickets: Vec<Ticket>,
    pub pull_requests: Vec<PullRequest>,
    /// Services calling each service (indices), for API consumers.
    pub callers: Vec<Vec<usize>>,
}

/// Quota of a rate over `n` items: at least one when the rate is positive and `n` allows it.
fn quota(rate: f64, n: usize) -> usize {
    if rate <= 0.0 || n < 5 {
        return 0;
    }
    ((rate * n as f64).round() as usize).max(1)
}

/// Like [`quota`] for small populations: at least one whenever the rate is positive.
fn quota_small(rate: f64, n: usize) -> usize {
    if rate <= 0.0 || n == 0 {
        return 0;
    }
    ((rate * n as f64).round() as usize).max(1).min(n)
}

fn numbered(base: &str, round: usize) -> String {
    if round == 0 {
        base.to_owned()
    } else {
        format!("{base} {}", round + 1)
    }
}

pub(crate) fn capitalise(word: &str) -> String {
    let mut chars = word.chars();
    chars
        .next()
        .map(|c| c.to_ascii_uppercase().to_string() + chars.as_str())
        .unwrap_or_default()
}

fn initial(name: &str) -> char {
    name.chars().next().unwrap_or('x').to_ascii_lowercase()
}

fn build_people_and_teams(spec: &SynthSpec) -> (Vec<Person>, Vec<Team>) {
    let mut rng = Rng::derive(spec.seed, "people", 0);
    let team_count = spec.teams.max(2);
    let people_count = spec.people.max(team_count);

    let mut combos: Vec<(usize, usize)> = (0..words::FIRST_NAMES.len())
        .flat_map(|f| (0..words::LAST_NAMES.len()).map(move |l| (f, l)))
        .collect();
    rng.shuffle(&mut combos);
    let mut handles = BTreeSet::new();
    let mut people: Vec<Person> = Vec::with_capacity(people_count);
    for i in 0..people_count {
        let (f, l) = combos[i % combos.len()];
        let round = i / combos.len();
        let (first, last) = (words::FIRST_NAMES[f], words::LAST_NAMES[l]);
        let stem = format!("{}{}", initial(first), last.to_ascii_lowercase());
        let mut handle = stem.clone();
        let mut n = 1;
        while !handles.insert(handle.clone()) {
            n += 1;
            handle = format!("{stem}{n}");
        }
        let role = if i < team_count {
            "Engineering Manager".to_owned()
        } else {
            (*rng.pick(words::ROLES)).to_owned()
        };
        people.push(Person {
            name: numbered(&format!("{first} {last}"), round),
            email: format!("{handle}@acme.example"),
            handle,
            role,
            reports_to: None,
            teams: vec![i % team_count],
        });
    }

    let mut combos: Vec<(usize, usize)> = (0..words::TEAM_KINDS.len())
        .flat_map(|k| (0..words::TEAM_AREAS.len()).map(move |a| (a, k)))
        .collect();
    rng.shuffle(&mut combos);
    let mut teams: Vec<Team> = (0..team_count)
        .map(|i| {
            let (a, k) = combos[i % combos.len()];
            let name = numbered(
                &format!("{} {}", words::TEAM_AREAS[a], words::TEAM_KINDS[k]),
                i / combos.len(),
            );
            let slug = keys::slugify(&name);
            Team {
                email: format!("{slug}@acme.example"),
                name,
                slug,
                lead: i,
                members: Vec::new(),
            }
        })
        .collect();
    // A tenth of the people sit in a second team: the "person in several teams" distractor.
    for person in people.iter_mut().skip(team_count) {
        if rng.chance(0.1) {
            let extra = rng.below(team_count);
            if !person.teams.contains(&extra) {
                person.teams.push(extra);
            }
        }
    }
    for (index, person) in people.iter().enumerate() {
        for &team in &person.teams {
            teams[team].members.push(index);
        }
    }
    for (index, person) in people.iter_mut().enumerate() {
        person.reports_to = match index {
            0 => None,
            i if i < team_count => Some(0),
            _ => Some(person.teams[0]),
        };
    }
    (people, teams)
}

fn pick_language(
    spec: &SynthSpec,
    rng: &mut Rng,
    index: usize,
    first_round: &[SynthLanguage],
) -> SynthLanguage {
    if let Some(language) = first_round.get(index) {
        return *language;
    }
    let total: u32 = spec.languages.iter().map(|l| l.weight).sum();
    if total == 0 {
        return SynthLanguage::Rust;
    }
    let mut roll = rng.below(total as usize) as u32;
    for entry in &spec.languages {
        if roll < entry.weight {
            return entry.language;
        }
        roll -= entry.weight;
    }
    SynthLanguage::Rust
}

#[derive(Clone, Copy)]
enum ApprovalKind {
    Approved,
    Pending,
    Rejected,
    ChangesRequestedOperation,
}

fn api_plan(
    index: usize,
    slug: &str,
    kind: ApprovalKind,
    rng: &mut Rng,
    approval_counter: &mut usize,
) -> ApiPlan {
    let wanted = 1 + rng.below(2);
    let mut chosen: Vec<&str> = Vec::new();
    while chosen.len() < wanted {
        let candidate = *rng.pick(words::RESOURCES);
        if !chosen.contains(&candidate) {
            chosen.push(candidate);
        }
    }
    let mut operations = Vec::new();
    for resource in &chosen {
        let pascal = capitalise(resource);
        operations.push(Operation {
            method: "GET",
            route: format!("/v1/{resource}"),
            id: format!("list{pascal}"),
            summary: format!("List {resource}"),
        });
        operations.push(Operation {
            method: "POST",
            route: format!("/v1/{resource}"),
            id: format!("create{pascal}"),
            summary: format!("Create a {resource} entry"),
        });
        operations.push(Operation {
            method: "GET",
            route: format!("/v1/{resource}/{{id}}"),
            id: format!("get{pascal}"),
            summary: format!("Fetch one {resource} entry"),
        });
    }
    let approvers = ["Security Review", "Architecture Board", "Data Governance"];
    let mut next = |decision: &'static str,
                    blocking: bool,
                    reason: String,
                    scope: Option<String>,
                    rng: &mut Rng| {
        *approval_counter += 1;
        Approval {
            id: format!("APR-{}", *approval_counter),
            approver: (*rng.pick(&approvers)).to_owned(),
            decision,
            blocking,
            reason,
            scope,
            day: 40 + rng.below(200) as i64,
        }
    };
    let standard = "Meets the platform API standard.".to_owned();
    let mut approvals = Vec::new();
    let status = match kind {
        ApprovalKind::Approved => {
            approvals.push(next("approved", false, standard, None, rng));
            "approved"
        }
        ApprovalKind::Pending => {
            approvals.push(next(
                "pending",
                false,
                "Awaiting the first review round.".to_owned(),
                None,
                rng,
            ));
            "in_review"
        }
        ApprovalKind::Rejected => {
            let reason = (*rng.pick(words::REJECTION_REASONS)).to_owned();
            approvals.push(next("rejected", true, reason, None, rng));
            "rejected"
        }
        ApprovalKind::ChangesRequestedOperation => {
            let target = &operations[1];
            let scope = format!("{} {}", target.method, target.route);
            approvals.push(next("approved", false, standard, None, rng));
            let reason = (*rng.pick(words::REJECTION_REASONS)).to_owned();
            approvals.push(next("changes_requested", true, reason, Some(scope), rng));
            "in_review"
        }
    };
    ApiPlan {
        portal_id: format!("API-{}", 100 + index),
        title: format!("{slug} API"),
        status,
        operations,
        approvals,
    }
}

impl Plan {
    /// Builds the plan for `spec`.
    pub(crate) fn build(spec: &SynthSpec) -> Plan {
        let (people, teams) = build_people_and_teams(spec);
        let mut rng = Rng::derive(spec.seed, "plan", 0);

        // --- hierarchy -----------------------------------------------------------------------
        let mut portfolios = Vec::new();
        let mut domains = Vec::new();
        let mut components: Vec<Component> = Vec::new();
        let mut slots: Vec<usize> = Vec::new(); // component index per repository
        let limit = spec.repositories();
        'outer: for p in 0..spec.portfolios.max(1) {
            portfolios.push(Portfolio {
                name: numbered(
                    words::PORTFOLIO_NAMES[p % words::PORTFOLIO_NAMES.len()],
                    p / words::PORTFOLIO_NAMES.len(),
                ),
                lead: rng.below(people.len()),
            });
            for _ in 0..spec.domains_per_portfolio.max(1) {
                let d = domains.len();
                domains.push(Domain {
                    name: numbered(
                        words::DOMAIN_NAMES[d % words::DOMAIN_NAMES.len()],
                        d / words::DOMAIN_NAMES.len(),
                    ),
                    portfolio: p,
                    lead: rng.below(people.len()),
                });
                for _ in 0..spec.components_per_domain.max(1) {
                    if slots.len() >= limit {
                        break 'outer;
                    }
                    let c = components.len();
                    components.push(Component {
                        name: numbered(
                            words::COMPONENT_NAMES[c % words::COMPONENT_NAMES.len()],
                            c / words::COMPONENT_NAMES.len(),
                        ),
                        domain: d,
                        owner: rng.below(teams.len()),
                        services: Vec::new(),
                    });
                    for _ in 0..spec.services_per_component.max(1) {
                        if slots.len() >= limit {
                            break;
                        }
                        slots.push(c);
                    }
                }
            }
        }
        let n = slots.len();

        // --- services ------------------------------------------------------------------------
        let mut first_round: Vec<SynthLanguage> = spec
            .languages
            .iter()
            .filter(|l| l.weight > 0)
            .map(|l| l.language)
            .collect();
        rng.shuffle(&mut first_round);
        let mut used_names = BTreeSet::new();
        let mut services: Vec<Service> = Vec::with_capacity(n);
        for (index, &component) in slots.iter().enumerate() {
            let mut slug = String::new();
            for attempt in 0..30 {
                let candidate = format!(
                    "{}-{}",
                    rng.pick(words::SERVICE_STEMS),
                    rng.pick(words::SERVICE_ROLES)
                );
                slug = if attempt < 20 {
                    candidate
                } else {
                    format!("{candidate}-{attempt}")
                };
                if !used_names.contains(&slug) {
                    break;
                }
            }
            while !used_names.insert(slug.clone()) {
                slug.push('x');
            }
            let language = pick_language(spec, &mut rng, index, &first_round);
            let start = rng.below(words::NOUNS.len());
            let modules = (0..spec.files_per_service.max(1))
                .map(|i| {
                    let base = words::NOUNS[(start + i) % words::NOUNS.len()];
                    if i < words::NOUNS.len() {
                        base.to_owned()
                    } else {
                        format!("{base}{i}")
                    }
                })
                .collect();
            services.push(Service {
                ident: slug.replace('-', "_"),
                slug,
                language,
                component: Some(component),
                style: OwnershipStyle::CodeOwners,
                owner: None,
                special_file: None,
                stale_team: None,
                reference_team: None,
                calls: Vec::new(),
                api: None,
                modules,
            });
        }

        // --- ownership cases (quotas over a shuffled order) ----------------------------------
        let mut order: Vec<usize> = (0..n).collect();
        rng.shuffle(&mut order);
        let mut cursor = 0;
        let mut take = |count: usize| -> Vec<usize> {
            let count = count.min(n.saturating_sub(cursor + 1));
            let slice = order[cursor..cursor + count].to_vec();
            cursor += count;
            slice
        };
        let orphans = take(quota(spec.rates.reference_only_owner, n));
        let inherited = take(quota(spec.rates.inherited_ownership, n));
        let conflicts = take(quota(spec.rates.conflict, n));
        let maintainers = take(quota(spec.rates.maintainers_file, n));
        let owner_files = take(quota(spec.rates.owners_file, n));
        let cross_team = take(quota(spec.rates.cross_team_owner, n));

        let other_team = |rng: &mut Rng, not: usize| -> usize {
            let t = rng.below(teams.len());
            if t == not { (t + 1) % teams.len() } else { t }
        };
        let mut specials = 0;
        for (index, service) in services.iter_mut().enumerate() {
            let component_owner = service.component.map_or(0, |c| components[c].owner);
            if orphans.contains(&index) {
                service.component = None;
                service.style = OwnershipStyle::None;
                service.reference_team = Some(rng.below(teams.len()));
            } else if inherited.contains(&index) {
                service.style = OwnershipStyle::None;
            } else if maintainers.contains(&index) || owner_files.contains(&index) {
                service.style = if maintainers.contains(&index) {
                    OwnershipStyle::Maintainers
                } else {
                    OwnershipStyle::Owners
                };
                service.owner = Some(Owner::Person(*rng.pick(&teams[component_owner].members)));
            } else {
                let team = if cross_team.contains(&index) {
                    other_team(&mut rng, component_owner)
                } else {
                    component_owner
                };
                service.owner = Some(Owner::Team(team));
                if conflicts.contains(&index) {
                    service.stale_team = Some(other_team(&mut rng, team));
                }
                if spec.files_per_service >= 2 && (specials == 0 || rng.chance(0.3)) {
                    specials += 1;
                    service.special_file = Some((1, *rng.pick(&teams[team].members)));
                }
            }
        }
        for (c, component) in components.iter_mut().enumerate() {
            component.services = services
                .iter()
                .enumerate()
                .filter(|(_, s)| s.component == Some(c))
                .map(|(i, _)| i)
                .collect();
        }

        // --- APIs and cross-service calls ----------------------------------------------------
        let api_count = spec.apis.min(n);
        let mut api_services: Vec<usize> = (0..n).collect();
        rng.shuffle(&mut api_services);
        api_services.truncate(api_count);
        let rejected = quota_small(spec.rates.rejected_api, api_count);
        let changes = quota_small(spec.rates.changes_requested_operation, api_count);
        let pending = quota_small(spec.rates.pending_api, api_count);
        let mut approval_counter = 0;
        for (i, &service) in api_services.iter().enumerate() {
            let kind = if i < rejected {
                ApprovalKind::Rejected
            } else if i < rejected + changes {
                ApprovalKind::ChangesRequestedOperation
            } else if i < rejected + changes + pending {
                ApprovalKind::Pending
            } else {
                ApprovalKind::Approved
            };
            let slug = services[service].slug.clone();
            services[service].api = Some(api_plan(i, &slug, kind, &mut rng, &mut approval_counter));
        }
        let mut callers: Vec<Vec<usize>> = vec![Vec::new(); n];
        if !api_services.is_empty() {
            let whole = spec.rates.calls_per_service.floor() as usize;
            let extra = spec.rates.calls_per_service - whole as f64;
            for (index, service) in services.iter_mut().enumerate() {
                let calls = whole + usize::from(rng.chance(extra));
                for _ in 0..calls {
                    let callee = *rng.pick(&api_services);
                    if callee != index && !service.calls.contains(&callee) {
                        service.calls.push(callee);
                        callers[callee].push(index);
                    }
                }
            }
        }

        let tickets = build_tickets(spec, &people, &services, &mut rng);
        let pull_requests =
            build_pull_requests(spec, &people, &teams, &services, &tickets, &mut rng);
        Plan {
            people,
            teams,
            portfolios,
            domains,
            components,
            services,
            tickets,
            pull_requests,
            callers,
        }
    }
}

fn project_keys(names: &[&str]) -> Vec<String> {
    let mut used = BTreeSet::new();
    names
        .iter()
        .map(|name| {
            let letters: String = name
                .chars()
                .filter(char::is_ascii_alphabetic)
                .take(3)
                .collect::<String>()
                .to_ascii_uppercase();
            let mut key = letters.clone();
            let mut n = 1;
            while !used.insert(key.clone()) {
                n += 1;
                key = format!("{letters}{n}");
            }
            key
        })
        .collect()
}

fn build_tickets(
    spec: &SynthSpec,
    people: &[Person],
    services: &[Service],
    rng: &mut Rng,
) -> Vec<Ticket> {
    let total = spec.tickets;
    if total == 0 || services.is_empty() {
        return Vec::new();
    }
    let names: Vec<&str> = words::TEAM_AREAS.iter().copied().take(6).collect();
    let projects = project_keys(&names);
    let mut counters = vec![100usize; projects.len()];
    let blocked = quota(spec.rates.blocked_ticket, total).min(total / 3);
    let epics = (total / 6)
        .max(1)
        .min(total.saturating_sub(blocked * 2).max(1));
    let kinds = ["Task", "Bug", "Story"];
    let statuses = ["To Do", "In Progress", "In Review", "Done"];
    let mut tickets: Vec<Ticket> = Vec::with_capacity(total);
    for i in 0..total {
        let project = i % projects.len();
        counters[project] += 1;
        let service = rng.below(services.len());
        let slug = &services[service].slug;
        let is_epic = i < epics;
        let is_blocked = !is_epic && i < epics + blocked;
        let is_blocker = !is_epic && !is_blocked && i < epics + 2 * blocked;
        let (kind, summary) = if is_epic {
            ("Epic", format!("Modernise the {slug} service"))
        } else {
            let kind = *rng.pick(&kinds);
            let verb = rng.pick(words::VERBS);
            let noun = rng.pick(words::NOUNS);
            (
                kind,
                format!("{} {noun} handling in {slug}", capitalise(verb)),
            )
        };
        let status = if is_blocked {
            "Blocked"
        } else if is_blocker {
            "In Progress"
        } else {
            *rng.pick(&statuses)
        };
        tickets.push(Ticket {
            key: format!("{}-{}", projects[project], counters[project]),
            summary,
            kind,
            status,
            component: services[service].component,
            service,
            assignee: rng.below(people.len()),
            reporter: rng.below(people.len()),
            parent: None,
            blocked_by: None,
            reason: None,
            day: 60 + rng.below(240) as i64,
        });
    }
    for b in 0..blocked {
        tickets[epics + b].blocked_by = Some(epics + blocked + b);
        tickets[epics + b].reason = Some((*rng.pick(words::BLOCK_REASONS)).to_owned());
    }
    for ticket in tickets.iter_mut().skip(epics) {
        if rng.chance(0.6) {
            ticket.parent = Some(rng.below(epics));
        }
    }
    tickets
}

fn build_pull_requests(
    spec: &SynthSpec,
    people: &[Person],
    teams: &[Team],
    services: &[Service],
    tickets: &[Ticket],
    rng: &mut Rng,
) -> Vec<PullRequest> {
    let total = spec.pull_requests;
    if total == 0 || services.is_empty() {
        return Vec::new();
    }
    let blocking = quota(spec.rates.blocking_review, total);
    let dismissed = quota(spec.rates.dismissed_review, total);
    let mut out = Vec::with_capacity(total);
    for i in 0..total {
        let service = i % services.len();
        let team = match services[service].owner {
            Some(Owner::Team(t)) => t,
            _ => rng.below(teams.len()),
        };
        let author = *rng.pick(&teams[team].members);
        let mut reviewer = *rng.pick(&teams[team].members);
        if reviewer == author {
            reviewer = (author + 1) % people.len();
        }
        let scenario = if i < blocking {
            ReviewScenario::Blocking
        } else if i < blocking + dismissed {
            ReviewScenario::Dismissed
        } else {
            ReviewScenario::Approved
        };
        let ticket = (!tickets.is_empty()).then(|| rng.below(tickets.len()));
        let verb = rng.pick(words::VERBS);
        let noun = rng.pick(words::NOUNS);
        let reason = match scenario {
            ReviewScenario::Blocking => (*rng.pick(words::REVIEW_BLOCK_REASONS)).to_owned(),
            ReviewScenario::Dismissed => (*rng.pick(words::DISMISSAL_REASONS)).to_owned(),
            ReviewScenario::Approved => "Looks good to me.".to_owned(),
        };
        out.push(PullRequest {
            number: 100 + i as u64,
            service,
            title: format!("{} {noun} in {}", capitalise(verb), services[service].slug),
            ticket,
            author,
            reviewer,
            scenario,
            reason,
            day: 100 + rng.below(200) as i64,
        });
    }
    out
}

/// The team the portal names as owner of a service's API: the direct team owner, else the team of
/// the component containing the service, else the team of a reference-level claim.
pub(crate) fn api_owner_team(plan: &Plan, service: usize) -> usize {
    let service = &plan.services[service];
    match (service.owner, service.component, service.reference_team) {
        (Some(Owner::Team(team)), _, _) => team,
        (_, Some(component), _) => plan.components[component].owner,
        (_, None, Some(team)) => team,
        _ => 0,
    }
}
