//! Builds the C runtime and publishes its location; see `build.rs`.
//!
//! Intentionally empty: nothing in Rust links or calls the runtime. `cc` links
//! it into generated programs. Cargo just requires a target to exist.
