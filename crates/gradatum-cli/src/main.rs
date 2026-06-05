//! # gradatum-cli
//!
//! End-user CLI: read, write, search via the gradatum-server HTTP API.
//!
//! ## Status
//!
//! Scaffolding stub — implementation in Phase 2. See [`docs/PHASES.md`](https://github.com/gradatum/gradatum/blob/main/docs/PHASES.md).

fn main() {
    println!("gradatum-cli v{} — scaffolding", env!("CARGO_PKG_VERSION"));
    println!("Not yet implemented. See docs/PHASES.md for roadmap.");
}

#[cfg(test)]
mod tests {
    #[test]
    fn version_is_set() {
        assert!(!env!("CARGO_PKG_VERSION").is_empty());
    }
}
