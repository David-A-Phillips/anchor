//! The Atlassian connector against a local mock of the Confluence and Jira REST endpoints.
//!
//! The mock is hermetic (127.0.0.1, ephemeral port) and implements only what the connector calls,
//! with the response shapes the connector expects: these tests prove the connector is consistent
//! with its own understanding of the APIs, not that the understanding is right (see
//! docs/atlassian.md).
#![allow(clippy::unwrap_used)]

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anchor::base::{EdgeKind, NodeKey, SourceKind, TrustLevel, keys};
use anchor::cli::atlassian::{
    AtlassianOptions, AtlassianReport, Credentials, RetryPolicy, Sleeper, fetch_atlassian,
};
use anchor::config::{AnchorConfig, AtlassianConfig, AtlassianDeployment, SourceConfig};
use anchor::engine::Engine;
use anchor::engine::router::ProviderKind;
use async_trait::async_trait;
use axum::extract::{Path as UrlPath, Query, State};
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use base64::Engine as _;
use parking_lot::Mutex;
use serde_json::{Value, json};

// ---------------------------------------------------------------------------------------------
// The mock site
// ---------------------------------------------------------------------------------------------

/// Answers the server gives out of turn: `(path fragment, status, Retry-After seconds)`.
type Injected = Vec<(String, u16, Option<u64>)>;

struct Inner {
    pages: Mutex<Vec<Value>>,
    comments: Mutex<HashMap<String, Vec<Value>>>,
    issues: Mutex<Vec<Value>>,
    issue_comments: Mutex<HashMap<String, Vec<Value>>>,
    log: Mutex<Vec<String>>,
    inject: Mutex<Injected>,
    expected_auth: String,
}

#[derive(Clone)]
struct Mock(Arc<Inner>);

const PAGE_CAP: usize = 2;

impl Mock {
    fn log(&self) -> Vec<String> {
        self.0.log.lock().clone()
    }

    fn clear_log(&self) {
        self.0.log.lock().clear();
    }

    /// Ids of the pages whose full content was requested.
    fn full_page_fetches(&self) -> Vec<String> {
        self.log()
            .iter()
            .filter_map(|line| {
                let path = line.split('?').next().unwrap();
                let id = path.rsplit_once("/content/")?.1;
                id.chars()
                    .all(|c| c.is_ascii_digit())
                    .then(|| id.to_owned())
            })
            .collect()
    }

    /// The JQLs of full (`fields=*all`) issue searches.
    fn full_issue_searches(&self) -> Vec<String> {
        self.log()
            .iter()
            .filter(|line| {
                line.contains("search")
                    && (line.contains("fields=*all") || line.contains("fields=%2Aall"))
            })
            .cloned()
            .collect()
    }

    fn inject(&self, fragment: &str, status: u16, retry_after: Option<u64>) {
        self.0
            .inject
            .lock()
            .push((fragment.to_owned(), status, retry_after));
    }

    fn edit_page(&self, id: &str, edit: impl FnOnce(&mut Value)) {
        let mut pages = self.0.pages.lock();
        let page = pages.iter_mut().find(|p| p["id"] == id).unwrap();
        edit(page);
    }

    fn remove_page(&self, id: &str) {
        self.0.pages.lock().retain(|p| p["id"] != id);
    }

    fn edit_issue(&self, key: &str, edit: impl FnOnce(&mut Value)) {
        let mut issues = self.0.issues.lock();
        edit(issues.iter_mut().find(|i| i["key"] == key).unwrap());
    }
}

/// Logs the request and applies injected failures and the authentication check.
fn gate(mock: &Mock, uri: &Uri, headers: &HeaderMap) -> Option<Response> {
    mock.0.log.lock().push(uri.to_string());
    let auth = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if auth != mock.0.expected_auth {
        return Some(StatusCode::UNAUTHORIZED.into_response());
    }
    let mut inject = mock.0.inject.lock();
    let position = inject
        .iter()
        .position(|(fragment, _, _)| uri.path().contains(fragment.as_str()))?;
    let (_, status, retry_after) = inject[position].clone();
    inject.remove(position);
    let mut response = StatusCode::from_u16(status).unwrap().into_response();
    if let Some(seconds) = retry_after {
        response
            .headers_mut()
            .insert("retry-after", seconds.to_string().parse().unwrap());
    }
    Some(response)
}

fn between<'a>(text: &'a str, start: &str, end: &str) -> Option<&'a str> {
    let from = text.find(start)? + start.len();
    let to = text[from..].find(end)? + from;
    Some(&text[from..to])
}

fn clip(updated: &str) -> String {
    updated[..16].replace('T', " ")
}

async fn content_search(
    State(mock): State<Mock>,
    uri: Uri,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    if let Some(response) = gate(&mock, &uri, &headers) {
        return response;
    }
    let cql = &q["cql"];
    let space = between(cql, "space = \"", "\"").unwrap();
    let since = between(cql, "lastmodified >= \"", "\"");
    let mut pages: Vec<Value> = mock
        .0
        .pages
        .lock()
        .iter()
        .filter(|p| p["space"]["key"] == space)
        .filter(|p| {
            since.is_none_or(|s| clip(p["version"]["when"].as_str().unwrap()).as_str() >= s)
        })
        .cloned()
        .collect();
    pages.sort_by_key(|p| p["version"]["when"].as_str().unwrap().to_owned());
    let start: usize = q["start"].parse().unwrap();
    let limit = q["limit"].parse::<usize>().unwrap().min(PAGE_CAP);
    let window: Vec<Value> = pages.iter().skip(start).take(limit).cloned().collect();
    let with_restrictions = q["expand"].contains("restrictions");
    let results: Vec<Value> = window
        .iter()
        .map(|p| {
            let mut slim = json!({
                "id": p["id"], "title": p["title"], "version": p["version"], "ancestors": p["ancestors"],
            });
            if with_restrictions {
                slim["restrictions"] = p["restrictions"].clone();
            }
            slim
        })
        .collect();
    let mut answer = json!({ "results": results, "size": results.len() });
    if start + window.len() < pages.len() {
        answer["_links"] = json!({ "next": "/next" });
    }
    Json(answer).into_response()
}

async fn content_get(
    State(mock): State<Mock>,
    uri: Uri,
    headers: HeaderMap,
    UrlPath(id): UrlPath<String>,
) -> Response {
    if let Some(response) = gate(&mock, &uri, &headers) {
        return response;
    }
    match mock.0.pages.lock().iter().find(|p| p["id"] == id.as_str()) {
        Some(page) => Json(page.clone()).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn content_comments(
    State(mock): State<Mock>,
    uri: Uri,
    headers: HeaderMap,
    UrlPath(id): UrlPath<String>,
) -> Response {
    if let Some(response) = gate(&mock, &uri, &headers) {
        return response;
    }
    let results = mock.0.comments.lock().get(&id).cloned().unwrap_or_default();
    Json(json!({ "results": results })).into_response()
}

async fn space_list(State(mock): State<Mock>, uri: Uri, headers: HeaderMap) -> Response {
    if let Some(response) = gate(&mock, &uri, &headers) {
        return response;
    }
    Json(json!({ "results": [{ "key": "ENG" }] })).into_response()
}

async fn jira_search(
    State(mock): State<Mock>,
    uri: Uri,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    if let Some(response) = gate(&mock, &uri, &headers) {
        return response;
    }
    let jql = &q["jql"];
    let project = between(jql, "project = \"", "\"");
    let since = between(jql, "updated >= \"", "\"");
    let keys: Option<Vec<String>> = between(jql, "key in (", ")").map(|list| {
        list.split(',')
            .map(|k| k.trim_matches('"').to_owned())
            .collect()
    });
    let mut issues: Vec<Value> = mock
        .0
        .issues
        .lock()
        .iter()
        .filter(|i| {
            project.is_none_or(|p| i["key"].as_str().unwrap().starts_with(&format!("{p}-")))
        })
        .filter(|i| {
            since.is_none_or(|s| clip(i["fields"]["updated"].as_str().unwrap()).as_str() >= s)
        })
        .filter(|i| {
            keys.as_ref()
                .is_none_or(|k| k.iter().any(|k| i["key"] == k.as_str()))
        })
        .cloned()
        .collect();
    issues.sort_by_key(|i| i["fields"]["updated"].as_str().unwrap().to_owned());
    let cloud = uri.path().ends_with("/search/jql");
    let start: usize = if cloud {
        q.get("nextPageToken").map_or(0, |t| t.parse().unwrap())
    } else {
        q["startAt"].parse().unwrap()
    };
    let size = q["maxResults"].parse::<usize>().unwrap().min(PAGE_CAP);
    let window: Vec<Value> = issues.iter().skip(start).take(size).cloned().collect();
    let only_updated = q["fields"] == "updated";
    let rendered: Vec<Value> = window
        .iter()
        .map(|i| {
            if only_updated {
                json!({ "key": i["key"], "fields": { "updated": i["fields"]["updated"] } })
            } else {
                i.clone()
            }
        })
        .collect();
    let mut answer = json!({ "issues": rendered });
    if q.get("expand").is_some_and(|e| e == "names") {
        answer["names"] = json!({ "customfield_10050": "Blocked Reason" });
    }
    if cloud {
        let more = start + window.len() < issues.len();
        answer["isLast"] = json!(!more);
        if more {
            answer["nextPageToken"] = json!((start + window.len()).to_string());
        }
    } else {
        answer["total"] = json!(issues.len());
        answer["startAt"] = json!(start);
    }
    Json(answer).into_response()
}

async fn issue_comments(
    State(mock): State<Mock>,
    uri: Uri,
    headers: HeaderMap,
    UrlPath(key): UrlPath<String>,
) -> Response {
    if let Some(response) = gate(&mock, &uri, &headers) {
        return response;
    }
    let comments = mock
        .0
        .issue_comments
        .lock()
        .get(&key)
        .cloned()
        .unwrap_or_default();
    Json(json!({ "comments": comments, "total": comments.len() })).into_response()
}

fn router(mock: &Mock) -> Router {
    let mut app = Router::new();
    for prefix in ["/rest/api", "/wiki/rest/api"] {
        app = app
            .route(&format!("{prefix}/content/search"), get(content_search))
            .route(&format!("{prefix}/content/{{id}}"), get(content_get))
            .route(
                &format!("{prefix}/content/{{id}}/child/comment"),
                get(content_comments),
            )
            .route(&format!("{prefix}/space"), get(space_list));
    }
    app.route("/rest/api/2/search", get(jira_search))
        .route("/rest/api/3/search/jql", get(jira_search))
        .route("/rest/api/2/issue/{key}/comment", get(issue_comments))
        .route("/rest/api/3/issue/{key}/comment", get(issue_comments))
        .with_state(mock.clone())
}

fn page(
    id: &str,
    title: &str,
    version: u64,
    when: &str,
    ancestors: &[(&str, &str)],
    body: &str,
    restricted: (&[&str], &[&str]),
) -> Value {
    json!({
        "id": id, "type": "page", "title": title, "space": { "key": "ENG" },
        "version": { "number": version, "when": when },
        "history": {
            "createdBy": { "displayName": "Alice Smith", "email": "alice@acme.example" },
            "createdDate": "2024-03-01T09:00:00.000Z",
        },
        "ancestors": ancestors.iter().map(|(i, t)| json!({ "id": i, "title": t })).collect::<Vec<_>>(),
        "metadata": { "labels": { "results": [{ "name": "payments" }, { "name": "oncall" }] } },
        "restrictions": { "read": { "restrictions": {
            "user": { "results": restricted.0.iter().map(|n| json!({ "displayName": n })).collect::<Vec<_>>() },
            "group": { "results": restricted.1.iter().map(|n| json!({ "name": n })).collect::<Vec<_>>() },
        } } },
        "body": { "storage": { "value": body } },
        "_links": { "base": "https://acme.example/wiki", "webui": format!("/spaces/ENG/pages/{id}") },
    })
}

fn comment(id: &str, who: &str, text: &str, parent: Option<&str>, extensions: Value) -> Value {
    json!({
        "id": id, "type": "comment",
        "body": { "storage": { "value": format!("<p>{text}</p>") } },
        "history": { "createdBy": { "displayName": who }, "createdDate": "2024-05-01T10:00:00.000Z" },
        "ancestors": parent.map(|p| vec![json!({ "id": p, "type": "comment" })]).unwrap_or_default(),
        "extensions": extensions,
    })
}

const RUNBOOK_BODY: &str = "<p>The payments-api service retries failed charges. Ask \
<ac:link><ri:user ri:account-id=\"u-42\"/></ac:link> and see PAY-123.</p>\
<ac:structured-macro ac:name=\"code\"><ac:plain-text-body><![CDATA[retry(charge)]]></ac:plain-text-body></ac:structured-macro>";

fn issue(key: &str, summary: &str, updated: &str, extra: Value) -> Value {
    let mut fields = json!({
        "summary": summary, "status": { "name": "Open" }, "created": "2024-04-01T09:00:00.000+0000",
        "updated": updated,
    });
    for (name, value) in extra.as_object().unwrap() {
        fields[name] = value.clone();
    }
    json!({ "id": "1", "key": key, "fields": fields })
}

fn seed(auth: &str) -> Mock {
    let pages = vec![
        page(
            "90",
            "Operations",
            1,
            "2024-04-01T08:00:00.000Z",
            &[],
            "<p>Ops home.</p>",
            (&[], &[]),
        ),
        page(
            "100",
            "Payments API Runbook",
            3,
            "2024-05-01T09:00:00.000Z",
            &[("90", "Operations")],
            RUNBOOK_BODY,
            (&[], &[]),
        ),
        page(
            "110",
            "HR Plan",
            1,
            "2024-05-03T09:00:00.000Z",
            &[],
            "<p>Secret.</p>",
            (&["Carol"], &["hr"]),
        ),
        page(
            "111",
            "Salary Bands",
            1,
            "2024-05-04T09:00:00.000Z",
            &[("110", "HR Plan")],
            "<p>Child of a restricted page.</p>",
            (&[], &[]),
        ),
    ];
    let comments = HashMap::from([(
        "100".to_owned(),
        vec![
            comment(
                "501",
                "Alice Smith",
                "Is retrying forever safe?",
                None,
                json!({ "location": "footer" }),
            ),
            comment(
                "502",
                "Bob Lee",
                "No, bounded.",
                Some("501"),
                json!({ "location": "footer" }),
            ),
            comment(
                "503",
                "Alice Smith",
                "Do we drop this?",
                None,
                json!({
                    "location": "inline",
                    "inlineProperties": { "originalSelection": "retries failed charges" },
                    "resolution": { "status": "dismissed",
                        "lastModifier": { "displayName": "Bob Lee" },
                        "lastModifiedAt": "2024-05-02T10:00:00.000Z" },
                }),
            ),
        ],
    )]);
    let issues = vec![
        issue(
            "PAY-7",
            "Vendor outage",
            "2024-05-01T08:00:00.000+0000",
            json!({}),
        ),
        issue(
            "PAY-123",
            "Charge retries in payments-api",
            "2024-05-02T08:00:00.000+0000",
            json!({
                "description": { "type": "doc", "version": 1, "content": [
                    { "type": "paragraph", "content": [{ "type": "text", "text": "Retry bounded." }] }] },
                "status": { "name": "Blocked" },
                "customfield_10050": "waiting for the vendor",
                "issuelinks": [{
                    "type": { "name": "Blocks", "inward": "is blocked by", "outward": "blocks" },
                    "inwardIssue": { "key": "PAY-7", "fields": { "summary": "Vendor outage" } },
                }],
                // The search result carries one of the two comments.
                "comment": { "total": 2, "comments": [
                    { "id": "1", "author": { "displayName": "Alice Smith" }, "body": "First" }] },
            }),
        ),
        issue(
            "PAY-124",
            "Ledger batching",
            "2024-05-03T08:00:00.000+0000",
            json!({}),
        ),
    ];
    let issue_comments = HashMap::from([(
        "PAY-123".to_owned(),
        vec![
            json!({ "id": "1", "author": { "displayName": "Alice Smith" }, "body": "First" }),
            json!({ "id": "2", "author": { "displayName": "Bob Lee" }, "body": "Second" }),
        ],
    )]);
    Mock(Arc::new(Inner {
        pages: Mutex::new(pages),
        comments: Mutex::new(comments),
        issues: Mutex::new(issues),
        issue_comments: Mutex::new(issue_comments),
        log: Mutex::default(),
        inject: Mutex::default(),
        expected_auth: auth.to_owned(),
    }))
}

async fn serve(mock: &Mock) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let app = router(mock);
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{address}")
}

#[derive(Default)]
struct Recorder(Mutex<Vec<Duration>>);

#[async_trait]
impl Sleeper for Recorder {
    async fn sleep(&self, duration: Duration) {
        self.0.lock().push(duration);
    }
}

impl Recorder {
    fn seconds(&self) -> Vec<u64> {
        self.0.lock().iter().map(Duration::as_secs).collect()
    }
}

struct Site {
    mock: Mock,
    options: AtlassianOptions,
    sleeper: Recorder,
    _dir: tempfile::TempDir,
}

const TOKEN: &str = "s3cret-token";

async fn site(deployment: AtlassianDeployment) -> Site {
    let (credentials, auth) = match deployment {
        AtlassianDeployment::DataCenter => (
            Credentials::Bearer {
                token: TOKEN.into(),
            },
            format!("Bearer {TOKEN}"),
        ),
        AtlassianDeployment::Cloud => (
            Credentials::Basic {
                email: "me@acme.example".into(),
                token: TOKEN.into(),
            },
            format!(
                "Basic {}",
                base64::engine::general_purpose::STANDARD
                    .encode(format!("me@acme.example:{TOKEN}"))
            ),
        ),
    };
    let mock = seed(&auth);
    let base_url = serve(&mock).await;
    let dir = tempfile::tempdir().unwrap();
    let options = AtlassianOptions {
        base_url,
        deployment,
        credentials,
        auth_hint: "the ATL_TOKEN environment variable".into(),
        spaces: vec!["ENG".into()],
        projects: vec!["PAY".into()],
        jql: None,
        out: dir.path().join("out"),
        include_restricted: false,
        max_pages: 0,
        max_issues: 0,
        timeout: Duration::from_secs(10),
        retry: RetryPolicy::default(),
    };
    Site {
        mock,
        options,
        sleeper: Recorder::default(),
        _dir: dir,
    }
}

impl Site {
    async fn run(&self) -> AtlassianReport {
        fetch_atlassian(&self.options, &self.sleeper).await.unwrap()
    }

    fn read(&self, rel: &str) -> String {
        std::fs::read_to_string(self.options.out.join(rel)).unwrap()
    }

    fn exists(&self, rel: &str) -> bool {
        self.options.out.join(rel).exists()
    }
}

// ---------------------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn exports_pages_comments_and_issues_and_skips_restricted_pages() {
    let site = site(AtlassianDeployment::DataCenter).await;
    // A rate limit with Retry-After on a page, and a 500 on the Jira search: both are retried.
    site.mock.inject("/content/100", 429, Some(7));
    site.mock.inject("/rest/api/2/search", 500, None);
    let report = site.run().await;

    assert_eq!(report.pages, 2, "{report:?}");
    assert_eq!(report.issues, 3);
    assert_eq!(
        report.restricted_skipped, 2,
        "the restricted page and its child"
    );
    assert!(!report.truncated);
    assert_eq!(
        site.sleeper.seconds(),
        [7, 1],
        "Retry-After honoured, then 1 s backoff"
    );

    let html = site.read("confluence/ENG/100.html");
    for expected in [
        "name=\"page-id\" content=\"100\"",
        "name=\"title\" content=\"Payments API Runbook\"",
        "name=\"parent-id\" content=\"90\"",
        "name=\"parent-title\" content=\"Operations\"",
        "name=\"labels\" content=\"payments,oncall\"",
        "name=\"author\" content=\"Alice Smith &lt;alice@acme.example&gt;\"",
        "name=\"created\" content=\"2024-03-01T09:00:00.000Z\"",
        "name=\"updated\" content=\"2024-05-01T09:00:00.000Z\"",
        "name=\"url\" content=\"https://acme.example/wiki/spaces/ENG/pages/100\"",
        "name=\"space\" content=\"ENG\"",
        "name=\"version\" content=\"3\"",
        "@u-42",
        "<pre><code>retry(charge)</code></pre>",
    ] {
        assert!(html.contains(expected), "missing {expected} in\n{html}");
    }
    assert!(!html.contains("restricted_to"));
    assert!(site.exists("confluence/ENG/90.html"));
    assert!(!site.exists("confluence/ENG/110.html"));
    assert!(
        !site.exists("confluence/ENG/111.html"),
        "inherits the restriction"
    );

    let sidecar: Value =
        serde_json::from_str(&site.read("confluence/ENG/100.comments.json")).unwrap();
    let threads = sidecar["threads"].as_array().unwrap();
    assert_eq!(threads.len(), 2);
    assert_eq!(threads[0]["id"], "501");
    assert_eq!(threads[0]["kind"], "page");
    assert_eq!(threads[0]["comments"][1]["reply_to"], "501");
    assert_eq!(threads[0]["comments"][1]["body"], "No, bounded.");
    assert_eq!(threads[1]["kind"], "inline");
    assert_eq!(threads[1]["status"], "dismissed");
    assert_eq!(threads[1]["anchor"], "retries failed charges");
    assert_eq!(threads[1]["closed_by"], "Bob Lee");
    assert!(!site.exists("confluence/ENG/90.comments.json"));

    let issue: Value = serde_json::from_str(&site.read("jira/PAY/PAY-123.json")).unwrap();
    assert_eq!(issue["key"], "PAY-123");
    assert_eq!(issue["names"]["customfield_10050"], "Blocked Reason");
    assert_eq!(issue["fields"]["description"]["type"], "doc");
    assert_eq!(
        issue["fields"]["comment"]["comments"]
            .as_array()
            .unwrap()
            .len(),
        2,
        "the missing comment is fetched separately"
    );
    assert!(site.exists("jira/PAY/PAY-7.json") && site.exists("jira/PAY/PAY-124.json"));

    let all: String = std::fs::read_dir(site.options.out.join("confluence/ENG"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(!all.contains(".part"), "no partial files left: {all}");
    assert!(
        !site.mock.log().iter().any(|l| l.contains(TOKEN)),
        "token never in URLs"
    );
}

#[tokio::test]
async fn restricted_pages_are_included_on_request_with_their_restrictions() {
    let mut site = site(AtlassianDeployment::DataCenter).await;
    site.options.include_restricted = true;
    let report = site.run().await;
    assert_eq!(report.pages, 4);
    assert_eq!(report.restricted_skipped, 0);
    let hr = site.read("confluence/ENG/110.html");
    assert!(hr.contains("name=\"restricted_to\""), "{hr}");
    assert!(
        hr.contains("[&quot;group:hr&quot;,&quot;user:Carol&quot;]"),
        "{hr}"
    );
    let child = site.read("confluence/ENG/111.html");
    assert!(
        child.contains("group:hr"),
        "inherited from the parent: {child}"
    );
}

#[tokio::test]
async fn a_page_that_becomes_restricted_or_deleted_is_removed_from_the_export() {
    let site = site(AtlassianDeployment::DataCenter).await;
    site.run().await;
    assert!(site.exists("confluence/ENG/90.html") && site.exists("confluence/ENG/100.html"));
    site.mock.edit_page("100", |p| {
        p["restrictions"]["read"]["restrictions"]["group"]["results"] = json!([{ "name": "eng" }]);
    });
    site.mock.remove_page("90");
    let report = site.run().await;
    assert_eq!(report.removed, 2);
    assert!(!site.exists("confluence/ENG/90.html"), "deleted upstream");
    assert!(!site.exists("confluence/ENG/100.html"), "restricted since");
    assert!(!site.exists("confluence/ENG/100.comments.json"));
}

#[tokio::test]
async fn an_incremental_run_downloads_only_what_changed() {
    let site = site(AtlassianDeployment::DataCenter).await;
    site.run().await;
    site.mock.clear_log();

    let report = site.run().await;
    assert_eq!((report.pages, report.issues), (0, 0), "{report:?}");
    assert!(
        site.mock.full_page_fetches().is_empty(),
        "{:?}",
        site.mock.log()
    );
    assert!(site.mock.full_issue_searches().is_empty());
    let cql_runs = site
        .mock
        .log()
        .iter()
        .filter(|l| l.contains("lastmodified"))
        .count();
    assert!(
        cql_runs > 0,
        "the listing is restricted with lastmodified >= ..."
    );
    assert!(
        site.mock
            .log()
            .iter()
            .any(|l| l.contains("updated+%3E%3D") || l.contains("updated%20%3E%3D"))
    );

    site.mock.edit_page("100", |p| {
        p["version"] = json!({ "number": 4, "when": "2024-05-09T09:00:00.000Z" });
        p["title"] = json!("Payments API Runbook v2");
    });
    site.mock.edit_issue("PAY-124", |i| {
        i["fields"]["updated"] = json!("2024-05-09T10:00:00.000+0000");
        i["fields"]["summary"] = json!("Ledger batching, revised");
    });
    site.mock.clear_log();
    let report = site.run().await;
    assert_eq!((report.pages, report.issues), (1, 1), "{report:?}");
    assert_eq!(site.mock.full_page_fetches(), ["100"]);
    assert_eq!(site.mock.full_issue_searches().len(), 1);
    assert!(
        site.read("confluence/ENG/100.html")
            .contains("Payments API Runbook v2")
    );
    assert!(site.read("jira/PAY/PAY-124.json").contains("revised"));
}

#[tokio::test]
async fn an_interrupted_run_resumes_without_downloading_again() {
    let site = site(AtlassianDeployment::DataCenter).await;
    // The second page (100) keeps failing: four 500s outlast the three retries.
    for _ in 0..4 {
        site.mock.inject("/content/100", 500, None);
    }
    let err = fetch_atlassian(&site.options, &site.sleeper)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("server error"), "{err}");
    assert!(
        site.exists("confluence/ENG/90.html"),
        "work before the failure is kept"
    );
    assert!(!site.exists("confluence/ENG/100.html"));
    assert_eq!(site.sleeper.seconds(), [1, 2, 4]);

    site.mock.clear_log();
    let report = site.run().await;
    assert_eq!(report.pages, 1);
    assert_eq!(
        site.mock.full_page_fetches(),
        ["100"],
        "page 90 is not downloaded again"
    );
    assert_eq!(report.issues, 3);
}

#[tokio::test]
async fn caps_split_the_work_over_runs_without_repeating_any() {
    let mut site = site(AtlassianDeployment::DataCenter).await;
    site.options.max_pages = 1;
    site.options.max_issues = 2;
    let first = site.run().await;
    assert!(first.truncated);
    assert_eq!((first.pages, first.issues), (1, 2));
    let second = site.run().await;
    assert_eq!((second.pages, second.issues), (1, 1));
    let third = site.run().await;
    assert_eq!((third.pages, third.issues), (0, 0));
    assert!(!third.truncated);
    let mut fetched = site.mock.full_page_fetches();
    fetched.sort();
    assert_eq!(fetched, ["100", "90"], "each page downloaded exactly once");
}

#[tokio::test]
async fn cloud_uses_the_wiki_prefix_basic_auth_and_token_paging() {
    let mut site = site(AtlassianDeployment::Cloud).await;
    site.options.spaces = vec!["*".into()];
    let report = site.run().await;
    assert_eq!((report.pages, report.issues), (2, 3), "{report:?}");
    let log = site.mock.log();
    assert!(log.iter().any(|l| l.starts_with("/wiki/rest/api/space")));
    assert!(
        log.iter()
            .any(|l| l.starts_with("/wiki/rest/api/content/search"))
    );
    assert!(log.iter().any(|l| l.starts_with("/rest/api/3/search/jql")));
    assert!(
        site.exists("jira/PAY/PAY-124.json"),
        "second search page via nextPageToken"
    );
}

#[tokio::test]
async fn rejected_credentials_name_the_environment_variable_but_not_the_secret() {
    let mut site = site(AtlassianDeployment::DataCenter).await;
    site.options.credentials = Credentials::Bearer {
        token: "wrong-value".into(),
    };
    let err = fetch_atlassian(&site.options, &site.sleeper)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("401") && err.contains("ATL_TOKEN"), "{err}");
    assert!(!err.contains("wrong-value") && !err.contains(TOKEN));
}

#[tokio::test]
async fn config_reads_credentials_from_the_environment() {
    let config = AtlassianConfig {
        base_url: "https://acme.atlassian.net/".into(),
        deployment: AtlassianDeployment::Cloud,
        email_env: Some("ATL_EMAIL".into()),
        token_env: "ATL_TOKEN".into(),
        confluence_spaces: vec!["ENG".into()],
        jira_projects: vec![],
        jira_jql: None,
        out: None,
        interval_minutes: 0,
        include_restricted: false,
        max_pages: 0,
        max_issues: 0,
    };
    let paths = AnchorConfig::default().paths();
    let env = |name: &str| match name {
        "ATL_EMAIL" => Some("me@acme.example".to_owned()),
        "ATL_TOKEN" => Some("abc".to_owned()),
        _ => None,
    };
    let options = AtlassianOptions::from_config(&config, &paths, &env).unwrap();
    assert_eq!(options.base_url, "https://acme.atlassian.net");
    assert!(options.out.ends_with("atlassian/acme.atlassian.net"));
    let missing = AtlassianOptions::from_config(&config, &paths, &|_| None).unwrap_err();
    assert!(missing.to_string().contains("ATL_TOKEN"));
}

fn source(id: &str, kind: SourceKind, dir: &Path) -> SourceConfig {
    let mut source = SourceConfig::new(id, kind, dir.to_string_lossy());
    source.trust = TrustLevel::ReferenceOnly;
    source
}

#[tokio::test]
async fn the_real_ingesters_read_the_export() {
    let site = site(AtlassianDeployment::DataCenter).await;
    site.run().await;
    let (wiki_dir, jira_dir) = (site.options.wiki_dir(), site.options.jira_dir());
    let data = site._dir.path().join("data");
    let engine = tokio::task::spawn_blocking(move || {
        let mut config = AnchorConfig {
            data_dir: Some(data),
            ..AnchorConfig::default()
        };
        config.models.system1.provider = ProviderKind::Disabled;
        config
            .sources
            .push(source("wiki", SourceKind::Wiki, &wiki_dir));
        config
            .sources
            .push(source("jira", SourceKind::Jira, &jira_dir));
        let engine = Engine::open(config).unwrap();
        let reports = engine.scan_all().unwrap();
        let warnings: Vec<String> = reports
            .iter()
            .flat_map(|r| r.warnings.iter().map(|w| format!("{}: {w}", r.source)))
            .collect();
        assert!(warnings.is_empty(), "{warnings:#?}");
        engine
    })
    .await
    .unwrap();

    let wiki = anchor::base::SourceId::new("wiki");
    let node = |key: &NodeKey| engine.store().node_by_key(key).unwrap();
    let page = node(&keys::wiki_page(&wiki, "100")).expect("page node");
    assert_eq!(page.attrs["title"], "Payments API Runbook");
    assert_eq!(page.attrs["space"], "ENG");
    assert!(
        node(&keys::wiki_page(&wiki, "110")).is_none(),
        "restricted page absent"
    );
    assert!(node(&keys::wiki_page(&wiki, "111")).is_none());

    let thread = node(&keys::thread(&wiki, "100/503")).expect("inline thread");
    assert_eq!(thread.attrs["thread_status"], "dismissed");
    assert_eq!(thread.attrs["thread_kind"], "inline");
    assert_eq!(thread.attrs["anchor_text"], "retries failed charges");
    assert_eq!(thread.attrs["closed_by"], "Bob Lee");
    let footer = node(&keys::thread(&wiki, "100/501")).expect("footer thread");
    assert_eq!(footer.attrs["thread_status"], "open");
    assert!(
        node(&keys::comment(&wiki, "100/501/502")).is_some(),
        "reply comment"
    );

    // PAY-123 "is blocked by" PAY-7: PAY-7 blocks PAY-123.
    let (blocker, blocked) = (
        node(&keys::ticket("PAY-7")).expect("PAY-7").id,
        node(&keys::ticket("PAY-123")).expect("PAY-123").id,
    );
    let edges = engine.store().edges_of(blocker, 10_000).unwrap();
    assert!(
        edges
            .iter()
            .any(|e| e.from == blocker && e.to == blocked && e.kind == EdgeKind::Blocks),
        "blocking link survived the export"
    );
}

/// The daemon's refresh body: export, then re-scan the sources reading the export directory.
#[tokio::test]
async fn a_refresh_exports_and_rescans_the_sources_that_read_the_export() {
    let site = site(AtlassianDeployment::DataCenter).await;
    let (wiki_dir, data) = (site.options.wiki_dir(), site._dir.path().join("data"));
    let engine = tokio::task::spawn_blocking(move || {
        let mut config = AnchorConfig {
            data_dir: Some(data),
            ..AnchorConfig::default()
        };
        config.models.system1.provider = ProviderKind::Disabled;
        config
            .sources
            .push(source("wiki", SourceKind::Wiki, &wiki_dir));
        Engine::open(config).unwrap()
    })
    .await
    .unwrap();
    assert_eq!(engine.store().stats().unwrap().node_count, 0);

    let report = anchor::cli::atlassian::refresh_once(&engine, &site.options, &site.sleeper)
        .await
        .unwrap();
    assert_eq!(report.pages, 2, "{report:?}");
    let wiki = anchor::base::SourceId::new("wiki");
    assert!(
        engine
            .store()
            .node_by_key(&keys::wiki_page(&wiki, "100"))
            .unwrap()
            .is_some(),
        "the exported page was ingested by the refresh itself"
    );

    // A second refresh finds nothing new and is still fine.
    let again = anchor::cli::atlassian::refresh_once(&engine, &site.options, &site.sleeper)
        .await
        .unwrap();
    assert_eq!(again.pages + again.issues + again.removed, 0, "{again:?}");
}

#[tokio::test]
async fn a_refresh_against_a_server_that_rejects_the_credentials_fails_and_ingests_nothing() {
    let mut site = site(AtlassianDeployment::DataCenter).await;
    site.options.credentials = Credentials::Bearer {
        token: "wrong".into(),
    };
    let data = site._dir.path().join("data");
    let engine = tokio::task::spawn_blocking(move || {
        let mut config = AnchorConfig {
            data_dir: Some(data),
            ..AnchorConfig::default()
        };
        config.models.system1.provider = ProviderKind::Disabled;
        Engine::open(config).unwrap()
    })
    .await
    .unwrap();
    let result = anchor::cli::atlassian::refresh_once(&engine, &site.options, &site.sleeper).await;
    assert!(result.is_err());
    assert_eq!(engine.store().stats().unwrap().node_count, 0);
}
