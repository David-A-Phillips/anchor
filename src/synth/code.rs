//! Source-code emitters: plausible modules, types and functions in six languages.
//!
//! Every service is one chain of functions: the entry function of the root file calls the first
//! function of the root file, each function calls the next one (the last one of a file calls the
//! first function of the next file), so call and import edges exist inside a file, across files
//! and, through the entry points of other services, across repositories. The chain is acyclic.

use std::fmt::Write as _;

use super::plan::{Plan, Service, capitalise};
use super::spec::{SynthLanguage, SynthSpec};
use super::words;

/// One generated file: repository-relative path and contents.
pub(crate) struct CodeFile {
    pub path: String,
    pub content: String,
}

macro_rules! w {
    ($out:expr) => {{ let _ = writeln!($out); }};
    ($out:expr, $($arg:tt)*) => {{ let _ = writeln!($out, $($arg)*); }};
}

fn pascal(ident: &str) -> String {
    ident.split(['_', '-']).map(capitalise).collect()
}

fn camel(ident: &str) -> String {
    let p = pascal(ident);
    let mut chars = p.chars();
    chars
        .next()
        .map(|c| c.to_ascii_lowercase().to_string() + chars.as_str())
        .unwrap_or_default()
}

/// Name of the entry function other services call.
pub(crate) fn entry_name(service: &Service) -> String {
    match service.language {
        SynthLanguage::Rust | SynthLanguage::Python | SynthLanguage::C => {
            format!("{}_entry", service.ident)
        }
        SynthLanguage::TypeScript | SynthLanguage::Java => {
            format!("{}Entry", camel(&service.ident))
        }
        SynthLanguage::Go => format!("{}Entry", pascal(&service.ident)),
    }
}

/// Repository-relative path of source file `index` (0 is the root file).
pub(crate) fn source_path(service: &Service, index: usize) -> String {
    let module = &service.modules[index];
    match (service.language, index) {
        (SynthLanguage::Rust, 0) => "src/lib.rs".to_owned(),
        (SynthLanguage::Rust, _) => format!("src/{module}.rs"),
        (SynthLanguage::Python, 0) => format!("{}/__init__.py", service.ident),
        (SynthLanguage::Python, _) => format!("{}/{module}.py", service.ident),
        (SynthLanguage::TypeScript, 0) => "src/index.ts".to_owned(),
        (SynthLanguage::TypeScript, _) => format!("src/{module}.ts"),
        (SynthLanguage::Go, 0) => format!("{}.go", service.ident),
        (SynthLanguage::Go, _) => format!("{module}.go"),
        (SynthLanguage::Java, _) => format!(
            "src/main/java/acme/{}/{}.java",
            service.ident,
            java_class(service, index)
        ),
        (SynthLanguage::C, 0) => format!("src/{}.c", service.ident),
        (SynthLanguage::C, _) => format!("src/{module}.c"),
    }
}

fn java_class(service: &Service, index: usize) -> String {
    if index == 0 {
        pascal(&service.ident)
    } else {
        format!("{}Part", pascal(&service.modules[index]))
    }
}

/// `(verb, noun)` of every function of every file, unique within the service.
fn function_names(service: &Service, spec: &SynthSpec, salt: usize) -> Vec<Vec<(String, String)>> {
    let per_file = spec.symbols_per_file.max(1);
    let verbs = words::VERBS.len();
    let nouns = words::NOUNS.len();
    (0..service.modules.len())
        .map(|file| {
            (0..per_file)
                .map(|j| {
                    let idx = file * per_file + j + salt;
                    let verb = words::VERBS[idx % verbs];
                    let mut noun = words::NOUNS[(idx / verbs + idx) % nouns].to_owned();
                    if idx - salt >= verbs * nouns {
                        noun.push_str(&idx.to_string());
                    }
                    (verb.to_owned(), noun)
                })
                .collect()
        })
        .collect()
}

fn fn_name(language: SynthLanguage, (verb, noun): &(String, String)) -> String {
    match language {
        SynthLanguage::Rust | SynthLanguage::Python | SynthLanguage::C => format!("{verb}_{noun}"),
        SynthLanguage::TypeScript | SynthLanguage::Java => {
            format!("{verb}{}", capitalise(noun))
        }
        SynthLanguage::Go => format!("{}{}", capitalise(verb), capitalise(noun)),
    }
}

struct Ctx<'a> {
    service: &'a Service,
    component: &'a str,
    names: Vec<Vec<String>>,
    callees: Vec<&'a Service>,
    files: usize,
}

impl Ctx<'_> {
    fn doc(&self, file: usize, j: usize, raw: &(String, String)) -> String {
        let _ = (file, j);
        format!(
            "{}s the {} for the {} service ({} component).",
            capitalise(&raw.0),
            raw.1,
            self.service.slug,
            self.component
        )
    }

    /// The function the `j`-th function of `file` calls, as `(file, name)`.
    fn next_of(&self, file: usize, j: usize) -> Option<(usize, &str)> {
        if j + 1 < self.names[file].len() {
            Some((file, self.names[file][j + 1].as_str()))
        } else if file + 1 < self.files {
            Some((file + 1, self.names[file + 1][0].as_str()))
        } else {
            None
        }
    }
}

/// All source files of a service.
pub(crate) fn source_files(
    plan: &Plan,
    service_index: usize,
    spec: &SynthSpec,
    salt: usize,
) -> Vec<CodeFile> {
    let service = &plan.services[service_index];
    let raw = function_names(service, spec, salt);
    let ctx = Ctx {
        service,
        component: service
            .component
            .map_or("unassigned", |c| plan.components[c].name.as_str()),
        names: raw
            .iter()
            .map(|file| file.iter().map(|n| fn_name(service.language, n)).collect())
            .collect(),
        callees: service.calls.iter().map(|&c| &plan.services[c]).collect(),
        files: service.modules.len(),
    };
    (0..ctx.files)
        .map(|index| {
            let docs: Vec<String> = raw[index]
                .iter()
                .enumerate()
                .map(|(j, n)| ctx.doc(index, j, n))
                .collect();
            let content = match service.language {
                SynthLanguage::Rust => rust(&ctx, index, &docs),
                SynthLanguage::Python => python(&ctx, index, &docs),
                SynthLanguage::TypeScript => typescript(&ctx, index, &docs),
                SynthLanguage::Go => go(&ctx, index, &docs),
                SynthLanguage::Java => java(&ctx, index, &docs),
                SynthLanguage::C => c(&ctx, index, &docs),
            };
            CodeFile {
                path: source_path(service, index),
                content,
            }
        })
        .collect()
}

fn module_doc(ctx: &Ctx, index: usize) -> String {
    format!(
        "The {} module of the {} service: {} logic for the {} component.",
        ctx.service.modules[index], ctx.service.slug, ctx.service.modules[index], ctx.component
    )
}

fn rust(ctx: &Ctx, index: usize, docs: &[String]) -> String {
    let mut out = String::new();
    let module = &ctx.service.modules[index];
    w!(out, "//! {}", module_doc(ctx, index));
    w!(out);
    if index == 0 {
        for other in &ctx.service.modules[1..] {
            w!(out, "pub mod {other};");
        }
        w!(out);
    }
    let last = ctx.names[index].len() - 1;
    if let Some((file, name)) = ctx.next_of(index, last)
        && file != index
    {
        w!(out, "use crate::{}::{name};", ctx.service.modules[file]);
    }
    if index == 0 {
        for callee in &ctx.callees {
            w!(out, "use {}::{};", callee.ident, entry_name(callee));
        }
    }
    w!(out);
    let ty = format!("{}Record", pascal(module));
    w!(
        out,
        "/// A {module} record owned by the {} service.",
        ctx.service.slug
    );
    w!(out, "#[derive(Debug, Clone)]");
    w!(out, "pub struct {ty} {{");
    w!(out, "    pub id: u64,");
    w!(out, "    pub label: String,");
    w!(out, "}}");
    w!(out);
    w!(out, "impl {ty} {{");
    w!(out, "    /// Creates a {module} record.");
    w!(out, "    pub fn new(id: u64) -> Self {{");
    w!(
        out,
        "        Self {{ id, label: String::from(\"{}\") }}",
        ctx.service.slug
    );
    w!(out, "    }}");
    w!(out, "}}");
    for (j, name) in ctx.names[index].iter().enumerate() {
        w!(out);
        w!(out, "/// {}", docs[j]);
        w!(out, "pub fn {name}(input: u64) -> u64 {{");
        match ctx.next_of(index, j) {
            Some((_, next)) => {
                w!(out, "    let base = {next}(input);");
                w!(out, "    base.wrapping_add({})", j + 1);
            }
            None => w!(out, "    input.wrapping_add({})", j + 1),
        }
        w!(out, "}}");
    }
    if index == 0 {
        w!(out);
        w!(out, "/// Entry point of the {} service.", ctx.service.slug);
        w!(
            out,
            "pub fn {}(input: u64) -> u64 {{",
            entry_name(ctx.service)
        );
        w!(out, "    let mut acc = {}(input);", ctx.names[0][0]);
        for callee in &ctx.callees {
            w!(
                out,
                "    acc = acc.wrapping_add({}(acc));",
                entry_name(callee)
            );
        }
        w!(out, "    acc");
        w!(out, "}}");
    }
    out
}

fn python(ctx: &Ctx, index: usize, docs: &[String]) -> String {
    let mut out = String::new();
    let module = &ctx.service.modules[index];
    w!(out, "\"\"\"{}\"\"\"", module_doc(ctx, index));
    w!(out);
    let last = ctx.names[index].len() - 1;
    if let Some((file, name)) = ctx.next_of(index, last)
        && file != index
    {
        w!(out, "from .{} import {name}", ctx.service.modules[file]);
    }
    if index == 0 {
        for callee in &ctx.callees {
            w!(out, "from {} import {}", callee.ident, entry_name(callee));
        }
    }
    w!(out);
    w!(out);
    w!(out, "class {}Record:", pascal(module));
    w!(
        out,
        "    \"\"\"A {module} record owned by the {} service.\"\"\"",
        ctx.service.slug
    );
    w!(out);
    w!(out, "    def __init__(self, ident: int) -> None:");
    w!(out, "        self.ident = ident");
    w!(out, "        self.label = \"{}\"", ctx.service.slug);
    for (j, name) in ctx.names[index].iter().enumerate() {
        w!(out);
        w!(out);
        w!(out, "def {name}(value: int) -> int:");
        w!(out, "    \"\"\"{}\"\"\"", docs[j]);
        match ctx.next_of(index, j) {
            Some((_, next)) => {
                w!(out, "    base = {next}(value)");
                w!(out, "    return base + {}", j + 1);
            }
            None => w!(out, "    return value + {}", j + 1),
        }
    }
    if index == 0 {
        w!(out);
        w!(out);
        w!(out, "def {}(value: int) -> int:", entry_name(ctx.service));
        w!(
            out,
            "    \"\"\"Entry point of the {} service.\"\"\"",
            ctx.service.slug
        );
        w!(out, "    acc = {}(value)", ctx.names[0][0]);
        for callee in &ctx.callees {
            w!(out, "    acc = acc + {}(acc)", entry_name(callee));
        }
        w!(out, "    return acc");
    }
    out
}

fn typescript(ctx: &Ctx, index: usize, docs: &[String]) -> String {
    let mut out = String::new();
    let module = &ctx.service.modules[index];
    w!(out, "/** {} */", module_doc(ctx, index));
    let last = ctx.names[index].len() - 1;
    if let Some((file, name)) = ctx.next_of(index, last)
        && file != index
    {
        w!(
            out,
            "import {{ {name} }} from \"./{}\";",
            ctx.service.modules[file]
        );
    }
    if index == 0 {
        for callee in &ctx.callees {
            w!(
                out,
                "import {{ {} }} from \"{}\";",
                entry_name(callee),
                callee.slug
            );
        }
    }
    w!(out);
    w!(
        out,
        "/** A {module} record owned by the {} service. */",
        ctx.service.slug
    );
    w!(out, "export class {}Record {{", pascal(module));
    w!(out, "  readonly label: string = \"{}\";", ctx.service.slug);
    w!(out);
    w!(out, "  constructor(readonly id: number) {{}}");
    w!(out, "}}");
    for (j, name) in ctx.names[index].iter().enumerate() {
        w!(out);
        w!(out, "/** {} */", docs[j]);
        w!(out, "export function {name}(value: number): number {{");
        match ctx.next_of(index, j) {
            Some((_, next)) => {
                w!(out, "  const base = {next}(value);");
                w!(out, "  return base + {};", j + 1);
            }
            None => w!(out, "  return value + {};", j + 1),
        }
        w!(out, "}}");
    }
    if index == 0 {
        w!(out);
        w!(
            out,
            "/** Entry point of the {} service. */",
            ctx.service.slug
        );
        w!(
            out,
            "export function {}(value: number): number {{",
            entry_name(ctx.service)
        );
        w!(out, "  let acc = {}(value);", ctx.names[0][0]);
        for callee in &ctx.callees {
            w!(out, "  acc = acc + {}(acc);", entry_name(callee));
        }
        w!(out, "  return acc;");
        w!(out, "}}");
    }
    out
}

fn go(ctx: &Ctx, index: usize, docs: &[String]) -> String {
    let mut out = String::new();
    let module = &ctx.service.modules[index];
    w!(
        out,
        "// Package {} is the {} service.",
        ctx.service.ident,
        ctx.service.slug
    );
    w!(out, "// {}", module_doc(ctx, index));
    w!(out, "package {}", ctx.service.ident);
    w!(out);
    if index == 0 && !ctx.callees.is_empty() {
        w!(out, "import (");
        for callee in &ctx.callees {
            w!(out, "\t\"acme.example/{}\"", callee.slug);
        }
        w!(out, ")");
        w!(out);
    }
    w!(
        out,
        "// {}Record is a {module} record owned by the {} service.",
        pascal(module),
        ctx.service.slug
    );
    w!(out, "type {}Record struct {{", pascal(module));
    w!(out, "\tID    int");
    w!(out, "\tLabel string");
    w!(out, "}}");
    for (j, name) in ctx.names[index].iter().enumerate() {
        w!(out);
        w!(out, "// {name} {}", docs[j].to_lowercase());
        w!(out, "func {name}(value int) int {{");
        match ctx.next_of(index, j) {
            Some((_, next)) => {
                w!(out, "\tbase := {next}(value)");
                w!(out, "\treturn base + {}", j + 1);
            }
            None => w!(out, "\treturn value + {}", j + 1),
        }
        w!(out, "}}");
    }
    if index == 0 {
        w!(out);
        let entry = entry_name(ctx.service);
        w!(
            out,
            "// {entry} is the entry point of the {} service.",
            ctx.service.slug
        );
        w!(out, "func {entry}(value int) int {{");
        w!(out, "\tacc := {}(value)", ctx.names[0][0]);
        for callee in &ctx.callees {
            w!(
                out,
                "\tacc = acc + {}.{}(acc)",
                callee.ident,
                entry_name(callee)
            );
        }
        w!(out, "\treturn acc");
        w!(out, "}}");
    }
    out
}

fn java(ctx: &Ctx, index: usize, docs: &[String]) -> String {
    let mut out = String::new();
    let class = java_class(ctx.service, index);
    w!(out, "package acme.{};", ctx.service.ident);
    w!(out);
    if index == 0 {
        for callee in &ctx.callees {
            w!(
                out,
                "import acme.{}.{};",
                callee.ident,
                java_class(callee, 0)
            );
        }
        if !ctx.callees.is_empty() {
            w!(out);
        }
    }
    w!(out, "/** {} */", module_doc(ctx, index));
    w!(out, "public final class {class} {{");
    w!(out, "    private final int id;");
    w!(out);
    w!(
        out,
        "    /** Creates a {} record. */",
        ctx.service.modules[index]
    );
    w!(out, "    public {class}(int id) {{");
    w!(out, "        this.id = id;");
    w!(out, "    }}");
    w!(out);
    w!(out, "    /** The record id. */");
    w!(out, "    public int id() {{");
    w!(out, "        return id;");
    w!(out, "    }}");
    for (j, name) in ctx.names[index].iter().enumerate() {
        w!(out);
        w!(out, "    /** {} */", docs[j]);
        w!(out, "    public static int {name}(int value) {{");
        match ctx.next_of(index, j) {
            Some((file, next)) => {
                let target = if file == index {
                    String::new()
                } else {
                    format!("{}.", java_class(ctx.service, file))
                };
                w!(out, "        int base = {target}{next}(value);");
                w!(out, "        return base + {};", j + 1);
            }
            None => w!(out, "        return value + {};", j + 1),
        }
        w!(out, "    }}");
    }
    if index == 0 {
        w!(out);
        w!(
            out,
            "    /** Entry point of the {} service. */",
            ctx.service.slug
        );
        w!(
            out,
            "    public static int {}(int value) {{",
            entry_name(ctx.service)
        );
        w!(out, "        int acc = {}(value);", ctx.names[0][0]);
        for callee in &ctx.callees {
            w!(
                out,
                "        acc = acc + {}.{}(acc);",
                java_class(callee, 0),
                entry_name(callee)
            );
        }
        w!(out, "        return acc;");
        w!(out, "    }}");
    }
    w!(out, "}}");
    out
}

fn c(ctx: &Ctx, index: usize, docs: &[String]) -> String {
    let mut out = String::new();
    let module = &ctx.service.modules[index];
    w!(out, "/** {} */", module_doc(ctx, index));
    let last = ctx.names[index].len() - 1;
    if let Some((file, name)) = ctx.next_of(index, last)
        && file != index
    {
        w!(out, "extern int {name}(int value);");
    }
    if index == 0 {
        for callee in &ctx.callees {
            w!(out, "extern int {}(int value);", entry_name(callee));
        }
    }
    w!(out);
    w!(
        out,
        "/** A {module} record owned by the {} service. */",
        ctx.service.slug
    );
    w!(out, "struct {module}_record {{");
    w!(out, "    int id;");
    w!(out, "    const char *label;");
    w!(out, "}};");
    for (j, name) in ctx.names[index].iter().enumerate() {
        w!(out);
        w!(out, "/** {} */", docs[j]);
        w!(out, "int {name}(int value) {{");
        match ctx.next_of(index, j) {
            Some((_, next)) => {
                w!(out, "    int base = {next}(value);");
                w!(out, "    return base + {};", j + 1);
            }
            None => w!(out, "    return value + {};", j + 1),
        }
        w!(out, "}}");
    }
    if index == 0 {
        w!(out);
        w!(
            out,
            "/** Entry point of the {} service. */",
            ctx.service.slug
        );
        w!(out, "int {}(int value) {{", entry_name(ctx.service));
        w!(out, "    int acc = {}(value);", ctx.names[0][0]);
        for callee in &ctx.callees {
            w!(out, "    acc = acc + {}(acc);", entry_name(callee));
        }
        w!(out, "    return acc;");
        w!(out, "}}");
    }
    out
}
