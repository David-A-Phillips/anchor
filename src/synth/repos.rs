//! `repos/<service>/`: source code, README, ownership files and OpenAPI definitions.

use std::fmt::Write as _;

use serde_json::{Value, json};

use super::plan::{Owner, OwnershipStyle, Service};
use super::rng::Rng;
use super::{Ctx, code};
use crate::base::Result;

/// An OpenAPI 3 document for `service`'s API.
pub(crate) fn openapi(service: &Service) -> Option<Value> {
    let api = service.api.as_ref()?;
    let mut paths = serde_json::Map::new();
    for op in &api.operations {
        let entry = paths
            .entry(op.route.clone())
            .or_insert_with(|| Value::Object(serde_json::Map::new()));
        let mut operation = json!({
            "operationId": op.id,
            "summary": op.summary,
            "responses": { "200": { "description": "Success" } },
        });
        if op.route.contains("{id}") {
            operation["parameters"] = json!([{
                "name": "id", "in": "path", "required": true, "schema": { "type": "string" }
            }]);
        }
        if let Some(object) = entry.as_object_mut() {
            object.insert(op.method.to_ascii_lowercase(), operation);
        }
    }
    Some(json!({
        "openapi": "3.0.3",
        "info": {
            "title": api.title,
            "version": "1.0.0",
            "description": format!("HTTP API of the {} service.", service.slug),
        },
        "paths": paths,
    }))
}

pub(crate) fn emit(ctx: &mut Ctx) -> Result<()> {
    let plan = ctx.plan;
    for (index, service) in plan.services.iter().enumerate() {
        let base = format!("repos/{}", service.slug);
        let component = service
            .component
            .map_or("no component", |c| plan.components[c].name.as_str());

        let mut readme = format!("# {}\n\n", service.slug);
        let _ = writeln!(
            readme,
            "The `{}` service belongs to the {component} component. Entry point: `{}`.",
            service.slug,
            code::entry_name(service)
        );
        if let Some(api) = &service.api {
            let _ = writeln!(
                readme,
                "\n## API\n\n{} is documented in `openapi.yaml`.",
                api.title
            );
            for op in &api.operations {
                let _ = writeln!(readme, "- `{} {}`: {}", op.method, op.route, op.summary);
            }
        }
        if !service.calls.is_empty() {
            let _ = writeln!(readme, "\n## Dependencies\n");
            for &callee in &service.calls {
                let _ = writeln!(readme, "- calls `{}`", plan.services[callee].slug);
            }
        }
        let _ = writeln!(
            readme,
            "\nOwnership is declared in the repository's ownership file."
        );
        ctx.out
            .write("repos", &format!("{base}/README.md"), &readme)?;

        let mut rng = Rng::derive(ctx.spec.seed, "code", index as u64);
        let salt = rng.below(64);
        for file in code::source_files(plan, index, ctx.spec, salt) {
            ctx.out
                .write("repos", &format!("{base}/{}", file.path), &file.content)?;
            *ctx.report
                .source_files_by_language
                .entry(format!("{:?}", service.language))
                .or_insert(0) += 1;
            ctx.report.source_files += 1;
        }

        if let Some(document) = openapi(service) {
            let text = serde_yaml_ng::to_string(&document)
                .map_err(|err| crate::base::Error::invalid(err.to_string()))?;
            ctx.out
                .write("repos", &format!("{base}/openapi.yaml"), &text)?;
        }

        match (service.style, service.owner) {
            (OwnershipStyle::CodeOwners, Some(Owner::Team(team))) => {
                let mut text = format!("# Code owners of {}\n", service.slug);
                let _ = writeln!(text, "*\t@acme/{}", plan.teams[team].slug);
                if let Some((file, person)) = service.special_file {
                    let _ = writeln!(
                        text,
                        "/{}\t@{}",
                        code::source_path(service, file),
                        plan.people[person].handle
                    );
                }
                ctx.out
                    .write("repos", &format!("{base}/CODEOWNERS"), &text)?;
            }
            (OwnershipStyle::Maintainers, Some(Owner::Person(person))) => {
                let who = &plan.people[person];
                let text = format!(
                    "Maintainers of {slug}\n\nEntries are matched against file paths.\n\n{title}\nM:\t{name} <{email}>\nS:\tMaintained\nF:\t*\n",
                    slug = service.slug,
                    title = service.slug.to_uppercase().replace('-', " "),
                    name = who.name,
                    email = who.email,
                );
                ctx.out
                    .write("repos", &format!("{base}/MAINTAINERS"), &text)?;
            }
            (OwnershipStyle::Owners, Some(Owner::Person(person))) => {
                let text = format!("approvers:\n- {}\n", plan.people[person].handle);
                ctx.out.write("repos", &format!("{base}/OWNERS"), &text)?;
            }
            _ => {}
        }
    }
    Ok(())
}
