//! Les 5 handlers `const TENANT` résolvent leur vault en
//! `VaultId` typé (dimension NAMESPACE), distincte du principal (`TenantId`).
//!
//! Vérifie que la migration `const TENANT: &str = "main"` → construction typée
//! directe `VaultId` :
//! - préserve la valeur littérale `"main"` (byte-identical) ;
//! - expose un point de résolution **typé** par handler (`target_vault()`), dont
//!   le type de retour `VaultId` est vérifié à la compilation (ne compile pas si
//!   la résolution retombe sur un `&str` nu ou une dimension principale `TenantId`).

use gradatum_core::scope::VaultId;
use gradatum_server::api_v1::{dashboard, jobs_v2, project_map, review, system};

/// Chaque handler résout le vault `"main"` typé `VaultId` (namespace).
///
/// Assertion logique unique : les cinq points de résolution renvoient le même
/// `VaultId` = `"main"`. Le type de retour `VaultId` est imposé par la signature
/// (échec de compilation sinon), ce qui couvre l'exigence « typé `VaultId` ».
#[test]
fn each_handler_resolves_main_vault_as_typed_vault_id() {
    let expected = VaultId::new("main");
    assert_eq!(dashboard::target_vault(), expected, "dashboard");
    assert_eq!(system::target_vault(), expected, "system");
    assert_eq!(jobs_v2::target_vault(), expected, "jobs_v2");
    assert_eq!(review::target_vault(), expected, "review");
    assert_eq!(project_map::target_vault(), expected, "project_map");
}
