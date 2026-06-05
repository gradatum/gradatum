# Tests gradatum-storage

## Tests actifs (exécutés par CI)

| Fichier | Tests | Description |
|---|---|---|
| `file_storage.rs` | 9 | Round-trip read/write, not-found, delete, list, exists, stat, overwrite |
| `nfs_check.rs` | 2 actifs | Local path OK, chemin inexistant avec parent local OK |

## Tests ignorés (gating)

### `nfs_path_returns_error`

Vérifie que `ensure_local_filesystem` retourne `StorageError::Core(VaultOnNfs)` sur un montage NFS réel.

**Pourquoi ignoré** : requiert un montage NFS sur la machine de test — absent en CI standard.

**Activation** :

```bash
GRADATUM_TEST_NFS_PATH=/mnt/nfs/gradatum-data \
  cargo test -p gradatum-storage -- --ignored nfs_path_returns_error
```

Remplacer `/mnt/nfs/gradatum-data` par le chemin réel d'un montage NFS disponible sur votre machine.

**Vérification que le chemin est bien NFS** :

```bash
stat -f /mnt/nfs/gradatum-data   # doit afficher Type: nfs
# ou
findmnt -n -o FSTYPE /mnt/nfs/gradatum-data   # doit afficher nfs ou nfs4
```
