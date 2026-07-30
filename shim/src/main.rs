//! PingMyBell hook shim — pre-release placeholder.
//!
//! Invariant (PRD AC-2.4): the shim always fails open — exit 0, no stdout —
//! so a broken or absent PingMyBell can never block or alter agent behavior.

fn main() {
    // Placeholder: no event handling yet. Exit 0 with no output.
}
