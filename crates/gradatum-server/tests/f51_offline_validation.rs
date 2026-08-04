//! Harnais de validation OFFLINE rejouable pour F-51 (réglage 2026-07-14).
//!
//! Charge une **COPIE** d'un index gradatum (jamais le LIVE) et rejoue `audit::detect()` sur
//! les notes scannées, puis confronte le résultat au corpus pilote labellisé
//! (`gradatum-meta/f51-corpus-pilote-2026-07-12.md`). Zéro fenêtre LIVE, zéro mutation.
//!
//! Exécution (le test est `#[ignore]` — hors CI par défaut) :
//! ```bash
//! cp /var/lib/gradatum/backups/index-pre-ga2-cold-*.db /tmp/index-copy.db
//! F51_VALIDATION_DB=/tmp/index-copy.db \
//!   cargo test -p gradatum-server --test f51_offline_validation -- --ignored --nocapture
//! ```
//!
//! Critères (dénominateur corrigé : les notes hors scan — `garbage`/`downgraded`/sections
//! protégées — ne comptent pas, elles ne sont pas retournées par `audit_scan`) :
//! - rappel DELETE-tier sur A∪B∪C∪D∪E ≥ 0.95 ;
//! - 0 note de catégorie F en tier Delete ;
//! - 0 contrôle négatif (lot import `01KR3F*` + notes nommées) en tier Delete.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

use gradatum_core::index::Index;
use gradatum_curator::audit::{self, AuditRecord, AuditThresholds, AuditTier};
use gradatum_index::{SqliteIndex, extract_h1_title};

// Corpus pilote labellisé (ground truth) — voir gradatum-meta/f51-corpus-pilote-2026-07-12.md.
const CORPUS_A: &[&str] = &[
    "01KT405YN141FA8XC1SK3BSJE4",
    "01KT42FF790WDMSXE7396MM75C",
    "01KT76KPHT1GA7C34S21TJCCBN",
    "01KT76NMCMR5STXHR8C4SBG0GN",
    "01KTW0XJ1V5PF77CV8ZK6PASEH",
    "01KTYC000000000000000000ZZ",
    "01KTYF4P6DN8RFTY98JKAEW7J0",
    "01KTYF4P6NAYMXRH7DM2QNH0KV",
    "01KTYKY8DDV9NDV9WQAKSV9Z8F",
    "01KTYM1JTT6T4YZBB4CYNR5TNK",
    "01KV2X0DXWXJKZR8R6TNT1D6YH",
    "01KV83XPZBPA24EF1ETK63Z2MR",
    "01KV83XSYJ7SFX280P1S3BZNG9",
    "01KV83XWXTNY700Q5ND1D6MA9D",
    "01KVE9W39490B1053M82WR1KEA",
];
const CORPUS_B: &[&str] = &[
    "01KST9T7VPZ3PRE8KZ6APKSGSN",
    "01KSV41D7YXV7BP60MZQW6E5S8",
    "01KSV60YC13KQJQFDJGKX71XBA",
    "01KT3YXYAJCCSBR00XJXZT2MMY",
    "01KT3ZHDE940796DDEHX0BC6MJ",
    "01KT47KM9F33WW1M3DZCRXH05C",
    "01KTA04SZ6Y86WKMJQ59FYAZSX",
    "01KTBMMV7RRF215MSGM890BWP0",
    "01KTDNKY892Z3QA51ZE26SFE2Y",
    "01KTEGM9223JM9PKFG69HP99NS",
    "01KTGE2NVNH9SBJE33WX9ZD6RP",
    "01KTSXMCQWQP2Y4NSAMWPJQZM4",
    "01KTT0VRX075XXD5QJHGYM8AE6",
    "01KTT8D65TJXKYMST5GVF978A1",
    "01KTTFEQPRHF81R862GSZ4HXRK",
    "01KTW28MKHPZ6HJ2D4G42TVK1S",
    "01KTW5B3JCWM179T6PEE5024RF",
    "01KV83PTXN6KJHD2BHN3B4DEBG",
    "01KVA164X3BY9VVKZ4DFC1G84Q",
    "01KVA20MJVD17QTTC9PM8Q60C0",
    "01KVK4DT7STZE9BWH8WHR17BZN",
    "01KVXV6KSB577C01A1E6F78ZEV",
];
const CORPUS_C: &[&str] = &[
    "01KSQGKGE8EE5FWNGA9RNH1K1Z",
    "01KSQGKNSY8QKPNHVWPR2QSJNF",
    "01KSQGM0GNVZ1G7WWM4PJDWKFN",
];
const CORPUS_D: &[&str] = &[
    "01KT4DWJ2D1MXZFRJBZBNTVT1W",
    "01KT4DWJ97X602VF27T12VBDDH",
    "01KT4DWJFZA8GC0TWA3ZM5J0M5",
    "01KT4DWJPPJ6CMSVBM1RDGSC7R",
    "01KT4DWJXBZTH875KSXJJ03R01",
    "01KT4DWK3SGXQ59V9MFSP7MGW6",
    "01KT4DWKABV7BNVN8WJ7JEZZD9",
    "01KT4DWKGVEXR3MF579PX4XGKC",
    "01KT4WMF87KR5HQRMW8DJTMRQB",
    "01KT4WMFENVX090ME6V7Y25BPW",
    "01KT4WMFN1ZRTJG39PNNFSM0PB",
    "01KT4WMFVFTWBK9ZX2AVFYCNMC",
    "01KT4WMG1ZFJG3QQ6Q1CNNQ22B",
    "01KT4WMG8DWDABTTXJE0VAKAJ5",
    "01KT4WMGEVG8ZHNRKHZ6RA0WDC",
    "01KT4WMGN7KQAE5KZSY51AYNTZ",
    "01KT4WMGVNH0HHPR7KQ1PX6VK4",
    "01KT4WMH22KCEV90RQZYWDGZMB",
    "01KT4WMH8ENV39RBT22CVV8R9K",
    "01KT4WMHEWJ4WX2B9SKDREV7V0",
    "01KT4WMHNA4S5DV98BJ78B7HJW",
    "01KT4WMHVRY97M52QAZ8HRHQJR",
    "01KT4WMJ24GKA4ZZY3F4D51HRG",
    "01KT4WP4MCSX0YAC63N9NW1Q7F",
    "01KT4WP4TRE9CB3YWN9XEN50ZX",
    "01KT4WP5181GZFZGVJXM6PZ3VT",
];
const CORPUS_E: &[&str] = &[
    "01KW82YZRT9ZN1KFY4S3X6N9Z9",
    "01KW8AHB4GTE3XQMBCTPWAJN93",
    "01KW8AJR5Q1ZWZ7ES4C9TC8T4B",
];
const CORPUS_F: &[&str] = &[
    "01KSRKYSTTE05CH81VJE9407SE",
    "01KSRKYXACFGAT8PPGH8HKT8ZR",
    "01KSRKZ801N2TVF7RWQFMDS99V",
    "01KSRKZ9PP6FZ27PX3FQF5P7C1",
    "01KSRKZBF2RSZRHE3H6AJAWYAP",
    "01KSRKZD2VFKH67VEX00DYEEGZ",
    "01KSRKZER9KDPPXZ26MYB4Q5RA",
    "01KSRKZGFGHAYCFKV1RHWAG29T",
    "01KSRKZK59GBHZ89QT14CJB32B",
    "01KSRKZPKAK2F1B9S4JRF7PEC0",
    "01KSRKZSZ65RWD6T508GKCTX9P",
    "01KSRM0NXMWG9D96GSV6B2SHKG",
    "01KSRM0W6FB2MBV8N4G1E8W502",
    "01KSRM13KY00HRKQJHE494ST0X",
    "01KSRM1G9NKKDJ2HD4018YRPF1",
    "01KSRM1QD3MYZZ86WB7CKVJM7D",
    "01KSRM20HC6964K7E8M9TZ4M8S",
    "01KSRM27GQM63689NBDZ3FEF2W",
    "01KSRM296YN3DV270NQDP061WY",
];
// Contrôles négatifs nommés (import 2026-05-08 authentique + fix CI réel).
const NEGATIVE_NAMED: &[&str] = &["01KR39ZMBT2J93FMVA4ZTV76A9", "01KSRCF692XNVR26YRP37FBY33"];

const DELETE_CATS: &[(&str, &[&str])] = &[
    ("A", CORPUS_A),
    ("B", CORPUS_B),
    ("C", CORPUS_C),
    ("D", CORPUS_D),
    ("E", CORPUS_E),
];

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requiert une COPIE de l'index LIVE via F51_VALIDATION_DB (zéro fenêtre LIVE)"]
async fn f51_offline_validation_matrix() {
    let db = std::env::var("F51_VALIDATION_DB")
        .expect("F51_VALIDATION_DB=<chemin d'une COPIE de l'index .db> requis");
    let idx = SqliteIndex::open(Path::new(&db))
        .await
        .expect("ouverture de la copie d'index");
    let index: Arc<dyn Index> = Arc::new(idx);

    let rows = index
        .audit_scan("main", 100_000)
        .await
        .expect("audit_scan sur la copie");
    let scanned: HashSet<String> = rows.iter().map(|r| r.note_id.clone()).collect();
    println!(
        "notes scannées (hors protégées/garbage/downgraded) : {}",
        scanned.len()
    );

    let records: Vec<AuditRecord> = rows
        .into_iter()
        .map(|r| {
            let title = r
                .title
                .filter(|t| !t.trim().is_empty())
                .or_else(|| extract_h1_title(&r.body_text))
                .unwrap_or_else(|| r.note_id.clone());
            AuditRecord {
                id: r.note_id,
                section: r.section,
                title,
                body: r.body_text,
                author_id: r.author_id,
                embedding: r.embedding,
                embedder_id: r.embedder_id,
            }
        })
        .collect();

    let cands = audit::detect(&records, &AuditThresholds::default());
    let by_id: HashMap<&str, &audit::AuditCandidate> =
        cands.iter().map(|c| (c.note_id.as_str(), c)).collect();

    println!("=== MATRICE DELETE-tier (dénominateur corrigé = notes en scan) ===");
    let (mut denom, mut hits) = (0usize, 0usize);
    for (name, ids) in DELETE_CATS {
        let (mut inscope, mut d, mut r, mut absent) = (0usize, 0usize, 0usize, 0usize);
        for u in *ids {
            if !scanned.contains(*u) {
                continue; // hors scan (garbage/downgraded/protégé) → hors dénominateur
            }
            inscope += 1;
            match by_id.get(u) {
                Some(c) if c.tier == AuditTier::Delete => d += 1,
                Some(_) => r += 1,
                None => absent += 1,
            }
        }
        denom += inscope;
        hits += d;
        println!("  cat {name}: in-scan={inscope} delete={d} review={r} absent={absent}");
    }
    let recall = hits as f64 / denom.max(1) as f64;
    println!("  RAPPEL DELETE = {hits}/{denom} = {recall:.3}");

    let f_delete: Vec<&str> = CORPUS_F
        .iter()
        .filter(|u| by_id.get(**u).is_some_and(|c| c.tier == AuditTier::Delete))
        .copied()
        .collect();
    println!("  cat F en delete (doit être 0) : {f_delete:?}");

    let neg_delete: Vec<&str> = cands
        .iter()
        .filter(|c| {
            c.tier == AuditTier::Delete
                && (c.note_id.starts_with("01KR3F") || NEGATIVE_NAMED.contains(&c.note_id.as_str()))
        })
        .map(|c| c.note_id.as_str())
        .collect();
    println!("  négatifs (01KR3F*/nommés) en delete (doit être 0) : {neg_delete:?}");

    assert!(recall >= 0.95, "rappel DELETE {recall:.3} < 0.95");
    assert!(f_delete.is_empty(), "catégorie F en delete : {f_delete:?}");
    assert!(
        neg_delete.is_empty(),
        "contrôles négatifs en delete : {neg_delete:?}"
    );
}
