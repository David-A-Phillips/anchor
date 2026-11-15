//! `org/org.yaml` (authoritative) and `legacy/legacy-directory.yaml` (stale, reference-only).

use std::collections::BTreeSet;

use serde_json::{Value, json};

use super::Ctx;
use crate::base::Result;

fn to_yaml(value: &Value) -> Result<String> {
    serde_yaml_ng::to_string(value).map_err(|err| crate::base::Error::invalid(err.to_string()))
}

pub(crate) fn emit(ctx: &mut Ctx) -> Result<()> {
    let plan = ctx.plan;
    let handle = |p: usize| plan.people[p].handle.clone();

    let portfolios: Vec<Value> = plan
        .portfolios
        .iter()
        .enumerate()
        .map(|(p, portfolio)| {
            let domains: Vec<Value> = plan
                .domains
                .iter()
                .enumerate()
                .filter(|(_, d)| d.portfolio == p)
                .map(|(d, domain)| {
                    let components: Vec<Value> = plan
                        .components
                        .iter()
                        .filter(|c| c.domain == d && !c.services.is_empty())
                        .map(|component| {
                            let services: Vec<Value> = component
                                .services
                                .iter()
                                .map(|&s| json!(plan.services[s].slug))
                                .collect();
                            json!({
                                "name": component.name,
                                "owner": plan.teams[component.owner].slug,
                                "description": format!("The {} component of the {} domain.", component.name, domain.name),
                                "services": services,
                            })
                        })
                        .collect();
                    json!({
                        "name": domain.name,
                        "lead": handle(domain.lead),
                        "description": format!("The {} domain of {}.", domain.name, portfolio.name),
                        "components": components,
                    })
                })
                .collect();
            json!({
                "name": portfolio.name,
                "lead": handle(portfolio.lead),
                "description": format!("The {} portfolio.", portfolio.name),
                "domains": domains,
            })
        })
        .collect();

    let teams: Vec<Value> = plan
        .teams
        .iter()
        .map(|team| {
            let members: Vec<Value> = team.members.iter().map(|&m| json!(handle(m))).collect();
            json!({
                "name": team.name,
                "handle": team.slug,
                "email": team.email,
                "lead": handle(team.lead),
                "description": format!("The {} team.", team.name),
                "members": members,
            })
        })
        .collect();

    let people: Vec<Value> = plan
        .people
        .iter()
        .map(|person| {
            let mut entry = json!({
                "name": person.name,
                "handle": person.handle,
                "email": person.email,
                "role": person.role,
            });
            if let (Some(boss), Some(object)) = (person.reports_to, entry.as_object_mut()) {
                object.insert("reports_to".to_owned(), json!(handle(boss)));
            }
            entry
        })
        .collect();

    let document = json!({
        "organisation": "acme",
        "portfolios": portfolios,
        "teams": teams,
        "people": people,
    });
    ctx.out.write("org", "org/org.yaml", &to_yaml(&document)?)?;

    // The legacy directory: an old export that still names the previous owners. It is wrong on
    // purpose (conflict cases) or the only thing that names an owner at all (fallback cases).
    let mut referenced = BTreeSet::new();
    let mut services = Vec::new();
    for service in &plan.services {
        let claim = service.stale_team.or(service.reference_team);
        if let Some(team) = claim {
            referenced.insert(team);
            services.push(json!({
                "name": service.slug,
                "owner": plan.teams[team].slug,
                "description": "Imported from the 2021 directory.",
            }));
        }
    }
    if !services.is_empty() {
        let teams: Vec<Value> = referenced
            .iter()
            .map(|&t| {
                json!({
                    "name": plan.teams[t].name,
                    "handle": plan.teams[t].slug,
                })
            })
            .collect();
        let legacy = json!({ "organisation": "acme", "teams": teams, "services": services });
        ctx.out
            .write("legacy", "legacy/legacy-directory.yaml", &to_yaml(&legacy)?)?;
    }
    Ok(())
}
