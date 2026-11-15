//! Strongly typed identifiers.
//!
//! Two families exist:
//!
//! * **Row ids** ([`NodeId`], [`EdgeId`], [`UnitId`], [`ChunkId`]) are SQLite rowids. They are compact
//!   and fast but only meaningful inside one local database.
//! * **Natural keys** ([`NodeKey`], [`UnitKey`], [`SourceId`], [`DeviceId`]) are stable strings that
//!   mean the same thing on every device; they are what gets hashed, exported and synchronised.

use std::fmt;

use serde::{Deserialize, Serialize};

macro_rules! row_id {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(
            Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize,
        )]
        #[serde(transparent)]
        pub struct $name(pub i64);

        impl $name {
            /// Raw rowid.
            pub const fn get(self) -> i64 {
                self.0
            }
        }

        impl From<i64> for $name {
            fn from(value: i64) -> Self {
                Self(value)
            }
        }

        impl From<$name> for i64 {
            fn from(value: $name) -> Self {
                value.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(f)
            }
        }
    };
}

row_id!(
    /// Local rowid of a materialised graph node.
    NodeId
);
row_id!(
    /// Local rowid of a materialised graph edge.
    EdgeId
);
row_id!(
    /// Local rowid of an ingested unit.
    UnitId
);
row_id!(
    /// Local rowid of a text chunk.
    ChunkId
);

macro_rules! string_key {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            /// Wraps a string without validation.
            pub fn new(value: impl Into<String>) -> Self {
                Self(value.into())
            }

            /// Borrowed view.
            pub fn as_str(&self) -> &str {
                &self.0
            }

            /// Consumes the key and returns the inner string.
            pub fn into_string(self) -> String {
                self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                &self.0
            }
        }

        impl From<String> for $name {
            fn from(value: String) -> Self {
                Self(value)
            }
        }

        impl From<&str> for $name {
            fn from(value: &str) -> Self {
                Self(value.to_owned())
            }
        }

        impl From<&$name> for $name {
            fn from(value: &$name) -> Self {
                value.clone()
            }
        }
    };
}

string_key!(
    /// Canonical natural key of a graph node, e.g. `service:payments-api` or
    /// `symbol:payments:src/lib.rs#Ledger::charge`. Built with [`crate::base::keys`].
    NodeKey
);
string_key!(
    /// Natural key of an ingestion unit (one file, wiki page, ticket, API definition, ...).
    /// Built with [`crate::base::keys::unit`].
    UnitKey
);
string_key!(
    /// Identifier of a configured source (`[[sources]] id = "..."`). Also the sync namespace.
    SourceId
);
string_key!(
    /// Identifier of a device/installation, derived from its Ed25519 public key.
    DeviceId
);
