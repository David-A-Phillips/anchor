//! `wiki/`: Markdown and HTML pages with comment threads (sidecar JSON and embedded HTML).

use std::collections::BTreeSet;
use std::fmt::Write as _;

use serde_json::{Value, json};

use super::plan::{Plan, capitalise, timestamp};
use super::rng::Rng;
use super::truth::{JustificationKind, JustificationQuestion};
use super::{Ctx, SOURCE_WIKI, code, question_id, words};
use crate::base::{Result, SourceId, keys};

enum Kind {
    Stale(usize),
    ComponentOwnership(usize),
    Runbook(usize),
    Design(usize, usize),
}

struct Page {
    id: String,
    title: String,
    kind: Kind,
}

struct Comment {
    id: String,
    author: usize,
    day: i64,
    body: String,
    reply_to: Option<String>,
}

struct Thread {
    id: String,
    status: &'static str,
    anchor: String,
    closed_by: Option<usize>,
    close_reason: Option<String>,
    closed_day: i64,
    comments: Vec<Comment>,
}

fn page_plan(ctx: &Ctx) -> Vec<Page> {
    let plan = ctx.plan;
    let budget = ctx.spec.wiki_pages;
    let mut pages: Vec<Page> = Vec::new();
    for (index, service) in plan.services.iter().enumerate() {
        if service.stale_team.is_some() && pages.len() < budget {
            pages.push(Page {
                id: format!("{}-ownership", service.slug),
                title: format!("{} Ownership (legacy)", service.slug),
                kind: Kind::Stale(index),
            });
        }
    }
    let n = plan.services.len();
    let topics = words::WIKI_TOPICS.len();
    let mut round = 0;
    while pages.len() < budget {
        let before = pages.len();
        if round < n {
            pages.push(Page {
                id: format!("{}-runbook", plan.services[round].slug),
                title: format!("{} Runbook", plan.services[round].slug),
                kind: Kind::Runbook(round),
            });
        }
        if pages.len() < budget && round < n * topics {
            let service = round % n;
            let topic = (round / n + service) % topics;
            pages.push(Page {
                id: keys::slugify(&format!(
                    "{} {}",
                    words::WIKI_TOPICS[topic],
                    plan.services[service].slug
                )),
                title: format!(
                    "{} design for {}",
                    words::WIKI_TOPICS[topic],
                    plan.services[service].slug
                ),
                kind: Kind::Design(topic, service),
            });
        }
        if pages.len() < budget && round < plan.components.len() {
            let component = &plan.components[round];
            if !component.services.is_empty() {
                pages.push(Page {
                    id: format!("{}-ownership", keys::slugify(&component.name)),
                    title: format!("{} Ownership", component.name),
                    kind: Kind::ComponentOwnership(round),
                });
            }
        }
        round += 1;
        if pages.len() == before && round > n * topics + plan.components.len() {
            break;
        }
    }
    pages.truncate(budget);
    pages
}

fn person_json(plan: &Plan, p: usize) -> Value {
    let person = &plan.people[p];
    json!({ "name": person.name, "handle": person.handle, "email": person.email })
}

fn make_threads(
    plan: &Plan,
    rng: &mut Rng,
    slug: &str,
    anchors: &[String],
    count: usize,
    (force_dismissed, rate): (bool, f64),
    base_day: i64,
) -> Vec<Thread> {
    let people = plan.people.len();
    let mut threads = Vec::new();
    for t in 0..count {
        let anchor = anchors[t % anchors.len()].clone();
        let author = rng.below(people);
        let mut closer = rng.below(people);
        if closer == author {
            closer = (author + 1) % people;
        }
        let day = base_day + 3 * t as i64;
        let dismissed =
            (t == 0 && (force_dismissed || rng.chance(rate))) || (t > 0 && rng.chance(rate / 2.0));
        let mut thread = Thread {
            id: format!("t-{}", t + 1),
            status: "open",
            anchor,
            closed_by: None,
            close_reason: None,
            closed_day: day + 2,
            comments: Vec::new(),
        };
        if dismissed {
            let reason = (*rng.pick(words::WIKI_DISMISS_REASONS)).to_owned();
            thread.status = "dismissed";
            thread.comments.push(Comment {
                id: "c-1".to_owned(),
                author,
                day,
                body: format!("Suggestion: add a retry loop around the {slug} client calls here."),
                reply_to: None,
            });
            thread.comments.push(Comment {
                id: "c-2".to_owned(),
                author: closer,
                day: day + 2,
                body: reason.clone(),
                reply_to: Some("c-1".to_owned()),
            });
            thread.closed_by = Some(closer);
            thread.close_reason = Some(reason);
        } else if t % 2 == 1 {
            thread.status = "resolved";
            thread.comments.push(Comment {
                id: "c-1".to_owned(),
                author,
                day,
                body: format!("Is the {slug} runbook step still accurate?"),
                reply_to: None,
            });
            let note = (*rng.pick(words::WIKI_RESOLVE_NOTES)).to_owned();
            thread.comments.push(Comment {
                id: "c-2".to_owned(),
                author: closer,
                day: day + 1,
                body: note.clone(),
                reply_to: Some("c-1".to_owned()),
            });
            thread.closed_by = Some(closer);
            thread.close_reason = Some(note);
        } else {
            thread.comments.push(Comment {
                id: "c-1".to_owned(),
                author,
                day,
                body: format!("Can we add the on-call contact for {slug}?"),
                reply_to: None,
            });
        }
        threads.push(thread);
    }
    threads
}

fn thread_json(plan: &Plan, thread: &Thread) -> Value {
    let comments: Vec<Value> = thread
        .comments
        .iter()
        .map(|c| {
            let mut entry = json!({
                "id": c.id,
                "author": person_json(plan, c.author),
                "created": timestamp(c.day, 40_000),
                "body": c.body,
            });
            if let (Some(reply), Some(object)) = (&c.reply_to, entry.as_object_mut()) {
                object.insert("reply_to".to_owned(), json!(reply));
            }
            entry
        })
        .collect();
    let mut entry = json!({
        "id": thread.id,
        "kind": "inline",
        "status": thread.status,
        "anchor": thread.anchor,
        "comments": comments,
    });
    if let (Some(closer), Some(object)) = (thread.closed_by, entry.as_object_mut()) {
        object.insert("closed_by".to_owned(), json!(plan.people[closer].name));
        object.insert("close_reason".to_owned(), json!(thread.close_reason));
        object.insert(
            "closed_at".to_owned(),
            json!(timestamp(thread.closed_day, 50_000)),
        );
    }
    entry
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn thread_html(plan: &Plan, thread: &Thread) -> String {
    let mut out = format!(
        "<div class=\"comment-thread\" data-thread-id=\"{}\" data-kind=\"inline\" data-status=\"{}\" data-anchor=\"{}\"",
        thread.id,
        thread.status,
        escape(&thread.anchor)
    );
    if let Some(closer) = thread.closed_by {
        let _ = write!(
            out,
            " data-closed-by=\"{}\" data-close-reason=\"{}\" data-closed-at=\"{}\"",
            escape(&plan.people[closer].name),
            escape(thread.close_reason.as_deref().unwrap_or("")),
            timestamp(thread.closed_day, 50_000)
        );
    }
    out.push_str(">\n");
    if let Some(first) = thread.comments.first() {
        let _ = writeln!(
            out,
            "  <div class=\"comment\" data-comment-id=\"{}\" data-author=\"{}\" data-created=\"{}\">",
            first.id,
            escape(&plan.people[first.author].name),
            timestamp(first.day, 40_000)
        );
        let _ = writeln!(
            out,
            "    <div class=\"comment-body\"><p>{}</p></div>",
            escape(&first.body)
        );
        for reply in &thread.comments[1..] {
            let _ = writeln!(
                out,
                "    <div class=\"comment\" data-comment-id=\"{}\" data-author=\"{}\" data-created=\"{}\">{}</div>",
                reply.id,
                escape(&plan.people[reply.author].name),
                timestamp(reply.day, 40_000),
                escape(&reply.body)
            );
        }
        out.push_str("  </div>\n");
    }
    out.push_str("</div>\n");
    out
}

fn record_truth(ctx: &mut Ctx, page: &Page, threads: &[Thread], counter: &mut usize) {
    let source = SourceId::new(SOURCE_WIKI);
    let page_key = keys::wiki_page(&source, &page.id);
    let dismissed: Vec<&Thread> = threads.iter().filter(|t| t.status == "dismissed").collect();
    for thread in &dismissed {
        let Some(reason) = &thread.close_reason else {
            continue;
        };
        *counter += 1;
        let thread_key = keys::thread(&source, &format!("{}/{}", page.id, thread.id));
        let closing = keys::comment(&source, &format!("{}/{}/c-2", page.id, thread.id));
        let question = if dismissed.len() == 1 && (*counter).is_multiple_of(2) {
            format!("Why was the suggestion on {} dismissed?", page.title)
        } else {
            format!(
                "Why was the suggestion on \"{}\" in {} dismissed?",
                thread.anchor, page.title
            )
        };
        ctx.truth.justification.push(JustificationQuestion {
            id: question_id("why-thread", *counter),
            kind: JustificationKind::ThreadDismissed,
            question,
            subject_key: page_key.to_string(),
            expected_keys: vec![thread_key.to_string(), closing.to_string()],
            key_phrases: vec![reason.trim_end_matches('.').to_owned()],
        });
    }
}

pub(crate) fn emit(ctx: &mut Ctx) -> Result<()> {
    let plan = ctx.plan;
    let pages = page_plan(ctx);
    let mut rng = Rng::derive(ctx.spec.seed, "wiki", 0);
    let mut thread_questions = 0;
    let mut forced_md = true;
    let mut forced_html = true;
    let space = "ENG";
    for (number, page) in pages.iter().enumerate() {
        let author = rng.below(plan.people.len());
        let who = &plan.people[author];
        let created = 20 + 2 * number as i64;
        let mut anchors: Vec<String> = Vec::new();
        let mut documents: Vec<String> = Vec::new();
        let mut labels = vec!["engineering"];
        let mut body = String::new();
        let mut updated = created + 100 + rng.below(100) as i64;
        let mut slug_for_threads = String::new();
        let mut thread_count = 0;
        let mut html = false;
        // (kind, question, phrase the page states)
        let mut prose: Vec<(&str, String, String)> = Vec::new();
        match &page.kind {
            Kind::Stale(index) => {
                let service = &plan.services[*index];
                let wrong = &plan.teams[service.stale_team.unwrap_or(0)];
                labels = vec!["ownership", "legacy"];
                updated = 5;
                documents.push(service.slug.clone());
                let _ = writeln!(body, "# {}\n", page.title);
                let _ = writeln!(
                    body,
                    "The {} service is owned by the {} team, led by {}. Contact {} for incidents.",
                    service.slug, wrong.name, plan.people[wrong.lead].name, wrong.name
                );
                anchors.push(format!("owned by the {} team", wrong.name));
                slug_for_threads = service.slug.clone();
                thread_count = 1;
                ctx.stale_pages.insert(*index, page.id.clone());
            }
            Kind::ComponentOwnership(c) => {
                let component = &plan.components[*c];
                let owner = &plan.teams[component.owner];
                labels = vec!["ownership"];
                documents.push(component.name.clone());
                let _ = writeln!(body, "# {}\n", page.title);
                let _ = writeln!(
                    body,
                    "The {} component is owned by the {} team, led by {}.\n",
                    component.name, owner.name, plan.people[owner.lead].name
                );
                let _ = writeln!(body, "Services in this component:\n");
                for &s in &component.services {
                    let _ = writeln!(body, "- {}", plan.services[s].slug);
                }
            }
            Kind::Runbook(index) => {
                let service = &plan.services[*index];
                labels = vec!["runbook", "oncall"];
                documents.push(service.slug.clone());
                let _ = writeln!(body, "# {}\n", page.title);
                let _ = writeln!(body, "## Overview\n");
                let _ = writeln!(
                    body,
                    "The `{}` service is operated through its entry point `{}`.\n",
                    service.slug,
                    code::entry_name(service)
                );
                let restart = format!(
                    "Restart the {} workers when the queue depth stays above the alert threshold.",
                    service.slug
                );
                let retry = format!(
                    "Retry failed {} calls with a bounded backoff.",
                    words::NOUNS[*index % words::NOUNS.len()]
                );
                let escalate =
                    "Escalate to the owning team if the incident lasts longer than thirty minutes."
                        .to_owned();
                let _ = writeln!(
                    body,
                    "## Procedure\n\n1. {restart}\n2. {retry}\n3. {escalate}\n"
                );
                if let Some(api) = &service.api {
                    let _ = writeln!(body, "## API\n");
                    for op in &api.operations {
                        let _ = writeln!(body, "- `{} {}`", op.method, op.route);
                    }
                    let _ = writeln!(body);
                    documents.push(format!(
                        "{} {}",
                        api.operations[0].method, api.operations[0].route
                    ));
                }
                if let Some(ticket) = plan.tickets.iter().find(|t| t.service == *index) {
                    let _ = writeln!(body, "Follow-up work is tracked in {}.", ticket.key);
                }
                let noun = words::NOUNS[*index % words::NOUNS.len()];
                prose.push((
                    "runbook_step",
                    format!(
                        "What does the {} runbook say to do when the queue depth stays above the alert threshold?",
                        service.slug
                    ),
                    format!("Restart the {} workers", service.slug),
                ));
                prose.push((
                    "runbook_step_paraphrased",
                    format!(
                        "The {} backlog keeps growing past its alarm limit. What is the documented remedy?",
                        service.slug
                    ),
                    format!("Restart the {} workers", service.slug),
                ));
                prose.push((
                    "runbook_step_paraphrased",
                    format!(
                        "In {}, requests for a {noun} keep erroring out. How are we supposed to handle that?",
                        service.slug
                    ),
                    format!("Retry failed {noun} calls with a bounded backoff"),
                ));
                anchors = vec![restart, retry, escalate];
                slug_for_threads = service.slug.clone();
                thread_count = 3;
            }
            Kind::Design(topic, index) => {
                html = true;
                let service = &plan.services[*index];
                labels = vec!["design"];
                documents.push(service.slug.clone());
                let first = format!(
                    "The {} design of {} keeps state in {}.",
                    words::WIKI_TOPICS[*topic].to_lowercase(),
                    service.slug,
                    words::NOUNS[(*index + *topic) % words::NOUNS.len()]
                );
                let second = format!(
                    "Callers reach the service through `{}`.",
                    code::entry_name(service)
                );
                body = format!(
                    "<h1>{}</h1>\n<p>{}</p>\n<p>{}</p>\n",
                    escape(&page.title),
                    escape(&first),
                    escape(&second)
                );
                let topic_name = words::WIKI_TOPICS[*topic].to_lowercase();
                let store = words::NOUNS[(*index + *topic) % words::NOUNS.len()];
                prose.push((
                    "design_statement",
                    format!(
                        "Where does the {topic_name} design of {} keep state?",
                        service.slug
                    ),
                    format!("keeps state in {store}"),
                ));
                prose.push((
                    "design_statement_paraphrased",
                    format!(
                        "For {}'s approach to {topic_name}, what holds the persisted data?",
                        service.slug
                    ),
                    format!("keeps state in {store}"),
                ));
                anchors = vec![first, second];
                slug_for_threads = service.slug.clone();
                thread_count = 2;
            }
        }

        let force = if html {
            std::mem::take(&mut forced_html)
        } else if matches!(page.kind, Kind::Runbook(_)) {
            std::mem::take(&mut forced_md)
        } else {
            false
        };
        let threads = if thread_count > 0 {
            make_threads(
                plan,
                &mut rng,
                &slug_for_threads,
                &anchors,
                thread_count,
                (force, ctx.spec.rates.dismissed_thread),
                created + 5,
            )
        } else {
            Vec::new()
        };
        record_truth(ctx, page, &threads, &mut thread_questions);
        let page_key = keys::wiki_page(&SourceId::new(SOURCE_WIKI), &page.id).to_string();
        for (kind, question, phrase) in prose {
            let id = question_id("prose", ctx.truth.prose.len() + 1);
            ctx.truth.prose.push(super::truth::ProseQuestion {
                id,
                kind: kind.to_owned(),
                question,
                subject_key: page_key.clone(),
                key_phrases: vec![phrase],
            });
        }
        ctx.report.wiki_pages += 1;
        ctx.report.wiki_threads += threads.len();

        let label_set: BTreeSet<&str> = labels.iter().copied().collect();
        let label_list = label_set.iter().copied().collect::<Vec<_>>().join(", ");
        if html {
            let mut text = format!(
                "<!DOCTYPE html>\n<html>\n<head>\n<meta charset=\"utf-8\">\n<meta name=\"page-id\" content=\"{id}\">\n<meta name=\"title\" content=\"{title}\">\n<meta name=\"labels\" content=\"{labels}\">\n<meta name=\"author\" content=\"{author} &lt;{email}&gt;\">\n<meta name=\"created\" content=\"{created}\">\n<meta name=\"updated\" content=\"{updated}\">\n<meta name=\"space\" content=\"{space}\">\n<meta name=\"version\" content=\"{version}\">\n<meta name=\"documents\" content=\"{documents}\">\n<title>{title}</title>\n</head>\n<body>\n<main>\n{body}</main>\n",
                id = page.id,
                title = escape(&page.title),
                labels = label_list,
                author = escape(&who.name),
                email = who.email,
                created = timestamp(created, 32_000),
                updated = timestamp(updated, 33_000),
                version = 1 + number % 7,
                documents = escape(&documents.join(", ")),
            );
            for thread in &threads {
                text.push_str(&thread_html(plan, thread));
            }
            text.push_str("</body>\n</html>\n");
            ctx.out
                .write("wiki", &format!("wiki/{}.html", page.id), &text)?;
        } else {
            let doc_list = documents
                .iter()
                .map(|d| format!("\"{d}\""))
                .collect::<Vec<_>>()
                .join(", ");
            let text = format!(
                "---\nid: {id}\ntitle: \"{title}\"\nlabels: [{labels}]\nauthor: {author} <{email}>\ncreated: {created}\nupdated: {updated}\nspace: {space}\nversion: {version}\ndocuments: [{docs}]\n---\n{body}",
                id = page.id,
                title = page.title,
                labels = label_list,
                author = who.name,
                email = who.email,
                created = timestamp(created, 32_000),
                updated = timestamp(updated, 33_000),
                version = 1 + number % 7,
                docs = doc_list,
            );
            ctx.out
                .write("wiki", &format!("wiki/{}.md", page.id), &text)?;
            if !threads.is_empty() {
                let sidecar = json!({
                    "threads": threads.iter().map(|t| thread_json(plan, t)).collect::<Vec<_>>()
                });
                ctx.out.write(
                    "wiki",
                    &format!("wiki/{}.comments.json", page.id),
                    &serde_json::to_string_pretty(&sidecar)?,
                )?;
            }
        }
        let _ = capitalise;
    }
    Ok(())
}
