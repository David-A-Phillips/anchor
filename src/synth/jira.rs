//! `jira/`: issue exports in Jira REST shape, with blocks links, blocked reasons and comments.

use serde_json::{Value, json};

use super::plan::{Plan, timestamp};
use super::truth::{JustificationKind, JustificationQuestion};
use super::{Ctx, SOURCE_JIRA, question_id};
use crate::base::{Result, SourceId, keys};

const BATCH: usize = 25;

fn jira_time(day: i64, second: u32) -> String {
    timestamp(day, second).replace('Z', ".000+0000")
}

fn user(plan: &Plan, p: usize) -> Value {
    let person = &plan.people[p];
    json!({ "displayName": person.name, "emailAddress": person.email, "name": person.handle })
}

pub(crate) fn emit(ctx: &mut Ctx) -> Result<()> {
    let plan = ctx.plan;
    let source = SourceId::new(SOURCE_JIRA);
    let mut batch: Vec<Value> = Vec::new();
    let mut files = 0;
    let mut asked = 0;
    for (index, ticket) in plan.tickets.iter().enumerate() {
        let service = &plan.services[ticket.service];
        let mut fields = json!({
            "summary": ticket.summary,
            "description": format!("Work item for the {} service.", service.slug),
            "status": { "name": ticket.status },
            "issuetype": { "name": ticket.kind },
            "priority": { "name": if ticket.status == "Blocked" { "High" } else { "Medium" } },
            "labels": [service.slug.clone()],
            "assignee": user(plan, ticket.assignee),
            "reporter": user(plan, ticket.reporter),
            "created": jira_time(ticket.day, 30_000),
            "updated": jira_time(ticket.day + 5, 31_000),
        });
        let mut issue = json!({ "key": ticket.key });
        let mut comments: Vec<Value> = Vec::new();
        if let Some(component) = ticket.component {
            fields["components"] = json!([{ "name": plan.components[component].name }]);
        }
        if let Some(parent) = ticket.parent {
            fields["parent"] = json!({ "key": plan.tickets[parent].key });
        }
        if let (Some(blocker), Some(reason)) = (ticket.blocked_by, &ticket.reason) {
            let blocker = &plan.tickets[blocker];
            issue["names"] = json!({ "customfield_10050": "Blocked Reason" });
            fields["customfield_10050"] = json!(reason);
            fields["issuelinks"] = json!([{
                "type": { "name": "Blocks", "inward": "is blocked by", "outward": "blocks" },
                "inwardIssue": { "key": blocker.key, "fields": { "summary": blocker.summary } },
            }]);
            comments.push(json!({
                "id": "1",
                "author": user(plan, ticket.assignee),
                "body": format!("Blocked: {reason}. The blocker is tracked in {}.", blocker.key),
                "created": jira_time(ticket.day + 1, 32_000),
            }));
            comments.push(json!({
                "id": "2",
                "author": user(plan, ticket.reporter),
                "body": format!("Thanks, we will pick this up again once {} moves.", blocker.key),
                "created": jira_time(ticket.day + 2, 33_000),
            }));
            asked += 1;
            let question = if asked % 2 == 0 {
                format!("What is blocking {}?", ticket.key)
            } else {
                format!("Why is {} blocked?", ticket.key)
            };
            ctx.truth.justification.push(JustificationQuestion {
                id: question_id("why-ticket", asked),
                kind: JustificationKind::TicketBlocked,
                question,
                subject_key: keys::ticket(&ticket.key).to_string(),
                expected_keys: vec![
                    keys::ticket(&blocker.key).to_string(),
                    keys::comment(&source, &format!("{}-1", ticket.key)).to_string(),
                ],
                key_phrases: vec![reason.clone()],
            });
        } else if index % 3 == 0 {
            comments.push(json!({
                "id": "1",
                "author": user(plan, ticket.reporter),
                "body": format!("Picking this up for the {} service next sprint.", service.slug),
                "created": jira_time(ticket.day + 1, 32_000),
            }));
        }
        if !comments.is_empty() {
            fields["comment"] = json!({ "comments": comments });
        }
        issue["fields"] = fields;
        batch.push(issue);
        if batch.len() == BATCH {
            files += 1;
            flush(ctx, &mut batch, files)?;
        }
    }
    if !batch.is_empty() {
        files += 1;
        flush(ctx, &mut batch, files)?;
    }
    Ok(())
}

fn flush(ctx: &mut Ctx, batch: &mut Vec<Value>, number: usize) -> Result<()> {
    let text = serde_json::to_string_pretty(&Value::Array(std::mem::take(batch)))?;
    ctx.out
        .write("jira", &format!("jira/issues-{number:04}.json"), &text)
}
