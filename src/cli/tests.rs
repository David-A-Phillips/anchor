//! Argument parsing of every `anchor` subcommand: flags, defaults, and the inputs clap must refuse.

use clap::{CommandFactory, Parser};

use super::*;
use crate::mcp::install::Client;

fn parse(args: &[&str]) -> Cli {
    let mut full = vec!["anchor"];
    full.extend_from_slice(args);
    Cli::try_parse_from(full).unwrap_or_else(|err| panic!("{args:?} should parse: {err}"))
}

fn rejected(args: &[&str]) -> String {
    let mut full = vec!["anchor"];
    full.extend_from_slice(args);
    match Cli::try_parse_from(full) {
        Ok(cli) => panic!("{args:?} should be rejected, parsed as {cli:?}"),
        Err(err) => err.to_string(),
    }
}

#[test]
fn the_command_definition_is_internally_consistent() {
    Cli::command().debug_assert();
}

#[test]
fn every_subcommand_is_listed_in_the_help() {
    let command = Cli::command();
    let names: Vec<&str> = command
        .get_subcommands()
        .map(clap::Command::get_name)
        .collect();
    for expected in [
        "setup",
        "init",
        "ingest",
        "query",
        "daemon",
        "status",
        "spend",
        "backlog",
        "enrich",
        "eval",
        "why",
        "compare",
        "fetch",
        "fetch-atlassian",
        "deepen",
        "export",
        "import",
        "mcp",
        "server",
        "sync",
        "demo",
        "ollama",
    ] {
        assert!(
            names.contains(&expected),
            "{expected} missing from {names:?}"
        );
    }
}

#[test]
fn simple_commands_parse_without_arguments() {
    assert!(matches!(parse(&["status"]).command, Command::Status));
    assert!(matches!(parse(&["backlog"]).command, Command::Backlog));
    assert!(matches!(parse(&["server"]).command, Command::Server));
    assert!(matches!(parse(&["sync"]).command, Command::Sync));
    assert!(matches!(
        parse(&["daemon"]).command,
        Command::Daemon { no_web: false }
    ));
    assert!(matches!(
        parse(&["daemon", "--no-web"]).command,
        Command::Daemon { no_web: true }
    ));
}

#[test]
fn setup_and_init_flags() {
    assert!(matches!(
        parse(&["setup"]).command,
        Command::Setup {
            yes: false,
            rebuild_test: false
        }
    ));
    assert!(matches!(
        parse(&["setup", "--yes", "--rebuild-test"]).command,
        Command::Setup {
            yes: true,
            rebuild_test: true
        }
    ));
    assert!(matches!(
        parse(&["init", "--force"]).command,
        Command::Init { force: true }
    ));
}

#[test]
fn ingest_takes_an_optional_source() {
    assert!(matches!(
        parse(&["ingest"]).command,
        Command::Ingest { source: None }
    ));
    let Command::Ingest { source } = parse(&["ingest", "--source", "payments-api"]).command else {
        panic!("not ingest");
    };
    assert_eq!(source.as_deref(), Some("payments-api"));
}

#[test]
fn query_needs_a_question_and_accepts_its_flags() {
    let Command::Query {
        text,
        json,
        local_only,
        retrieval_only,
        explain,
    } = parse(&["query", "Who owns payments-api?"]).command
    else {
        panic!("not query");
    };
    assert_eq!(text, "Who owns payments-api?");
    assert!(!json && !local_only && !retrieval_only && !explain);

    let Command::Query {
        json,
        local_only,
        retrieval_only,
        explain,
        ..
    } = parse(&[
        "query",
        "q",
        "--json",
        "--local-only",
        "--retrieval-only",
        "--explain",
    ])
    .command
    else {
        panic!("not query");
    };
    assert!(json && local_only && retrieval_only && explain);

    assert!(rejected(&["query"]).contains("<TEXT>"));
}

#[test]
fn spend_and_enrich_have_numeric_options_with_defaults() {
    let Command::Spend { days } = parse(&["spend"]).command else {
        panic!("not spend");
    };
    assert_eq!(days, 30);
    let Command::Spend { days } = parse(&["spend", "--days", "7"]).command else {
        panic!("not spend");
    };
    assert_eq!(days, 7);
    let Command::Enrich { limit } = parse(&["enrich"]).command else {
        panic!("not enrich");
    };
    assert_eq!(limit, 200);
    assert!(!rejected(&["spend", "--days", "-1"]).is_empty());
    assert!(!rejected(&["spend", "--days", "many"]).is_empty());
    assert!(!rejected(&["enrich", "--limit", "x"]).is_empty());
}

#[test]
fn eval_requires_the_truth_file() {
    assert!(rejected(&["eval"]).contains("--truth"));
    let Command::Eval {
        truth,
        local_only,
        retrieval_only,
        json,
    } = parse(&["eval", "--truth", "gt.json", "--json"]).command
    else {
        panic!("not eval");
    };
    assert_eq!(truth, PathBuf::from("gt.json"));
    assert!(json && !local_only && !retrieval_only);
}

#[test]
fn why_takes_a_target() {
    let Command::Why { target, json } = parse(&["why", "src/lib.rs:10-20", "--json"]).command
    else {
        panic!("not why");
    };
    assert_eq!(target, "src/lib.rs:10-20");
    assert!(json);
    assert!(rejected(&["why"]).contains("<TARGET>"));
}

#[test]
fn compare_flags_and_the_reindex_dependency() {
    let Command::Compare {
        dir,
        truth,
        kinds,
        reindex,
        json,
    } = parse(&["compare", "work"]).command
    else {
        panic!("not compare");
    };
    assert_eq!(dir, PathBuf::from("work"));
    assert!(truth.is_none() && !kinds && !reindex && !json);

    let Command::Compare {
        truth,
        kinds,
        reindex,
        ..
    } = parse(&[
        "compare",
        "work",
        "--truth",
        "t.json",
        "--kinds",
        "--reindex",
    ])
    .command
    else {
        panic!("not compare");
    };
    assert_eq!(truth, Some(PathBuf::from("t.json")));
    assert!(kinds && reindex);

    assert!(
        rejected(&["compare", "work", "--reindex"]).contains("--truth"),
        "--reindex without --truth is refused"
    );
    assert!(rejected(&["compare"]).contains("<DIR>"));
}

#[test]
fn fetch_defaults_match_the_documented_limits() {
    let Command::Fetch {
        repository,
        out,
        issues,
        discussions,
        pulls,
    } = parse(&["fetch"]).command
    else {
        panic!("not fetch");
    };
    assert!(repository.is_none() && out.is_none());
    assert_eq!((issues, discussions, pulls), (100, 50, 50));

    let Command::Fetch {
        repository,
        out,
        issues,
        ..
    } = parse(&["fetch", "acme/ledger", "--out", "x", "--issues", "3"]).command
    else {
        panic!("not fetch");
    };
    assert_eq!(repository.as_deref(), Some("acme/ledger"));
    assert_eq!(out, Some(PathBuf::from("x")));
    assert_eq!(issues, 3);
}

#[test]
fn fetch_atlassian_flags() {
    let Command::FetchAtlassian(args) = parse(&[
        "fetch-atlassian",
        "https://acme.atlassian.net",
        "--space",
        "ENG",
        "--space",
        "OPS",
        "--project",
        "PAY",
    ])
    .command
    else {
        panic!("not fetch-atlassian");
    };
    assert_eq!(args.base_url.as_deref(), Some("https://acme.atlassian.net"));
    assert_eq!(args.spaces, ["ENG", "OPS"]);
    assert_eq!(args.projects, ["PAY"]);
    assert_eq!(args.token_env, "ATLASSIAN_TOKEN");
    assert!(matches!(
        parse(&["fetch-atlassian"]).command,
        Command::FetchAtlassian(_)
    ));
}

#[test]
fn deepen_parses_the_depth_names() {
    use crate::config::Depth;
    let Command::Deepen { source, depth } = parse(&["deepen", "repo"]).command else {
        panic!("not deepen");
    };
    assert_eq!((source.as_str(), depth), ("repo", Depth::Full));
    for (text, expected) in [("phone_book", Depth::PhoneBook), ("map", Depth::Map)] {
        let Command::Deepen { depth, .. } = parse(&["deepen", "repo", "--depth", text]).command
        else {
            panic!("not deepen");
        };
        assert_eq!(depth, expected);
    }
    assert!(!rejected(&["deepen", "repo", "--depth", "deeper"]).is_empty());
    assert!(rejected(&["deepen"]).contains("<SOURCE>"));
}

#[test]
fn export_and_import_paths() {
    assert!(matches!(
        parse(&["export"]).command,
        Command::Export { output: None }
    ));
    let Command::Export { output } = parse(&["export", "-o", "out.contextpack"]).command else {
        panic!("not export");
    };
    assert_eq!(output, Some(PathBuf::from("out.contextpack")));
    let Command::Import { file } = parse(&["import", "in.contextpack"]).command else {
        panic!("not import");
    };
    assert_eq!(file, PathBuf::from("in.contextpack"));
    assert!(rejected(&["import"]).contains("<FILE>"));
}

#[test]
fn mcp_serves_by_default_and_has_install_and_uninstall() {
    assert!(matches!(
        parse(&["mcp"]).command,
        Command::Mcp { action: None }
    ));
    let Command::Mcp {
        action: Some(McpAction::Install(args)),
    } = parse(&[
        "mcp", "install", "--client", "cursor", "--print", "--scope", "user",
    ])
    .command
    else {
        panic!("not mcp install");
    };
    assert_eq!(args.client, Client::Cursor);
    assert!(args.print);
    assert_eq!(args.scope.as_deref(), Some("user"));
    assert_eq!(args.name, "anchor");

    let Command::Mcp {
        action: Some(McpAction::Uninstall(args)),
    } = parse(&["mcp", "uninstall", "--name", "x"]).command
    else {
        panic!("not mcp uninstall");
    };
    assert_eq!(args.client, Client::ClaudeCode);
    assert_eq!(args.name, "x");
    assert!(!rejected(&["mcp", "install", "--client", "emacs"]).is_empty());
}

#[test]
fn demo_scale_and_seed() {
    let Command::Demo { out, scale, seed } = parse(&["demo", "--out", "d"]).command else {
        panic!("not demo");
    };
    assert_eq!(out, PathBuf::from("d"));
    assert!(matches!(scale, DemoScale::Small));
    assert_eq!(seed, 1);
    let Command::Demo { scale, seed, .. } =
        parse(&["demo", "--out", "d", "--scale", "tiny", "--seed", "42"]).command
    else {
        panic!("not demo");
    };
    assert!(matches!(scale, DemoScale::Tiny));
    assert_eq!(seed, 42);
    assert!(rejected(&["demo"]).contains("--out"));
    assert!(!rejected(&["demo", "--out", "d", "--scale", "huge"]).is_empty());
}

#[test]
fn ollama_actions() {
    assert!(matches!(
        parse(&["ollama", "status"]).command,
        Command::Ollama {
            action: OllamaAction::Status
        }
    ));
    let Command::Ollama {
        action: OllamaAction::Provision { model },
    } = parse(&["ollama", "provision", "--model", "tiny:latest"]).command
    else {
        panic!("not provision");
    };
    assert_eq!(model.as_deref(), Some("tiny:latest"));
    for name in ["teardown", "rebuild"] {
        let cli = parse(&["ollama", name, "--keep-binary", "--keep-downloads"]);
        match cli.command {
            Command::Ollama {
                action:
                    OllamaAction::Teardown {
                        keep_binary,
                        keep_downloads,
                    }
                    | OllamaAction::Rebuild {
                        keep_binary,
                        keep_downloads,
                    },
            } => assert!(keep_binary && keep_downloads, "{name}"),
            other => panic!("{name} parsed as {other:?}"),
        }
    }
    assert!(!rejected(&["ollama"]).is_empty());
}

#[test]
fn the_config_flag_is_global() {
    for args in [
        &["--config", "a.toml", "status"][..],
        &["status", "--config", "a.toml"][..],
    ] {
        assert_eq!(
            parse(args).config,
            Some(PathBuf::from("a.toml")),
            "{args:?}"
        );
    }
}

#[test]
fn unknown_commands_and_flags_are_refused() {
    assert!(rejected(&["frobnicate"]).contains("frobnicate"));
    assert!(rejected(&["status", "--bogus"]).contains("--bogus"));
    assert!(!rejected(&[]).is_empty(), "a subcommand is required");
}
