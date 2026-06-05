//! B7 — JSONL audit throughput via BufWriter 64KB (P1)
//!
//! Mesure le débit d'écriture d'événements `AuditEvent` sérialisés en JSONL
//! via `BufWriter<File>` (pattern audit log Gradatum — caveat C2 / spec §2.9).
//! Target spec §3 : > 50 000 events/sec.
//!
//! Le bench simule l'écriture de 1000 événements `AuditEvent::Created` avec
//! buffer 64KB sur un fichier temporaire NVMe ZFS (ou tmpfs selon OS).

use std::io::{BufWriter, Write as _};

use chrono::Utc;
use criterion::{black_box, criterion_group, criterion_main, Criterion};

use gradatum_core::audit::{AuditEvent, AuditEventType};
use gradatum_core::author::AuthorRef;
use gradatum_core::frontmatter::ExtraFields;
use gradatum_core::identity::NoteId;

/// Construit un `AuditEvent::Created` minimal (event le plus fréquent).
fn make_audit_event() -> AuditEvent {
    AuditEvent {
        note_id: NoteId::new(),
        event_type: AuditEventType::Created,
        actor: AuthorRef::system("bench-worker"),
        occurred_at: Utc::now(),
        extra: ExtraFields::empty(),
        correlation_id: None,
    }
}

fn bench_jsonl_audit(c: &mut Criterion) {
    let mut group = c.benchmark_group("B7-jsonl-audit");
    group.sample_size(20);

    // Batch de 1000 events pré-sérialisés hors boucle de mesure.
    let events: Vec<AuditEvent> = (0..1000).map(|_| make_audit_event()).collect();

    group.bench_function("1000-events-bufwriter-64k", |b| {
        b.iter(|| {
            let tmpfile = tempfile::NamedTempFile::new().expect("tempfile");
            let file = tmpfile.reopen().expect("reopen tempfile");
            let mut writer = BufWriter::with_capacity(64 * 1024, file);

            for event in &events {
                let line = serde_json::to_string(black_box(event)).expect("serde_json::to_string");
                writer
                    .write_all(line.as_bytes())
                    .expect("write_all JSONL line");
                writer.write_all(b"\n").expect("write newline");
            }
            writer.flush().expect("flush BufWriter");
            black_box(writer);
        });
    });

    group.finish();
}

criterion_group!(benches, bench_jsonl_audit);
criterion_main!(benches);
