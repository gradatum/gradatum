use gradatum_auth::revocation::{InMemoryRevocationStore, RevocationStore, SqliteRevocationStore};
use std::time::{Duration, SystemTime};

async fn run_roundtrip<S: RevocationStore>(store: S) {
    let now = SystemTime::now();
    let exp = now + Duration::from_secs(60);

    assert!(!store.is_revoked("jti-1", "main").await.unwrap());
    store.revoke("jti-1", "main", exp).await.unwrap();
    assert!(store.is_revoked("jti-1", "main").await.unwrap());
}

#[tokio::test]
async fn inmemory_roundtrip() {
    run_roundtrip(InMemoryRevocationStore::new()).await;
}

#[tokio::test]
async fn sqlite_roundtrip() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let store = SqliteRevocationStore::new(tmp.path()).await.unwrap();
    run_roundtrip(store).await;
}

#[tokio::test]
async fn sqlite_gc_removes_expired() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let store = SqliteRevocationStore::new(tmp.path()).await.unwrap();
    let past = SystemTime::now() - Duration::from_secs(60);
    store.revoke("jti-old", "main", past).await.unwrap();
    let removed = store.gc("main").await.unwrap();
    assert_eq!(removed, 1);
    assert!(!store.is_revoked("jti-old", "main").await.unwrap());
}

#[test]
fn boot_guard_refuses_memory_on_lan() {
    let r = gradatum_auth::revocation::boot_guard_check(false, "memory");
    assert!(r.is_err());
}

#[test]
fn boot_guard_allows_memory_on_loopback() {
    gradatum_auth::revocation::boot_guard_check(true, "memory").unwrap();
}

#[test]
fn boot_guard_allows_sqlite_anywhere() {
    gradatum_auth::revocation::boot_guard_check(false, "sqlite").unwrap();
    gradatum_auth::revocation::boot_guard_check(true, "sqlite").unwrap();
}

// ─── Multi-tenant isolation tests (P0 #2) ────────────────────────────────

#[tokio::test]
async fn cross_tenant_is_revoked_isolates_jti() {
    // Un JTI révoqué pour le tenant A ne doit PAS être visible depuis le tenant B.
    let store = InMemoryRevocationStore::new();
    let exp = SystemTime::now() + Duration::from_secs(3600);

    store.revoke("shared-jti", "tenant-a", exp).await.unwrap();

    // Le tenant A voit bien le JTI révoqué.
    assert!(store.is_revoked("shared-jti", "tenant-a").await.unwrap());

    // Le tenant B ne voit PAS ce JTI — il est isolé.
    assert!(!store.is_revoked("shared-jti", "tenant-b").await.unwrap());

    // Le tenant B peut avoir son propre JTI avec le même identifiant.
    store.revoke("shared-jti", "tenant-b", exp).await.unwrap();
    assert!(store.is_revoked("shared-jti", "tenant-b").await.unwrap());
}

#[tokio::test]
async fn sqlite_cross_tenant_isolation() {
    // Même test que ci-dessus, mais sur le store SQLite (production).
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let store = SqliteRevocationStore::new(tmp.path()).await.unwrap();
    let exp = SystemTime::now() + Duration::from_secs(3600);

    store
        .revoke("shared-jti-sqlite", "tenant-a", exp)
        .await
        .unwrap();

    assert!(
        store
            .is_revoked("shared-jti-sqlite", "tenant-a")
            .await
            .unwrap()
    );
    assert!(
        !store
            .is_revoked("shared-jti-sqlite", "tenant-b")
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn gc_scopes_to_tenant_only() {
    // gc("tenant-a") ne doit PAS supprimer les entrées de tenant-b.
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let store = SqliteRevocationStore::new(tmp.path()).await.unwrap();
    let past = SystemTime::now() - Duration::from_secs(60);
    let future = SystemTime::now() + Duration::from_secs(3600);

    // Un JTI expiré pour tenant-a, un JTI actif pour tenant-b.
    store.revoke("jti-ta", "tenant-a", past).await.unwrap();
    store.revoke("jti-tb", "tenant-b", future).await.unwrap();

    // gc("tenant-a") doit supprimer jti-ta mais PAS jti-tb.
    let removed = store.gc("tenant-a").await.unwrap();
    assert_eq!(removed, 1);
    assert!(!store.is_revoked("jti-ta", "tenant-a").await.unwrap());

    // jti-tb du tenant-b est toujours actif.
    assert!(store.is_revoked("jti-tb", "tenant-b").await.unwrap());

    // gc("tenant-b") ne trouve rien maintenant (jti-tb n'est pas expiré).
    let removed_b = store.gc("tenant-b").await.unwrap();
    assert_eq!(removed_b, 0);
}
