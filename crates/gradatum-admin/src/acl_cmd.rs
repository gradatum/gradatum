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
//!
//! Beyond the parse, the command reports on stderr every consumer whose `read_patterns` hold
//! no positive pattern. Such a consumer is valid, boots, and then denies every read, which no
//! exit code can express: the warning is the only signal, and it never changes the verdict.

use std::io::Write;
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
        AclCmd::Validate(args) => validate_preset(&args.preset, &mut std::io::stderr()),
    }
}

/// Validates an ACL preset file.
///
/// Reads `preset`, parses it through [`AclEngine::from_preset_str`] (the same path the
/// server uses at boot, so a removed preset key is rejected here too), and, on success,
/// prints a one-line summary — number of consumers and number of `!` negation patterns —
/// followed, on `warnings`, by one line per consumer that holds no positive read pattern.
///
/// The warnings never change the outcome: a report that a consumer reads nothing is not a
/// reason to refuse a preset the engine loads happily. They exist because that state is
/// otherwise invisible — see [`consumers_with_no_positive_read_pattern`].
///
/// # Errors
///
/// Returns an error if the file cannot be read, or if the preset does not parse (including
/// a preset that still carries a key removed in this version — the parse error names it).
/// The error is written to stderr by the caller and the process exits non-zero.
fn validate_preset(preset: &Path, warnings: &mut dyn Write) -> anyhow::Result<()> {
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

    // A preset that loads and then denies a consumer every read is the failure this command
    // exists to catch before a rollout, and nothing else in its output would show it.
    for identity in consumers_with_no_positive_read_pattern(&parsed) {
        writeln!(
            warnings,
            "warning: consumer '{identity}' has no positive read pattern — it reads nothing"
        )?;
    }
    Ok(())
}

/// Identities of the consumers that hold no positive read pattern, in preset order.
///
/// A pattern is *positive* when it does not start with `!` and is not blank. The engine splits
/// `read_patterns` into an allow set — every non-negated entry — and a deny set (the `!`-prefixed
/// ones), and admits a read on a match in the allow set alone. A consumer whose allow set is
/// empty, or holds only a pattern no locus matches, therefore parses cleanly, lets the server
/// boot, and denies every read — at run time indistinguishable from an identity that holds no
/// rights at all.
fn consumers_with_no_positive_read_pattern(preset: &AclPreset) -> Vec<&str> {
    preset
        .consumer
        .iter()
        .filter(|c| {
            !c.read_patterns
                .iter()
                .any(|p| !p.starts_with('!') && !p.trim().is_empty())
        })
        .map(|c| c.identity.as_str())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::path::PathBuf;

    /// Crate root, resolved at run time.
    ///
    /// The value baked in at compile time goes stale as soon as a test binary is reused
    /// from another checkout directory (shared build artefacts): the environment variable
    /// wins, and the compile-time value only serves as a fallback outside cargo.
    fn manifest_dir() -> PathBuf {
        std::env::var_os("CARGO_MANIFEST_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")))
    }

    /// (a) The shipped hierarchical preset (post-F-297) validates cleanly.
    #[test]
    fn shipped_hierarchical_preset_is_valid() {
        let preset = manifest_dir().join("presets/hierarchical.toml");
        validate_preset(&preset, &mut std::io::sink())
            .expect("the shipped hierarchical preset must validate");
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
        let err = validate_preset(&path, &mut std::io::sink())
            .expect_err("a preset carrying the removed key must be rejected");
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
        validate_preset(&path, &mut std::io::sink())
            .expect_err("a missing preset file must be an error");
    }

    /// (d) A consumer holding no positive read pattern is named by a warning.
    ///
    /// Such a consumer parses cleanly and then denies every read, so the command that exists to
    /// be run before a rollout would report it as valid with nothing to notice. The warning is
    /// what makes the state visible without changing the verdict.
    #[test]
    fn consumer_without_positive_read_pattern_is_warned_about() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("bearer.toml");
        std::fs::write(
            &path,
            "[[consumer]]\nidentity = \"reader\"\nread_patterns = [\"**\"]\nwrite_patterns = []\n\
             [[consumer]]\nidentity = \"write-only\"\n\
             read_patterns = [\"!personal-classified\", \"!**/personal-classified/**\"]\n\
             write_patterns = [\"main/event-log\"]\n",
        )
        .expect("write preset");

        let (result, warnings) = validate_capturing(&path);
        result.expect("the preset parses — that is exactly why the warning is needed");
        assert!(
            warnings.contains("warning: consumer 'write-only' has no positive read pattern"),
            "the warning must name the consumer holding no positive read pattern: {warnings:?}"
        );
        assert!(
            !warnings.contains("'reader'"),
            "a consumer with a positive pattern must not be named: {warnings:?}"
        );
    }

    /// (e) A preset whose every consumer holds a positive read pattern produces no warning.
    #[test]
    fn preset_where_every_consumer_can_read_is_not_warned_about() {
        let preset = manifest_dir().join("presets/flat.toml");
        let (result, warnings) = validate_capturing(&preset);
        result.expect("the shipped flat preset must validate");
        assert!(
            warnings.is_empty(),
            "a preset with a positive pattern per consumer must not be warned about: {warnings:?}"
        );
    }

    /// (f) A blank pattern is not a positive pattern.
    ///
    /// The engine's allow set is built from the non-negated entries whatever they hold, so
    /// `" "` is admitted as a pattern and matches no locus in practice. Counting it as positive
    /// would silence the warning on a consumer that still reads nothing.
    #[test]
    fn blank_read_pattern_is_not_a_positive_pattern() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("bearer.toml");
        std::fs::write(
            &path,
            "[[consumer]]\nidentity = \"blank\"\nread_patterns = [\" \"]\nwrite_patterns = []\n",
        )
        .expect("write preset");

        let (result, warnings) = validate_capturing(&path);
        result.expect("the preset parses");
        assert!(
            warnings.contains("warning: consumer 'blank' has no positive read pattern"),
            "a blank pattern reads nothing and must be reported: {warnings:?}"
        );
    }

    /// Runs `validate_preset` with the warning stream captured.
    ///
    /// Warnings go to stderr in production. Capturing them is what makes "the consumer is
    /// named" and "nothing is reported" assertable rather than assumed.
    fn validate_capturing(preset: &Path) -> (anyhow::Result<()>, String) {
        let mut warnings: Vec<u8> = Vec::new();
        let result = validate_preset(preset, &mut warnings);
        let warnings = String::from_utf8(warnings).expect("the warnings are UTF-8");
        (result, warnings)
    }
}
