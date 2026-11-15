//! Format parsers that turn non-code sources into [`Unit`](crate::base::Unit)s: wiki snapshots with
//! comment threads, code-review exports, tracker exports, API approval portals, ownership files and
//! organisation models.
//!
//! Shared helpers:
//!
//! * [`mentions`] — candidate entity mentions in free text;
//! * [`party`] — people and teams as sources name them.

pub mod mentions;
pub mod party;
