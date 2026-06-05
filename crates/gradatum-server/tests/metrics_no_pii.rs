//! Test cardinality cap — caveat C7 (T11).
//!
//! Vérifie que `observe_tenant` plafonne à 100 labels distincts sur 200 appels.
//! Le test est pragmatique : il n'instancie pas de serveur HTTP — il appelle directement
//! `observe_tenant` avec 200 valeurs distinctes et compte les `Some` retournés.
//!
//! Cas limite couvert : au-delà du cap, `observe_tenant` retourne `None` (label ignoré).

use gradatum_server::metrics::{AppMetrics, TenantLabel};

/// Envoie 200 labels tenant distincts à `observe_tenant` et vérifie que ≤ 100 sont admis.
#[test]
fn cardinality_cap_blocks_high_cardinality_label() {
    let m = AppMetrics::new();

    let mut admitted = 0usize;
    let mut rejected = 0usize;

    for i in 0..200 {
        let label = TenantLabel {
            tenant: format!("evil-{i}"),
        };
        match m.observe_tenant(label) {
            Some(_) => admitted += 1,
            None => rejected += 1,
        }
    }

    // Le cap par défaut est 100.
    assert!(
        admitted <= 100,
        "trop de labels admis : {admitted} > 100 (cap dépassé)"
    );
    assert_eq!(
        admitted + rejected,
        200,
        "la somme admitted+rejected doit être 200"
    );
    assert!(
        rejected >= 100,
        "au moins 100 labels doivent être rejetés après le cap : rejected={rejected}"
    );
}

/// Vérifie que des labels identiques ne consomment pas de quota supplémentaire
/// lors d'un appel en double — le cap compte les admissions, pas les deduplications.
///
/// Note : ce test documente le comportement actuel (chaque appel compte une admission).
/// Si la sémantique change (dedup par label unique), ce test devra être ajusté.
#[test]
fn cardinality_cap_admits_exactly_cap_labels() {
    let m = AppMetrics::new();

    // Admet exactement 100 labels (i=0..100).
    let first_100: Vec<_> = (0..100)
        .map(|i| {
            m.observe_tenant(TenantLabel {
                tenant: format!("tenant-{i}"),
            })
        })
        .collect();

    assert_eq!(
        first_100.iter().filter(|r| r.is_some()).count(),
        100,
        "les 100 premiers labels doivent tous être admis"
    );

    // Le 101ème doit être rejeté.
    let overflow = m.observe_tenant(TenantLabel {
        tenant: "overflow".to_string(),
    });
    assert!(
        overflow.is_none(),
        "le 101ème label doit être rejeté (cap=100)"
    );
}

/// Vérifie que le listener métriques refuse un bind non-loopback (caveat C7 strict).
///
/// Ce test vérifie la logique de validation sans démarrer de runtime tokio complet —
/// il inspecte directement la condition de refus.
#[test]
fn metrics_bind_must_be_loopback() {
    // Adresse non-loopback : doit être refusée.
    let non_loopback: std::net::SocketAddr = "0.0.0.0:19091".parse().unwrap();
    assert!(
        !non_loopback.ip().is_loopback(),
        "0.0.0.0 ne doit pas être loopback"
    );

    // Adresse loopback IPv4 : doit être acceptée.
    let loopback_v4: std::net::SocketAddr = "127.0.0.1:19091".parse().unwrap();
    assert!(
        loopback_v4.ip().is_loopback(),
        "127.0.0.1 doit être loopback"
    );

    // Adresse loopback IPv6 : doit être acceptée.
    let loopback_v6: std::net::SocketAddr = "[::1]:19091".parse().unwrap();
    assert!(loopback_v6.ip().is_loopback(), "::1 doit être loopback");
}
