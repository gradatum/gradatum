use gradatum_auth::revocation::{InMemoryRevocationStore, RevocationStore, SqliteRevocationStore};
use std::time::{Duration, SystemTime};

async fn run_roundtrip<S: RevocationStore>(store: S) {
    let now = SystemTime::now();
    let exp = now + Duration::from_secs(60);

    assert!(!store.is_revoked("jti-1").await.unwrap());
    store.revoke("jti-1", exp).await.unwrap();
    assert!(store.is_revoked("jti-1").await.unwrap());
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
    store.revoke("jti-old", past).await.unwrap();
    let removed = store.gc().await.unwrap();
    assert_eq!(removed, 1);
    assert!(!store.is_revoked("jti-old").await.unwrap());
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
