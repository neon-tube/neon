//! The warnings the compiler can emit, as a type.
//!
//! One enum, three consumers: `expand`'s `@allow` processor validates a suppression
//! name against it, the checker stamps each `Warning` with its variant, and an editor
//! front end can key quick-fixes off the variant rather than parsing message text.
//! Adding a lint means adding a variant, and the compiler then insists on a spelling
//! for it (`name`) and a place in the registry (`ALL`) — there is no string to let
//! drift.

use crate::ast::{AnnArg, Annotation};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lint {
    /// A read of a list placed after the `list::set` that consumed it, inside a loop:
    /// the old list stays live, so every iteration clones. See `check.rs`'s
    /// `stale_write_lint`.
    StaleWrite,
}

/// Every lint. The `@allow` error message enumerates this, so a new variant is
/// user-discoverable the day it lands.
pub const ALL: &[Lint] = &[Lint::StaleWrite];

impl Lint {
    /// The name `@allow` suppresses this lint under.
    pub fn name(self) -> &'static str {
        match self {
            Lint::StaleWrite => "stale_write",
        }
    }

    pub fn from_name(name: &str) -> Option<Lint> {
        ALL.iter().copied().find(|l| l.name() == name)
    }
}

/// Whether `@allow(<lint>)` on this declaration suppresses `lint`. The names were
/// validated by `expand` — a misspelling is a compile error there — so an argument that
/// fails to parse here is simply not a suppression.
pub fn allows(annotations: &[Annotation], lint: Lint) -> bool {
    annotations.iter().any(|a| {
        a.name == "allow"
            && a.args.iter().any(|arg| match arg {
                AnnArg::Item { path, args, .. } if args.is_empty() => match path.as_slice() {
                    [one] => Lint::from_name(one) == Some(lint),
                    _ => false,
                },
                _ => false,
            })
    })
}
