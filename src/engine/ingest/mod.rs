//! Format parsers that turn non-code sources into [`Unit`]s: wiki snapshots with comment threads,
//! code-review exports, tracker exports, API approval portals, ownership files and organisation
//! models.
//!
//! Shared helpers:
//!
//! * [`mentions`] — candidate entity mentions in free text;
//! * [`party`] — people and teams as sources name them;
//! * [`IngestOutput`] / [`IngestWarning`] — what every parser returns.

pub mod api_portal;
pub mod docs;
pub mod jira;
pub mod mentions;
pub mod org;
pub mod ownership;
pub mod party;
pub mod review;
pub mod wiki;

use crate::base::Unit;

/// A non-fatal problem found while ingesting one item of a source. Parsers never abort a whole
/// source because one page, ticket or record is malformed: they skip or partially ingest the item
/// and report why.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IngestWarning {
    /// Where the problem is: a path relative to the source root, or an item identifier.
    pub location: String,
    /// What is wrong, in a form a user can act on.
    pub message: String,
}

impl IngestWarning {
    /// Creates a warning.
    pub fn new(location: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            location: location.into(),
            message: message.into(),
        }
    }
}

impl std::fmt::Display for IngestWarning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.location, self.message)
    }
}

/// What ingesting a source (or one document of it) produced.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct IngestOutput {
    /// Units in canonical form, ready to apply.
    pub units: Vec<Unit>,
    /// Problems that did not prevent ingestion.
    pub warnings: Vec<IngestWarning>,
}

impl IngestOutput {
    /// An output holding one unit and no warnings.
    pub fn single(unit: Unit) -> Self {
        Self {
            units: vec![unit],
            warnings: Vec::new(),
        }
    }

    /// Records a warning.
    pub fn warn(&mut self, location: impl Into<String>, message: impl Into<String>) {
        self.warnings.push(IngestWarning::new(location, message));
    }

    /// Appends the units and warnings of another output.
    pub fn extend(&mut self, other: IngestOutput) {
        self.units.extend(other.units);
        self.warnings.extend(other.warnings);
    }

    /// `true` when nothing was produced and nothing went wrong.
    pub fn is_empty(&self) -> bool {
        self.units.is_empty() && self.warnings.is_empty()
    }
}
