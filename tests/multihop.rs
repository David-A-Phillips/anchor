//! Two-hop retrieval over plain prose: a hermetic stand-in for the multi-hop benchmarks in
//! `docs/benchmarks.md`, so a ranking change that loses the second hop fails here first.
//!
//! The corpus is a small encyclopaedia of films, directors and towns. Each question ("Where was
//! the director of The Amber Lantern born?") needs two pages: the film's, which shares the
//! question's words, and its director's, which shares only words every director's page has
//! ("director", "born"). The director's name appears in the film's text and nowhere in the
//! question, so only a link between the two pages brings the second one into the top five.
#![allow(clippy::unwrap_used)]

use std::path::Path;

use anchor::base::SourceKind;
use anchor::config::{AnchorConfig, SourceConfig};
use anchor::engine::Engine;
use anchor::engine::query::{QueryDeps, QueryEngine, QueryMode, QueryRequest};
use anchor::engine::router::ProviderKind;

const FILMS: usize = 60;
const TOWNS: usize = 25;

/// Deterministic names: no two share a word.
struct Names {
    state: u64,
    used: std::collections::HashSet<String>,
}

impl Names {
    fn new(seed: u64) -> Self {
        Self {
            state: seed,
            used: std::collections::HashSet::new(),
        }
    }

    fn next(&mut self) -> u64 {
        self.state = self
            .state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.state >> 33
    }

    fn word(&mut self) -> String {
        const ONSETS: [&str; 12] = ["b", "d", "k", "l", "m", "n", "r", "s", "t", "v", "z", "th"];
        const VOWELS: [&str; 5] = ["a", "e", "i", "o", "u"];
        loop {
            let mut word = String::new();
            for _ in 0..3 {
                word.push_str(ONSETS[self.next() as usize % ONSETS.len()]);
                word.push_str(VOWELS[self.next() as usize % VOWELS.len()]);
            }
            word.push_str(ONSETS[self.next() as usize % ONSETS.len()]);
            let mut chars = word.chars();
            let word: String = chars.next().unwrap().to_uppercase().chain(chars).collect();
            if self.used.insert(word.clone()) {
                return word;
            }
        }
    }

    fn name(&mut self, words: usize) -> String {
        (0..words)
            .map(|_| self.word())
            .collect::<Vec<_>>()
            .join(" ")
    }
}

struct Question {
    text: String,
    gold: [String; 2],
}

fn corpus(dir: &Path) -> Vec<Question> {
    let mut names = Names::new(7);
    let towns: Vec<String> = (0..TOWNS).map(|_| names.name(1)).collect();
    let page = |title: &str, body: String| {
        let file = dir
            .join("wiki")
            .join(format!("{}.md", title.replace(' ', "-")));
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(file, format!("# {title}\n\n{body}\n")).unwrap();
    };
    for (i, town) in towns.iter().enumerate() {
        page(
            town,
            format!(
                "{town} is a town of {} thousand people on the river.",
                10 + i
            ),
        );
    }
    let mut questions = Vec::new();
    for i in 0..FILMS {
        let film = format!("The {}", names.name(2));
        let director = names.name(2);
        let born = &towns[i % TOWNS];
        let shot = &towns[(i * 7 + 3) % TOWNS];
        page(
            &film,
            format!(
                "{film} is a drama film from {} directed by {director}. It was shot in {shot}.",
                1950 + i
            ),
        );
        page(
            &director,
            format!(
                "{director} is a film director, born in {born}, who made {} feature films.",
                2 + i % 9
            ),
        );
        questions.push(Question {
            text: format!("Where was the director of {film} born?"),
            gold: [film, director],
        });
    }
    questions
}

async fn recall_at_5(title_links: bool) -> f64 {
    let dir = tempfile::tempdir().unwrap();
    let questions = corpus(dir.path());
    let mut config = AnchorConfig {
        data_dir: Some(dir.path().join("data")),
        ..AnchorConfig::default()
    };
    config.models.system1.provider = ProviderKind::Disabled;
    config.graph.title_links = title_links;
    let mut wiki = SourceConfig::new(
        "encyclopaedia",
        SourceKind::Wiki,
        dir.path().join("wiki").to_string_lossy(),
    );
    wiki.trust = anchor::base::TrustLevel::SourceOfTruth;
    config.sources.push(wiki);
    let engine = Engine::open(config).unwrap();
    engine.scan_all().unwrap();
    let queries = QueryEngine::new(QueryDeps {
        store: engine.store().clone(),
        graph: engine.graph(),
        vectors: Some(engine.vectors()),
        embedder: Some(engine.embedder()),
        router: engine.router(),
        backlog: None,
        graph_config: engine.config().graph.clone(),
        query_config: engine.config().query.clone(),
    });

    let mut found = 0;
    for question in &questions {
        let mut request = QueryRequest::new(&question.text);
        request.mode = QueryMode::RetrievalOnly;
        let (_, bundle, _) = queries.retrieve(&request).await.unwrap();
        let mut top: Vec<&str> = Vec::new();
        for item in &bundle.items {
            if !top.contains(&item.node.name.as_str()) {
                top.push(&item.node.name);
            }
        }
        top.truncate(5);
        found += question
            .gold
            .iter()
            .filter(|gold| top.contains(&gold.as_str()))
            .count();
    }
    found as f64 / (2 * questions.len()) as f64
}

#[tokio::test]
async fn linked_pages_bring_the_second_hop_into_the_top_five() {
    let linked = recall_at_5(true).await;
    let unlinked = recall_at_5(false).await;
    println!("two-hop recall@5: {linked:.3} with title links, {unlinked:.3} without");
    assert!(linked >= 0.95, "with title links: {linked:.3}");
    assert!(
        unlinked <= 0.75,
        "without links the second hop should be out of reach: {unlinked:.3}"
    );
}
