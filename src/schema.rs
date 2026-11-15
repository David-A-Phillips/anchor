//! Registry of every module's SQLite schema history.
//!
//! Each module owns its tables and declares them as a `MIGRATIONS` constant next to the code that
//! uses them; [`crate::base::Db::open`] applies whatever is pending. Version numbers are per module,
//! so modules evolve their schemas independently.

use crate::base::MigrationSet;

/// All module schemas, applied in this order.
pub const ALL: &[MigrationSet] = &[
    MigrationSet {
        module: "store",
        migrations: crate::engine::store::MIGRATIONS,
    },
    MigrationSet {
        module: "embed",
        migrations: crate::engine::embed::MIGRATIONS,
    },
    MigrationSet {
        module: "metrics",
        migrations: crate::metrics::MIGRATIONS,
    },
    MigrationSet {
        module: "backlog",
        migrations: crate::backlog::MIGRATIONS,
    },
    MigrationSet {
        module: "sync",
        migrations: crate::sync::MIGRATIONS,
    },
];
