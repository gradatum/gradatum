//! The client against the real sidecar, running the real model.
//!
//! Everything else in this crate tests the client against a fake written from the
//! protocol header. That proves the client speaks the protocol the header
//! describes; it cannot prove the header describes the sidecar. This test closes
//! that gap by launching the actual binary, over a socket of its own, and pointing
//! the actual client at it.
//!
//! It is `#[ignore]`d, and gated on two environment variables naming artefacts that
//! a checkout does not carry: a built sidecar and the weights it loads. When it is
//! asked to run and either is missing, it fails rather than skipping — a test that
//! reports success without having run anything is worse than no test.
//!
//! ```text
//! PF_SIDECAR_BIN=/path/to/pf-sidecar \
//! PF_MODEL_PATH=/path/to/model.gguf \
//!   cargo test -p gradatum-privacy --test sidecar_real -- --ignored --nocapture
//! ```

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

use gradatum_core::pii::{Budget, DetectInput, PiiCategory, PiiDetectError, PiiDetector, Surface};
use gradatum_privacy::{ADAPTER_TABLE_V, SidecarConfig, SidecarDetector};
use sha2::{Digest, Sha256};

/// The aggregation mode the protocol defines, and the one the client pins.
const SCORE_MODE: &str = "mean_token_prob";

/// The sidecar's own refusal threshold, passed on its command line.
///
/// Deliberately far below the client's ceiling, so that a body the client cuts into
/// one window is a body the sidecar refuses — which is what makes the size path
/// reachable without a body of a quarter of a megabyte.
const SERVER_MAX_INPUT_BYTES: u64 = 4_096;

/// The client's per-window ceiling: the protocol's own default.
const CLIENT_CEILING: usize = 262_144;

/// How long the model may take to load before the test gives up. Bounded, because
/// a sidecar that never listens must fail the test rather than hold it.
const STARTUP_TIMEOUT: Duration = Duration::from_secs(300);

/// A fictitious French body.
///
/// Every value in it is fictitious, and every address in it is reserved for
/// documentation: `192.0.2.0/24` is TEST-NET-1, which routes nowhere and belongs to
/// nobody. No real person's data is scanned by this test.
const ADDRESS: &str = "jeanne.dupont@example.org";
const DOCUMENTARY_ADDRESS: &str = "192.0.2.10";
const BODY: &str = "Bonjour, je m'appelle Jeanne Dupont et je travaille au service \
     comptabilité. Vous pouvez m'écrire à jeanne.dupont@example.org avant vendredi \
     soir. Le serveur de recette répond à l'adresse 192.0.2.10, qui est une adresse \
     de documentation réservée aux exemples. Mon numéro de dossier est le \
     06 12 34 56 78.";

/// The two artefacts the environment names, or a failure saying which is missing.
fn fixtures() -> (PathBuf, PathBuf) {
    let binary = std::env::var("PF_SIDECAR_BIN").unwrap_or_else(|_| {
        panic!(
            "PF_SIDECAR_BIN must name a built pf-sidecar: this test is ignored by \
             default and must not be asked to run without its artefacts"
        )
    });
    let model = std::env::var("PF_MODEL_PATH")
        .unwrap_or_else(|_| panic!("PF_MODEL_PATH must name the model weights the sidecar loads"));

    // The sidecar digests its own running image, so the path the client pins must
    // be the file the process was started from, with symlinks resolved.
    let binary = std::fs::canonicalize(&binary)
        .unwrap_or_else(|error| panic!("cannot resolve {binary}: {error}"));
    let model = std::fs::canonicalize(&model)
        .unwrap_or_else(|error| panic!("cannot resolve {model}: {error}"));
    for (label, path) in [("PF_SIDECAR_BIN", &binary), ("PF_MODEL_PATH", &model)] {
        assert!(
            path.is_file(),
            "{label} does not name a readable file: {}",
            path.display()
        );
    }
    (binary, model)
}

/// Lowercase hexadecimal SHA-256 of a file's bytes.
fn sha256_file(path: &Path) -> String {
    let mut file = File::open(path).expect("the file the digests are computed over");
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 1 << 20];
    loop {
        let read = file.read(&mut buffer).expect("a readable artefact");
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    hasher
        .finalize()
        .iter()
        .fold(String::with_capacity(64), |mut out, byte| {
            use std::fmt::Write as _;
            let _ = write!(out, "{byte:02x}");
            out
        })
}

/// A sidecar process serving a socket of its own, killed when this is dropped.
struct RunningSidecar {
    child: Child,
    socket: PathBuf,
    log: PathBuf,
    _dir: tempfile::TempDir,
}

impl RunningSidecar {
    /// Launches the sidecar and waits until it is listening.
    fn start(binary: &Path, model: &Path) -> Self {
        let dir = tempfile::TempDir::new().expect("a temporary directory");
        // The server refuses to bind onto a path that already exists, so the socket
        // is named inside a directory that was just created empty.
        let socket = dir.path().join("sidecar.sock");
        let log = dir.path().join("sidecar.log");

        let stdout = File::create(&log).expect("a log file");
        let stderr = stdout.try_clone().expect("a log file");
        let child = Command::new(binary)
            .arg("--socket")
            .arg(&socket)
            .arg("--model")
            .arg(model)
            .arg("--threads")
            .arg("2")
            .arg("--max-input-bytes")
            .arg(SERVER_MAX_INPUT_BYTES.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr))
            .spawn()
            .expect("the sidecar binary starts");

        let mut running = Self {
            child,
            socket,
            log,
            _dir: dir,
        };
        running.await_listening();
        running
    }

    /// Waits for the listening socket, failing with the sidecar's own output if the
    /// process gave up first.
    fn await_listening(&mut self) {
        let deadline = Instant::now() + STARTUP_TIMEOUT;
        loop {
            match self.child.try_wait() {
                Ok(Some(status)) => panic!(
                    "the sidecar exited with {status} before listening; its output was:\n{}",
                    self.log()
                ),
                Ok(None) => {}
                Err(error) => panic!("cannot observe the sidecar process: {error}"),
            }
            if self.socket.exists() {
                return;
            }
            if Instant::now() >= deadline {
                let _ = self.child.kill();
                panic!(
                    "the sidecar did not listen within {STARTUP_TIMEOUT:?}; its output was:\n{}",
                    self.log()
                );
            }
            sleep(Duration::from_millis(50));
        }
    }

    /// Everything the sidecar wrote to its log.
    fn log(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_else(|_| String::from("<no output>"))
    }
}

impl Drop for RunningSidecar {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A configuration pinning the digests computed from the artefacts themselves.
///
/// The per-kilobyte term is calibrated for a real model on a real host rather than
/// for a fake that answers in microseconds: a synchronous call here costs a model
/// load's worth of work, not a socket round trip.
fn config(socket: &Path, binary_digest: &str, model_digest: &str) -> SidecarConfig {
    SidecarConfig::new(socket, binary_digest, model_digest)
        .with_score_mode(SCORE_MODE)
        .with_timeout_ms_sync_per_kib(60_000)
        .with_max_input_bytes(CLIENT_CEILING)
        .with_window_overlap_bytes(512)
        .with_breaker(3, 30)
}

/// A caller budget with no allowance of its own, so the detector's calibration
/// governs.
fn input(text: &str) -> DetectInput<'_> {
    DetectInput::new(text, Surface::Sync, Budget::new(0, CLIENT_CEILING))
}

#[test]
#[ignore = "needs a built pf-sidecar (PF_SIDECAR_BIN) and its weights (PF_MODEL_PATH)"]
fn the_real_sidecar_is_reached_and_reads_as_the_protocol_describes() {
    let (binary, model) = fixtures();
    let binary_digest = sha256_file(&binary);
    let model_digest = sha256_file(&model);

    let sidecar = RunningSidecar::start(&binary, &model);
    let detector = SidecarDetector::connect(config(&sidecar.socket, &binary_digest, &model_digest))
        .expect("a configuration the client accepts");

    // The identity is what a persisted span list is read back under, so it must name
    // the artefacts this test actually ran — computed here, not taken on trust.
    let identity = detector.identity().expect("a configured detector");
    assert_eq!(identity.model_digest, model_digest);
    assert_eq!(identity.sidecar_v, binary_digest);
    assert_eq!(identity.score_mode, SCORE_MODE);
    assert_eq!(identity.mapping_v, ADAPTER_TABLE_V);

    let detection = detector
        .detect(&input(BODY))
        .unwrap_or_else(|error| panic!("the real sidecar did not answer: {error}"));

    // The fixture is checked against itself before it is used as evidence: a body
    // that had lost the address would make every assertion below about a value that
    // was never there.
    assert!(
        BODY.contains(ADDRESS) && BODY.contains(DOCUMENTARY_ADDRESS),
        "the fixture must carry the values it claims to: {BODY}"
    );

    // What the sidecar announced about its own labelling, read off the handshake it
    // sent: reported, never compared.
    let diagnostics = detector.diagnostics();
    assert!(
        diagnostics.available,
        "a handshake this test pinned must leave the detector available: {diagnostics:?}"
    );
    let labels_v = diagnostics
        .labels_v
        .as_deref()
        .expect("the handshake announces a label-set digest");
    assert!(
        labels_v.starts_with("sha256:") && labels_v.len() == "sha256:".len() + 64,
        "the label-set digest is a sha256-tagged string, got {labels_v:?}"
    );
    let mapping_v = diagnostics
        .mapping_v
        .expect("the handshake announces a mapping revision");
    assert!(
        mapping_v >= gradatum_privacy::SUPPORTED_MAPPING_V_MIN,
        "an accepted handshake cannot be below the floor, got {mapping_v}"
    );

    // The address must be found, and found where it is. The deterministic floor
    // recognises this exact address and the model is expected to as well; the two
    // fuse onto one span, so the span read here is the fused one.
    let address_start = BODY.find(ADDRESS).expect("the fixture carries the address");
    let email = detection
        .spans
        .iter()
        .find(|span| span.category == PiiCategory::Email)
        .unwrap_or_else(|| {
            panic!(
                "the model found no e-mail address in the fixture: {:?}",
                detection.spans
            )
        });
    assert_eq!(
        email.start, address_start,
        "the e-mail span must start where the address starts: {:?}",
        detection.spans
    );
    assert_eq!(
        email.len,
        ADDRESS.len(),
        "the e-mail span must cover exactly the address: {:?}",
        detection.spans
    );
    assert_eq!(
        &BODY[email.start..email.start + email.len],
        ADDRESS,
        "the reported offsets must slice the address out of the body"
    );

    // Every span is a slice of the body, and the fusion keeps them disjoint.
    for span in &detection.spans {
        assert!(
            BODY.is_char_boundary(span.start) && BODY.is_char_boundary(span.start + span.len),
            "a span must fall on code-point boundaries: {span:?}"
        );
        assert!(span.len > 0, "a span of no bytes is not a span: {span:?}");
    }

    // Coherence of the two halves of a detection: what the table mapped, and what it
    // had to leave. A body carrying an address must produce at least one span, and
    // the same body scanned twice must produce the same reading — a count that moved
    // between two identical scans would not be a count of anything.
    assert!(
        !detection.spans.is_empty(),
        "a body with a documented address must yield at least one span"
    );
    let again = detector
        .detect(&input(BODY))
        .expect("a second scan of the same body");
    assert_eq!(
        again.unmapped_labels, detection.unmapped_labels,
        "two scans of one body must leave the table with the same count of unmapped labels"
    );
    assert_eq!(
        again.spans, detection.spans,
        "two scans of one body must report the same spans"
    );

    // A body the sidecar refuses for its size is refused through the client as a
    // size state, and the length reported is the refused window's — not the body's,
    // and not a number invented on this side.
    let oversized = "a".repeat((SERVER_MAX_INPUT_BYTES as usize) * 2);
    let result = detector.detect(&input(&oversized));
    let Err(PiiDetectError::InputTooLarge { len, budget }) = result else {
        panic!("a body over the sidecar's ceiling must be a size state, got {result:?}");
    };
    assert_eq!(
        len,
        oversized.len(),
        "the window submitted in one piece is the whole body here"
    );
    assert_eq!(
        budget, CLIENT_CEILING,
        "the reported ceiling is the one the client submitted the window under"
    );

    // What the sidecar wrote about what it did. The client can report a scan it
    // never received, so the evidence that the model ran is the sidecar's own line
    // about it — the audit surface, not the answer. Each of the three events below
    // is a distinct claim: it announced itself, it classified a body through the
    // model, and it refused one on its own threshold rather than on the client's
    // guess.
    let log = sidecar.log();
    eprintln!("the sidecar wrote:\n{log}");
    eprintln!("the detection was: {detection:?}");
    for event in ["ev=handshake", "ev=classify ", "status=input_too_large"] {
        assert!(
            log.contains(event),
            "the sidecar's log carries no {event:?}: the client's answer alone does \
             not show that this happened. It wrote:\n{log}"
        );
    }
}
