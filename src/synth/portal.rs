//! `portal/`: API records with approvals, plus the OpenAPI definitions they point to.

use serde_json::{Value, json};

use super::plan::timestamp;
use super::{Ctx, repos};
use crate::base::Result;

pub(crate) fn emit(ctx: &mut Ctx) -> Result<()> {
    let plan = ctx.plan;
    for (index, service) in plan.services.iter().enumerate() {
        let Some(api) = &service.api else { continue };
        let Some(definition) = repos::openapi(service) else {
            continue;
        };
        let to_yaml = |value: &Value| {
            serde_yaml_ng::to_string(value)
                .map_err(|err| crate::base::Error::invalid(err.to_string()))
        };
        ctx.out.write(
            "portal",
            &format!("portal/specs/{}.yaml", service.slug),
            &to_yaml(&definition)?,
        )?;

        let approvals: Vec<Value> = api
            .approvals
            .iter()
            .map(|approval| {
                let mut entry = json!({
                    "id": approval.id,
                    "approver": format!("team:{}", approval.approver),
                    "decision": approval.decision,
                    "blocking": approval.blocking,
                    "reason": approval.reason,
                    "decided_at": timestamp(approval.day, 36_000),
                });
                if let (Some(scope), Some(object)) = (&approval.scope, entry.as_object_mut()) {
                    object.insert("scope".to_owned(), json!(scope));
                }
                entry
            })
            .collect();
        let consumers: Vec<Value> = plan.callers[index]
            .iter()
            .map(|&c| json!(plan.services[c].slug))
            .collect();
        let owner = &plan.teams[super::plan::api_owner_team(plan, index)];
        let record = json!({
            "id": api.portal_id,
            "name": service.slug,
            "title": api.title,
            "version": "1.0.0",
            "description": format!("HTTP API of the {} service.", service.slug),
            "status": api.status,
            "owner_team": owner.name,
            "spec": format!("specs/{}.yaml", service.slug),
            "implemented_by": [service.slug],
            "consumers": consumers,
            "url": format!("https://portal.acme.example/apis/{}", api.portal_id),
            "approvals": approvals,
        });
        ctx.out.write(
            "portal",
            &format!("portal/{}.yaml", service.slug),
            &to_yaml(&record)?,
        )?;
        ctx.report.apis += 1;
        ctx.report.approvals += api.approvals.len();
    }
    Ok(())
}
