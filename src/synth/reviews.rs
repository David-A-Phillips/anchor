//! `reviews/`: pull-request exports (GitHub REST shape) with blocking and dismissed reviews.

use serde_json::json;

use super::code;
use super::plan::{ReviewScenario, timestamp};
use super::truth::{JustificationKind, JustificationQuestion};
use super::{Ctx, SOURCE_REVIEWS, question_id};
use crate::base::{Result, SourceId, keys};

pub(crate) fn emit(ctx: &mut Ctx) -> Result<()> {
    let plan = ctx.plan;
    let source = SourceId::new(SOURCE_REVIEWS);
    let mut dismissed_asked = 0;
    let mut blocking_asked = 0;
    for pr in &plan.pull_requests {
        let service = &plan.services[pr.service];
        let author = &plan.people[pr.author];
        let reviewer = &plan.people[pr.reviewer];
        let n = pr.number;
        let review_id = n * 10 + 1;
        let comment_id = n * 10 + 2;
        let path = code::source_path(service, 0);
        let ticket = pr.ticket.map(|t| plan.tickets[t].key.clone());
        let body = match &ticket {
            Some(key) => format!("Implements {key} for the {} service.", service.slug),
            None => format!("Changes the {} service.", service.slug),
        };
        let (state, merged) = match pr.scenario {
            ReviewScenario::Blocking => ("open", false),
            _ => ("closed", true),
        };
        let mut pull_request = json!({
            "number": n,
            "title": pr.title,
            "body": body,
            "state": state,
            "merged": merged,
            "draft": false,
            "user": { "login": author.handle },
            "html_url": format!("https://git.acme.example/acme/{}/pull/{n}", service.slug),
            "created_at": timestamp(pr.day, 28_000),
            "updated_at": timestamp(pr.day + 3, 29_000),
            "base": { "ref": "main", "repo": { "name": service.slug, "full_name": format!("acme/{}", service.slug) } },
            "head": { "ref": match &ticket { Some(key) => format!("feature/{key}"), None => "feature/change".to_owned() } },
            "labels": [{ "name": "backend" }],
        });
        if merged {
            pull_request["merged_at"] = json!(timestamp(pr.day + 3, 29_000));
        }
        let diff_hunk = "@@ -10,6 +10,8 @@\n fn handle(input: u64) -> u64 {\n+    retry(input)\n }";
        let mut document = json!({ "pull_request": pull_request });
        let thread_key = format!("{}-pr-{n}-thread-{comment_id}", service.slug);
        match pr.scenario {
            ReviewScenario::Approved => {
                document["reviews"] = json!([{
                    "id": review_id,
                    "user": { "login": reviewer.handle },
                    "body": pr.reason,
                    "submitted_at": timestamp(pr.day + 2, 30_000),
                    "state": "APPROVED",
                }]);
            }
            ReviewScenario::Blocking | ReviewScenario::Dismissed => {
                let dismissed = pr.scenario == ReviewScenario::Dismissed;
                let mut review = json!({
                    "id": review_id,
                    "user": { "login": reviewer.handle },
                    "body": if dismissed { format!("Requesting changes on {}.", service.slug) } else { pr.reason.clone() },
                    "submitted_at": timestamp(pr.day + 1, 30_000),
                    "state": if dismissed { "DISMISSED" } else { "CHANGES_REQUESTED" },
                });
                if dismissed {
                    review["dismissed_state"] = json!("CHANGES_REQUESTED");
                    review["dismissed_by"] =
                        json!({ "login": plan.people[plan.teams[0].lead].handle });
                    review["dismissal_message"] = json!(pr.reason);
                }
                document["reviews"] = json!([review]);
                document["review_comments"] = json!([{
                    "id": comment_id,
                    "in_reply_to_id": null,
                    "pull_request_review_id": review_id,
                    "user": { "login": reviewer.handle },
                    "body": if dismissed { format!("Please reconsider the error handling in {path}.") } else { pr.reason.clone() },
                    "path": path,
                    "line": 12,
                    "original_line": 12,
                    "diff_hunk": diff_hunk,
                    "created_at": timestamp(pr.day + 1, 30_500),
                }]);
                document["review_threads"] = json!([{
                    "id": format!("T{n}"),
                    "is_resolved": false,
                    "is_outdated": false,
                    "comments": [comment_id],
                }]);
                let approval = keys::approval(&source, &format!("pr-{n}-review-{review_id}"));
                let pr_key = keys::pull_request(&service.slug, n);
                if dismissed {
                    dismissed_asked += 1;
                    ctx.truth.justification.push(JustificationQuestion {
                        id: question_id("why-review", dismissed_asked),
                        kind: JustificationKind::ReviewDismissed,
                        question: format!("Why was the review on {}#{n} dismissed?", service.slug),
                        subject_key: pr_key.to_string(),
                        expected_keys: vec![
                            approval.to_string(),
                            keys::thread(&source, &thread_key).to_string(),
                        ],
                        key_phrases: vec![pr.reason.trim_end_matches('.').to_owned()],
                    });
                } else {
                    blocking_asked += 1;
                    ctx.truth.justification.push(JustificationQuestion {
                        id: question_id("why-pr", blocking_asked),
                        kind: JustificationKind::PullRequestBlocked,
                        question: format!("What is blocking pull request {}#{n}?", service.slug),
                        subject_key: pr_key.to_string(),
                        expected_keys: vec![approval.to_string()],
                        key_phrases: vec![pr.reason.trim_end_matches('.').to_owned()],
                    });
                }
            }
        }
        document["issue_comments"] = json!([{
            "id": n * 10 + 3,
            "user": { "login": author.handle },
            "body": format!("Ready for review: {}", pr.title),
            "created_at": timestamp(pr.day, 28_500),
        }]);
        ctx.out.write(
            "reviews",
            &format!("reviews/{}-{n}.json", service.slug),
            &serde_json::to_string_pretty(&document)?,
        )?;
    }
    Ok(())
}
