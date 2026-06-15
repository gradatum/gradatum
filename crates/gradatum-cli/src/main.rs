//! # gradatum-cli
//!
//! End-user CLI: read, write, search via the gradatum-server HTTP API.
//!
//! ## Status
//!
//! Not yet implemented — planned for a future release.

fn main() {
    eprintln!("gradatum-cli: not yet implemented");
    std::process::exit(1);
}

#[cfg(test)]
mod tests {
    #[test]
    fn version_is_set() {
        assert!(!env!("CARGO_PKG_VERSION").is_empty());
    }
}
