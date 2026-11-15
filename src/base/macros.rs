//! Internal macros shared by the `core` vocabulary types.

/// Declares a fieldless enum with a stable textual name and a stable numeric code per variant.
///
/// The textual name is used in configuration files, JSON payloads and `.contextpack` objects; the
/// numeric code is used in SQLite columns and in the compact in-memory graph. Both are part of the
/// on-disk format: never renumber or rename an existing variant, only append new ones.
macro_rules! string_enum {
    (
        $(#[$meta:meta])*
        $vis:vis enum $name:ident {
            $( $(#[$vmeta:meta])* $variant:ident = $code:literal => $text:literal ),+ $(,)?
        }
    ) => {
        $(#[$meta])*
        #[derive(
            Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord,
            ::serde::Serialize, ::serde::Deserialize,
        )]
        #[repr(u8)]
        $vis enum $name {
            $( $(#[$vmeta])* #[serde(rename = $text)] $variant = $code ),+
        }

        impl $name {
            /// Every variant, in declaration order.
            pub const ALL: &'static [$name] = &[ $( $name::$variant ),+ ];

            /// Stable textual name (configuration files, JSON, keys).
            pub const fn as_str(self) -> &'static str {
                match self { $( $name::$variant => $text ),+ }
            }

            /// Stable numeric code (SQLite columns, in-memory graph weights).
            pub const fn code(self) -> u8 {
                self as u8
            }

            /// Inverse of [`Self::code`].
            pub const fn from_code(code: u8) -> Option<Self> {
                match code {
                    $( $code => Some($name::$variant), )+
                    _ => None,
                }
            }
        }

        impl ::std::fmt::Display for $name {
            fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                f.write_str(self.as_str())
            }
        }

        impl ::std::str::FromStr for $name {
            type Err = $crate::base::Error;

            fn from_str(s: &str) -> ::std::result::Result<Self, Self::Err> {
                match s {
                    $( $text => Ok($name::$variant), )+
                    other => Err($crate::base::Error::Invalid(format!(
                        concat!("unknown ", stringify!($name), " `{}`"),
                        other
                    ))),
                }
            }
        }
    };
}

pub(crate) use string_enum;
