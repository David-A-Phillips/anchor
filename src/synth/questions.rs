//! Ownership, API-justification and multi-hop relationship questions derived from the plan.

use super::plan::{Owner, Plan, api_owner_team};
use super::rng::Rng;
use super::truth::{
    JustificationKind, JustificationQuestion, OwnershipKind, OwnershipQuestion, OwnershipScenario,
    PathStep, RelationshipKind, RelationshipQuestion,
};
use super::{Ctx, SOURCE_PORTAL, code, question_id};
use crate::base::{SourceId, keys};

fn team_key(plan: &Plan, t: usize) -> String {
    keys::team(&plan.teams[t].name).to_string()
}

fn person_key(plan: &Plan, p: usize) -> String {
    keys::person(&plan.people[p].handle).to_string()
}

fn lead_key(plan: &Plan, t: usize) -> String {
    person_key(plan, plan.teams[t].lead)
}

fn step(key: impl Into<String>, edge: &str, forward: bool) -> PathStep {
    PathStep {
        key: key.into(),
        edge: Some(edge.to_owned()),
        forward,
    }
}

fn start(key: impl Into<String>) -> PathStep {
    PathStep {
        key: key.into(),
        edge: None,
        forward: true,
    }
}

pub(crate) fn emit(ctx: &mut Ctx) {
    let plan = ctx.plan;
    let mut rng = Rng::derive(ctx.spec.seed, "truth", 0);
    let portal = SourceId::new(SOURCE_PORTAL);
    let mut own = 0;
    let mut push_own = |ctx: &mut Ctx, mut q: OwnershipQuestion| {
        own += 1;
        q.id = question_id("own", own);
        ctx.truth.ownership.push(q);
    };

    // --- ownership ---------------------------------------------------------------------------
    for (index, service) in plan.services.iter().enumerate() {
        let service_key = keys::service(&service.slug).to_string();
        let phrasing = [
            format!("Who owns the {} service?", service.slug),
            format!("Who is responsible for the {} service?", service.slug),
            format!("Who should I ask about the {} service?", service.slug),
        ];
        let question = rng.pick(&phrasing).clone();
        let mut q = OwnershipQuestion {
            id: String::new(),
            kind: OwnershipKind::Service,
            question,
            subject_key: service_key.clone(),
            expected_owner_keys: Vec::new(),
            expected_lead_keys: Vec::new(),
            evidence_keys: vec![service_key.clone()],
            stale_claim_keys: Vec::new(),
            scenario: OwnershipScenario::Direct,
        };
        match (service.owner, service.component, service.reference_team) {
            (Some(Owner::Team(team)), _, _) => {
                q.expected_owner_keys.push(team_key(plan, team));
                q.expected_lead_keys.push(lead_key(plan, team));
                q.evidence_keys.push(team_key(plan, team));
                if let Some(wrong) = service.stale_team {
                    q.scenario = OwnershipScenario::Conflict;
                    q.stale_claim_keys.push(team_key(plan, wrong));
                    if let Some(page) = ctx.stale_pages.get(&index) {
                        q.stale_claim_keys.push(
                            keys::wiki_page(&SourceId::new(super::SOURCE_WIKI), page).to_string(),
                        );
                    }
                }
            }
            (Some(Owner::Person(person)), _, _) => {
                q.expected_owner_keys.push(person_key(plan, person));
                q.evidence_keys.push(person_key(plan, person));
            }
            (None, Some(component), _) => {
                let owner = plan.components[component].owner;
                q.scenario = OwnershipScenario::Inherited;
                q.expected_owner_keys.push(team_key(plan, owner));
                q.expected_lead_keys.push(lead_key(plan, owner));
                q.evidence_keys
                    .push(keys::component(&plan.components[component].name).to_string());
                q.evidence_keys.push(team_key(plan, owner));
            }
            (None, None, Some(team)) => {
                q.scenario = OwnershipScenario::ReferenceOnly;
                q.expected_owner_keys.push(team_key(plan, team));
                q.expected_lead_keys.push(lead_key(plan, team));
                q.evidence_keys.push(team_key(plan, team));
            }
            (None, None, None) => continue,
        }
        push_own(ctx, q);

        // Files: the catch-all owner, and the person a special CODEOWNERS rule names.
        if let (Some(Owner::Team(team)), true) = (service.owner, service.stale_team.is_none()) {
            let id = SourceId::new(service.slug.as_str());
            let path = code::source_path(service, 0);
            let file_key = keys::file(&id, &path).to_string();
            let phrasing = [
                format!("Who maintains {path}?"),
                format!("Who owns {path} in {}?", service.slug),
            ];
            let question = rng.pick(&phrasing).clone();
            push_own(
                ctx,
                OwnershipQuestion {
                    id: String::new(),
                    kind: OwnershipKind::File,
                    question,
                    subject_key: file_key.clone(),
                    expected_owner_keys: vec![team_key(plan, team)],
                    expected_lead_keys: vec![lead_key(plan, team)],
                    evidence_keys: vec![file_key, team_key(plan, team)],
                    stale_claim_keys: Vec::new(),
                    scenario: OwnershipScenario::Direct,
                },
            );
            if let Some((file, person)) = service.special_file {
                let path = code::source_path(service, file);
                let file_key = keys::file(&id, &path).to_string();
                push_own(
                    ctx,
                    OwnershipQuestion {
                        id: String::new(),
                        kind: OwnershipKind::File,
                        question: format!("Who maintains {path}?"),
                        subject_key: file_key.clone(),
                        expected_owner_keys: vec![person_key(plan, person)],
                        expected_lead_keys: Vec::new(),
                        evidence_keys: vec![file_key, person_key(plan, person)],
                        stale_claim_keys: Vec::new(),
                        scenario: OwnershipScenario::Direct,
                    },
                );
            }
        }

        // APIs and their endpoints.
        if let Some(api) = &service.api {
            let owner = api_owner_team(plan, index);
            let api_key = keys::api_spec(&service.slug).to_string();
            let phrasing = [
                format!("Who owns API {}?", service.slug),
                format!("Who owns the {} API?", service.slug),
                format!("Which team owns the {} API?", service.slug),
            ];
            let question = rng.pick(&phrasing).clone();
            push_own(
                ctx,
                OwnershipQuestion {
                    id: String::new(),
                    kind: OwnershipKind::Api,
                    question,
                    subject_key: api_key.clone(),
                    expected_owner_keys: vec![team_key(plan, owner)],
                    expected_lead_keys: vec![lead_key(plan, owner)],
                    evidence_keys: vec![api_key.clone(), team_key(plan, owner)],
                    stale_claim_keys: Vec::new(),
                    scenario: OwnershipScenario::Direct,
                },
            );
            let op = &api.operations[0];
            let endpoint = keys::api_endpoint(&service.slug, op.method, &op.route).to_string();
            push_own(
                ctx,
                OwnershipQuestion {
                    id: String::new(),
                    kind: OwnershipKind::Endpoint,
                    question: format!(
                        "Who should I contact about {} {} in the {} API?",
                        op.method, op.route, service.slug
                    ),
                    subject_key: endpoint.clone(),
                    expected_owner_keys: vec![team_key(plan, owner)],
                    expected_lead_keys: vec![lead_key(plan, owner)],
                    evidence_keys: vec![endpoint, api_key, team_key(plan, owner)],
                    stale_claim_keys: Vec::new(),
                    scenario: OwnershipScenario::Direct,
                },
            );
        }
    }
    for component in plan.components.iter().filter(|c| !c.services.is_empty()) {
        let key = keys::component(&component.name).to_string();
        let phrasing = [
            format!(
                "Which team is responsible for the {} component?",
                component.name
            ),
            format!("Who owns the {} component?", component.name),
        ];
        let question = rng.pick(&phrasing).clone();
        push_own(
            ctx,
            OwnershipQuestion {
                id: String::new(),
                kind: OwnershipKind::Component,
                question,
                subject_key: key.clone(),
                expected_owner_keys: vec![team_key(plan, component.owner)],
                expected_lead_keys: vec![lead_key(plan, component.owner)],
                evidence_keys: vec![key, team_key(plan, component.owner)],
                stale_claim_keys: Vec::new(),
                scenario: OwnershipScenario::Direct,
            },
        );
    }

    // --- API approvals -----------------------------------------------------------------------
    let mut why = 0;
    for service in &plan.services {
        let Some(api) = &service.api else { continue };
        for approval in api.approvals.iter().filter(|a| a.decision != "approved") {
            why += 1;
            let approval_key = keys::approval(&portal, &approval.id).to_string();
            let phrase = approval.reason.trim_end_matches('.').to_owned();
            let (kind, question, subject) = match &approval.scope {
                None => (
                    JustificationKind::ApiNotApproved,
                    format!("Why is the {} API not approved?", service.slug),
                    keys::api_spec(&service.slug).to_string(),
                ),
                Some(scope) => {
                    let (method, route) = scope.split_once(' ').unwrap_or(("GET", scope));
                    (
                        JustificationKind::EndpointBlocked,
                        format!("Why is {scope} in the {} API blocked?", service.slug),
                        keys::api_endpoint(&service.slug, method, route).to_string(),
                    )
                }
            };
            ctx.truth.justification.push(JustificationQuestion {
                id: question_id("why-api", why),
                kind,
                question,
                subject_key: subject,
                expected_keys: vec![approval_key],
                key_phrases: vec![phrase],
            });
        }
    }

    // --- relationships -----------------------------------------------------------------------
    let mut rel = 0;
    for (index, service) in plan.services.iter().enumerate() {
        let service_key = keys::service(&service.slug).to_string();
        if let Some(component_index) = service.component {
            let component = &plan.components[component_index];
            let domain = &plan.domains[component.domain];
            let portfolio = &plan.portfolios[domain.portfolio];
            let component_key = keys::component(&component.name).to_string();
            let domain_key = keys::domain(&domain.name).to_string();
            let portfolio_key = keys::portfolio(&portfolio.name).to_string();
            rel += 1;
            ctx.truth.relationships.push(RelationshipQuestion {
                id: question_id("rel", rel),
                kind: RelationshipKind::ServicePortfolio,
                question: format!(
                    "Which portfolio does the {} service belong to?",
                    service.slug
                ),
                subject_key: service_key.clone(),
                expected_keys: vec![portfolio_key.clone()],
                paths: vec![vec![
                    start(service_key.clone()),
                    step(&component_key, "Contains", false),
                    step(&domain_key, "Contains", false),
                    step(&portfolio_key, "Contains", false),
                ]],
            });
            rel += 1;
            let team = component.owner;
            ctx.truth.relationships.push(RelationshipQuestion {
                id: question_id("rel", rel),
                kind: RelationshipKind::ServiceTeamLead,
                question: format!(
                    "Who leads the team that owns the component containing the {} service?",
                    service.slug
                ),
                subject_key: service_key.clone(),
                expected_keys: vec![lead_key(plan, team)],
                paths: vec![vec![
                    start(service_key.clone()),
                    step(&component_key, "Contains", false),
                    step(team_key(plan, team), "Owns", false),
                    step(lead_key(plan, team), "Leads", false),
                ]],
            });
            if let Some(api) = &service.api {
                let api_key = keys::api_spec(&service.slug).to_string();
                let op = &api.operations[0];
                let endpoint = keys::api_endpoint(&service.slug, op.method, &op.route).to_string();
                rel += 1;
                ctx.truth.relationships.push(RelationshipQuestion {
                    id: question_id("rel", rel),
                    kind: RelationshipKind::EndpointDomain,
                    question: format!(
                        "Which domain owns the code that implements {} {} of the {} API?",
                        op.method, op.route, service.slug
                    ),
                    subject_key: endpoint.clone(),
                    expected_keys: vec![domain_key.clone()],
                    paths: vec![vec![
                        start(endpoint),
                        step(&api_key, "Contains", false),
                        step(&service_key, "Implements", false),
                        step(&component_key, "Contains", false),
                        step(&domain_key, "Contains", false),
                    ]],
                });
            }
        }
        if service.api.is_some() && !plan.callers[index].is_empty() {
            let api_key = keys::api_spec(&service.slug).to_string();
            let consumers: Vec<String> = plan.callers[index]
                .iter()
                .map(|&c| keys::service(&plan.services[c].slug).to_string())
                .collect();
            rel += 1;
            ctx.truth.relationships.push(RelationshipQuestion {
                id: question_id("rel", rel),
                kind: RelationshipKind::ApiConsumers,
                question: format!("Which services consume the {} API?", service.slug),
                subject_key: api_key.clone(),
                expected_keys: consumers.clone(),
                paths: consumers
                    .iter()
                    .map(|c| vec![start(api_key.clone()), step(c, "Consumes", false)])
                    .collect(),
            });
        }
    }
}
