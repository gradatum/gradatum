//! `gradatum-admin acl` — read-only inspection of an ACL preset.
//!
//! The single sub-command, `acl validate --preset <path>`, loads a preset **the exact way
//! the server does at boot** — through [`gradatum_acl_policy::AclEngine::from_preset_str`],
//! which is the only path that reaches the removed-key guard. It is a go/no-go check to run
//! **before** switching a deployment to a version that removed a preset key: a preset that
//! still carries a removed key parses to an error here, loudly, instead of being silently
//! ignored and taking the server down to DENY-ALL at boot.
//!
//! It is strictly read-only: it opens no store, mints nothing, writes nothing. (`api-key
//! create` also reaches the parse, but it mints a key as a side effect, so it is not a
//! validation command.)

use std::path::{Path, PathBuf};

use clap::{Args, Subcommand};

use gradatum_acl_policy::{AclEngine, AclPreset};

/// `acl` sub-commands.
#[derive(Debug, Subcommand)]
pub enum AclCmd {
    /// Validates an ACL preset file without any side effect (no store, no mint, no write).
    Validate(AclValidateArgs),
}

/// Arguments for `acl validate`.
#[derive(Debug, Args)]
pub struct AclValidateArgs {
    /// Path to the ACL preset TOML file to validate (read-only).
    #[arg(long)]
    preset: PathBuf,
}

/// Dispatches an `acl` sub-command.
///
/// # Errors
///
/// Propagates the error of the selected sub-command (see `validate_preset`).
pub fn run(cmd: AclCmd) -> anyhow::Result<()> {
    match cmd {
        AclCmd::Validate(args) => validate_preset(&args.preset),
    }
}

/// Validates an ACL preset file.
///
/// Reads `preset`, parses it through [`AclEngine::from_preset_str`] (the same path the
/// server uses at boot, so a removed preset key is rejected here too), and, on success,
/// prints a one-line summary — number of consumers and number of `!` negation patterns.
///
/// # Errors
///
/// Returns an error if the file cannot be read, or if the preset does not parse (including
/// a preset that still carries a key removed in this version — the parse error names it).
/// The error is written to stderr by the caller and the process exits non-zero.
fn validate_preset(preset: &Path) -> anyhow::Result<()> {
    let content = std::fs::read_to_string(preset)
        .map_err(|e| anyhow::anyhow!("cannot read the ACL preset {}: {e}", preset.display()))?;

    // Validation: the same path the server takes at boot. `from_preset_str` is the only
    // entry that reaches the removed-key guard, so a preset still carrying a removed key
    // fails here, naming the key, instead of being silently ignored.
    AclEngine::from_preset_str(&content)
        .map_err(|e| anyhow::anyhow!("the ACL preset {} does not parse: {e}", preset.display()))?;

    // Summary (read-only). Re-parse the public deserialization struct to count consumers
    // and negation patterns; the parse cannot fail here since `from_preset_str` just
    // accepted the same content.
    let parsed: AclPreset = toml::from_str(&content)
        .map_err(|e| anyhow::anyhow!("the ACL preset {} does not parse: {e}", preset.display()))?;
    let consumers = parsed.consumer.len();
    let negations = parsed
        .consumer
        .iter()
        .flat_map(|c| c.read_patterns.iter().chain(c.write_patterns.iter()))
        .filter(|p| p.starts_with('!'))
        .count();
    println!(
        "ACL preset {} is valid — {consumers} consumer(s), {negations} negation pattern(s).",
        preset.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// (a) The shipped hierarchical preset (post-F-297) validates cleanly.
    #[test]
    fn shipped_hierarchical_preset_is_valid() {
        let preset = Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/presets/hierarchical.toml"
        ));
        validate_preset(preset).expect("the shipped hierarchical preset must validate");
    }

    /// (b) A preset still carrying the removed `sees_personal_classified` key is rejected,
    /// and the error names the key.
    #[test]
    fn preset_with_removed_key_is_rejected_and_names_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("bearer.toml");
        let mut f = std::fs::File::create(&path).expect("create preset");
        writeln!(
            f,
            "[[consumer]]\nidentity = \"test\"\nread_patterns = [\"**\"]\nwrite_patterns = [\"**\"]\nsees_personal_classified = false\n"
        )
        .expect("write preset");
        let err =
            validate_preset(&path).expect_err("a preset carrying the removed key must be rejected");
        assert!(
            err.to_string().contains("sees_personal_classified"),
            "the error must name the removed key: {err}"
        );
    }

    /// (c) A missing preset file is an error.
    #[test]
    fn missing_preset_file_is_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("does-not-exist.toml");
        validate_preset(&path).expect_err("a missing preset file must be an error");
    }
}
