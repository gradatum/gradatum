//! B4 — Drift Phase A scan 3 niveaux (P0)
//!
//! Mesure `scan_phase_a()` sur 100 fichiers temp avec checksums pré-enregistrés.
//! Cible : < 100ms / 10K fichiers.
//!
//! Ce bench utilise 100 fichiers réels sur tmpfs (tempdir).
//! La valeur mesurée (100 fichiers) est extrapolée × 100 pour l'estimation 10K.
//!
//! Tous les fichiers sont "stables" (size + prefix correspondent) → court-circuit
//! systématique Niveau 2. Meilleur cas — représente le hot path de production.

use std::io::Write as _;
use std::time::{SystemTime, UNIX_EPOCH};

use criterion::{criterion_group, criterion_main, Criterion};

use gradatum_core::index::{FileChecksumEntry, FileKind};
use gradatum_index::drift::scan_phase_a;
use gradatum_index::SqliteIndex;
use gradatum_storage::FileStorage;

/// Hash SHA-256 d'un contenu.
fn sha256_bytes(data: &[u8]) -> [u8; 32] {
    use sha2::Digest as _;
    sha2::Sha256::digest(data).into()
}

fn bench_drift_phase_a(c: &mut Criterion) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");

    // Prépare 100 fichiers sur tmpfs.
    let tmpdir = tempfile::TempDir::new().expect("tempdir");
    let vault_root = tmpdir.path().to_path_buf();
    let file_content = b"# Note Markdown\n\nContenu de bench pour drift phase A.\n";

    let idx = rt
        .block_on(async { SqliteIndex::open_in_memory().await })
        .expect("SqliteIndex::open_in_memory");

    let now_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;

    let full_hash = sha256_bytes(file_content);
    // prefix-4KB = hash des min(4096, len) premiers bytes.
    // Le contenu de bench est < 4096 bytes → hash complet = hash prefix.
    let prefix_len = file_content.len().min(4096);
    let prefix_hash = sha256_bytes(&file_content[..prefix_len]);

    rt.block_on(async {
        for i in 0..100usize {
            let relative = format!("bench-note-{i:04}.md");
            let abs = vault_root.join(&relative);
            // Écrit le fichier sur disque.
            let mut f = std::fs::File::create(&abs).expect("create bench file");
            f.write_all(file_content).expect("write bench file");

            let entry = FileChecksumEntry {
                relative_path: relative,
                file_kind: FileKind::Note,
                expected_size: file_content.len() as u64,
                expected_hash_prefix_4kb: prefix_hash,
                expected_hash: full_hash,
                expected_mtime: now_secs,
                last_verified: now_secs,
            };
            idx.upsert_file_checksum(&entry)
                .await
                .expect("upsert_file_checksum");
        }
    });

    let mut group = c.benchmark_group("B4-drift-phase-a");
    group.sample_size(20);

    let storage = FileStorage::new(&vault_root).expect("FileStorage bench");

    group.bench_function("scan-100-files-all-stable", |b| {
        b.iter(|| {
            rt.block_on(async {
                let result = scan_phase_a(&storage, &idx)
                    .await
                    .expect("scan_phase_a failed");
                // Tous les fichiers stables → level2_prefix_match == 100.
                assert_eq!(
                    result.level2_prefix_match, 100,
                    "attendu 100 fichiers stable"
                );
                assert_eq!(result.level3_full_hash_mismatch, 0);
                result
            })
        });
    });

    group.finish();
}

criterion_group!(benches, bench_drift_phase_a);
criterion_main!(benches);
