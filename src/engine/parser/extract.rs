//! Symbol, reference and import extraction from a Tree-sitter syntax tree.
//!
//! Extraction is a pure function of `(language, text, tree)`: it never looks at the previous parse.
//! That is what makes the incremental result identical to a from-scratch one, as long as the
//! incremental tree equals the fresh tree.

use std::collections::{HashMap, HashSet};

use tree_sitter::{Node, Tree};

use crate::base::{ContentHash, SymbolKind};

use super::Language;
use super::model::{Import, ParsedFile, Reference, ReferenceKind, Symbol, SyntaxError, Visibility};

/// Nesting beyond this is not descended into: bounds recursion on pathological inputs.
const MAX_DEPTH: usize = 160;
const MAX_SIGNATURE_CHARS: usize = 240;
const MAX_MODULE_DOC_CHARS: usize = 4000;
const MAX_QUALIFIER_CHARS: usize = 80;

/// Extracts everything the parser reports about `text`, given its syntax `tree`.
pub(crate) fn extract(language: Language, tree: &Tree, text: &str) -> ParsedFile {
    let root = tree.root_node();
    let mut extractor = Extractor {
        src: text,
        lang: language,
        out: ParsedFile::empty(language),
        scope: Vec::new(),
        counts: HashMap::new(),
        wrapper: None,
        exported: false,
        used_docs: HashSet::new(),
        def_names: HashSet::new(),
        depth: 0,
    };
    extractor.visit(root);
    extractor.out.module_doc = extractor.module_doc(root);
    collect_errors(root, &mut extractor.out.syntax_errors);
    extractor.out
}

fn collect_errors(root: Node<'_>, out: &mut Vec<SyntaxError>) {
    if !root.has_error() {
        return;
    }
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if out.len() >= ParsedFile::MAX_SYNTAX_ERRORS {
            return;
        }
        let message = if node.is_error() {
            "unexpected syntax".to_owned()
        } else if node.is_missing() {
            format!("missing `{}`", node.kind())
        } else {
            if node.has_error() {
                let kids = kids(node);
                stack.extend(kids.into_iter().rev());
            }
            continue;
        };
        let start = node.start_position();
        out.push(SyntaxError {
            line: start.row as u32 + 1,
            column: start.column as u32 + 1,
            byte_range: node.byte_range(),
            message,
        });
    }
}

fn kids(node: Node<'_>) -> Vec<Node<'_>> {
    let mut cursor = node.walk();
    node.children(&mut cursor).collect()
}

fn named_kids(node: Node<'_>) -> Vec<Node<'_>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor).collect()
}

fn field<'t>(node: Node<'t>, name: &str) -> Option<Node<'t>> {
    node.child_by_field_name(name)
}

fn fields<'t>(node: Node<'t>, name: &str) -> Vec<Node<'t>> {
    let mut cursor = node.walk();
    node.children_by_field_name(name, &mut cursor).collect()
}

fn collapse(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn truncate_chars(text: String, max: usize) -> String {
    if text.len() <= max {
        return text;
    }
    text.chars().take(max).collect()
}

fn is_comment(kind: &str) -> bool {
    kind.ends_with("comment")
}

fn end_row(node: Node<'_>) -> usize {
    let end = node.end_position();
    if end.column == 0 && end.row > node.start_position().row {
        end.row - 1
    } else {
        end.row
    }
}

fn unquote(text: &str) -> &str {
    text.trim().trim_matches(['"', '\'', '`'])
}

/// A receiver is only kept when it is a short, simple expression.
fn simple_expr(text: &str) -> Option<String> {
    let text = text.trim();
    let simple = !text.is_empty()
        && text.len() <= MAX_QUALIFIER_CHARS
        && text
            .chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '_' | '$' | '.' | ':' | '#' | '@'));
    simple.then(|| text.to_owned())
}

struct Scope {
    index: usize,
    prefix: String,
    members_public: bool,
    generics: Vec<String>,
}

struct Def<'t> {
    name: String,
    kind: SymbolKind,
    node: Node<'t>,
    name_node: Option<Node<'t>>,
    body: Option<Node<'t>>,
    visibility: Visibility,
    child_prefix: Option<String>,
    qualifier: Option<String>,
    parent: Option<usize>,
    signature: Option<String>,
    docstring: Option<String>,
    members_public: bool,
}

impl<'t> Def<'t> {
    fn new(name: impl Into<String>, kind: SymbolKind, node: Node<'t>) -> Self {
        Self {
            name: name.into(),
            kind,
            node,
            name_node: None,
            body: None,
            visibility: Visibility::Private,
            child_prefix: None,
            qualifier: None,
            parent: None,
            signature: None,
            docstring: None,
            members_public: false,
        }
    }

    fn named(mut self, name_node: Option<Node<'t>>) -> Self {
        self.name_node = name_node;
        self
    }

    fn body(mut self, body: Option<Node<'t>>) -> Self {
        self.body = body;
        self
    }

    fn vis(mut self, visibility: Visibility) -> Self {
        self.visibility = visibility;
        self
    }
}

struct Extractor<'t, 's> {
    src: &'s str,
    lang: Language,
    out: ParsedFile,
    scope: Vec<Scope>,
    counts: HashMap<String, u32>,
    /// Enclosing node (`export ...`, decorators, `type` declaration) that the next symbol
    /// should span and take its doc comment from.
    wrapper: Option<Node<'t>>,
    exported: bool,
    used_docs: HashSet<usize>,
    /// Name nodes of definitions: they are not references.
    def_names: HashSet<usize>,
    depth: usize,
}

impl<'t, 's> Extractor<'t, 's> {
    fn text(&self, node: Node<'_>) -> &'s str {
        let src: &'s str = self.src;
        src.get(node.byte_range()).unwrap_or("")
    }

    fn ftext(&self, node: Node<'_>, name: &str) -> Option<&'s str> {
        field(node, name).map(|n| self.text(n))
    }

    fn top_kind(&self) -> Option<SymbolKind> {
        self.scope
            .last()
            .map(|scope| self.out.symbols[scope.index].kind)
    }

    fn prefixed(&self, name: &str) -> String {
        match self.scope.last() {
            Some(scope) if !scope.prefix.is_empty() => format!("{}::{name}", scope.prefix),
            _ => name.to_owned(),
        }
    }

    fn is_generic(&self, name: &str) -> bool {
        self.scope
            .iter()
            .any(|scope| scope.generics.iter().any(|g| g == name))
    }

    // ---- traversal ---------------------------------------------------------------------

    fn visit(&mut self, node: Node<'t>) {
        if self.depth >= MAX_DEPTH {
            return;
        }
        self.depth += 1;
        match self.lang {
            Language::Rust => self.rust(node),
            Language::Python => self.python(node),
            lang if lang.is_js_family() => self.js(node),
            Language::Go => self.go(node),
            Language::Java => self.java(node),
            _ => self.c(node),
        }
        self.depth -= 1;
    }

    fn recurse(&mut self, node: Node<'t>) {
        for child in kids(node) {
            if child.is_named() {
                self.visit(child);
            }
        }
    }

    fn define(&mut self, def: Def<'t>) {
        let node = def.node;
        let generics = self.generic_names(node);
        let (index, base) = self.emit(&def);
        self.scope.push(Scope {
            index,
            prefix: def.child_prefix.unwrap_or(base),
            members_public: def.members_public,
            generics,
        });
        self.recurse(node);
        self.scope.pop();
    }

    fn generic_names(&self, node: Node<'_>) -> Vec<String> {
        let Some(params) = field(node, "type_parameters") else {
            return Vec::new();
        };
        named_kids(params)
            .into_iter()
            .filter_map(|param| {
                let name = field(param, "name")
                    .or_else(|| named_kids(param).into_iter().next())
                    .unwrap_or(param);
                matches!(name.kind(), "identifier" | "type_identifier")
                    .then(|| self.text(name).to_owned())
            })
            .collect()
    }

    /// Records a symbol; returns its index and its unsuffixed qualified name.
    fn emit(&mut self, def: &Def<'t>) -> (usize, String) {
        let outer = self.wrapper.take().unwrap_or(def.node);
        self.exported = false;
        if let Some(name_node) = def.name_node {
            self.def_names.insert(name_node.id());
        }
        let start = outer.start_byte();
        let mut end = outer.end_byte().min(self.src.len());
        let bytes = self.src.as_bytes();
        while end > start && bytes[end - 1].is_ascii_whitespace() {
            end -= 1;
        }
        let line_start = outer.start_position().row as u32 + 1;
        let line_end = if end == outer.end_byte() {
            end_row(outer) as u32 + 1
        } else {
            let newlines = self
                .src
                .get(start..end)
                .map_or(0, |s| s.matches('\n').count());
            line_start + newlines as u32
        };
        let prefix = def
            .qualifier
            .clone()
            .or_else(|| self.scope.last().map(|scope| scope.prefix.clone()));
        let base = match prefix {
            Some(prefix) if !prefix.is_empty() => format!("{prefix}::{}", def.name),
            _ => def.name.clone(),
        };
        let seen = self.counts.entry(base.clone()).or_insert(0);
        *seen += 1;
        let qualified_name = if *seen == 1 {
            base.clone()
        } else {
            format!("{base}~{seen}")
        };
        let sig_start = if outer.kind() == "decorated_definition" {
            def.node.start_byte()
        } else {
            start
        };
        let signature = def
            .signature
            .clone()
            .unwrap_or_else(|| self.signature(sig_start, def.body, end));
        let doc = if self.lang == Language::Python {
            def.docstring.clone()
        } else {
            self.doc_of(outer)
        };
        let parent = def
            .parent
            .or_else(|| self.scope.last().map(|scope| scope.index));
        let source_range = start..end.max(start);
        let content_hash = ContentHash::of(
            self.src
                .as_bytes()
                .get(source_range.clone())
                .unwrap_or_default(),
        );
        self.out.symbols.push(Symbol {
            name: def.name.clone(),
            qualified_name,
            kind: def.kind,
            parent,
            byte_range: source_range,
            line_start,
            line_end: line_end.max(line_start),
            signature,
            doc,
            visibility: def.visibility,
            content_hash,
        });
        (self.out.symbols.len() - 1, base)
    }

    fn signature(&self, from: usize, body: Option<Node<'_>>, end: usize) -> String {
        let to = body
            .map(|b| b.start_byte())
            .filter(|b| *b >= from && *b <= end)
            .unwrap_or(end);
        let raw = self.src.get(from..to).unwrap_or("");
        let collapsed = collapse(raw);
        let trimmed = collapsed.trim_end_matches(['{', ' ']).to_owned();
        truncate_chars(trimmed, MAX_SIGNATURE_CHARS)
    }

    // ---- references --------------------------------------------------------------------

    fn add_ref(
        &mut self,
        name: &str,
        qualifier: Option<String>,
        qualifier_is_path: bool,
        kind: ReferenceKind,
        node: Node<'_>,
    ) {
        let name = name.trim();
        if name.is_empty() || name.len() > 200 {
            return;
        }
        let from_symbol = self.scope.last().map(|scope| scope.index);
        self.out.references.push(Reference {
            name: name.to_owned(),
            qualifier,
            qualifier_is_path,
            kind,
            from_symbol,
            line: node.start_position().row as u32 + 1,
        });
    }

    /// Predeclared types are never defined in the repository; references to them are noise.
    fn is_builtin_type(&self, name: &str) -> bool {
        const GO: &[&str] = &[
            "any",
            "bool",
            "byte",
            "comparable",
            "complex64",
            "complex128",
            "error",
            "float32",
            "float64",
            "int",
            "int8",
            "int16",
            "int32",
            "int64",
            "rune",
            "string",
            "uint",
            "uint8",
            "uint16",
            "uint32",
            "uint64",
            "uintptr",
        ];
        const PYTHON: &[&str] = &[
            "bool",
            "bytes",
            "dict",
            "float",
            "frozenset",
            "int",
            "list",
            "object",
            "set",
            "str",
            "tuple",
        ];
        match self.lang {
            Language::Go => GO.contains(&name),
            Language::Python => PYTHON.contains(&name),
            _ => false,
        }
    }

    fn type_ref(&mut self, node: Node<'_>) {
        if self.def_names.contains(&node.id()) {
            return;
        }
        let name = self.text(node);
        if name == "Self" || self.is_generic(name) || self.is_builtin_type(name) {
            return;
        }
        self.add_ref(name, None, false, ReferenceKind::Type, node);
    }

    fn scoped_type_ref(
        &mut self,
        node: Node<'_>,
        qualifier: Option<Node<'_>>,
        name: Option<Node<'_>>,
    ) {
        if let Some(name) = name {
            let qualifier = qualifier.map(|q| collapse(self.text(q)));
            let qualifier = qualifier.and_then(|q| simple_expr(&q));
            self.add_ref(self.text(name), qualifier, true, ReferenceKind::Type, node);
        }
    }

    fn call_ref(
        &mut self,
        name: Option<Node<'_>>,
        qualifier: Option<Node<'_>>,
        path: bool,
        at: Node<'_>,
    ) {
        let Some(name) = name else { return };
        let qualifier = qualifier.and_then(|q| simple_expr(self.text(q)));
        self.add_ref(self.text(name), qualifier, path, ReferenceKind::Call, at);
    }

    fn push_import(&mut self, path: String, names: Vec<String>, relative: bool, node: Node<'_>) {
        if path.is_empty() {
            return;
        }
        self.out.imports.push(Import {
            path,
            names,
            relative,
            line: node.start_position().row as u32 + 1,
        });
    }

    // ---- documentation -----------------------------------------------------------------

    fn comment_accepted(&self, text: &str) -> bool {
        match self.lang {
            Language::Rust => {
                (text.starts_with("///") && !text.starts_with("////"))
                    || (text.starts_with("/**")
                        && !text.starts_with("/**/")
                        && !text.starts_with("/***"))
            }
            Language::JavaScript | Language::TypeScript | Language::Tsx | Language::Java => {
                text.starts_with("/**") && !text.starts_with("/**/")
            }
            Language::Go => text.starts_with("//") && !text.starts_with("//go:"),
            Language::C => true,
            Language::Python => false,
        }
    }

    fn doc_of(&mut self, anchor: Node<'t>) -> Option<String> {
        let mut run: Vec<Node<'t>> = Vec::new();
        let mut current = anchor;
        while let Some(prev) = current.prev_sibling() {
            if self.lang == Language::Rust && prev.kind() == "attribute_item" {
                current = prev;
                continue;
            }
            if !is_comment(prev.kind())
                || end_row(prev) + 1 < current.start_position().row
                || !self.comment_accepted(self.text(prev))
            {
                break;
            }
            run.push(prev);
            current = prev;
        }
        run.reverse();
        // A comment trailing the previous declaration on its own line belongs to that one.
        if let Some(first) = run.first()
            && let Some(before) = first.prev_sibling()
            && !is_comment(before.kind())
            && end_row(before) == first.start_position().row
        {
            run.remove(0);
        }
        if run.is_empty() {
            return None;
        }
        let parts: Vec<String> = run.iter().map(|c| clean_comment(self.text(*c))).collect();
        self.used_docs.extend(run.iter().map(|c| c.id()));
        let doc = parts.join("\n").trim().to_owned();
        (!doc.is_empty()).then_some(doc)
    }

    fn module_doc(&self, root: Node<'_>) -> Option<String> {
        let doc = match self.lang {
            Language::Python => {
                let first = named_kids(root).into_iter().next()?;
                python_docstring_of_statement(self, first)?
            }
            Language::Rust => {
                let mut lines = Vec::new();
                for child in named_kids(root) {
                    if !is_comment(child.kind()) {
                        break;
                    }
                    let text = self.text(child);
                    if text.starts_with("//!") || text.starts_with("/*!") {
                        lines.push(clean_comment(text));
                    } else {
                        break;
                    }
                }
                lines.join("\n")
            }
            _ => {
                let mut parts = Vec::new();
                for child in named_kids(root) {
                    if !is_comment(child.kind()) {
                        break;
                    }
                    if !self.used_docs.contains(&child.id()) {
                        parts.push(clean_comment(self.text(child)));
                    }
                }
                parts.join("\n\n")
            }
        };
        let doc = doc.trim();
        (!doc.is_empty()).then(|| truncate_chars(doc.to_owned(), MAX_MODULE_DOC_CHARS))
    }

    // ---- Rust --------------------------------------------------------------------------

    fn rust_vis(&self, node: Node<'_>) -> Visibility {
        for child in kids(node) {
            if child.kind() == "visibility_modifier" {
                return if collapse(self.text(child)) == "pub" {
                    Visibility::Public
                } else {
                    Visibility::Restricted
                };
            }
        }
        if self.scope.last().is_some_and(|s| s.members_public) {
            Visibility::Public
        } else {
            Visibility::Private
        }
    }

    fn rust_type_name(&self, node: Node<'_>) -> String {
        match node.kind() {
            "type_identifier" => self.text(node).to_owned(),
            "generic_type" => field(node, "type").map_or_else(
                || self.text(node).to_owned(),
                |inner| self.rust_type_name(inner),
            ),
            "scoped_type_identifier" => field(node, "name").map_or_else(
                || self.text(node).to_owned(),
                |name| self.text(name).to_owned(),
            ),
            "reference_type" | "pointer_type" => field(node, "type").map_or_else(
                || collapse(self.text(node)),
                |inner| self.rust_type_name(inner),
            ),
            _ => collapse(self.text(node)),
        }
    }

    fn rust(&mut self, node: Node<'t>) {
        let simple = |kind: SymbolKind| Some(kind);
        let kind = match node.kind() {
            "function_item" | "function_signature_item" => {
                if matches!(self.top_kind(), Some(SymbolKind::Impl | SymbolKind::Trait)) {
                    simple(SymbolKind::Method)
                } else {
                    simple(SymbolKind::Function)
                }
            }
            "struct_item" => simple(SymbolKind::Struct),
            "enum_item" => simple(SymbolKind::Enum),
            "union_item" => simple(SymbolKind::Union),
            "trait_item" => simple(SymbolKind::Trait),
            "mod_item" => simple(SymbolKind::Module),
            "const_item" => simple(SymbolKind::Constant),
            "static_item" => simple(SymbolKind::Variable),
            "type_item" => simple(SymbolKind::TypeAlias),
            "macro_definition" => simple(SymbolKind::Macro),
            _ => None,
        };
        if let Some(kind) = kind {
            let Some(name) = field(node, "name") else {
                return self.recurse(node);
            };
            let mut def = Def::new(self.text(name), kind, node)
                .named(Some(name))
                .body(field(node, "body"))
                .vis(self.rust_vis(node));
            def.members_public = kind == SymbolKind::Trait;
            return self.define(def);
        }
        match node.kind() {
            "impl_item" => {
                let Some(ty) = field(node, "type") else {
                    return self.recurse(node);
                };
                let type_text = collapse(self.text(ty));
                let tr = field(node, "trait");
                let name = match tr {
                    Some(tr) => format!("impl {} for {type_text}", collapse(self.text(tr))),
                    None => format!("impl {type_text}"),
                };
                let mut def = Def::new(name, SymbolKind::Impl, node).body(field(node, "body"));
                def.child_prefix = Some(self.prefixed(&self.rust_type_name(ty)));
                def.members_public = tr.is_some();
                def.visibility = if tr.is_some() {
                    Visibility::Public
                } else {
                    Visibility::Private
                };
                self.define(def);
            }
            "use_declaration" => {
                if let Some(arg) = field(node, "argument") {
                    let path = collapse(self.text(arg));
                    let mut names = Vec::new();
                    self.rust_use_names(arg, &mut names);
                    let relative = ["crate::", "self::", "super::"]
                        .iter()
                        .any(|p| path.starts_with(p))
                        || path == "crate"
                        || path == "self"
                        || path == "super";
                    self.push_import(path, names, relative, node);
                }
            }
            "call_expression" => {
                if let Some(function) = field(node, "function") {
                    self.rust_callee(function, node);
                }
                self.recurse(node);
            }
            "macro_invocation" => {
                if let Some(mac) = field(node, "macro") {
                    let (name, qualifier) = match mac.kind() {
                        "scoped_identifier" => (field(mac, "name"), field(mac, "path")),
                        _ => (Some(mac), None),
                    };
                    if let Some(name) = name {
                        let qualifier = qualifier.and_then(|q| simple_expr(self.text(q)));
                        self.add_ref(self.text(name), qualifier, true, ReferenceKind::Macro, node);
                    }
                }
                self.recurse(node);
            }
            "type_identifier" => self.type_ref(node),
            "scoped_type_identifier" => {
                self.scoped_type_ref(node, field(node, "path"), field(node, "name"));
            }
            _ => self.recurse(node),
        }
    }

    fn rust_use_names(&self, node: Node<'_>, names: &mut Vec<String>) {
        match node.kind() {
            "identifier" => names.push(self.text(node).to_owned()),
            "scoped_identifier" => {
                if let Some(name) = field(node, "name") {
                    names.push(self.text(name).to_owned());
                }
            }
            "use_as_clause" => {
                if let Some(path) = field(node, "path") {
                    self.rust_use_names(path, names);
                }
            }
            "use_list" => {
                for child in named_kids(node) {
                    self.rust_use_names(child, names);
                }
            }
            "scoped_use_list" => {
                if let Some(list) = field(node, "list") {
                    self.rust_use_names(list, names);
                }
            }
            _ => {}
        }
    }

    fn rust_callee(&mut self, function: Node<'_>, at: Node<'_>) {
        match function.kind() {
            "identifier" => self.call_ref(Some(function), None, false, at),
            "scoped_identifier" => {
                self.call_ref(field(function, "name"), field(function, "path"), true, at);
            }
            "field_expression" => {
                self.call_ref(
                    field(function, "field"),
                    field(function, "value"),
                    false,
                    at,
                );
            }
            "generic_function" => {
                if let Some(inner) = field(function, "function") {
                    self.rust_callee(inner, at);
                }
            }
            _ => {}
        }
    }

    // ---- Python ------------------------------------------------------------------------

    fn python_vis(name: &str) -> Visibility {
        if name.starts_with("__") && name.ends_with("__") {
            Visibility::Public
        } else if name.starts_with("__") {
            Visibility::Private
        } else if name.starts_with('_') {
            Visibility::Restricted
        } else {
            Visibility::Public
        }
    }

    fn python(&mut self, node: Node<'t>) {
        match node.kind() {
            "decorated_definition" => {
                self.wrapper = Some(node);
                self.recurse(node);
                self.wrapper = None;
            }
            "function_definition" | "class_definition" => {
                let Some(name) = field(node, "name") else {
                    return self.recurse(node);
                };
                let text = self.text(name);
                let is_class = node.kind() == "class_definition";
                let kind = if is_class {
                    SymbolKind::Class
                } else if self.top_kind() == Some(SymbolKind::Class) {
                    SymbolKind::Method
                } else {
                    SymbolKind::Function
                };
                let body = field(node, "body");
                let mut def = Def::new(text, kind, node)
                    .named(Some(name))
                    .body(body)
                    .vis(Self::python_vis(text));
                def.docstring = body
                    .and_then(|b| named_kids(b).into_iter().next())
                    .and_then(|first| python_docstring_of_statement(self, first));
                self.define(def);
            }
            "expression_statement" if self.scope.is_empty() => {
                if let Some(assign) = named_kids(node)
                    .into_iter()
                    .find(|c| c.kind() == "assignment")
                    && let Some(left) = field(assign, "left")
                    && left.kind() == "identifier"
                {
                    let name = self.text(left);
                    let kind =
                        if name.chars().any(char::is_alphabetic) && name == name.to_uppercase() {
                            SymbolKind::Constant
                        } else {
                            SymbolKind::Variable
                        };
                    let def = Def::new(name, kind, node)
                        .named(Some(left))
                        .vis(Self::python_vis(name));
                    self.emit(&def);
                }
                self.recurse(node);
            }
            "call" => {
                if let Some(function) = field(node, "function") {
                    match function.kind() {
                        "identifier" => self.call_ref(Some(function), None, false, node),
                        "attribute" => self.call_ref(
                            field(function, "attribute"),
                            field(function, "object"),
                            false,
                            node,
                        ),
                        _ => {}
                    }
                }
                self.recurse(node);
            }
            "type" => self.python_type(node),
            "argument_list"
                if node
                    .parent()
                    .is_some_and(|p| p.kind() == "class_definition") =>
            {
                for child in named_kids(node) {
                    match child.kind() {
                        "identifier" => {
                            self.add_ref(self.text(child), None, false, ReferenceKind::Type, child);
                        }
                        "attribute" => self.python_type(child),
                        _ => self.visit(child),
                    }
                }
            }
            "import_statement" => {
                for name in fields(node, "name") {
                    let target = if name.kind() == "aliased_import" {
                        field(name, "name").unwrap_or(name)
                    } else {
                        name
                    };
                    self.push_import(collapse(self.text(target)), Vec::new(), false, node);
                }
            }
            "import_from_statement" => {
                let path = self
                    .ftext(node, "module_name")
                    .map(collapse)
                    .unwrap_or_default();
                let names = fields(node, "name")
                    .into_iter()
                    .map(|name| {
                        let target = if name.kind() == "aliased_import" {
                            field(name, "name").unwrap_or(name)
                        } else {
                            name
                        };
                        collapse(self.text(target))
                    })
                    .collect();
                let relative = path.starts_with('.');
                self.push_import(path, names, relative, node);
            }
            _ => self.recurse(node),
        }
    }

    fn python_type(&mut self, node: Node<'t>) {
        if self.depth >= MAX_DEPTH {
            return;
        }
        self.depth += 1;
        match node.kind() {
            "identifier" => {
                let name = self.text(node);
                if !self.is_generic(name) && !self.is_builtin_type(name) {
                    self.add_ref(name, None, false, ReferenceKind::Type, node);
                }
            }
            "attribute" => {
                let qualifier = self.ftext(node, "object").and_then(simple_expr);
                if let Some(name) = self.ftext(node, "attribute") {
                    self.add_ref(name, qualifier, true, ReferenceKind::Type, node);
                }
            }
            "string" => {}
            _ => {
                for child in named_kids(node) {
                    self.python_type(child);
                }
            }
        }
        self.depth -= 1;
    }

    // ---- JavaScript / TypeScript -------------------------------------------------------

    fn js_member_vis(&self, node: Node<'_>, name: &str) -> Visibility {
        if name.starts_with('#') {
            return Visibility::Private;
        }
        for child in kids(node) {
            if child.kind() == "accessibility_modifier" {
                return match self.text(child) {
                    "private" => Visibility::Private,
                    "protected" => Visibility::Restricted,
                    _ => Visibility::Public,
                };
            }
        }
        Visibility::Public
    }

    fn js_vis(&self, node: Node<'_>, name: &str, member: bool) -> Visibility {
        if member {
            self.js_member_vis(node, name)
        } else if self.exported {
            Visibility::Public
        } else {
            Visibility::Private
        }
    }

    fn js(&mut self, node: Node<'t>) {
        let member_scope = matches!(
            self.top_kind(),
            Some(SymbolKind::Class | SymbolKind::Interface)
        );
        match node.kind() {
            "export_statement" => {
                if let Some(source) = field(node, "source") {
                    let mut names = Vec::new();
                    self.js_specifiers(node, &mut names);
                    let path = unquote(self.text(source)).to_owned();
                    let relative = path.starts_with('.');
                    self.push_import(path, names, relative, node);
                    return;
                }
                self.exported = true;
                self.wrapper = Some(node);
                self.recurse(node);
                self.exported = false;
                self.wrapper = None;
            }
            "function_declaration" | "generator_function_declaration" | "function_signature" => {
                self.js_named(node, SymbolKind::Function, member_scope);
            }
            "class_declaration" | "abstract_class_declaration" => {
                self.js_named(node, SymbolKind::Class, member_scope);
            }
            "interface_declaration" => self.js_named(node, SymbolKind::Interface, member_scope),
            "type_alias_declaration" => self.js_named(node, SymbolKind::TypeAlias, member_scope),
            "enum_declaration" => self.js_named(node, SymbolKind::Enum, member_scope),
            "internal_module" | "module" => self.js_named(node, SymbolKind::Module, member_scope),
            "method_definition" | "method_signature" | "abstract_method_signature"
                if member_scope || node.parent().is_some_and(|p| p.kind() == "class_body") =>
            {
                let Some(name) = field(node, "name") else {
                    return self.recurse(node);
                };
                if matches!(name.kind(), "computed_property_name") {
                    return self.recurse(node);
                }
                let text = unquote(self.text(name));
                let vis = if self.top_kind() == Some(SymbolKind::Interface) {
                    Visibility::Public
                } else {
                    self.js_member_vis(node, text)
                };
                let def = Def::new(text, SymbolKind::Method, node)
                    .named(Some(name))
                    .body(field(node, "body"))
                    .vis(vis);
                self.define(def);
            }
            "public_field_definition" | "field_definition" => {
                let value = field(node, "value");
                let name = field(node, "name").or_else(|| field(node, "property"));
                if let (Some(name), Some(value)) = (name, value)
                    && matches!(
                        value.kind(),
                        "arrow_function" | "function_expression" | "function"
                    )
                {
                    let text = self.text(name);
                    let vis = self.js_member_vis(node, text);
                    let def = Def::new(text, SymbolKind::Method, node)
                        .named(Some(name))
                        .body(field(value, "body"))
                        .vis(vis);
                    return self.define(def);
                }
                self.recurse(node);
            }
            "lexical_declaration" | "variable_declaration" => self.js_variables(node),
            "call_expression" => {
                if let Some(function) = field(node, "function") {
                    match function.kind() {
                        "identifier" if self.text(function) == "require" => {
                            if let Some(path) = self.js_first_string_arg(node) {
                                let relative = path.starts_with('.');
                                self.push_import(path, Vec::new(), relative, node);
                                return;
                            }
                            self.call_ref(Some(function), None, false, node);
                        }
                        "identifier" => self.call_ref(Some(function), None, false, node),
                        "import" => {
                            if let Some(path) = self.js_first_string_arg(node) {
                                let relative = path.starts_with('.');
                                self.push_import(path, Vec::new(), relative, node);
                            }
                        }
                        "member_expression" => self.call_ref(
                            field(function, "property"),
                            field(function, "object"),
                            false,
                            node,
                        ),
                        _ => {}
                    }
                }
                self.recurse(node);
            }
            "new_expression" => {
                if let Some(constructor) = field(node, "constructor") {
                    match constructor.kind() {
                        "identifier" => {
                            self.add_ref(
                                self.text(constructor),
                                None,
                                false,
                                ReferenceKind::Type,
                                node,
                            );
                        }
                        "member_expression" => {
                            let qualifier = self.ftext(constructor, "object").and_then(simple_expr);
                            if let Some(name) = self.ftext(constructor, "property") {
                                self.add_ref(name, qualifier, true, ReferenceKind::Type, node);
                            }
                        }
                        _ => {}
                    }
                }
                self.recurse(node);
            }
            "import_statement" => {
                if let Some(source) = field(node, "source") {
                    let mut names = Vec::new();
                    self.js_specifiers(node, &mut names);
                    let path = unquote(self.text(source)).to_owned();
                    let relative = path.starts_with('.');
                    self.push_import(path, names, relative, node);
                }
            }
            "type_identifier" => self.type_ref(node),
            "nested_type_identifier" => {
                self.scoped_type_ref(node, field(node, "module"), field(node, "name"));
            }
            "jsx_opening_element" | "jsx_self_closing_element" => {
                if let Some(name) = field(node, "name")
                    && name.kind() == "identifier"
                    && self.text(name).starts_with(char::is_uppercase)
                {
                    self.add_ref(self.text(name), None, false, ReferenceKind::Type, node);
                }
                self.recurse(node);
            }
            "extends_clause" | "class_heritage" => {
                for child in named_kids(node) {
                    match child.kind() {
                        "identifier" => {
                            self.add_ref(self.text(child), None, false, ReferenceKind::Type, child);
                        }
                        "member_expression" => {
                            let qualifier = self.ftext(child, "object").and_then(simple_expr);
                            if let Some(name) = self.ftext(child, "property") {
                                self.add_ref(name, qualifier, true, ReferenceKind::Type, child);
                            }
                        }
                        _ => self.visit(child),
                    }
                }
            }
            _ => self.recurse(node),
        }
    }

    fn js_first_string_arg(&self, call: Node<'_>) -> Option<String> {
        let args = field(call, "arguments")?;
        let first = named_kids(args).into_iter().next()?;
        matches!(first.kind(), "string" | "template_string")
            .then(|| unquote(self.text(first)).to_owned())
    }

    fn js_specifiers(&self, node: Node<'_>, names: &mut Vec<String>) {
        for child in named_kids(node) {
            match child.kind() {
                "import_specifier" | "export_specifier" => {
                    if let Some(name) = field(child, "name") {
                        names.push(unquote(self.text(name)).to_owned());
                    }
                }
                "import_clause" | "named_imports" | "export_clause" => {
                    self.js_specifiers(child, names);
                }
                _ => {}
            }
        }
    }

    fn js_named(&mut self, node: Node<'t>, kind: SymbolKind, member_scope: bool) {
        let Some(name) = field(node, "name") else {
            return self.recurse(node);
        };
        let text = self.text(name);
        let vis = self.js_vis(node, text, false);
        let _ = member_scope;
        let def = Def::new(text, kind, node)
            .named(Some(name))
            .body(field(node, "body"))
            .vis(vis);
        self.define(def);
    }

    fn js_variables(&mut self, node: Node<'t>) {
        let exported = self.exported;
        let is_const = kids(node).iter().any(|c| c.kind() == "const");
        let declarators: Vec<_> = named_kids(node)
            .into_iter()
            .filter(|c| c.kind() == "variable_declarator")
            .collect();
        if declarators.len() != 1 {
            self.wrapper = None;
        } else if self.wrapper.is_none() {
            self.wrapper = Some(node);
        }
        for declarator in declarators {
            let name = field(declarator, "name").filter(|n| n.kind() == "identifier");
            let value = field(declarator, "value");
            let function_value = value.filter(|v| {
                matches!(
                    v.kind(),
                    "arrow_function" | "function_expression" | "function" | "generator_function"
                )
            });
            let class_value = value.filter(|v| matches!(v.kind(), "class"));
            let top_level = self.scope.is_empty();
            let Some(name) = name else {
                self.wrapper = None;
                self.visit(declarator);
                continue;
            };
            let text = self.text(name);
            let vis = if exported {
                Visibility::Public
            } else {
                Visibility::Private
            };
            if let Some(value) = function_value {
                let def = Def::new(text, SymbolKind::Function, declarator)
                    .named(Some(name))
                    .body(field(value, "body"))
                    .vis(vis);
                self.define(def);
            } else if class_value.is_some() {
                let def = Def::new(text, SymbolKind::Class, declarator)
                    .named(Some(name))
                    .body(value.and_then(|v| field(v, "body")))
                    .vis(vis);
                self.define(def);
            } else if top_level {
                let kind = if is_const {
                    SymbolKind::Constant
                } else {
                    SymbolKind::Variable
                };
                let def = Def::new(text, kind, declarator).named(Some(name)).vis(vis);
                self.emit(&def);
                self.recurse(declarator);
            } else {
                self.wrapper = None;
                self.visit(declarator);
            }
        }
        self.wrapper = None;
    }

    // ---- Go ----------------------------------------------------------------------------

    fn go_vis(name: &str) -> Visibility {
        if name.starts_with(char::is_uppercase) {
            Visibility::Public
        } else {
            Visibility::Private
        }
    }

    fn go_type_name(&self, node: Node<'_>) -> String {
        match node.kind() {
            "type_identifier" => self.text(node).to_owned(),
            "pointer_type" | "generic_type" | "parenthesized_type" => named_kids(node)
                .into_iter()
                .next()
                .map_or_else(String::new, |inner| self.go_type_name(inner)),
            _ => collapse(self.text(node)),
        }
    }

    fn go(&mut self, node: Node<'t>) {
        match node.kind() {
            "function_declaration" => {
                let Some(name) = field(node, "name") else {
                    return self.recurse(node);
                };
                let text = self.text(name);
                let def = Def::new(text, SymbolKind::Function, node)
                    .named(Some(name))
                    .body(field(node, "body"))
                    .vis(Self::go_vis(text));
                self.define(def);
            }
            "method_declaration" => {
                let Some(name) = field(node, "name") else {
                    return self.recurse(node);
                };
                let text = self.text(name);
                let receiver = field(node, "receiver")
                    .and_then(|r| named_kids(r).into_iter().find_map(|p| field(p, "type")))
                    .map(|t| self.go_type_name(t))
                    .filter(|t| !t.is_empty());
                let mut def = Def::new(text, SymbolKind::Method, node)
                    .named(Some(name))
                    .body(field(node, "body"))
                    .vis(Self::go_vis(text));
                if let Some(receiver) = receiver {
                    def.parent = self.out.symbols.iter().position(|s| {
                        s.parent.is_none()
                            && s.name == receiver
                            && matches!(
                                s.kind,
                                SymbolKind::Struct | SymbolKind::Interface | SymbolKind::TypeAlias
                            )
                    });
                    def.qualifier = Some(receiver);
                }
                self.define(def);
            }
            "type_declaration" => {
                let specs: Vec<_> = named_kids(node)
                    .into_iter()
                    .filter(|c| matches!(c.kind(), "type_spec" | "type_alias"))
                    .collect();
                for spec in &specs {
                    if specs.len() == 1 {
                        self.wrapper = Some(node);
                    }
                    self.go_type_spec(*spec);
                    self.wrapper = None;
                }
            }
            "const_declaration" | "var_declaration" if self.scope.is_empty() => {
                let kind = if node.kind() == "const_declaration" {
                    SymbolKind::Constant
                } else {
                    SymbolKind::Variable
                };
                let mut specs = Vec::new();
                for child in named_kids(node) {
                    if child.kind() == "var_spec_list" {
                        specs.extend(named_kids(child));
                    } else {
                        specs.push(child);
                    }
                }
                let specs: Vec<_> = specs
                    .into_iter()
                    .filter(|s| matches!(s.kind(), "const_spec" | "var_spec"))
                    .collect();
                for spec in &specs {
                    let names = fields(*spec, "name");
                    if specs.len() == 1 && names.len() == 1 {
                        self.wrapper = Some(node);
                    }
                    for name in names {
                        let text = self.text(name);
                        let def = Def::new(text, kind, *spec)
                            .named(Some(name))
                            .vis(Self::go_vis(text));
                        self.emit(&def);
                        self.wrapper = None;
                    }
                    self.recurse(*spec);
                }
            }
            "method_elem" => {
                let Some(name) = field(node, "name") else {
                    return self.recurse(node);
                };
                let text = self.text(name);
                let def = Def::new(text, SymbolKind::Method, node)
                    .named(Some(name))
                    .vis(Self::go_vis(text));
                self.define(def);
            }
            "call_expression" => {
                if let Some(function) = field(node, "function") {
                    match function.kind() {
                        "identifier" => self.call_ref(Some(function), None, false, node),
                        "selector_expression" => self.call_ref(
                            field(function, "field"),
                            field(function, "operand"),
                            false,
                            node,
                        ),
                        _ => {}
                    }
                }
                self.recurse(node);
            }
            "type_identifier" => self.type_ref(node),
            "qualified_type" => {
                self.scoped_type_ref(node, field(node, "package"), field(node, "name"));
            }
            "import_spec" => {
                if let Some(path) = self.ftext(node, "path") {
                    self.push_import(unquote(path).to_owned(), Vec::new(), false, node);
                }
            }
            _ => self.recurse(node),
        }
    }

    fn go_type_spec(&mut self, spec: Node<'t>) {
        let Some(name) = field(spec, "name") else {
            return self.recurse(spec);
        };
        let text = self.text(name);
        let ty = field(spec, "type");
        let (kind, keyword) = match ty.map(|t| t.kind()) {
            Some("struct_type") => (SymbolKind::Struct, Some("struct")),
            Some("interface_type") => (SymbolKind::Interface, Some("interface")),
            _ => (SymbolKind::TypeAlias, None),
        };
        let mut def = Def::new(text, kind, spec)
            .named(Some(name))
            .vis(Self::go_vis(text));
        if let (Some(ty), Some(keyword)) = (ty, keyword) {
            let start = self.wrapper.map_or(spec.start_byte(), |w| w.start_byte());
            let head = self.src.get(start..ty.start_byte()).unwrap_or("");
            let head = collapse(head);
            let head = if head.starts_with("type") {
                head
            } else {
                format!("type {head}")
            };
            def.signature = Some(format!("{head} {keyword}"));
        }
        self.define(def);
    }

    // ---- Java --------------------------------------------------------------------------

    fn java_modifiers(&self, node: Node<'_>) -> Vec<&'static str> {
        let mut found = Vec::new();
        for child in kids(node) {
            if child.kind() == "modifiers" {
                for modifier in kids(child) {
                    for word in ["public", "protected", "private", "static", "final"] {
                        if modifier.kind() == word {
                            found.push(word);
                        }
                    }
                }
            }
        }
        found
    }

    fn java_vis(&self, modifiers: &[&str]) -> Visibility {
        if modifiers.contains(&"public") {
            Visibility::Public
        } else if modifiers.contains(&"private") {
            Visibility::Private
        } else if modifiers.contains(&"protected") {
            Visibility::Restricted
        } else if self.top_kind() == Some(SymbolKind::Interface) {
            Visibility::Public
        } else {
            Visibility::Restricted
        }
    }

    fn java(&mut self, node: Node<'t>) {
        let kind = match node.kind() {
            "class_declaration" | "record_declaration" => Some(SymbolKind::Class),
            "interface_declaration" | "annotation_type_declaration" => Some(SymbolKind::Interface),
            "enum_declaration" => Some(SymbolKind::Enum),
            "method_declaration" | "constructor_declaration" => Some(SymbolKind::Method),
            "enum_constant" => Some(SymbolKind::Constant),
            _ => None,
        };
        if let Some(kind) = kind {
            let Some(name) = field(node, "name") else {
                return self.recurse(node);
            };
            let modifiers = self.java_modifiers(node);
            let vis = if kind == SymbolKind::Constant {
                Visibility::Public
            } else {
                self.java_vis(&modifiers)
            };
            let def = Def::new(self.text(name), kind, node)
                .named(Some(name))
                .body(field(node, "body"))
                .vis(vis);
            return self.define(def);
        }
        match node.kind() {
            "field_declaration" => {
                let modifiers = self.java_modifiers(node);
                if modifiers.contains(&"static") && modifiers.contains(&"final") {
                    let vis = self.java_vis(&modifiers);
                    let wrapper_ok = fields(node, "declarator").len() == 1;
                    for declarator in fields(node, "declarator") {
                        if let Some(name) = field(declarator, "name") {
                            if !wrapper_ok {
                                self.wrapper = None;
                            }
                            let def = Def::new(self.text(name), SymbolKind::Constant, node)
                                .named(Some(name))
                                .vis(vis);
                            self.emit(&def);
                        }
                    }
                }
                self.recurse(node);
            }
            "method_invocation" => {
                self.call_ref(field(node, "name"), field(node, "object"), false, node);
                self.recurse(node);
            }
            "type_identifier" => self.type_ref(node),
            "scoped_type_identifier" => {
                let full = collapse(self.text(node));
                if let Some((qualifier, name)) = full.rsplit_once('.') {
                    self.add_ref(
                        name,
                        simple_expr(qualifier),
                        true,
                        ReferenceKind::Type,
                        node,
                    );
                }
            }
            "import_declaration" => {
                let mut path = String::new();
                let mut wildcard = false;
                for child in kids(node) {
                    match child.kind() {
                        "scoped_identifier" | "identifier" => path = collapse(self.text(child)),
                        "asterisk" => wildcard = true,
                        _ => {}
                    }
                }
                let names = if wildcard {
                    path.push_str(".*");
                    Vec::new()
                } else {
                    path.rsplit('.')
                        .next()
                        .map(str::to_owned)
                        .into_iter()
                        .collect()
                };
                self.push_import(path, names, false, node);
            }
            _ => self.recurse(node),
        }
    }

    // ---- C -----------------------------------------------------------------------------

    fn c_declarator_name<'a>(&self, mut node: Node<'a>) -> Option<Node<'a>> {
        for _ in 0..32 {
            match node.kind() {
                "identifier" | "type_identifier" | "field_identifier" => return Some(node),
                "function_declarator"
                | "pointer_declarator"
                | "array_declarator"
                | "init_declarator" => node = field(node, "declarator")?,
                "parenthesized_declarator" => node = named_kids(node).into_iter().next()?,
                _ => return None,
            }
        }
        None
    }

    fn c_is_function_declarator(&self, mut node: Node<'_>) -> bool {
        for _ in 0..32 {
            match node.kind() {
                "function_declarator" => return true,
                "pointer_declarator" | "parenthesized_declarator" => {
                    match field(node, "declarator").or_else(|| named_kids(node).into_iter().next())
                    {
                        Some(inner) => node = inner,
                        None => return false,
                    }
                }
                _ => return false,
            }
        }
        false
    }

    fn c_vis(&self, node: Node<'_>) -> Visibility {
        let is_static = kids(node)
            .iter()
            .any(|c| c.kind() == "storage_class_specifier" && self.text(*c) == "static");
        if is_static {
            Visibility::Private
        } else {
            Visibility::Public
        }
    }

    fn c(&mut self, node: Node<'t>) {
        match node.kind() {
            "function_definition" => {
                let name = field(node, "declarator").and_then(|d| self.c_declarator_name(d));
                let Some(name) = name else {
                    return self.recurse(node);
                };
                let def = Def::new(self.text(name), SymbolKind::Function, node)
                    .named(Some(name))
                    .body(field(node, "body"))
                    .vis(self.c_vis(node));
                self.define(def);
            }
            "struct_specifier" | "union_specifier" | "enum_specifier" => {
                let kind = match node.kind() {
                    "struct_specifier" => SymbolKind::Struct,
                    "union_specifier" => SymbolKind::Union,
                    _ => SymbolKind::Enum,
                };
                match (field(node, "name"), field(node, "body")) {
                    (Some(name), Some(body)) => {
                        let def = Def::new(self.text(name), kind, node)
                            .named(Some(name))
                            .body(Some(body))
                            .vis(Visibility::Public);
                        self.define(def);
                    }
                    _ => self.recurse(node),
                }
            }
            "type_definition" => {
                let name = field(node, "declarator").and_then(|d| self.c_declarator_name(d));
                let Some(name) = name else {
                    return self.recurse(node);
                };
                let text = self.text(name);
                let ty = field(node, "type");
                let aggregate = ty.filter(|t| {
                    matches!(
                        t.kind(),
                        "struct_specifier" | "union_specifier" | "enum_specifier"
                    ) && field(*t, "body").is_some()
                });
                match aggregate {
                    Some(spec) => {
                        let spec_name = self.ftext(spec, "name");
                        if spec_name == Some(text) {
                            self.def_names.insert(name.id());
                            return self.recurse(node);
                        }
                        let kind = if spec_name.is_none() {
                            match spec.kind() {
                                "struct_specifier" => SymbolKind::Struct,
                                "union_specifier" => SymbolKind::Union,
                                _ => SymbolKind::Enum,
                            }
                        } else {
                            SymbolKind::TypeAlias
                        };
                        let def = Def::new(text, kind, node)
                            .named(Some(name))
                            .body(field(spec, "body"))
                            .vis(Visibility::Public);
                        self.define(def);
                    }
                    None => {
                        let def = Def::new(text, SymbolKind::TypeAlias, node)
                            .named(Some(name))
                            .vis(Visibility::Public);
                        self.define(def);
                    }
                }
            }
            "preproc_def" | "preproc_function_def" => {
                if let Some(name) = field(node, "name") {
                    let def = Def::new(self.text(name), SymbolKind::Macro, node)
                        .named(Some(name))
                        .vis(Visibility::Public);
                    self.emit(&def);
                }
            }
            "declaration" if self.scope.is_empty() => {
                let declarators = fields(node, "declarator");
                let is_const = kids(node)
                    .iter()
                    .any(|c| c.kind() == "type_qualifier" && self.text(*c) == "const");
                let single = declarators.len() == 1;
                for declarator in declarators {
                    let Some(name) = self.c_declarator_name(declarator) else {
                        continue;
                    };
                    let kind = if self.c_is_function_declarator(declarator) {
                        SymbolKind::Function
                    } else if is_const {
                        SymbolKind::Constant
                    } else {
                        SymbolKind::Variable
                    };
                    let _ = single;
                    let def = Def::new(self.text(name), kind, node)
                        .named(Some(name))
                        .vis(self.c_vis(node));
                    self.emit(&def);
                }
                self.recurse(node);
            }
            "call_expression" => {
                if let Some(function) = field(node, "function") {
                    match function.kind() {
                        "identifier" => self.call_ref(Some(function), None, false, node),
                        "field_expression" => self.call_ref(
                            field(function, "field"),
                            field(function, "argument"),
                            false,
                            node,
                        ),
                        _ => {}
                    }
                }
                self.recurse(node);
            }
            "type_identifier" => self.type_ref(node),
            "preproc_include" => {
                if let Some(path) = field(node, "path") {
                    let text = self.text(path).trim();
                    let relative = text.starts_with('"');
                    let cleaned = text.trim_matches(['"', '<', '>']).to_owned();
                    self.push_import(cleaned, Vec::new(), relative, node);
                }
            }
            _ => self.recurse(node),
        }
    }
}

fn python_docstring_of_statement(
    extractor: &Extractor<'_, '_>,
    statement: Node<'_>,
) -> Option<String> {
    if statement.kind() != "expression_statement" {
        return None;
    }
    let string = named_kids(statement).into_iter().next()?;
    if string.kind() != "string" {
        return None;
    }
    let doc = clean_python_string(extractor.text(string));
    (!doc.is_empty()).then_some(doc)
}

fn clean_python_string(text: &str) -> String {
    let text = text.trim_start_matches(|c: char| "rRuUbBfF".contains(c));
    let body = ["\"\"\"", "'''"]
        .iter()
        .find_map(|q| text.strip_prefix(q).and_then(|t| t.strip_suffix(q)))
        .or_else(|| {
            ['"', '\'']
                .iter()
                .find_map(|q| text.strip_prefix(*q).and_then(|t| t.strip_suffix(*q)))
        })
        .unwrap_or(text);
    dedent(body)
}

/// Mirrors `inspect.cleandoc`: first line trimmed, the rest dedented, blank edges dropped.
fn dedent(body: &str) -> String {
    let mut lines = body.lines();
    let first = lines.next().unwrap_or("").trim().to_owned();
    let rest: Vec<&str> = lines.collect();
    let indent = rest
        .iter()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.len() - l.trim_start().len())
        .min()
        .unwrap_or(0);
    let mut out = vec![first];
    out.extend(rest.iter().map(|l| {
        let cut = l.len().min(indent);
        l.get(cut..).unwrap_or("").trim_end().to_owned()
    }));
    out.join("\n").trim().to_owned()
}

/// Strips comment markers from one comment node.
fn clean_comment(text: &str) -> String {
    let text = text.trim();
    if let Some(block) = text.strip_prefix("/*") {
        let block = block.strip_suffix("*/").unwrap_or(block);
        let block = block.strip_prefix(['*', '!']).unwrap_or(block);
        let lines: Vec<String> = block
            .lines()
            .map(|line| {
                let line = line.trim_start();
                let line = line.strip_prefix('*').unwrap_or(line);
                line.strip_prefix(' ').unwrap_or(line).trim_end().to_owned()
            })
            .collect();
        return lines.join("\n").trim().to_owned();
    }
    let stripped = text
        .strip_prefix("///")
        .or_else(|| text.strip_prefix("//!"))
        .or_else(|| text.strip_prefix("//"))
        .unwrap_or(text);
    stripped
        .strip_prefix(' ')
        .unwrap_or(stripped)
        .trim_end()
        .to_owned()
}
