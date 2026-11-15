use std::time::Instant;

use proptest::prelude::*;
use tree_sitter::Point;

use crate::base::{
    ChunkKind, EdgeKind, NodeKind, NodeRef, SourceId, SymbolKind, UnitKind, attrs, keys,
};

use super::fixtures::*;
use super::incremental::compute_edit;
use super::*;

fn parse(language: Language, text: &str) -> ParsedFile {
    parse_source(language, text).expect("grammar loads")
}

fn sym<'a>(parsed: &'a ParsedFile, qualified: &str) -> &'a Symbol {
    parsed
        .symbol(qualified)
        .unwrap_or_else(|| panic!("no symbol {qualified}; have {:?}", names(parsed)))
}

fn names(parsed: &ParsedFile) -> Vec<&str> {
    parsed
        .symbols
        .iter()
        .map(|s| s.qualified_name.as_str())
        .collect()
}

fn has_ref(parsed: &ParsedFile, name: &str, qualifier: Option<&str>, kind: ReferenceKind) -> bool {
    parsed
        .references
        .iter()
        .any(|r| r.name == name && r.qualifier.as_deref() == qualifier && r.kind == kind)
}

fn find_ref<'a>(parsed: &'a ParsedFile, name: &str) -> &'a Reference {
    parsed
        .references
        .iter()
        .find(|r| r.name == name)
        .unwrap_or_else(|| panic!("no reference {name}"))
}

// ---- extraction per language ---------------------------------------------------------------

#[test]
fn rust_symbols_references_imports() {
    let p = parse(Language::Rust, RUST);
    assert!(p.syntax_errors.is_empty());
    assert_eq!(p.module_doc.as_deref(), Some("Ledger crate.\nSecond line."));

    let ledger = sym(&p, "Ledger");
    assert_eq!(ledger.kind, SymbolKind::Struct);
    assert_eq!((ledger.line_start, ledger.line_end), (9, 11));
    assert_eq!(ledger.signature, "pub struct Ledger<T>");
    assert_eq!(ledger.doc.as_deref(), Some("A ledger of charges."));
    assert_eq!(ledger.visibility, Visibility::Public);

    let imp = sym(&p, "impl Ledger<T>");
    assert_eq!(imp.kind, SymbolKind::Impl);
    let new = sym(&p, "Ledger::new");
    assert_eq!(new.kind, SymbolKind::Method);
    assert_eq!(
        p.symbols[new.parent.unwrap()].qualified_name,
        "impl Ledger<T>"
    );
    assert_eq!(new.signature, "pub fn new() -> Self");
    assert_eq!(new.doc.as_deref(), Some("Creates one."));
    assert_eq!(sym(&p, "Ledger::charge").visibility, Visibility::Restricted);
    assert_eq!(sym(&p, "Ledger::helper").visibility, Visibility::Private);
    assert_eq!(sym(&p, "Ledger::fmt").kind, SymbolKind::Method);
    assert_eq!(sym(&p, "Audit").kind, SymbolKind::Trait);
    assert_eq!(sym(&p, "inner::f").kind, SymbolKind::Function);

    let charge = sym(&p, "Ledger::charge");
    assert_eq!(
        charge.content_hash,
        crate::base::ContentHash::of(&RUST.as_bytes()[charge.byte_range.clone()])
    );
    assert!(has_ref(
        &p,
        "record",
        Some("self.audit"),
        ReferenceKind::Call
    ));
    assert!(has_ref(&p, "println", None, ReferenceKind::Macro));
    assert!(has_ref(&p, "g", Some("crate::util"), ReferenceKind::Call));
    assert!(find_ref(&p, "g").qualifier_is_path);
    assert!(has_ref(&p, "Money", None, ReferenceKind::Type));
    // Generic parameters are not references.
    assert!(!p.references.iter().any(|r| r.name == "T"));

    assert_eq!(p.imports.len(), 2);
    assert_eq!(p.imports[0].names, ["HashMap", "HashSet"]);
    assert!(!p.imports[0].relative);
    assert_eq!(p.imports[1].names, ["Invoice"]);
    assert!(p.imports[1].relative);
}

#[test]
fn python_symbols_references_imports() {
    let p = parse(Language::Python, PYTHON);
    assert!(p.syntax_errors.is_empty());
    assert_eq!(
        p.module_doc.as_deref(),
        Some("Billing helpers.\n\nMore text.")
    );
    assert_eq!(sym(&p, "MAX_RETRIES").kind, SymbolKind::Constant);
    let account = sym(&p, "Account");
    assert_eq!(account.kind, SymbolKind::Class);
    assert_eq!(account.signature, "class Account(Base, mixins.Audit):");
    assert_eq!(account.doc.as_deref(), Some("An account."));
    // The decorator belongs to the definition.
    assert_eq!((account.line_start, account.line_end), (12, 28));
    let charge = sym(&p, "Account::charge");
    assert_eq!(charge.kind, SymbolKind::Method);
    assert_eq!(charge.doc.as_deref(), Some("Charge it.\n\nDetails."));
    assert_eq!(
        charge.signature,
        "async def charge(self, amount: int) -> Optional[Receipt]:"
    );
    assert_eq!(
        sym(&p, "Account::_private").visibility,
        Visibility::Restricted
    );
    assert_eq!(sym(&p, "Account::__init__").visibility, Visibility::Public);
    assert_eq!(sym(&p, "helper::inner").kind, SymbolKind::Function);

    assert!(has_ref(
        &p,
        "record",
        Some("self.ledger"),
        ReferenceKind::Call
    ));
    assert!(has_ref(&p, "helper", None, ReferenceKind::Call));
    assert!(has_ref(&p, "Base", None, ReferenceKind::Type));
    assert!(has_ref(&p, "Audit", Some("mixins"), ReferenceKind::Type));
    assert!(has_ref(&p, "Receipt", None, ReferenceKind::Type));
    let paths: Vec<_> = p
        .imports
        .iter()
        .map(|i| (i.path.as_str(), i.relative))
        .collect();
    assert_eq!(
        paths,
        [
            ("os.path", false),
            (".", true),
            (".ledger", true),
            ("..pkg.mod", true)
        ]
    );
    assert_eq!(p.imports[2].names, ["Ledger", "charge"]);
}

#[test]
fn typescript_symbols_references_imports() {
    let p = parse(Language::TypeScript, TS);
    assert!(p.syntax_errors.is_empty());
    assert_eq!(p.module_doc.as_deref(), Some("Module comment."));
    let service = sym(&p, "Service");
    assert_eq!(service.kind, SymbolKind::Class);
    assert_eq!(service.visibility, Visibility::Public);
    assert_eq!(service.doc.as_deref(), Some("A service."));
    assert_eq!(
        service.signature,
        "export class Service<T> extends Base implements Runnable"
    );
    let run = sym(&p, "Service::run");
    assert_eq!(run.kind, SymbolKind::Method);
    assert_eq!(run.signature, "async run(input: T): Promise<Money>");
    assert_eq!((run.line_start, run.line_end), (18, 22));
    assert_eq!(sym(&p, "Service::tick").visibility, Visibility::Restricted);
    assert_eq!(sym(&p, "Runnable").kind, SymbolKind::Interface);
    assert_eq!(sym(&p, "Runnable::run").kind, SymbolKind::Method);
    assert_eq!(sym(&p, "Id").kind, SymbolKind::TypeAlias);
    assert_eq!(sym(&p, "Mode").kind, SymbolKind::Enum);
    assert_eq!(sym(&p, "helper").kind, SymbolKind::Function);
    assert_eq!(sym(&p, "helper").visibility, Visibility::Public);
    assert_eq!(sym(&p, "LIMIT").kind, SymbolKind::Constant);
    assert_eq!(sym(&p, "LIMIT").visibility, Visibility::Private);
    assert_eq!(sym(&p, "local").visibility, Visibility::Private);
    assert_eq!(sym(&p, "main").visibility, Visibility::Public);
    assert_eq!(sym(&p, "Util::f").kind, SymbolKind::Function);

    assert!(has_ref(
        &p,
        "charge",
        Some("this.ledger"),
        ReferenceKind::Call
    ));
    assert!(has_ref(&p, "helper", None, ReferenceKind::Call));
    assert!(has_ref(&p, "Ledger", None, ReferenceKind::Type));
    assert!(has_ref(&p, "Base", None, ReferenceKind::Type));
    assert!(!p.references.iter().any(|r| r.name == "T"));
    let paths: Vec<_> = p
        .imports
        .iter()
        .map(|i| (i.path.as_str(), i.relative))
        .collect();
    assert_eq!(
        paths,
        [
            ("./ledger", true),
            ("axios", false),
            ("node:fs", false),
            ("lodash/fp", false)
        ]
    );
    assert_eq!(p.imports[0].names, ["Ledger", "Money"]);
}

#[test]
fn javascript_and_tsx() {
    let p = parse(Language::JavaScript, JS);
    assert_eq!(sym(&p, "Counter").kind, SymbolKind::Class);
    assert_eq!(sym(&p, "Counter::inc").kind, SymbolKind::Method);
    assert_eq!(sym(&p, "make").kind, SymbolKind::Function);
    assert!(has_ref(&p, "bump", Some("util"), ReferenceKind::Call));
    assert_eq!(p.imports[0].path, "../lib/a.js");

    let p = parse(Language::Tsx, TSX);
    assert!(p.syntax_errors.is_empty());
    assert_eq!(sym(&p, "App").kind, SymbolKind::Function);
    assert!(has_ref(&p, "Button", None, ReferenceKind::Type));
}

#[test]
fn go_symbols_references_imports() {
    let p = parse(Language::Go, GO);
    assert!(p.syntax_errors.is_empty());
    assert_eq!(
        p.module_doc.as_deref(),
        Some("Package ledger does ledger things.")
    );
    assert_eq!(sym(&p, "MaxEntries").kind, SymbolKind::Constant);
    assert_eq!(sym(&p, "count").kind, SymbolKind::Variable);
    let ledger = sym(&p, "Ledger");
    assert_eq!(
        (ledger.kind, ledger.signature.as_str()),
        (SymbolKind::Struct, "type Ledger struct")
    );
    assert_eq!(ledger.doc.as_deref(), Some("Ledger holds entries."));
    assert_eq!((ledger.line_start, ledger.line_end), (16, 18));
    assert_eq!(sym(&p, "Store").kind, SymbolKind::Interface);
    assert_eq!(sym(&p, "Store::Save").kind, SymbolKind::Method);
    assert_eq!(sym(&p, "ID").kind, SymbolKind::TypeAlias);
    let charge = sym(&p, "Ledger::Charge");
    assert_eq!(charge.kind, SymbolKind::Method);
    assert_eq!(
        charge.signature,
        "func (l *Ledger) Charge(a money.Amount) error"
    );
    assert_eq!(charge.parent, Some(2));
    assert_eq!(charge.doc.as_deref(), Some("Charge charges."));
    assert_eq!(sym(&p, "helper").visibility, Visibility::Private);
    assert_eq!(sym(&p, "New").visibility, Visibility::Public);
    assert!(has_ref(&p, "Println", Some("fmt"), ReferenceKind::Call));
    assert!(has_ref(&p, "Amount", Some("money"), ReferenceKind::Type));
    assert!(!p.references.iter().any(|r| r.name == "error"));
    let paths: Vec<_> = p.imports.iter().map(|i| i.path.as_str()).collect();
    assert_eq!(paths, ["fmt", "strings", "github.com/acme/money"]);
}

#[test]
fn java_symbols_references_imports() {
    let p = parse(Language::Java, JAVA);
    assert!(p.syntax_errors.is_empty());
    let ledger = sym(&p, "Ledger");
    assert_eq!(ledger.kind, SymbolKind::Class);
    assert_eq!(ledger.doc.as_deref(), Some("A ledger."));
    assert_eq!(
        ledger.signature,
        "public class Ledger<T> extends Base implements Auditable"
    );
    assert_eq!(sym(&p, "Ledger::MAX").kind, SymbolKind::Constant);
    let charge = sym(&p, "Ledger::charge");
    assert_eq!(
        (charge.kind, charge.visibility),
        (SymbolKind::Method, Visibility::Public)
    );
    assert_eq!(charge.doc.as_deref(), Some("Charges."));
    assert_eq!(
        sym(&p, "Ledger::packagePrivate").visibility,
        Visibility::Restricted
    );
    assert_eq!(sym(&p, "Ledger::Mode").kind, SymbolKind::Enum);
    assert_eq!(sym(&p, "Auditable::audit").visibility, Visibility::Public);
    assert!(has_ref(&p, "audit", Some("this"), ReferenceKind::Call));
    assert!(has_ref(&p, "log", Some("Util"), ReferenceKind::Call));
    assert!(has_ref(&p, "Money", None, ReferenceKind::Type));
    assert!(!p.references.iter().any(|r| r.name == "T"));
    assert_eq!(p.imports[0].path, "java.util.List");
    assert_eq!(p.imports[0].names, ["List"]);
    assert_eq!(p.imports[3].path, "java.io.*");
    assert!(p.imports[3].names.is_empty());
}

#[test]
fn c_symbols_references_imports() {
    let p = parse(Language::C, C);
    assert!(p.syntax_errors.is_empty());
    assert_eq!(p.module_doc.as_deref(), Some("Ledger implementation."));
    assert_eq!(sym(&p, "MAX_ENTRIES").kind, SymbolKind::Macro);
    assert_eq!(sym(&p, "SQUARE").kind, SymbolKind::Macro);
    // `typedef struct Entry {..} Entry;` is one symbol, an anonymous typedef'd struct is named
    // after the typedef.
    assert_eq!(p.symbols.iter().filter(|s| s.name == "Entry").count(), 1);
    assert_eq!(sym(&p, "Pair").kind, SymbolKind::Struct);
    assert_eq!(sym(&p, "callback_t").kind, SymbolKind::TypeAlias);
    assert_eq!(sym(&p, "Mode").kind, SymbolKind::Enum);
    assert_eq!(sym(&p, "counter").visibility, Visibility::Private);
    assert_eq!(sym(&p, "total").kind, SymbolKind::Variable);
    let charge = sym(&p, "ledger_charge");
    assert_eq!(
        charge.signature,
        "int ledger_charge(struct Ledger *l, int amount)"
    );
    assert_eq!(charge.doc.as_deref(), Some("Charges the ledger."));
    assert_eq!((charge.line_start, charge.line_end), (27, 31));
    assert_eq!(sym(&p, "hidden").visibility, Visibility::Private);
    assert!(has_ref(&p, "helper", None, ReferenceKind::Call));
    assert!(has_ref(&p, "audit", Some("l"), ReferenceKind::Call));
    assert!(p.imports[0].path == "stdio.h" && !p.imports[0].relative);
    assert!(p.imports[1].path == "ledger.h" && p.imports[1].relative);
}

#[test]
fn repeated_names_get_numbered_suffixes() {
    let p = parse(
        Language::Python,
        "def f():\n    pass\n\ndef f():\n    pass\n\ndef f():\n    pass\n",
    );
    assert_eq!(names(&p), ["f", "f~2", "f~3"]);
    let p = parse(
        Language::Rust,
        "struct A; impl A { fn x(&self) {} } impl A { fn x(&self) {} }",
    );
    assert_eq!(names(&p), ["A", "impl A", "A::x", "impl A~2", "A::x~2"]);
}

#[test]
fn language_detection() {
    assert_eq!(Language::from_path("src/a.rs"), Some(Language::Rust));
    assert_eq!(Language::from_path("a/b.PYI"), Some(Language::Python));
    assert_eq!(Language::from_path("x.tsx"), Some(Language::Tsx));
    assert_eq!(Language::from_path("x.h"), Some(Language::C));
    assert_eq!(Language::from_path("README.md"), None);
    assert_eq!(Language::from_name("go"), Some(Language::Go));
}

// ---- error recovery ------------------------------------------------------------------------

#[test]
fn broken_code_is_recovered_and_reported() {
    let text = "fn good() { helper(); }\n\nfn broken( {\n\nstruct Later;\n";
    let p = parse(Language::Rust, text);
    assert!(!p.syntax_errors.is_empty());
    assert!(p.syntax_errors.len() <= ParsedFile::MAX_SYNTAX_ERRORS);
    assert!(p.syntax_errors.iter().all(|e| e.line >= 1 && e.column >= 1));
    assert_eq!(sym(&p, "good").kind, SymbolKind::Function);
    assert!(has_ref(&p, "helper", None, ReferenceKind::Call));
}

#[test]
fn garbage_and_pathological_input_never_panic() {
    let deep = format!("{}1{}", "(".repeat(5000), ")".repeat(5000));
    for (language, _) in ALL {
        for text in [
            "",
            "\u{0}\u{1}",
            "}}}{{{",
            "日本語",
            &deep,
            "\"unterminated",
        ] {
            let p = parse(language, text);
            assert_eq!(p.language, language);
        }
    }
    let many_errors = "fn ( ) { let = ; }\n".repeat(500);
    let p = parse(Language::Rust, &many_errors);
    assert!(!p.syntax_errors.is_empty());
    assert!(p.syntax_errors.len() <= ParsedFile::MAX_SYNTAX_ERRORS);
}

// ---- edit computation ----------------------------------------------------------------------

fn point_at(text: &str, byte: usize) -> Point {
    let head = &text[..byte];
    Point::new(
        head.matches('\n').count(),
        byte - head.rfind('\n').map_or(0, |i| i + 1),
    )
}

fn check_edit(old: &str, new: &str) {
    let Some(edit) = compute_edit(old, new) else {
        assert_eq!(old, new);
        return;
    };
    assert_ne!(old, new);
    assert!(old.is_char_boundary(edit.start_byte) && old.is_char_boundary(edit.old_end_byte));
    assert!(new.is_char_boundary(edit.new_end_byte));
    assert_eq!(&old[..edit.start_byte], &new[..edit.start_byte]);
    assert_eq!(&old[edit.old_end_byte..], &new[edit.new_end_byte..]);
    assert_eq!(edit.start_position, point_at(old, edit.start_byte));
    assert_eq!(edit.old_end_position, point_at(old, edit.old_end_byte));
    assert_eq!(edit.new_end_position, point_at(new, edit.new_end_byte));
}

#[test]
fn edits_are_minimal_and_correct() {
    let cases = [
        ("", "fn a() {}"),
        ("fn a() {}", ""),
        ("", ""),
        ("same", "same"),
        ("fn a() {}", "// c\nfn a() {}"),
        ("fn a() {}", "fn a() {}\nfn b() {}"),
        ("fn a() {}\nfn b() {}", "fn a() {}"),
        ("a\r\nb\r\nc", "a\r\nbb\r\nc"),
        ("a\r\nb", "a\nb"),
        ("a\nb", "a\r\nb"),
        ("let s = \"héllo\";", "let s = \"hello\";"),
        ("x = 'é'", "x = 'è'"),
        ("日本語", "日本人語"),
        ("aaaa", "aaaaa"),
        ("abcabc", "abc"),
        ("line1\nline2\n", "line1\n\nline2\n"),
    ];
    for (old, new) in cases {
        check_edit(old, new);
        check_edit(new, old);
    }
    let edit = compute_edit("fn a() {}\nfn b() {}", "fn a() {}\nfn bb() {}").unwrap();
    assert_eq!(
        (edit.start_byte, edit.old_end_byte, edit.new_end_byte),
        (14, 14, 15)
    );
    assert_eq!(edit.start_position, Point::new(1, 4));
    // An insertion in the middle of a multi-byte character run lands on character boundaries.
    let edit = compute_edit("é", "è").unwrap();
    assert_eq!(
        (edit.start_byte, edit.old_end_byte, edit.new_end_byte),
        (0, 2, 2)
    );
}

// ---- incremental parser --------------------------------------------------------------------

fn parser() -> IncrementalParser {
    IncrementalParser::new(&ParserConfig::default())
}

#[test]
fn first_parse_is_not_incremental_and_everything_is_added() {
    let parser = parser();
    let out = parser
        .parse("a.rs", Language::Rust, "fn a() {}\nfn b() {}\n")
        .unwrap();
    assert!(!out.reused_tree && !out.skipped_too_large);
    assert_eq!(out.delta.added, ["a", "b"]);
    assert!(out.delta.removed.is_empty() && out.delta.modified.is_empty());
    assert_eq!(out.changed_ranges.len(), 1);
    assert_eq!(out.changed_ranges[0], 0..20);
    assert_eq!(parser.cached_files(), 1);
}

#[test]
fn reparse_reuses_tree_and_reports_delta() {
    let parser = parser();
    let v1 = "fn a() { 1 }\nfn b() { 2 }\nfn c() { 3 }\n";
    parser.parse("f.rs", Language::Rust, v1).unwrap();
    let v2 = "fn a() { 1 }\nfn b() { 22 }\nfn d() { 4 }\n";
    let out = parser.parse("f.rs", Language::Rust, v2).unwrap();
    assert!(out.reused_tree);
    assert_eq!(out.delta.modified, ["b"]);
    assert_eq!(out.delta.added, ["d"]);
    assert_eq!(out.delta.removed, ["c"]);
    assert!(!out.changed_ranges.is_empty());
    assert_eq!(out.parsed, parse(Language::Rust, v2));

    let same = parser.parse("f.rs", Language::Rust, v2).unwrap();
    assert!(same.reused_tree && same.delta.is_empty() && same.changed_ranges.is_empty());
    assert_eq!(same.parsed, out.parsed);
}

#[test]
fn forget_language_change_and_eviction_drop_the_cache() {
    let parser = IncrementalParser::new(&ParserConfig {
        tree_cache_entries: 2,
        ..ParserConfig::default()
    });
    parser.parse("a", Language::Rust, "fn a() {}").unwrap();
    assert!(
        parser
            .parse("a", Language::Rust, "fn a() {} ")
            .unwrap()
            .reused_tree
    );
    parser.forget("a");
    assert!(
        !parser
            .parse("a", Language::Rust, "fn a() {}")
            .unwrap()
            .reused_tree
    );
    // A different language never re-uses the tree.
    let out = parser
        .parse("a", Language::Python, "def a(): pass")
        .unwrap();
    assert!(!out.reused_tree);
    assert_eq!(out.delta.added, ["a"]);
    parser.parse("b", Language::Rust, "fn b() {}").unwrap();
    parser.parse("c", Language::Rust, "fn c() {}").unwrap();
    assert_eq!(parser.cached_files(), 2);
    assert!(
        !parser
            .parse("a", Language::Python, "def a(): pass\n")
            .unwrap()
            .reused_tree
    );
}

#[test]
fn oversized_files_are_skipped() {
    let parser = IncrementalParser::new(&ParserConfig {
        max_file_bytes: 16,
        ..ParserConfig::default()
    });
    parser.parse("a", Language::Rust, "fn a() {}").unwrap();
    let out = parser
        .parse("a", Language::Rust, "fn a() {}\nfn b() {}\nfn c() {}")
        .unwrap();
    assert!(out.skipped_too_large && out.parsed.symbols.is_empty());
    assert_eq!(parser.cached_files(), 0);
}

#[test]
fn parser_is_usable_from_many_threads() {
    let parser = parser();
    std::thread::scope(|scope| {
        for worker in 0..4 {
            let parser = &parser;
            scope.spawn(move || {
                for round in 0..20 {
                    let key = format!("k{}", round % 3);
                    let text = format!("fn f{worker}() {{ {round} }}\n");
                    let out = parser.parse(&key, Language::Rust, &text).unwrap();
                    assert_eq!(out.parsed, parse(Language::Rust, &text));
                }
            });
        }
    });
}

// ---- incremental == from scratch (property) ------------------------------------------------

fn apply_edit(text: &str, at: usize, delete: usize, insert: &str) -> String {
    let mut start = at % (text.len() + 1);
    while !text.is_char_boundary(start) {
        start -= 1;
    }
    let mut end = (start + delete).min(text.len());
    while !text.is_char_boundary(end) {
        end += 1;
    }
    format!("{}{insert}{}", &text[..start], &text[end..])
}

const SNIPPETS: [&str; 14] = [
    "",
    "x",
    " ",
    "\n",
    "\r\n",
    "é",
    "日本",
    "// c\n",
    "{",
    "}",
    "(",
    ")",
    "\"",
    "fn q() {}\n",
];

proptest! {
    #![proptest_config(ProptestConfig::with_cases(24))]

    #[test]
    fn incremental_equals_full_parse(
        language_index in 0..8usize,
        edits in proptest::collection::vec((0..4000usize, 0..24usize, 0..SNIPPETS.len()), 1..7),
    ) {
        let (language, fixture) = ALL[language_index];
        let parser = parser();
        let mut text = fixture.to_owned();
        parser.parse("f", language, &text).unwrap();
        let mut last = None;
        for (at, delete, snippet) in edits {
            text = apply_edit(&text, at, delete, SNIPPETS[snippet]);
            last = Some(parser.parse("f", language, &text).unwrap());
        }
        let incremental = last.unwrap().parsed;
        let full = parse(language, &text);
        prop_assert_eq!(&incremental.symbols, &full.symbols);
        prop_assert_eq!(&incremental.references, &full.references);
        prop_assert_eq!(&incremental.imports, &full.imports);
        prop_assert_eq!(&incremental.module_doc, &full.module_doc);
    }

    #[test]
    fn edit_computation_is_consistent(
        old in "[a-z\\n\\r é日]{0,40}",
        new in "[a-z\\n\\r é日]{0,40}",
    ) {
        check_edit(&old, &new);
    }
}

#[test]
fn line_by_line_editing_session_stays_equal_to_full_parse() {
    for (language, fixture) in ALL {
        let parser = parser();
        let lines: Vec<&str> = fixture.lines().collect();
        parser.parse("f", language, fixture).unwrap();
        // Delete each line in turn, then restore it: every intermediate state is compared.
        for skip in 0..lines.len() {
            let without: String = lines
                .iter()
                .enumerate()
                .filter(|(i, _)| *i != skip)
                .map(|(_, l)| format!("{l}\n"))
                .collect();
            let incremental = parser.parse("f", language, &without).unwrap().parsed;
            assert_eq!(
                incremental,
                parse(language, &without),
                "{language} without line {skip}"
            );
            let restored = parser.parse("f", language, fixture).unwrap().parsed;
            assert_eq!(
                restored,
                parse(language, fixture),
                "{language} restored line {skip}"
            );
        }
    }
}

// ---- performance ---------------------------------------------------------------------------

fn big_rust_file() -> String {
    let mut text = String::new();
    for i in 0..130 {
        text.push_str(&format!(
            "/// Function number {i}.\npub fn function_{i}(a: u32, b: &str) -> Result<u32, Error> {{\n    let total = helper_{i}(a) + 1;\n    for item in 0..a {{\n        if item % 2 == 0 {{\n            record(item, b);\n        }}\n    }}\n    // trailing comment\n    Ok(total)\n}}\n\n"
        ));
        text.push_str(&format!("struct S{i} {{\n    field: u32,\n}}\n\n"));
    }
    text
}

#[test]
fn performance_budgets() {
    let text = big_rust_file();
    assert!(text.lines().count() >= 2000);
    let parser = parser();
    parser.parse("big", Language::Rust, &text).unwrap();

    let start = Instant::now();
    let full = parse(Language::Rust, &text);
    let full_time = start.elapsed();
    assert_eq!(full.symbols.len(), 260);
    assert!(full_time.as_millis() < 60, "full parse took {full_time:?}");

    let edited = text.replacen(
        "let total = helper_50(a) + 1;",
        "let total = helper_50(a) + 2;",
        1,
    );
    let start = Instant::now();
    let out = parser.parse("big", Language::Rust, &edited).unwrap();
    let incremental_time = start.elapsed();
    assert!(out.reused_tree);
    assert_eq!(out.delta.modified, ["function_50"]);
    assert!(
        incremental_time.as_millis() < 25,
        "re-parse took {incremental_time:?}"
    );
}

// ---- file units ----------------------------------------------------------------------------

fn unit_of(source: &str, path: &str, language: Language, text: &str) -> crate::base::Unit {
    build_file_unit(&SourceId::new(source), path, text, &parse(language, text))
}

fn edges_of(unit: &crate::base::Unit, kind: EdgeKind) -> Vec<(&NodeRef, &NodeRef, f32)> {
    unit.edges
        .iter()
        .filter(|e| e.kind == kind)
        .map(|e| (&e.from, &e.to, e.weight))
        .collect()
}

fn lookup_of(node: &NodeRef) -> &crate::base::Lookup {
    match node {
        NodeRef::Lookup(lookup) => lookup,
        NodeRef::Key(key) => panic!("expected a lookup, got key {key}"),
    }
}

#[test]
fn file_unit_nodes_keys_and_attributes() {
    let source = SourceId::new("payments-api");
    let unit = unit_of("payments-api", "src/ledger.rs", Language::Rust, RUST);
    assert_eq!(unit.kind, UnitKind::File);
    assert_eq!(
        unit.key,
        keys::unit(&source, UnitKind::File, "src/ledger.rs")
    );

    let file_key = keys::file(&source, "src/ledger.rs");
    let file = unit.nodes.iter().find(|n| n.key == file_key).unwrap();
    assert_eq!(
        (file.kind, file.name.as_str()),
        (NodeKind::File, "ledger.rs")
    );
    assert!(file.aliases.contains(&"src/ledger.rs".to_owned()));
    assert!(file.aliases.contains(&"src/ledger".to_owned()));
    assert_eq!(file.attrs[attrs::LANGUAGE], "rust");
    assert_eq!(file.attrs[attrs::PATH], "src/ledger.rs");

    let charge_key = keys::symbol(&source, "src/ledger.rs", "Ledger::charge");
    let charge = unit.nodes.iter().find(|n| n.key == charge_key).unwrap();
    assert_eq!(
        (charge.kind, charge.name.as_str()),
        (NodeKind::Symbol, "charge")
    );
    assert!(charge.aliases.contains(&"charge".to_owned()));
    assert!(charge.aliases.contains(&"Ledger::charge".to_owned()));
    assert_eq!(charge.attrs[attrs::SYMBOL_KIND], "method");
    assert_eq!(charge.attrs[attrs::VISIBILITY], "crate");
    assert_eq!(charge.attrs[attrs::LINE_START], 20);
    assert_eq!(charge.attrs[attrs::LINE_END], 23);
    assert_eq!(
        charge.attrs[attrs::SIGNATURE],
        "pub(crate) fn charge(&mut self, amount: Money)"
    );
    // Every symbol is a node; the service node is not asserted.
    assert_eq!(unit.nodes.len(), 1 + 11);
    assert!(unit.nodes.iter().all(|n| n.kind != NodeKind::Service));
}

#[test]
fn file_unit_containment_calls_and_imports() {
    let source = SourceId::new("payments-api");
    let unit = unit_of("payments-api", "src/ledger.rs", Language::Rust, RUST);
    let file_key = keys::file(&source, "src/ledger.rs");
    let key = |q: &str| keys::symbol(&source, "src/ledger.rs", q);

    let contains = edges_of(&unit, EdgeKind::Contains);
    let has_contains = |from: &crate::base::NodeKey, to: &crate::base::NodeKey| {
        contains
            .iter()
            .any(|(f, t, _)| **f == NodeRef::Key(from.clone()) && **t == NodeRef::Key(to.clone()))
    };
    assert!(has_contains(&keys::service("payments-api"), &file_key));
    assert!(has_contains(&file_key, &key("Ledger")));
    assert!(has_contains(&file_key, &key("impl Ledger<T>")));
    assert!(has_contains(&key("impl Ledger<T>"), &key("Ledger::new")));
    assert!(!has_contains(&file_key, &key("Ledger::new")));

    let calls = edges_of(&unit, EdgeKind::Calls);
    let charge_calls: Vec<_> = calls
        .iter()
        .filter(|(from, _, _)| **from == NodeRef::Key(key("Ledger::charge")))
        .map(|(_, to, _)| lookup_of(to))
        .collect();
    let record = charge_calls.iter().find(|l| l.text == "record").unwrap();
    assert_eq!(record.kinds, [NodeKind::Symbol]);
    let scope = record.scope.as_ref().unwrap();
    assert_eq!(
        scope.source.as_ref().map(SourceId::as_str),
        Some("payments-api")
    );
    assert_eq!(scope.path.as_deref(), Some("src/ledger.rs"));
    // A path qualifier becomes the qualified name, a receiver expression does not.
    assert!(
        calls
            .iter()
            .any(|(_, to, _)| lookup_of(to).text == "util::g")
    );
    assert!(
        calls
            .iter()
            .any(|(_, to, _)| lookup_of(to).text == "Vec::new")
    );

    let refs = edges_of(&unit, EdgeKind::References);
    assert!(refs.iter().any(
        |(from, to, _)| **from == NodeRef::Key(key("Ledger::charge"))
            && lookup_of(to).text == "Money"
    ));
    assert!(
        refs.iter()
            .any(|(from, to, _)| **from == NodeRef::Key(file_key.clone())
                && lookup_of(to).text == "HashMap")
    );
    assert!(
        refs.iter()
            .any(|(_, to, _)| lookup_of(to).text == "Invoice")
    );

    let imports = edges_of(&unit, EdgeKind::Imports);
    assert_eq!(imports.len(), 1);
    let (_, to, weight) = imports[0];
    assert_eq!(lookup_of(to).kinds, [NodeKind::Service]);
    assert_eq!(lookup_of(to).text, "std");
    assert!((weight - 0.5).abs() < f32::EPSILON);
}

#[test]
fn file_level_references_originate_from_the_file_node() {
    let text = "helper()\nclass A:\n    pass\n";
    let unit = unit_of("repo", "m.py", Language::Python, text);
    let file_key = keys::file(&SourceId::new("repo"), "m.py");
    let refs = edges_of(&unit, EdgeKind::References);
    assert_eq!(refs.len(), 1);
    assert_eq!(*refs[0].0, NodeRef::Key(file_key));
    assert!(edges_of(&unit, EdgeKind::Calls).is_empty());
}

#[test]
fn relative_imports_become_file_lookups() {
    let ts = "import { a } from './ledger';\nimport b from '../shared/util.js';\nimport c from '@scope/pkg/sub';\n";
    let unit = unit_of("web", "src/app/main.ts", Language::TypeScript, ts);
    let imports = edges_of(&unit, EdgeKind::Imports);
    let targets: Vec<_> = imports
        .iter()
        .map(|(_, to, w)| (lookup_of(to).kinds[0], lookup_of(to).text.as_str(), *w))
        .collect();
    assert!(targets.contains(&(NodeKind::File, "src/app/ledger", 1.0)));
    assert!(targets.contains(&(NodeKind::File, "src/shared/util", 1.0)));
    assert!(targets.contains(&(NodeKind::Service, "pkg", 0.5)));

    let py = "from .ledger import Ledger\nfrom .. import peer\nimport os.path\n";
    let unit = unit_of("svc", "pkg/sub/mod.py", Language::Python, py);
    let imports = edges_of(&unit, EdgeKind::Imports);
    let targets: Vec<_> = imports
        .iter()
        .map(|(_, to, _)| lookup_of(to).text.as_str())
        .collect();
    assert!(targets.contains(&"pkg/sub/ledger"));
    assert!(targets.contains(&"pkg/peer"));
    assert!(targets.contains(&"os"));
    let refs: Vec<_> = edges_of(&unit, EdgeKind::References)
        .iter()
        .map(|(_, to, _)| lookup_of(to).text.clone())
        .collect();
    assert!(refs.contains(&"Ledger".to_owned()));

    let c = unit_of("fw", "src/drv/uart.c", Language::C, C);
    let targets: Vec<_> = edges_of(&c, EdgeKind::Imports)
        .iter()
        .map(|(_, to, _)| (lookup_of(to).kinds[0], lookup_of(to).text.clone()))
        .collect();
    assert!(targets.contains(&(NodeKind::File, "src/drv/ledger.h".to_owned())));
}

#[test]
fn file_unit_chunks_and_determinism() {
    let source = SourceId::new("payments-api");
    let unit = unit_of("payments-api", "src/ledger.rs", Language::Rust, RUST);
    let file_key = keys::file(&source, "src/ledger.rs");
    let new_key = keys::symbol(&source, "src/ledger.rs", "Ledger::new");

    let module_doc = unit.chunks.iter().find(|c| c.node == file_key).unwrap();
    assert_eq!(
        (module_doc.kind, module_doc.text.as_str()),
        (ChunkKind::Docstring, "Ledger crate.\nSecond line.")
    );
    let for_new: Vec<_> = unit.chunks.iter().filter(|c| c.node == new_key).collect();
    assert_eq!(for_new.len(), 2);
    assert_eq!(for_new[0].kind, ChunkKind::Docstring);
    assert_eq!(for_new[0].text, "Creates one.");
    assert_eq!(for_new[1].kind, ChunkKind::Code);
    assert!(for_new[1].text.starts_with("pub fn new() -> Self {"));
    assert!(for_new[1].text.contains("Ledger { entries"));

    let again = unit_of("payments-api", "src/ledger.rs", Language::Rust, RUST);
    assert_eq!(unit.content_hash().unwrap(), again.content_hash().unwrap());
    let other = unit_of(
        "payments-api",
        "src/ledger.rs",
        Language::Rust,
        &RUST.replace("Creates one.", "Makes one."),
    );
    assert_ne!(unit.content_hash().unwrap(), other.content_hash().unwrap());
}

#[test]
fn code_chunks_are_bounded() {
    let body: String = (0..200)
        .map(|i| format!("    let value_{i} = compute({i});\n"))
        .collect();
    let text = format!("fn long() {{\n{body}}}\n");
    let unit = unit_of("r", "long.rs", Language::Rust, &text);
    let chunk = unit
        .chunks
        .iter()
        .find(|c| c.kind == ChunkKind::Code)
        .unwrap();
    assert!(chunk.text.lines().count() <= 40);
    assert!(chunk.text.chars().count() <= 1200);
    assert!(chunk.text.starts_with("fn long() {"));

    let one_line = format!("const BIG: &str = \"{}\";\n", "x".repeat(5000));
    let unit = unit_of("r", "big.rs", Language::Rust, &one_line);
    let chunk = unit
        .chunks
        .iter()
        .find(|c| c.kind == ChunkKind::Code)
        .unwrap();
    assert_eq!(chunk.text.chars().count(), 1200);
}

#[test]
fn empty_file_unit_is_just_the_file_node() {
    let unit = unit_of("r", "empty.rs", Language::Rust, "");
    assert_eq!(unit.nodes.len(), 1);
    assert_eq!(unit.edges.len(), 1);
    assert!(unit.chunks.is_empty());
}
