# Tests gradatum-storage

## Tests actifs (exécutés par CI)

| Fichier | Tests | Description |
|---|---|---|
| `file_storage.rs` | 9 | Round-trip read/write, not-found, delete, list, exists, stat, overwrite (backend `fs`) |
| `storage_factory.rs` | 3 | `build_storage` : défaut → `fs` + round-trip, service inconnu échoue, `s3` sans `bucket` échoue |
| `path_traversal_guard.rs` | 12 | Garde anti-traversal (`validate_relative_path`) sur tous les points d'entrée du trait |

## Test ignoré (gating)

### `s3_round_trip_real` (`storage_factory.rs`)

Round-trip de bout en bout contre un vrai endpoint S3. `#[ignore]`d : ne tourne jamais dans
la suite normale. Nécessite la feature `s3` et un environnement provisionné — s'auto-saute
sinon (`GRADATUM_S3_TEST_ENDPOINT` absent).

**Activation** :

```bash
export AWS_ACCESS_KEY_ID="<your-access-key-id>"
export AWS_SECRET_ACCESS_KEY="<your-secret-access-key>"
export GRADATUM_S3_TEST_ENDPOINT="<your-s3-endpoint-url>"
export GRADATUM_S3_TEST_BUCKET="<your-bucket>"
export GRADATUM_S3_TEST_REGION="<your-region>"   # optionnel

cargo test -p gradatum-storage --features s3 -- --ignored s3_round_trip_real
```

Aucun secret n'est porté par le test lui-même — les identifiants sont chargés par OpenDAL
depuis les variables d'environnement standard `AWS_*`.
