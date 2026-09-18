//! The preset `init` writes, measured as a *file on disk* rather than as a
//! value returned by the generator.
//!
//! ## The defects measured
//!
//! Two failures live on the same path, both silent, both leaving the operator
//! with a `config/bearer.toml` that does not say what it does:
//!
//! 1. A template token surviving into a *value*. A preset carrying `${PROJECT}`
//!    inside a `read_patterns` entry, given a project it cannot resolve to a
//!    single value, must not be written — the server would boot into a policy
//!    that matches nothing, or refuse to boot at all. The generator refuses the
//!    write instead, naming the offending token.
//!
//! 2. `--projects` on a preset that declares no project-scoped placeholder. The
//!    shipped `hierarchical` preset carries concrete `[[consumer]]` blocks and no
//!    `${PROJECT}`/`${PROJECTS}` token, so a `--projects` value is substituted
//!    nowhere. Returning `rc=0` while writing a file with no project-scoped
//!    identity hands the operator a per-project boundary they never configured.
//!    The generator refuses instead, before any write.
//!
//! ## What these tests require
//!
//! - the shipped `hierarchical` preset, materialized **without** `--projects`,
//!   carries no unsubstituted `${PROJECT}` token and no template token in any
//!   *value* it declares, and `gradatum-admin acl validate --preset` accepts it,
//!   run as the real command so the assertion is on the built artefact;
//! - the same preset, given `--projects`, is refused and leaves no file behind;
//! - a custom preset that *does* carry a placeholder keeps the existing behaviour:
//!   refused when the project is ambiguous, substituted when it is singular.
//!
//! The assertions are on the file's literal bytes, on its absence, and on the
//! command's exit status on purpose: the defects were things nobody could see in
//! a successful-looking run, and only the produced artefact tells them apart.

use std::process::Command;

const GENERATED_PRESET_RELATIVE_PATH: &str = "config/bearer.toml";

/// Runs `init`'s materialization step and returns the bytes it wrote.
fn materialize_and_read(preset: &str, projects: Option<&str>) -> (tempfile::TempDir, String) {
    let tmp = tempfile::tempdir().expect("tempdir");
    // The root layout is `create_layout`'s job in `init`; this test drives the
    // materialization step alone, so it creates the one directory that step writes into.
    std::fs::create_dir_all(tmp.path().join("config")).expect("create config dir");
    gradatum_admin::materialize_preset(
        tmp.path(),
        preset,
        projects,
        gradatum_admin::ExistingPreset::Overwrite,
    )
    .expect("materialize_preset must not fail on a shipped preset");
    let path = tmp.path().join(GENERATED_PRESET_RELATIVE_PATH);
    let content = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("reading back {}: {e}", path.display()));
    (tmp, content)
}

/// Every `${...}` occurrence of the text, in order.
fn template_tokens(content: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut rest = content;
    while let Some(start) = rest.find("${") {
        let after = &rest[start..];
        let Some(end) = after.find('}') else { break };
        tokens.push(after[..=end].to_owned());
        rest = &after[end + 1..];
    }
    tokens
}

/// The literal criterion: the generated file does not carry the token.
#[test]
fn generated_preset_carries_no_unsubstituted_project_token() {
    let (_tmp, content) = materialize_and_read("hierarchical", None);

    let occurrences = content.matches("${PROJECT}").count();
    assert_eq!(
        occurrences, 0,
        "le preset généré ne doit porter aucun jeton `${{PROJECT}}` non substitué — \
         c'est la valeur littérale que l'opérateur lit dans son fichier de \
         configuration déployé. Obtenu : {occurrences} occurrence(s)"
    );
}

/// The stronger criterion, and the one that matters at runtime: not only is the
/// exact token gone, but **no** template token survives in a value the engine
/// would evaluate. A comment may legitimately document the syntax; a
/// `read_patterns` entry may not be a placeholder.
#[test]
fn generated_preset_has_no_template_token_in_any_value() {
    let (_tmp, content) = materialize_and_read("hierarchical", None);

    let doc: toml::Value = toml::from_str(&content).expect("the generated preset parses as TOML");
    let mut offenders = Vec::new();
    collect_tokens_in_values(&doc, String::new(), &mut offenders);

    assert!(
        offenders.is_empty(),
        "aucune valeur du preset généré ne doit porter un jeton de gabarit — une \
         valeur qui dépend d'un jeton non substitué est une politique que le \
         moteur ne peut pas évaluer. Valeurs fautives : {offenders:?}"
    );
}

/// Walks a parsed TOML document and records every `key = "…${TOKEN}…"` string.
fn collect_tokens_in_values(value: &toml::Value, path: String, out: &mut Vec<String>) {
    match value {
        toml::Value::String(s) => {
            for token in template_tokens(s) {
                out.push(format!("{path} = {token}"));
            }
        }
        toml::Value::Array(items) => {
            for (i, item) in items.iter().enumerate() {
                collect_tokens_in_values(item, format!("{path}[{i}]"), out);
            }
        }
        toml::Value::Table(table) => {
            for (key, item) in table {
                collect_tokens_in_values(item, format!("{path}.{key}"), out);
            }
        }
        _ => {}
    }
}

/// The generated file is accepted by the command operators are told to run on a
/// preset before switching a deployment to it.
#[test]
fn generated_preset_passes_acl_validate() {
    let (tmp, _content) = materialize_and_read("hierarchical", None);
    let preset = tmp.path().join(GENERATED_PRESET_RELATIVE_PATH);

    let output = Command::new(env!("CARGO_BIN_EXE_gradatum-admin"))
        .args(["acl", "validate", "--preset"])
        .arg(&preset)
        .output()
        .expect("running gradatum-admin acl validate");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "`acl validate --preset` doit accepter le preset généré (rc=0). rc={:?}\n\
         stdout: {stdout}\nstderr: {stderr}",
        output.status.code()
    );
    // A validation that succeeds on an empty document would satisfy the exit
    // status without proving anything: require the exact consumer count the
    // shipped `hierarchical` preset declares (13 `[[consumer]]` blocks). The
    // summary line is `ACL preset … is valid — N consumer(s), M negation … `.
    assert!(
        stdout.contains("13 consumer(s)"),
        "la validation doit compter les 13 consumers du preset `hierarchical` — un \
         fichier vide ou tronqué validerait avec un autre compte. stdout: {stdout}"
    );
}

/// `--projects` on the shipped `hierarchical` preset is refused: it declares
/// concrete consumers and carries no `${PROJECT}`/`${PROJECTS}` placeholder, so
/// the value would be substituted nowhere. Returning `rc=0` while writing a
/// `bearer.toml` with no project-scoped identity would hand the operator a
/// per-project boundary they never configured — the refusal is the honest answer,
/// and it must leave no file behind.
#[test]
fn hierarchical_with_projects_is_refused_when_it_declares_no_placeholder() {
    let tmp = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(tmp.path().join("config")).expect("create config dir");

    let err = gradatum_admin::materialize_preset(
        tmp.path(),
        "hierarchical",
        Some("foo"),
        gradatum_admin::ExistingPreset::Overwrite,
    )
    .expect_err("--projects on a placeholder-less preset must be refused");

    let msg = err.to_string();
    assert!(
        msg.contains("no project-scoped placeholder"),
        "le refus doit expliquer que le preset ne déclare aucun placeholder de \
         projet, pour que l'opérateur sache que --projects n'a aucun effet ici. \
         Obtenu : {msg}"
    );
    assert!(
        !tmp.path().join(GENERATED_PRESET_RELATIVE_PATH).exists(),
        "aucun bearer.toml ne doit être écrit quand --projects est refusé : le \
         refus précède toute écriture, sinon l'opérateur garde un fichier trompeur"
    );
}

/// A preset an operator wrote themselves, with the placeholder inside a value.
///
/// The generator cannot invent the project for them when `--projects` names
/// several — one placeholder has one value. Refusing is the only answer that
/// cannot end in a deployed file claiming a project literally named
/// `projecta,projectb`.
#[test]
fn custom_preset_with_a_token_in_a_value_is_refused_when_the_project_is_ambiguous() {
    let tmp = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(tmp.path().join("config")).expect("create config dir");
    let custom = tmp.path().join("custom.toml");
    std::fs::write(
        &custom,
        r#"
[[consumer]]
identity = "main-agent"
read_patterns = ["${PROJECT}/**"]
write_patterns = ["${PROJECT}/**"]
"#,
    )
    .expect("write custom preset");

    let err = gradatum_admin::materialize_preset(
        tmp.path(),
        custom.to_str().expect("utf-8 path"),
        Some("projecta,projectb"),
        gradatum_admin::ExistingPreset::Overwrite,
    )
    .expect_err("un jeton qui ne peut pas être substitué doit refuser l'écriture");

    let msg = err.to_string();
    assert!(
        msg.contains("PROJECT"),
        "le refus doit nommer le jeton fautif pour que l'opérateur sache quoi \
         remplacer. Obtenu : {msg}"
    );
    assert!(
        !tmp.path().join(GENERATED_PRESET_RELATIVE_PATH).exists(),
        "aucun preset fautif ne doit être écrit : le serveur refuserait de démarrer \
         dessus, et un fichier écrit serait un piège laissé sur le disque"
    );
}

/// The same custom preset, with a single project: the placeholder has exactly
/// one value, so it is substituted and the file is written.
#[test]
fn custom_preset_with_a_token_in_a_value_is_substituted_for_a_single_project() {
    let tmp = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(tmp.path().join("config")).expect("create config dir");
    let custom = tmp.path().join("custom.toml");
    std::fs::write(
        &custom,
        r#"
[[consumer]]
identity = "main-agent"
read_patterns = ["${PROJECT}/**"]
write_patterns = ["${PROJECT}/**"]
"#,
    )
    .expect("write custom preset");

    gradatum_admin::materialize_preset(
        tmp.path(),
        custom.to_str().expect("utf-8 path"),
        Some("projecta"),
        gradatum_admin::ExistingPreset::Overwrite,
    )
    .expect("un projet unique doit être substitué");

    let content =
        std::fs::read_to_string(tmp.path().join(GENERATED_PRESET_RELATIVE_PATH)).expect("read");
    assert!(
        content.contains("projecta/**"),
        "le jeton doit être remplacé par le nom de projet effectif. Obtenu : {content}"
    );
}
