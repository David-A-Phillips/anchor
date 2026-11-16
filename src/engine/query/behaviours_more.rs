//! More query-engine behaviours: dismissal reasons, documentation quoting and relation chains.

use super::behaviours::{REFERENCE, TRUTH, ask, gap_kinds, source, unit, world};
use super::*;
use crate::base::{ChunkKind, EdgeAssertion, NodeAssertion, SourceKind, UnitKind, keys};

/// Runbook pages for two services, each with a dismissed suggestion of the same title but a
/// different reason; plus a service node per page so the question can name it.
fn runbooks() -> QueryEngine {
    let wiki = SourceId::new("wiki");
    let mut units = Vec::new();
    for (service, reason, said, who) in [
        (
            "billing-api",
            "Out of scope: tracked separately as PAY-9",
            "Retries hammer the gateway",
            "carol",
        ),
        (
            "payments-api",
            "Rejected: the gateway already deduplicates",
            "Jitter is not needed",
            "dave",
        ),
    ] {
        let mut org = unit("org", UnitKind::OrgModel, service);
        org.add_node(NodeAssertion::new(
            NodeKind::Service,
            keys::service(service),
            service,
        ));
        units.push(org);

        let page_id = format!("{service}-runbook");
        let mut page = unit("wiki", UnitKind::WikiPage, &page_id);
        let page_node = page.add_node(
            NodeAssertion::new(
                NodeKind::WikiPage,
                keys::wiki_page(&wiki, &page_id),
                format!("{service} runbook"),
            )
            .alias(page_id.as_str()),
        );
        let thread_key = keys::thread(&wiki, &format!("{page_id}/t-1"));
        let thread = page.add_node(
            NodeAssertion::new(NodeKind::Thread, thread_key, "Add jitter to retries")
                .attr(attrs::THREAD_STATUS, attrs::THREAD_DISMISSED)
                .attr(attrs::CLOSE_REASON, reason)
                .attr(attrs::CLOSED_BY, who)
                .attr(attrs::REASON, said),
        );
        page.add_edge(EdgeAssertion::new(&thread, EdgeKind::Discusses, &page_node));
        units.push(page);
    }
    world(
        &[
            source("wiki", SourceKind::Wiki, REFERENCE),
            source("org", SourceKind::OrgModel, TRUTH),
        ],
        units,
    )
}

#[tokio::test]
async fn a_thread_title_shared_by_two_pages_is_narrowed_by_the_service_the_question_names() {
    let engine = runbooks();
    for (service, reason, who, other) in [
        (
            "billing-api",
            "Out of scope: tracked separately as PAY-9",
            "carol",
            "dave",
        ),
        (
            "payments-api",
            "Rejected: the gateway already deduplicates",
            "dave",
            "carol",
        ),
    ] {
        let response = ask(
            &engine,
            &format!("Why was the suggestion \"Add jitter to retries\" dismissed in {service}?"),
        )
        .await;
        let answer = response.answer.clone().expect("a direct answer");
        assert!(
            answer.contains(reason) && answer.contains(&format!("by {who}")),
            "{answer}"
        );
        assert!(!answer.contains(other), "{answer}");
        assert!(
            !gap_kinds(&response).contains(&"ambiguous_entity"),
            "{:?}",
            response.gaps
        );
    }
}

#[tokio::test]
async fn without_a_scope_the_shared_thread_title_is_flagged_ambiguous() {
    let engine = runbooks();
    let response = ask(
        &engine,
        "Why was the suggestion \"Add jitter to retries\" dismissed?",
    )
    .await;
    let gap = response
        .gaps
        .iter()
        .find(|gap| gap.kind == "ambiguous_entity")
        .expect("ambiguity is reported");
    assert!(gap.detail.contains("2 entities") && gap.detail.contains("Add jitter to retries"));
    // Both candidates are still offered as evidence.
    let threads = response
        .context
        .items
        .iter()
        .filter(|item| item.node.kind == NodeKind::Thread)
        .count();
    assert_eq!(threads, 2);
}

#[tokio::test]
async fn the_reason_a_thing_was_closed_comes_before_what_it_originally_said() {
    let engine = runbooks();
    // Asked about the page, the thread that discusses it explains itself: dismissal reason
    // first, the original objection in parentheses after it.
    let page = ask(
        &engine,
        "Why was the suggestion on billing-api-runbook dismissed?",
    )
    .await
    .answer
    .expect("answer");
    let closed = page.find("Out of scope").expect("dismissal reason");
    let said = page
        .find("it had said: Retries hammer the gateway")
        .expect("original text");
    assert!(closed < said, "{page}");
    assert!(
        page.contains("by carol: Out of scope"),
        "who dismissed it leads the reason: {page}"
    );
    assert!(page.contains("(thread, dismissed)"), "{page}");

    // Asked about the thread itself, only the dismissal is the answer.
    let thread = ask(
        &engine,
        "Why was the suggestion \"Add jitter to retries\" dismissed in billing-api?",
    )
    .await
    .answer
    .expect("answer");
    assert!(
        thread.contains("is dismissed: by carol: Out of scope"),
        "{thread}"
    );
    assert!(!thread.contains("Retries hammer"), "{thread}");
}

#[tokio::test]
async fn a_dismissal_without_a_stated_reason_still_reports_that_it_was_dismissed() {
    let wiki = SourceId::new("wiki");
    let mut page = unit("wiki", UnitKind::WikiPage, "gateway-notes");
    let page_node = page.add_node(NodeAssertion::new(
        NodeKind::WikiPage,
        keys::wiki_page(&wiki, "gateway-notes"),
        "gateway notes",
    ));
    let thread = page.add_node(
        NodeAssertion::new(
            NodeKind::Thread,
            keys::thread(&wiki, "gateway-notes/t-1"),
            "Drop the legacy endpoint",
        )
        .attr(attrs::THREAD_STATUS, attrs::THREAD_DISMISSED),
    );
    page.add_edge(EdgeAssertion::new(&thread, EdgeKind::Discusses, &page_node));
    let engine = world(&[source("wiki", SourceKind::Wiki, REFERENCE)], vec![page]);
    let answer = ask(
        &engine,
        "Why was the suggestion on gateway-notes dismissed?",
    )
    .await
    .answer
    .expect("answer");
    assert!(
        answer.contains("Drop the legacy endpoint") && answer.contains("dismissed"),
        "{answer}"
    );
    assert!(!answer.contains("it had said"), "{answer}");
}

// ---- documentation pages -------------------------------------------------------------------

#[test]
fn question_terms_keep_content_words_lightly_stemmed() {
    let terms = question_terms("How do I rotate the signing keys that the Billing-API uses?");
    assert!(terms.contains("rotate"), "{terms:?}");
    assert!(
        terms.contains("sign"),
        "'signing' loses its suffix: {terms:?}"
    );
    assert!(
        terms.contains("keys"),
        "a stem under four letters is not taken: {terms:?}"
    );
    assert!(
        terms.contains("bill"),
        "'billing' loses its suffix: {terms:?}"
    );
    assert!(
        terms.contains("uses"),
        "a stem under four letters is not taken: {terms:?}"
    );
    assert!(
        !terms.contains("that") && !terms.contains("the"),
        "stop words go: {terms:?}"
    );
    assert!(
        !terms.contains("api"),
        "words under four letters go: {terms:?}"
    );

    assert_eq!(
        question_terms("Rotated, rotating and rotates"),
        HashSet::from(["rotat".to_owned()]),
        "inflections of one word collapse to one term"
    );
    assert!(question_terms("").is_empty());
    assert!(question_terms("?! ... --").is_empty());
    assert!(question_terms("What does this have with that").is_empty());
    // Non-ASCII letters are words too and lower-case properly.
    assert!(question_terms("Überprüfung der Schlüssel").contains("überprüfung"));
}

#[test]
fn truncation_respects_character_boundaries_and_short_texts_pass_through() {
    assert_eq!(truncate("short", 10), "short");
    assert_eq!(truncate("exactly10!", 10), "exactly10!");
    assert_eq!(
        truncate("héllo wörld", 2),
        "h…",
        "cut inside a two-byte character moves back"
    );
    assert_eq!(truncate("日本語のテキスト", 4), "日…");
    assert_eq!(truncate("anything", 0), "…");
}

/// A service with two documentation pages (a long runbook and a short design note) and one page
/// that documents something else.
fn documented_service() -> QueryEngine {
    let wiki = SourceId::new("wiki");
    let mut org = unit("org", UnitKind::OrgModel, "billing-api");
    org.add_node(NodeAssertion::new(
        NodeKind::Service,
        keys::service("billing-api"),
        "billing-api",
    ));
    org.add_node(NodeAssertion::new(
        NodeKind::Service,
        keys::service("search-api"),
        "search-api",
    ));

    let mut runbook = unit("wiki", UnitKind::WikiPage, "billing-api-runbook");
    let page = runbook.add_node(NodeAssertion::new(
        NodeKind::WikiPage,
        keys::wiki_page(&wiki, "billing-api-runbook"),
        "Billing API runbook",
    ));
    for text in [
        "Overview: the billing service charges cards and issues invoices every night.",
        "Rotate the signing keys by running the rotate-keys job, then restart the workers so they reload the keystore.",
        "Escalation contacts are listed in the on-call schedule; page the secondary after fifteen minutes.",
        "Rollbacks are performed by redeploying the previous release tag from the pipeline.",
    ] {
        runbook.add_chunk(&page, ChunkKind::Text, text);
    }
    runbook.add_edge(EdgeAssertion::new(
        &page,
        EdgeKind::Documents,
        keys::service("billing-api"),
    ));

    let mut design = unit("wiki", UnitKind::WikiPage, "billing-api-design");
    let note = design.add_node(NodeAssertion::new(
        NodeKind::WikiPage,
        keys::wiki_page(&wiki, "billing-api-design"),
        "Billing API design note",
    ));
    design.add_chunk(
        &note,
        ChunkKind::Text,
        "The ledger is append-only; nothing is ever updated in place.",
    );
    design.add_edge(EdgeAssertion::new(
        &note,
        EdgeKind::Documents,
        keys::service("billing-api"),
    ));

    let mut other = unit("wiki", UnitKind::WikiPage, "search-api-runbook");
    let elsewhere = other.add_node(NodeAssertion::new(
        NodeKind::WikiPage,
        keys::wiki_page(&wiki, "search-api-runbook"),
        "Search API runbook",
    ));
    other.add_chunk(
        &elsewhere,
        ChunkKind::Text,
        "Rotate the signing keys of the search index with reindex-all.",
    );
    other.add_edge(EdgeAssertion::new(
        &elsewhere,
        EdgeKind::Documents,
        keys::service("search-api"),
    ));
    world(
        &[
            source("wiki", SourceKind::Wiki, REFERENCE),
            source("org", SourceKind::OrgModel, TRUTH),
        ],
        vec![org, runbook, design, other],
    )
}

#[tokio::test]
async fn a_how_to_question_quotes_the_documentation_of_the_entity_it_names() {
    let engine = documented_service();
    let response = ask(&engine, "How do I rotate the signing keys for billing-api?").await;
    assert_eq!(
        response.intent.kind,
        IntentKind::HowTo,
        "{:?}",
        response.intent
    );
    let titles: Vec<&str> = response
        .context
        .items
        .iter()
        .map(|i| i.node.name.as_str())
        .collect();
    assert!(titles.contains(&"Billing API runbook"), "{titles:?}");
    assert!(titles.contains(&"Billing API design note"), "{titles:?}");
    assert!(
        !titles.contains(&"Search API runbook")
            || titles.iter().position(|t| *t == "Search API runbook")
                > titles.iter().position(|t| *t == "Billing API runbook"),
        "another service's page never outranks this one's: {titles:?}"
    );

    let runbook = response
        .context
        .items
        .iter()
        .find(|item| item.node.name == "Billing API runbook")
        .expect("runbook in the context");
    assert_ne!(runbook.role, ItemRole::Overridden);
    assert_eq!(
        runbook.snippets.len(),
        2,
        "two passages are quoted, not the whole page"
    );
    let quoted: String = runbook
        .snippets
        .iter()
        .map(|s| s.text.as_str())
        .collect::<Vec<_>>()
        .join(" | ");
    assert!(
        quoted.contains("Rotate the signing keys by running the rotate-keys job"),
        "{quoted}"
    );
    assert!(
        !quoted.contains("Escalation") && !quoted.contains("Rollbacks"),
        "passages that share nothing with the question are left out: {quoted}"
    );
    // Passages keep their order in the page.
    let positions: Vec<usize> = ["Overview", "Rotate", "Escalation", "Rollbacks"]
        .iter()
        .filter_map(|word| quoted.find(word))
        .collect();
    assert!(positions.windows(2).all(|w| w[0] < w[1]), "{quoted}");
}

#[tokio::test]
async fn documentation_is_ranked_by_how_much_of_the_question_it_covers() {
    let engine = documented_service();
    let response = ask(&engine, "How do I rotate the signing keys for billing-api?").await;
    let order: Vec<&str> = response
        .context
        .items
        .iter()
        .filter(|item| item.node.kind == NodeKind::WikiPage)
        .map(|item| item.node.name.as_str())
        .collect();
    assert_eq!(
        order.first().copied(),
        Some("Billing API runbook"),
        "{order:?}"
    );

    // A question that fits the design note better puts it first.
    let ledger = ask(
        &engine,
        "How does billing-api update the append-only ledger?",
    )
    .await;
    let order: Vec<&str> = ledger
        .context
        .items
        .iter()
        .filter(|item| item.node.kind == NodeKind::WikiPage)
        .map(|item| item.node.name.as_str())
        .collect();
    assert_eq!(
        order.first().copied(),
        Some("Billing API design note"),
        "{order:?}"
    );
}

#[tokio::test]
async fn documentation_is_not_pulled_in_for_questions_that_are_not_how_to_or_definitions() {
    let engine = documented_service();
    let ownership = ask(&engine, "Who owns billing-api?").await;
    assert!(
        ownership
            .context
            .items
            .iter()
            .all(|item| item.role == ItemRole::Evidence || item.node.kind != NodeKind::WikiPage)
    );
    let pages: Vec<_> = ownership
        .context
        .items
        .iter()
        .filter(|item| item.node.kind == NodeKind::WikiPage)
        .collect();
    assert!(
        pages.iter().all(|item| item.score < 0.85),
        "no documentation boost for an ownership question: {:?}",
        pages
            .iter()
            .map(|p| (&p.node.name, p.score))
            .collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn a_how_to_question_about_an_unknown_entity_reports_a_gap_instead_of_inventing_documentation()
 {
    let engine = documented_service();
    let response = ask(
        &engine,
        "How do I rotate the signing keys for frobnicator-api?",
    )
    .await;
    assert!(
        gap_kinds(&response).contains(&"unresolved_entity")
            || gap_kinds(&response).contains(&"low_confidence"),
        "{:?}",
        response.gaps
    );
    assert!(response.answer.is_none());
}

// ---- relation chains -----------------------------------------------------------------------

/// Commerce (portfolio) > Payments (domain) > Billing (component) > billing-api and checkout;
/// Billing Core owns the component and is led by Alice; checkout consumes billing-api's API;
/// `orphan-api` is a service nobody contains and nobody owns.
fn organisation() -> QueryEngine {
    let mut org = unit("org", UnitKind::OrgModel, "acme");
    let portfolio = org.add_node(NodeAssertion::new(
        NodeKind::Portfolio,
        keys::portfolio("Commerce"),
        "Commerce",
    ));
    let domain = org.add_node(NodeAssertion::new(
        NodeKind::Domain,
        keys::domain("Payments"),
        "Payments",
    ));
    let component = org.add_node(NodeAssertion::new(
        NodeKind::Component,
        keys::component("Billing"),
        "Billing",
    ));
    let billing = org.add_node(NodeAssertion::new(
        NodeKind::Service,
        keys::service("billing-api"),
        "billing-api",
    ));
    let checkout = org.add_node(NodeAssertion::new(
        NodeKind::Service,
        keys::service("checkout"),
        "checkout",
    ));
    org.add_node(NodeAssertion::new(
        NodeKind::Service,
        keys::service("orphan-api"),
        "orphan-api",
    ));
    let team = org.add_node(NodeAssertion::new(
        NodeKind::Team,
        keys::team("Billing Core"),
        "Billing Core",
    ));
    let alice = org.add_node(NodeAssertion::new(
        NodeKind::Person,
        keys::person("alice"),
        "Alice Smith",
    ));
    let api = org.add_node(NodeAssertion::new(
        NodeKind::ApiSpec,
        keys::api_spec("ledger-api"),
        "Ledger API",
    ));
    org.add_edge(EdgeAssertion::new(&portfolio, EdgeKind::Contains, &domain));
    org.add_edge(EdgeAssertion::new(&domain, EdgeKind::Contains, &component));
    org.add_edge(EdgeAssertion::new(&component, EdgeKind::Contains, &billing));
    org.add_edge(EdgeAssertion::new(
        &component,
        EdgeKind::Contains,
        &checkout,
    ));
    org.add_edge(EdgeAssertion::new(&team, EdgeKind::Owns, &component));
    org.add_edge(EdgeAssertion::new(&alice, EdgeKind::Leads, &team));
    org.add_edge(EdgeAssertion::new(&billing, EdgeKind::Implements, &api));
    org.add_edge(EdgeAssertion::new(&checkout, EdgeKind::Consumes, &api));
    world(&[source("org", SourceKind::OrgModel, TRUTH)], vec![org])
}

#[tokio::test]
async fn a_three_hop_question_is_followed_edge_by_edge_to_a_direct_answer() {
    let engine = organisation();
    let response = ask(
        &engine,
        "Who leads the team that owns the component containing billing-api?",
    )
    .await;
    let answer = response.answer.clone().expect("a chain answer");
    assert!(
        answer.starts_with("Lead of billing-api: Alice Smith"),
        "{answer}"
    );
    assert!(answer.contains("[source of truth]"), "{answer}");
    assert!(
        answer.contains("via"),
        "the intermediate entities are named: {answer}"
    );
    assert!(answer.contains("Billing"), "{answer}");
    assert!(answer.ends_with('.'));
    assert_eq!(response.context.items[0].node.key, "person:alice");
    assert!(
        !response.context.items[0].path.is_empty(),
        "the answer carries the path that reached it"
    );
}

#[tokio::test]
async fn single_hop_chains_name_their_relation() {
    let engine = organisation();
    for (question, label, expected) in [
        (
            "Which portfolio does billing-api belong to?",
            "Portfolio of billing-api",
            "Commerce",
        ),
        (
            "What domain contains billing-api?",
            "Domain of billing-api",
            "Payments",
        ),
        (
            "Which component contains billing-api?",
            "Component of billing-api",
            "Billing",
        ),
        (
            "Which team owns billing-api?",
            "Owning team of billing-api",
            "Billing Core",
        ),
    ] {
        let response = ask(&engine, question).await;
        let answer = response.answer.clone().unwrap_or_default();
        assert!(
            answer.starts_with(label) && answer.contains(expected),
            "{question}: {answer}"
        );
    }
}

#[tokio::test]
async fn collecting_hops_list_every_related_entity() {
    let engine = organisation();
    let consumers = ask(&engine, "Which services consume the Ledger API?").await;
    let answer = consumers.answer.clone().expect("answer");
    assert!(
        answer.starts_with("Consumers of Ledger API: checkout"),
        "{answer}"
    );
    assert_eq!(consumers.intent.kind, IntentKind::Dependency);

    let consumed = ask(&engine, "Which APIs does checkout consume?").await;
    let answer = consumed.answer.clone().expect("answer");
    assert!(
        answer.contains("Consumed APIs of checkout: Ledger API"),
        "{answer}"
    );
}

#[tokio::test]
async fn a_chain_whose_hop_leads_nowhere_falls_back_to_ordinary_retrieval() {
    let engine = organisation();
    // orphan-api is in no component: the second hop finds nothing, so the question is answered
    // like any other, from what lies around the entity, without a chain answer.
    let response = ask(
        &engine,
        "Who leads the team that owns the component containing orphan-api?",
    )
    .await;
    let answer = response.answer.clone().unwrap_or_default();
    assert!(
        !answer.starts_with("Lead of"),
        "no invented chain answer: {answer}"
    );
    assert!(
        response
            .context
            .items
            .iter()
            .any(|item| item.node.key == "service:orphan-api"),
        "the entity itself is still offered: {:?}",
        response
            .context
            .items
            .iter()
            .map(|i| &i.node.key)
            .collect::<Vec<_>>()
    );
    assert!(
        response
            .context
            .items
            .iter()
            .all(|item| item.node.key != "person:alice"),
        "nobody else's lead is substituted"
    );

    // A hop that exists for one entity but not for another is decided per entity.
    let working = ask(&engine, "Which component contains checkout?").await;
    assert!(
        working
            .answer
            .expect("answer")
            .contains("Component of checkout: Billing")
    );
}

#[tokio::test]
async fn a_question_naming_no_entity_follows_no_chain() {
    let engine = organisation();
    let response = ask(&engine, "Who leads the team that owns the component?").await;
    assert!(!response.answer.unwrap_or_default().starts_with("Lead of"));
}

#[tokio::test]
async fn plain_ownership_is_not_a_chain_and_keeps_its_conflict_handling() {
    let engine = organisation();
    let response = ask(&engine, "Who owns billing-api?").await;
    let answer = response.answer.expect("answer");
    assert!(answer.contains("is owned by Billing Core"), "{answer}");
}

proptest::proptest! {
    #![proptest_config(proptest::prelude::ProptestConfig::with_cases(256))]

    /// Whatever a user types, reading a relation chain or the question's terms never panics
    /// (both slice the text by byte offsets they compute themselves).
    #[test]
    fn arbitrary_questions_never_break_term_extraction_or_chain_parsing(question in "(?s).{0,200}") {
        let _ = question_terms(&question);
        if let Some(chain) = relation::parse(&question) {
            proptest::prop_assert!(!chain.hops.is_empty());
        }
        let truncated = truncate(&question, 7);
        proptest::prop_assert!(truncated.len() <= 7 + '…'.len_utf8());
    }

    #[test]
    fn phrases_embedded_in_unicode_noise_are_still_found_without_panicking(
        prefix in "\\PC{0,20}",
        suffix in "\\PC{0,20}",
    ) {
        let question = format!("{prefix} who leads the team that owns billing-api {suffix}");
        let _ = relation::parse(&question);
    }
}
