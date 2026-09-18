//! `[privacy.sidecar]` — mounting the out-of-process detector from the server's own
//! configuration.
//!
//! ## Two refusals, and why they happen at different moments
//!
//! A pin that is not a digest is refused when the **file** is read: the operator still
//! has the file in front of them, and a message naming the field is the whole repair.
//! A sidecar that does not answer, or answers a handshake this deployment did not pin,
//! is refused when the **detector is mounted**: the file is well formed and the
//! deployment is not, and only the second can be observed from the process that has to
//! live with it.
//!
//! Neither refusal falls back to the in-process detector. A deployment that wrote the
//! block down asked for a model that runs outside the server; a quiet fallback would
//! leave it scanning with the engine it did not choose, and the only signal would be a
//! detector name in a health report nobody had a reason to read. Failing to start is
//! loud, bounded and reversible — the operator removes the block or starts the sidecar.
//!
//! ## The shape of the fake
//!
//! The fake sidecar is the one the privacy crate drives, included rather than copied:
//! the mount tests speak the same protocol as the client tests, and a second copy of
//! the wire would be a second description of it.

use std::io::Write;

use gradatum_server::config::{ConfigError, ServerConfig};
use gradatum_server::state::{AppState, PrivacyMountError};

#[path = "../../gradatum-privacy/tests/sidecar_fake/mod.rs"]
mod fake;

/// Digest of the sidecar build this deployment pins, and the one the fake announces.
///
/// Sixty-four lowercase hexadecimal characters, bare: the configuration carries a
/// digest, not the tagged form a handshake may use for its own display.
const BINARY_DIGEST: &str = "3f1a5c0b7d9e246813579bdf0246ace813579bdf0246ace813579bdf0246ace8";

/// Digest of the model weights this deployment pins.
const MODEL_DIGEST: &str = "b7d9e246b7d9e246b7d9e246b7d9e246b7d9e246b7d9e246b7d9e246b7d9e246";

/// Digest over the model's label vocabulary the fake announces.
///
/// Opaque to this client, which reports it rather than comparing it — the value only
/// has to be a string the handshake can carry.
const LABELS_V: &str = "sha256:0f1e2d3c4b5a69788796a5b4c3d2e1f00f1e2d3c4b5a69788796a5b4c3d2e1f0";

/// A build that is not the pinned one: a sidecar swapped without a configuration change.
const OTHER_BINARY_DIGEST: &str =
    "a0b1c2d3e4f506172839405162738495a0b1c2d3e4f506172839405162738495";

/// Writes `toml` to a file of its own and reads it back through the product's loader.
///
/// The loader, never a local figment: what a deployment is refused is what
/// `ServerConfig::load` refuses, and a test that re-implemented the load would be
/// measuring itself.
fn load(toml: &str) -> Result<ServerConfig, ConfigError> {
    let mut file = tempfile::NamedTempFile::new().expect("a temporary configuration file");
    file.write_all(toml.as_bytes())
        .expect("the configuration is written");
    ServerConfig::load(Some(file.path()))
}

/// The `[privacy]` block that mounts a sidecar, over an activation that is not empty.
///
/// `categories` is what decides that a detector is mounted at all; a sidecar without it
/// is a deployment that configured a detector nothing will ask for, which is a
/// different test.
fn block(socket_path: &str, binary_digest: &str, model_digest: &str) -> String {
    format!(
        "[privacy]\ncategories = [\"email\"]\n\
         [privacy.sidecar]\nsocket_path = \"{socket_path}\"\n\
         binary_digest = \"{binary_digest}\"\n\
         model_digest = \"{model_digest}\"\n"
    )
}

/// A socket path that exists in the configuration and nowhere else.
fn an_absent_socket() -> String {
    let dir = tempfile::tempdir().expect("a temporary directory");
    dir.path()
        .join("nothing-listens-here.sock")
        .to_string_lossy()
        .into_owned()
}

/// A conformant sidecar listening on a socket of its own.
fn a_conformant_sidecar() -> fake::FakeSidecar {
    fake::FakeSidecar::start(
        fake::handshake(BINARY_DIGEST, MODEL_DIGEST, LABELS_V, 0),
        |_, _, _| fake::Reply::Spans(Vec::new()),
    )
}

/// Attempts the mount and returns the refusal, failing the test on a success.
///
/// The refusal is returned rather than unwrapped: an `Ok` here is the defect under
/// test, and it has to fail where it happens rather than be lost in a type.
fn refusing_mount(config: &ServerConfig) -> PrivacyMountError {
    match AppState::new().with_privacy(config.privacy.clone()) {
        Ok(_) => panic!(
            "the mount succeeded, so this deployment would have scanned with an engine \
             it did not choose — the block asks for the out-of-process detector"
        ),
        Err(error) => error,
    }
}

/// A well-formed block mounts, and the handshake probe reaches the sidecar.
///
/// The control the two refusals below are read against: without it, a mount that
/// refused every configuration would pass both of them.
#[test]
fn a_conformant_sidecar_is_mounted_and_its_handshake_is_taken() {
    let sidecar = a_conformant_sidecar();
    let cfg = load(&block(
        &sidecar.path().to_string_lossy(),
        BINARY_DIGEST,
        MODEL_DIGEST,
    ))
    .expect("a well-formed block loads");

    AppState::new()
        .with_privacy(cfg.privacy.clone())
        .expect("a sidecar that answers its handshake mounts");

    assert!(
        sidecar.connections() >= 1,
        "mounting has to take the handshake: a detector held without one is a \
         deployment that finds out at its first write instead of at its start"
    );
}

/// A sidecar that cannot be reached refuses the start, and the refusal names the cause.
#[test]
fn an_unreachable_sidecar_refuses_the_start_naming_the_cause() {
    let cfg = load(&block(&an_absent_socket(), BINARY_DIGEST, MODEL_DIGEST))
        .expect("the configuration itself is well formed — the sidecar is what is missing");

    let error = refusing_mount(&cfg);
    assert!(
        matches!(error, PrivacyMountError::Sidecar(_)),
        "the refusal has to be the sidecar's, not another mount failure: {error}"
    );
    let message = error.to_string();
    assert!(
        message.contains("could not be reached"),
        "the refusal must name the cause an operator acts on, got: {message}"
    );
    assert!(
        !message.contains("regex"),
        "a silent fallback to the in-process detector is exactly what this refusal \
         exists to prevent, got: {message}"
    );
}

/// A sidecar that answers a handshake this deployment did not pin refuses the start.
///
/// The socket is up and the process answers, so this is the refusal a reachability
/// check alone would let through: the build in place is a different one, and every span
/// it produced would be read under a provenance the configuration does not claim.
#[test]
fn a_sidecar_announcing_another_build_refuses_the_start() {
    let sidecar = fake::FakeSidecar::start(
        fake::handshake(OTHER_BINARY_DIGEST, MODEL_DIGEST, LABELS_V, 0),
        |_, _, _| fake::Reply::Spans(Vec::new()),
    );
    let cfg = load(&block(
        &sidecar.path().to_string_lossy(),
        BINARY_DIGEST,
        MODEL_DIGEST,
    ))
    .expect("the configuration is well formed");

    let error = refusing_mount(&cfg);
    let message = error.to_string();
    assert!(
        message.contains("the sidecar build is not the pinned one"),
        "the refusal must name the divergence it found, got: {message}"
    );
}

/// A pin that is not sixty-four hexadecimal characters is refused when the file is read.
#[test]
fn a_pin_that_is_not_a_digest_is_refused_at_load() {
    let socket = an_absent_socket();
    let cases: Vec<(&str, String, &str)> = vec![
        (
            "binary_digest",
            "not-a-digest".to_owned(),
            "not hexadecimal at all",
        ),
        (
            "binary_digest",
            BINARY_DIGEST[..63].to_owned(),
            "one character short",
        ),
        (
            "model_digest",
            format!("{}z", &MODEL_DIGEST[..63]),
            "one character outside hexadecimal",
        ),
        (
            "model_digest",
            MODEL_DIGEST.to_ascii_uppercase(),
            "uppercase hexadecimal, which is not the form the sidecar reports",
        ),
    ];

    for (field, value, case) in cases {
        let toml = match field {
            "binary_digest" => block(&socket, &value, MODEL_DIGEST),
            _ => block(&socket, BINARY_DIGEST, &value),
        };
        let error =
            load(&toml).expect_err(&format!("a {field} that is {case} must be refused at load"));
        let message = error.to_string();
        assert!(
            message.contains(field),
            "the refusal must name the field to repair, got: {message}"
        );
        assert!(
            !message.contains(&value),
            "a refusal renders the field, never the value written in it: {message}"
        );
    }
}

/// An empty socket path is refused when the file is read.
///
/// A block that names no socket describes no detector: it would be admitted, mount
/// nothing that can be reached, and refuse the start for a reason the file does not
/// state.
#[test]
fn an_empty_socket_path_is_refused_at_load() {
    let error = load(&block("", BINARY_DIGEST, MODEL_DIGEST))
        .expect_err("a block with no socket path must be refused at load");
    let message = error.to_string();
    assert!(
        message.contains("socket_path"),
        "the refusal must name the field to repair, got: {message}"
    );
}

/// The block is optional, and its absence changes nothing.
///
/// The counter-test of every refusal above: an operator who configures no sidecar is
/// not refused, and the in-process detector is what a non-empty activation still mounts.
#[test]
fn a_configuration_without_the_block_still_loads() {
    let cfg = load("[privacy]\ncategories = [\"email\"]\n")
        .expect("the block is optional and its absence is not a refusal");
    assert!(
        cfg.privacy.sidecar.is_none(),
        "an absent block must stay absent rather than be filled with a default"
    );
}
