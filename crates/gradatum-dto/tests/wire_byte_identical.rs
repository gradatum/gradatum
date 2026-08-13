//! Wire byte-identity lock for the DTO typing.
//!
//! Prouve que renforcer `tenant_id` en [`TenantId`] (principal) et `vault_id` en
//! [`VaultId`] (namespace) — deux newtypes `#[serde(transparent)]` — ne change RIEN sur
//! le fil : la représentation JSON et le schéma JSON MCP généré restent identiques au
//! contrat `String`/`Option<String>` d'avant typage.
//!
//! Red→green : ce fichier référence `TenantId`/`VaultId` sur les champs des DTO ; il ne
//! compile QUE si le typage a bien été appliqué. Il échoue si un `default`, une dérive
//! `serde`, la transparence des newtypes ou l'override `schemars(with = "String")` est
//! perdu.

use gradatum_core::scope::{TenantId, VaultId};
use gradatum_dto::{
    ArchiveEntryDto, PersistCuratedRequest, PersistEmbeddingRequest, VaultLifecycleRequest,
    VaultPurgeRequest, VaultReadRequest, VaultSearchRequest, VaultTimelineRequest,
    VaultWriteRequest,
};

// ── 1. Transparence des newtypes (garantie racine JSON) ──

#[test]
fn tenant_id_json_transparent() {
    let typed = serde_json::to_string(&TenantId::new("main")).expect("json TenantId");
    let raw = serde_json::to_string(&"main".to_string()).expect("json String");
    assert_eq!(typed, raw, "TenantId sérialise comme un String nu");
    assert_eq!(typed, "\"main\"");
    let back: TenantId = serde_json::from_str("\"vault-b\"").expect("désérialise bare string");
    assert_eq!(back, TenantId::new("vault-b"));
}

#[test]
fn vault_id_json_transparent() {
    let typed = serde_json::to_string(&VaultId::new("vault-b")).expect("json VaultId");
    let raw = serde_json::to_string(&"vault-b".to_string()).expect("json String");
    assert_eq!(typed, raw, "VaultId sérialise comme un String nu");
    assert_eq!(typed, "\"vault-b\"");
}

// ── 2. VaultWriteRequest — JSON, struct entière ──

/// Miroir à l'identique de [`VaultWriteRequest`] mais avec les types "nus".
///
/// Lot A1 : `tenant_id` est passé de `String` à `Option<String>` pour refléter le nouveau
/// champ `Option<TenantId>`. Le newtype `#[serde(transparent)]` garantit que
/// `Some(TenantId)` reste JSON-identique à `Some(String)` — la transparence n'est pas
/// perdue par le passage à `Option`. L'ordre des champs DOIT matcher : `serde_json` émet
/// les clés dans l'ordre de déclaration, donc le miroir doit suivre le même ordre pour un
/// JSON byte-identique. Valeur toujours `Some(_)` dans ce test (le serveur pose le tenant
/// effectif avant l'enqueue) — `skip_serializing_if` ne se déclenche donc pas.
#[derive(serde::Serialize)]
struct VaultWriteRequestStringMirror {
    title: String,
    body: String,
    author: Option<String>,
    tags: Vec<String>,
    section_hint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tenant_id: Option<String>,
    expected_sha256: Option<String>,
    note_id: Option<String>,
    occurred_at: Option<String>,
}

#[test]
fn vault_write_request_json_matches_string_mirror() {
    let mut typed = VaultWriteRequest::new("T".to_string(), "B".to_string());
    typed.author = Some("main-agent".to_string());
    typed.tags = vec!["x".to_string()];
    typed.section_hint = Some("decisions".to_string());
    typed.tenant_id = Some(TenantId::new("main"));
    let mirror = VaultWriteRequestStringMirror {
        title: "T".to_string(),
        body: "B".to_string(),
        author: Some("main-agent".to_string()),
        tags: vec!["x".to_string()],
        section_hint: Some("decisions".to_string()),
        tenant_id: Some("main".to_string()),
        expected_sha256: None,
        note_id: None,
        occurred_at: None,
    };
    assert_eq!(
        serde_json::to_string(&typed).expect("json typed"),
        serde_json::to_string(&mirror).expect("json mirror"),
        "JSON de VaultWriteRequest byte-identique"
    );
}

// ── 3. Round-trip JSON par forme de DTO ──

#[test]
fn vault_read_tenant_default_and_explicit() {
    // Lot A1 : absent → `None` (le tenant est résolu côté serveur depuis l'identité du
    // credential, plus jamais le défaut implicite "main").
    let req: VaultReadRequest =
        serde_json::from_str(r#"{"path":"decisions/x"}"#).expect("read minimal");
    assert_eq!(
        req.tenant_id, None,
        "tenant_id absent → None (résolu serveur, plus de défaut \"main\")"
    );
    // Présent → Some(TenantId) typé (écho vérifié pour cohérence côté serveur).
    let req: VaultReadRequest =
        serde_json::from_str(r#"{"tenant_id":"tenant-x","path":"decisions/x"}"#)
            .expect("read explicit");
    assert_eq!(req.tenant_id, Some(TenantId::new("tenant-x")));
}

#[test]
fn vault_search_tenant_default_and_vault_namespace() {
    // vault_id absent → None ; tenant absent → None (A1 : plus de défaut "main").
    let req: VaultSearchRequest = serde_json::from_str(r#"{"query":"q"}"#).expect("search minimal");
    assert_eq!(req.tenant_id, None);
    assert_eq!(req.vault_id, None, "vault_id absent → None");
    // vault_id présent → Some(VaultId) ; principal echo distinct.
    let req: VaultSearchRequest =
        serde_json::from_str(r#"{"tenant_id":"main","query":"q","vault_id":"vault-b"}"#)
            .expect("search cross-vault");
    assert_eq!(req.tenant_id, Some(TenantId::new("main")));
    assert_eq!(req.vault_id, Some(VaultId::new("vault-b")));
}

#[test]
fn vault_timeline_vault_namespace() {
    let req: VaultTimelineRequest = serde_json::from_str(r#"{}"#).expect("timeline vide");
    assert_eq!(req.tenant_id, None);
    assert_eq!(req.vault_id, None);
    let req: VaultTimelineRequest =
        serde_json::from_str(r#"{"vault_id":"vault-b"}"#).expect("timeline cross-vault");
    assert_eq!(req.vault_id, Some(VaultId::new("vault-b")));
}

#[test]
fn persist_embedding_vault_namespace_internal() {
    // internal.rs : vault_id Option<VaultId>, pas de schemars.
    let req: PersistEmbeddingRequest =
        serde_json::from_str(r#"{"note_id":"X","embedder_id":"bge-m3","dim":1024,"vector":[]}"#)
            .expect("embedding sans vault_id");
    assert_eq!(
        req.vault_id, None,
        "vault_id omis → None (byte-identical pré-B2)"
    );
    let req: PersistEmbeddingRequest = serde_json::from_str(
        r#"{"note_id":"X","embedder_id":"bge-m3","dim":1024,"vector":[],"vault_id":"vault-b"}"#,
    )
    .expect("embedding avec vault_id");
    assert_eq!(req.vault_id, Some(VaultId::new("vault-b")));
    // skip_serializing_if : None ne réémet pas le champ (byte-identical schéma pré-B2).
    let req = PersistEmbeddingRequest::new("X".to_string(), "bge-m3".to_string(), 1024, vec![]);
    assert!(
        !serde_json::to_string(&req)
            .expect("json")
            .contains("vault_id"),
        "vault_id=None omis du payload"
    );
}

#[test]
fn vault_admin_vault_id_direct_and_confirm() {
    // vault_admin : vault_id = identité de l'op (VaultId, pas d'écho principal).
    let req: VaultLifecycleRequest =
        serde_json::from_str(r#"{"vault_id":"vault-b"}"#).expect("lifecycle");
    assert_eq!(req.vault_id, VaultId::new("vault-b"));
    let req: VaultPurgeRequest =
        serde_json::from_str(r#"{"vault_id":"vault-b","confirm_vault_id":"vault-b"}"#)
            .expect("purge");
    assert_eq!(req.vault_id, VaultId::new("vault-b"));
    assert_eq!(req.confirm_vault_id, Some(VaultId::new("vault-b")));
}

#[test]
fn archive_entry_vault_id_default_main() {
    // ArchiveEntryDto.vault_id : défaut "main" (default_main_vault), Serialize round-trip.
    let entry = ArchiveEntryDto {
        note_id: "01HTEST00000000000000000AB".to_string(),
        vault_id: VaultId::new("main"),
        section: "feedback".to_string(),
        title: None,
        original_locus: None,
        archive_path: ".archive/main/x.md".to_string(),
        archived_at: 1,
        archived_by: None,
        gc_due: 2,
        gc_at: None,
        restored_at: None,
    };
    let json = serde_json::to_string(&entry).expect("json entry");
    let back: ArchiveEntryDto = serde_json::from_str(&json).expect("roundtrip entry");
    assert_eq!(back.vault_id, VaultId::new("main"));
    // vault_id absent du JSON → défaut "main".
    let back: ArchiveEntryDto = serde_json::from_str(
        r#"{"note_id":"Y","section":"s","archive_path":"p","archived_at":1,"gc_due":2}"#,
    )
    .expect("entry sans vault_id");
    assert_eq!(back.vault_id, "main", "vault_id absent → défaut \"main\"");
}

// ── 4. Schéma JSON MCP inchangé (feature schemars) ──

#[cfg(feature = "schemars")]
mod schema_lock {
    use schemars::schema_for;
    use serde_json::Value;

    fn property_type(schema: &Value, field: &str) -> Value {
        schema
            .pointer(&format!("/properties/{field}/type"))
            .cloned()
            .unwrap_or(Value::Null)
    }

    #[test]
    fn tenant_id_schema_stays_plain_string() {
        // Lot A1 : le champ Rust devient `Option<TenantId>`, mais l'override schemars
        // reste `with = "String"` → le schéma MCP du champ demeure `{"type":"string"}`
        // (optionnel via `#[serde(default)]`, non listé dans `required`). Le fil MCP est
        // ainsi BYTE-IDENTICAL au contrat pré-A1 : un `tenant_id` présent est une string,
        // omis il est simplement absent. Aucune fuite du newtype `TenantId` en objet.
        let schema =
            serde_json::to_value(schema_for!(gradatum_dto::VaultWriteRequest)).expect("schema");
        assert_eq!(
            property_type(&schema, "tenant_id"),
            Value::String("string".to_string()),
            "tenant_id doit rester {{\"type\":\"string\"}} (schemars with = String)"
        );
    }

    #[test]
    fn vault_id_schema_stays_string_based() {
        let schema =
            serde_json::to_value(schema_for!(gradatum_dto::VaultSearchRequest)).expect("schema");
        // Option<String> en schemars 1.0 : type ["string","null"] ou variante nullable.
        // On verrouille que le schéma du champ reste string-based (identique à Option<String>).
        let field = schema
            .pointer("/properties/vault_id")
            .cloned()
            .unwrap_or(Value::Null);
        let dump = serde_json::to_string(&field).expect("dump");
        assert!(
            dump.contains("string"),
            "vault_id doit rester string-based (Option<String>), obtenu : {dump}"
        );
    }
}

// ── 5. C1 (T14) — PersistCuratedRequest.target_vault : rétro-compat + wire byte-identical ──

/// Un job persist ANTÉRIEUR (sans `target_vault` ni `curator_decision`) désérialise via
/// `#[serde(default)]` → la queue LIVE ne casse pas au déploiement du champ.
#[test]
fn old_job_row_without_target_deserializes() {
    let old_row = serde_json::json!({
        "note_id": "01HZZZZZZZZZZZZZZZZZZZZZZZZ",
        "tenant_id": "main",
        "title": "t",
        "body": "b",
        "section": "decisions",
        "tags": [],
        "author": null,
        "status": "live",
        "trust": null,
        "expected_sha256": null,
        "temporal": null,
        "links": [],
        "provenance": null
    });
    let req: PersistCuratedRequest = serde_json::from_value(old_row)
        .expect("job antérieur (sans target_vault) doit désérialiser");
    assert!(
        req.target_vault.is_none(),
        "champ absent → None (default), pas d'erreur de désérialisation"
    );
}

/// `target_vault = None` est OMIS du JSON (`skip_serializing_if`) → wire byte-identical au
/// schéma pré-C1 (aucun `"target_vault":null` émis).
#[test]
fn target_vault_none_omitted_from_wire() {
    let json = serde_json::json!({
        "note_id": "01HZZZZZZZZZZZZZZZZZZZZZZZZ",
        "tenant_id": "main",
        "title": "t",
        "body": "b",
        "section": "decisions",
        "tags": [],
        "author": null,
        "status": "live",
        "trust": null,
        "expected_sha256": null,
        "temporal": null,
        "links": [],
        "provenance": null
    });
    let req: PersistCuratedRequest = serde_json::from_value(json).expect("désérialise");
    let wire = serde_json::to_string(&req).expect("sérialise");
    assert!(
        !wire.contains("target_vault"),
        "target_vault None doit être omis du wire (skip_serializing_if), obtenu : {wire}"
    );
}
