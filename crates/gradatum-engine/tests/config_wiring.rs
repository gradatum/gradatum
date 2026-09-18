//! F-157 — garde de câblage de configuration (« test de classe »).
//!
//! ## La classe de défaut visée
//!
//! Un champ de configuration désérialisé au runtime, **puis jamais lu** par aucun
//! site de consommation, alors que sa doc décrit un comportement actif. C'était
//! exactement l'incident des deux champs de [`EngineConfig`] `warm_up` (stratégie de
//! préchauffage) et `max_tokens` (plafond de génération) : parsés, validés, puis
//! silencieusement ignorés — détectés seulement au pré-vol d'un tag, à la main
//! (release-guard `G-CONFIG-SCAN` : grep champ → site de consommation).
//!
//! Les deux ont été soldés par F-150 : `warm_up` **retiré** (champ mort, stratégie sans
//! valeur), `max_tokens` **câblé** vers `--n-predict` (autoritaire ; défaut illimité). Ce
//! test verrouille désormais que `max_tokens` reste consommé et que la classe entière
//! reste détectable pour tout futur champ.
//!
//! Ce test **automatise** ce scan pour [`EngineConfig`] et rend la classe entière
//! détectable en CI — pas les deux cas seulement.
//!
//! ## Faisabilité : pourquoi PAS un test 100 % générique
//!
//! Un test qui, par réflexion, énumérerait tous les champs de toute struct de config
//! et prouverait leur consommation est **infaisable en Rust** sans macro ou annotation
//! dédiée : le langage n'expose ni les noms de champs à l'exécution (pas de réflexion),
//! ni un graphe « ce champ atteint un `open`/`connect`/`bind`/`spawn` ». Deux pièges
//! concrets rencontrés ici le confirment :
//!   - **indirection par accesseur** : `bind_addr` / `metrics_port` ne sont jamais lus
//!     directement en production — seulement via `resolved_bind_addr()` /
//!     `resolved_metrics_port()`. Un grep `\.bind_addr` rendrait un faux « orphelin ».
//!   - **collision de nom de méthode** : le champ `port` entre en collision avec la
//!     méthode `SocketAddr::port()`. Un grep `\.port` compterait des `.port()` qui ne
//!     sont pas des lectures du champ.
//!
//! ## Mécanisme borné retenu (robuste, sans macro, sans dépendance)
//!
//! 1. **Complétude au compile-time** — un *déstructuring exhaustif* de [`EngineConfig`]
//!    (sans `..`) : ajouter un champ à la struct casse la compilation de ce test tant
//!    qu'il n'est pas nommé ici ET passé à [`entry`] (donc classé). Retirer un champ
//!    casse aussi la compilation. C'est le piège « un nouveau champ apparaît non classé ».
//! 2. **Registre de classification** — chaque champ est déclaré `Wired { proof }` ou
//!    `NotWired`. `proof` est un jeton discriminant (choisi à la main, comme le fait
//!    `G-CONFIG-SCAN`) qui DOIT apparaître dans les sources de **production** du crate.
//! 3. **Scan de production** — toutes les sources du crate sauf `config.rs`, dont les
//!    modules `#[cfg(test)]` sont retirés (sinon une *fixture* de test référençant un
//!    champ créerait un faux « consommé » et neutraliserait la garde). C'est ce qui rend
//!    vrai « retirer un câblage → le test rougit » : sans le stripping, la ligne de test
//!    `config.gpu_layers = 0` masquerait la suppression de la consommation de production.
//!
//! Défauts détectés (les trois branches de la classe) :
//!   - un `Wired` perd sa consommation (câblage retiré) → jeton absent → ROUGE.
//!   - un `NotWired` perd son annotation `// not yet wired` → marqueur absent → ROUGE.
//!   - un `NotWired` gagne une consommation (câblé sans reclasser) → jeton présent → ROUGE.
//!   - un champ apparaît/disparaît → erreur de **compilation** (déstructuring).
//!
//! ## Portée de cette livraison (F-157, version bornée)
//!
//! Couvre [`EngineConfig`] — la struct de l'incident, désérialisée à l'entrée de
//! `gradatum-engine` (`EngineConfig::load_local`). Le mécanisme se réplique struct par
//! struct ; l'étendre à `ServerConfig` (≈30 sous-structs, 100+ champs feuilles), au
//! `Config` du gateway et aux sections worker est un travail de registre séparé
//! (arbitrage de portée), délibérément hors de cette première livraison.
//!
//! Compiler/exécuter avec : `cargo test -p gradatum-engine --features serve --test config_wiring`
#![cfg(feature = "serve")]

use std::fs;
use std::path::{Path, PathBuf};

use gradatum_engine::config::EngineConfig;

/// Classification d'un champ de configuration vis-à-vis de sa consommation runtime.
enum Wiring {
    /// Le champ est câblé : la valeur atteint un site de consommation. `proof` est un
    /// jeton discriminant qui doit apparaître dans les sources de production du crate
    /// (hors `config.rs`, hors modules de test). Retirer le câblage retire le jeton.
    Wired { proof: &'static str },
    /// Le champ est délibérément parsé-mais-non-consommé. Il DOIT porter le marqueur
    /// `// not yet wired` près de sa déclaration dans `config.rs`, et NE DOIT PAS être
    /// consommé (aucune lecture `.<champ>` en production).
    ///
    /// Depuis F-150, aucun champ de [`EngineConfig`] n'est classé `NotWired` (tous câblés).
    /// La variante — et la branche de détection qui l'accompagne — est conservée : c'est le
    /// vocabulaire de classification de la garde ; un futur champ parsé-mais-non-consommé
    /// devra être classé ici. La retirer amputerait la garde d'une des branches de la classe.
    #[expect(
        dead_code,
        reason = "vocabulaire de classification F-157 conservé pour un futur champ NotWired"
    )]
    NotWired,
}

/// Une entrée du registre : nom du champ + sa classification.
struct Entry {
    name: &'static str,
    wiring: Wiring,
}

/// Construit une entrée en **consommant** la liaison issue du déstructuring.
///
/// Le paramètre `_binding` n'est pas utilisé pour sa valeur : il force le lien mécanique
/// entre le déstructuring exhaustif et le registre. Ajouter un champ à [`EngineConfig`]
/// oblige à le nommer dans le déstructuring (sinon E0027), et la liaison inutilisée n'est
/// silencée qu'en la passant ici — c'est-à-dire en ajoutant une entrée de registre.
fn entry<T>(name: &'static str, _binding: &T, wiring: Wiring) -> Entry {
    Entry { name, wiring }
}

/// Registre de câblage de [`EngineConfig`].
///
/// Le déstructuring exhaustif ci-dessous est la garde de complétude au compile-time :
/// tout ajout/retrait de champ casse la compilation jusqu'à mise à jour du registre.
fn engine_registry(sample: &EngineConfig) -> Vec<Entry> {
    // Déstructuring EXHAUSTIF — aucun `..`. C'est intentionnel : c'est la garde.
    let EngineConfig {
        model_path,
        model_kind,
        runtime,
        gpu_layers,
        n_threads,
        context_len,
        port,
        bind_addr,
        metrics_port,
        timeout_secs,
        max_tokens,
        gradatum_url,
        agent_id,
        llama_server_bin,
        child_port,
        parallel,
        extra_args,
        mmproj_path,
        draft_model_path,
        spec_type,
        spec_draft_n_max,
        spec_draft_p_min,
        startup_timeout_secs,
        child_restart_max,
        min_stable_uptime_secs,
        body_limit_bytes,
    } = sample;

    vec![
        entry(
            "model_path",
            model_path,
            Wiring::Wired {
                proof: ".model_path",
            },
        ),
        entry(
            "model_kind",
            model_kind,
            Wiring::Wired {
                proof: ".model_kind",
            },
        ),
        entry("runtime", runtime, Wiring::Wired { proof: ".runtime" }),
        entry(
            "gpu_layers",
            gpu_layers,
            Wiring::Wired {
                proof: ".gpu_layers",
            },
        ),
        entry(
            "n_threads",
            n_threads,
            Wiring::Wired {
                proof: ".n_threads",
            },
        ),
        entry(
            "context_len",
            context_len,
            Wiring::Wired {
                proof: ".context_len",
            },
        ),
        // `port` : jeton `config.port` pour éviter la collision avec `SocketAddr::port()`.
        entry(
            "port",
            port,
            Wiring::Wired {
                proof: "config.port",
            },
        ),
        // `bind_addr` : lu uniquement via l'accesseur `resolved_bind_addr()`.
        entry(
            "bind_addr",
            bind_addr,
            Wiring::Wired {
                proof: "resolved_bind_addr",
            },
        ),
        // `metrics_port` : lu uniquement via l'accesseur `resolved_metrics_port()`.
        entry(
            "metrics_port",
            metrics_port,
            Wiring::Wired {
                proof: "resolved_metrics_port",
            },
        ),
        // `timeout_secs` : jeton nu `.timeout_secs` conservé — le champ est lu sous PLUSIEURS
        // liaisons en production (`config.timeout_secs` au bin, `s.timeout_secs` dans le
        // handler), un ancrage `config.` produirait un faux-rouge en ignorant l'autre chemin.
        // LIMITE ASSUMÉE : jeton partagé avec un champ homonyme `timeout_secs` sur `AppState`
        // (`s.timeout_secs`, server.rs) ; l'unwiring de ce champ précis peut ne pas rougir
        // tant que l'état lit le sien. TODO durcissement F-157 (registre par (struct, champ)).
        entry(
            "timeout_secs",
            timeout_secs,
            Wiring::Wired {
                proof: ".timeout_secs",
            },
        ),
        // F-150 : plafond de génération câblé — `build_child_args` émet `--n-predict <n>`
        // quand `Some(n)`, et la garde de `LlamaServerSupervisor::new` lit `config.max_tokens`.
        entry(
            "max_tokens",
            max_tokens,
            Wiring::Wired {
                proof: ".max_tokens",
            },
        ),
        entry(
            "gradatum_url",
            gradatum_url,
            Wiring::Wired {
                proof: ".gradatum_url",
            },
        ),
        // `agent_id` : jeton `config.agent_id` (site unique `bin config.agent_id.clone()`).
        // Le jeton nu `.agent_id` entrerait en collision avec `self.agent_id` (champ de
        // `HttpEventSink`, sink.rs) — la lecture du sink maintiendrait le jeton présent même
        // si le câblage de `EngineConfig.agent_id` était retiré → zone morte de détection.
        entry(
            "agent_id",
            agent_id,
            Wiring::Wired {
                proof: "config.agent_id",
            },
        ),
        entry(
            "llama_server_bin",
            llama_server_bin,
            Wiring::Wired {
                proof: ".llama_server_bin",
            },
        ),
        // `child_port` : jeton nu `.child_port` conservé — le champ est lu sous PLUSIEURS
        // liaisons en production (`config.child_port`, `cfg.child_port`, `self.child_port`),
        // un ancrage `config.` produirait un faux-rouge en ignorant les autres chemins.
        // LIMITE ASSUMÉE : jeton partagé avec un champ homonyme `child_port` sur
        // `LlamaServerSupervisor` (`self.child_port`, supervisor.rs) ; l'unwiring de ce champ
        // précis peut ne pas rougir tant que le superviseur lit le sien. TODO durcissement
        // F-157 (registre par (struct, champ) plutôt que par jeton textuel).
        entry(
            "child_port",
            child_port,
            Wiring::Wired {
                proof: ".child_port",
            },
        ),
        entry("parallel", parallel, Wiring::Wired { proof: ".parallel" }),
        entry(
            "extra_args",
            extra_args,
            Wiring::Wired {
                proof: ".extra_args",
            },
        ),
        entry(
            "mmproj_path",
            mmproj_path,
            Wiring::Wired {
                proof: ".mmproj_path",
            },
        ),
        entry(
            "draft_model_path",
            draft_model_path,
            Wiring::Wired {
                proof: ".draft_model_path",
            },
        ),
        entry(
            "spec_type",
            spec_type,
            Wiring::Wired {
                proof: ".spec_type",
            },
        ),
        entry(
            "spec_draft_n_max",
            spec_draft_n_max,
            Wiring::Wired {
                proof: ".spec_draft_n_max",
            },
        ),
        entry(
            "spec_draft_p_min",
            spec_draft_p_min,
            Wiring::Wired {
                proof: ".spec_draft_p_min",
            },
        ),
        entry(
            "startup_timeout_secs",
            startup_timeout_secs,
            Wiring::Wired {
                proof: ".startup_timeout_secs",
            },
        ),
        entry(
            "child_restart_max",
            child_restart_max,
            Wiring::Wired {
                proof: ".child_restart_max",
            },
        ),
        entry(
            "min_stable_uptime_secs",
            min_stable_uptime_secs,
            Wiring::Wired {
                proof: ".min_stable_uptime_secs",
            },
        ),
        // `body_limit_bytes` : jeton `config.body_limit_bytes` (site unique
        // `bin config.body_limit_bytes`). Le jeton nu `.body_limit_bytes` entrerait en
        // collision avec `state.body_limit_bytes` (champ de `AppState`, server.rs) — la
        // lecture de l'état maintiendrait le jeton présent même si le câblage de
        // `EngineConfig.body_limit_bytes` était retiré → zone morte de détection.
        entry(
            "body_limit_bytes",
            body_limit_bytes,
            Wiring::Wired {
                proof: "config.body_limit_bytes",
            },
        ),
    ]
}

// ---------------------------------------------------------------------------
// Scan des sources de production (retrait des modules #[cfg(test)])
// ---------------------------------------------------------------------------

fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// `//` … fin de ligne. `b[i]` est le premier `/`.
fn skip_line_comment(b: &[u8], i: usize) -> usize {
    let mut j = i + 2;
    while j < b.len() && b[j] != b'\n' {
        j += 1;
    }
    j
}

/// `/* … */`. `b[i]` est le premier `/`.
fn skip_block_comment(b: &[u8], i: usize) -> usize {
    let mut j = i + 2;
    while j + 1 < b.len() && !(b[j] == b'*' && b[j + 1] == b'/') {
        j += 1;
    }
    (j + 2).min(b.len())
}

/// Chaîne `"…"` avec échappements. `b[i] == '"'`.
fn skip_string(b: &[u8], i: usize) -> usize {
    let mut j = i + 1;
    while j < b.len() {
        match b[j] {
            b'\\' => j += 2,
            b'"' => {
                j += 1;
                break;
            }
            _ => j += 1,
        }
    }
    j
}

/// Chaîne brute `r"…"` / `r#"…"#`. `b[i] == 'r'`. `None` si ce n'en est pas une.
fn skip_raw_string(b: &[u8], i: usize) -> Option<usize> {
    let mut j = i + 1;
    let mut hashes = 0usize;
    while j < b.len() && b[j] == b'#' {
        hashes += 1;
        j += 1;
    }
    if j >= b.len() || b[j] != b'"' {
        return None;
    }
    j += 1;
    while j < b.len() {
        if b[j] == b'"' {
            let mut k = j + 1;
            let mut cnt = 0;
            while k < b.len() && cnt < hashes && b[k] == b'#' {
                cnt += 1;
                k += 1;
            }
            if cnt == hashes {
                return Some(k);
            }
        }
        j += 1;
    }
    Some(b.len())
}

/// Littéral de caractère `'x'` / `'\n'` ou étiquette de vie `'a`. `b[i] == '\''`.
fn skip_char_or_lifetime(b: &[u8], i: usize) -> usize {
    let n = b.len();
    if i + 1 >= n {
        return i + 1;
    }
    if b[i + 1] == b'\\' {
        // Littéral échappé : jusqu'au `'` fermant.
        let mut j = i + 2;
        while j < n && b[j] != b'\'' {
            j += 1;
        }
        return (j + 1).min(n);
    }
    if i + 2 < n && b[i + 2] == b'\'' {
        return i + 3; // 'x'
    }
    i + 1 // durée de vie / label : consommer seulement l'apostrophe
}

/// Si `src[i..]` commence par une attribute `#[cfg(...)]` dont le prédicat active
/// `test` (`#[cfg(test)]`, `#[cfg(all(test, feature = "serve"))]`, …), renvoie l'indice
/// juste après le `]` fermant. Sinon `None` (attribute non-test, ex. `#[cfg(feature =
/// "serve")]`, ou pas une attribute cfg).
///
/// Conservateur : un prédicat contenant `not` n'est jamais traité comme test (on ne veut
/// pas retirer du code `#[cfg(not(test))]`, qui est du code de production).
fn test_cfg_attr_end(src: &str, i: usize) -> Option<usize> {
    if !src[i..].starts_with("#[cfg(") {
        return None;
    }
    let b = src.as_bytes();
    let n = b.len();
    // Le '(' de `cfg(` est à i+5 ; on apparie les parenthèses (en ignorant les chaînes).
    let mut j = i + 6;
    let mut depth = 1i32;
    while j < n && depth > 0 {
        match b[j] {
            b'"' => j = skip_string(b, j),
            b'\'' => j = skip_char_or_lifetime(b, j),
            b'(' => {
                depth += 1;
                j += 1;
            }
            b')' => {
                depth -= 1;
                j += 1;
            }
            _ => j += 1,
        }
    }
    // `j` est juste après la `)` du `cfg(...)`. On exige le `]` fermant.
    if j >= n || b[j] != b']' {
        return None;
    }
    let pred = &src[i + 6..j - 1];
    if cfg_pred_is_test(pred) {
        Some(j + 1)
    } else {
        None
    }
}

/// `true` si le prédicat `cfg` active la compilation de test : contient l'atome `test`
/// (hors chaîne, borné en mot) et aucun `not` (conservateur).
fn cfg_pred_is_test(pred: &str) -> bool {
    if pred.contains("not") {
        return false;
    }
    let pb = pred.as_bytes();
    let n = pb.len();
    let mut i = 0;
    while i < n {
        if pb[i] == b'"' {
            i = skip_string(pb, i);
            continue;
        }
        if pred[i..].starts_with("test") {
            let before_ok = i == 0 || !is_ident_byte(pb[i - 1]);
            let after = i + 4;
            let after_ok = after >= n || !is_ident_byte(pb[after]);
            if before_ok && after_ok {
                return true;
            }
        }
        i += 1;
    }
    false
}

/// Fin (exclusif) de l'item attribué commençant juste après l'attribute cfg-test, à
/// `attr_end`. D'éventuelles attributes empilées (`#[ignore]`, `#[tokio::test]`) sont
/// franchies. Gère `mod tests { … }` / `fn f() { … }` (fin = `}` appariée) et
/// `use …;` (fin = `;`). Le comptage d'accolades ignore commentaires, chaînes (brutes
/// incluses) et littéraux de caractère.
fn skip_item_after_attr(src: &str, attr_end: usize) -> usize {
    let b = src.as_bytes();
    let n = b.len();
    let mut i = attr_end;
    let mut depth: i32 = 0;
    let mut seen_brace = false;
    while i < n {
        if b[i] == b'/' && i + 1 < n && b[i + 1] == b'/' {
            i = skip_line_comment(b, i);
            continue;
        }
        if b[i] == b'/' && i + 1 < n && b[i + 1] == b'*' {
            i = skip_block_comment(b, i);
            continue;
        }
        if b[i] == b'r'
            && let Some(e) = skip_raw_string(b, i)
        {
            i = e;
            continue;
        }
        if b[i] == b'"' {
            i = skip_string(b, i);
            continue;
        }
        if b[i] == b'\'' {
            i = skip_char_or_lifetime(b, i);
            continue;
        }
        match b[i] {
            b'{' => {
                depth += 1;
                seen_brace = true;
                i += 1;
            }
            b'}' => {
                depth -= 1;
                i += 1;
                if seen_brace && depth == 0 {
                    return i;
                }
            }
            b';' if !seen_brace && depth == 0 => return i + 1,
            _ => i += 1,
        }
    }
    n
}

/// Retire les items `#[cfg(test)]` d'une source Rust et remplace les commentaires par
/// un espace (un jeton mentionné en commentaire n'est pas une consommation réelle).
/// Les chaînes/littéraux sont conservés tels quels.
fn strip_tests_and_comments(src: &str) -> String {
    let b = src.as_bytes();
    let n = b.len();
    let mut out = String::with_capacity(n);
    let mut i = 0;
    while i < n {
        if b[i] == b'/' && i + 1 < n && b[i + 1] == b'/' {
            out.push(' ');
            i = skip_line_comment(b, i);
            continue;
        }
        if b[i] == b'/' && i + 1 < n && b[i + 1] == b'*' {
            out.push(' ');
            i = skip_block_comment(b, i);
            continue;
        }
        if b[i] == b'r'
            && let Some(e) = skip_raw_string(b, i)
        {
            out.push_str(&src[i..e]);
            i = e;
            continue;
        }
        if b[i] == b'"' {
            let e = skip_string(b, i);
            out.push_str(&src[i..e]);
            i = e;
            continue;
        }
        if b[i] == b'\'' {
            let e = skip_char_or_lifetime(b, i);
            out.push_str(&src[i..e]);
            i = e;
            continue;
        }
        if b[i] == b'#'
            && let Some(attr_end) = test_cfg_attr_end(src, i)
        {
            i = skip_item_after_attr(src, attr_end);
            continue;
        }
        let ch = src[i..]
            .chars()
            .next()
            .expect("indice sur une frontière utf8");
        let l = ch.len_utf8();
        out.push_str(&src[i..i + l]);
        i += l;
    }
    out
}

/// `true` si `token` apparaît dans `haystack` suivi d'une frontière de mot à droite
/// (le caractère suivant n'est pas un caractère d'identifiant).
///
/// La frontière gauche est portée par le préfixe littéral du jeton (`.`, `config.`,
/// `resolved_`). La frontière droite distingue `.warm_up` de `.warm_up_state` et
/// `.timeout_secs` de `.startup_timeout_secs`.
fn reads(haystack: &str, token: &str) -> bool {
    let hb = haystack.as_bytes();
    let mut from = 0;
    while let Some(rel) = haystack[from..].find(token) {
        let idx = from + rel;
        let after = idx + token.len();
        if after >= hb.len() || !is_ident_byte(hb[after]) {
            return true;
        }
        from = idx + 1;
    }
    false
}

/// Chemins `.rs` sous `dir`, récursivement.
fn collect_rs(dir: &Path, out: &mut Vec<PathBuf>) {
    for e in fs::read_dir(dir).expect("read_dir du répertoire source") {
        let p = e.expect("entrée de répertoire").path();
        if p.is_dir() {
            collect_rs(&p, out);
        } else if p.extension().is_some_and(|x| x == "rs") {
            out.push(p);
        }
    }
}

/// Racine du crate, résolue à l'exécution.
///
/// La valeur figée à la compilation devient périmée dès qu'un binaire de test est
/// réutilisé depuis un autre répertoire de checkout (artefacts de build partagés) :
/// la variable d'environnement prime, la valeur de compilation ne servant que de
/// repli hors cargo.
fn manifest_dir() -> PathBuf {
    std::env::var_os("CARGO_MANIFEST_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")))
}

/// Corpus de production : concaténation de toutes les sources du crate sauf `config.rs`,
/// modules de test retirés, commentaires neutralisés.
fn production_corpus() -> String {
    let src_dir = manifest_dir().join("src");
    let mut files = Vec::new();
    collect_rs(&src_dir, &mut files);
    files.sort();

    let mut corpus = String::new();
    for f in &files {
        if f.file_name().is_some_and(|n| n == "config.rs") {
            continue;
        }
        let raw = fs::read_to_string(f).expect("lecture d'une source de production");
        corpus.push_str(&strip_tests_and_comments(&raw));
        corpus.push('\n');
    }
    corpus
}

/// Source de `config.rs`, non modifiée (pour la vérification des marqueurs d'annotation).
fn config_source() -> String {
    let path = manifest_dir().join("src").join("config.rs");
    fs::read_to_string(path).expect("lecture de config.rs")
}

/// Vrai si le champ `field` porte un marqueur `not yet wired` dans les ~400 caractères
/// précédant sa déclaration `pub <field>:` dans `config.rs`.
fn has_not_wired_marker(config_src: &str, field: &str) -> bool {
    let decl = format!("pub {field}:");
    let Some(idx) = config_src.find(&decl) else {
        return false; // champ introuvable → échec (force l'investigation)
    };
    // `idx` est une frontière de caractère (début du match). On cherche le marqueur le
    // plus proche AVANT la déclaration et on borne sa distance (≈ le bloc de doc du champ),
    // sans découpe d'octets (config.rs contient des tirets cadratins multi-octets).
    match config_src[..idx].rfind("not yet wired") {
        Some(m) => idx - m < 500,
        None => false,
    }
}

// ---------------------------------------------------------------------------
// Auto-vérification de l'instrument de scan
// ---------------------------------------------------------------------------

/// Le stripper ne doit ni sur-retirer (garder le code de production) ni sous-retirer
/// (supprimer les modules de test). Sans cette garde, une régression du scanner
/// rendrait le test vert par cécité.
#[test]
fn scanner_is_sound() {
    let corpus = production_corpus();
    assert!(
        corpus.contains("build_child_args"),
        "le corpus doit garder le code de production (fn build_child_args) — sur-stripping ?"
    );
    assert!(
        !corpus.contains("#[test]"),
        "le corpus ne doit contenir aucun #[test] — un module de test a survécu au stripping"
    );
    assert!(
        !corpus.contains("mod tests"),
        "le corpus ne doit contenir aucun `mod tests` — module de test non retiré"
    );
    // Témoin STRUCTUREL name-agnostic : les deux témoins ci-dessus (`#[test]`, `mod tests`)
    // ratent un module de test tokio-only nommé autrement — ex. `server.rs::mod
    // transparent_handler` (fns `#[tokio::test]`, aucun `#[test]`, pas de `mod tests`).
    // Or TOUT module de test est gardé par `#[cfg(test)]` (ou `#[cfg(all(test, …))]`), et
    // le stripper retire l'attribut lui-même : après stripping, aucune de ces gardes ne doit
    // subsister. Cet invariant est indépendant du nom du module — il prouve que le stripper
    // a bien franchi CHAQUE région gardée-test, pas seulement celles au nom canonique.
    assert!(
        !corpus.contains("#[cfg(test)]"),
        "le corpus ne doit contenir aucun `#[cfg(test)]` — une région gardée-test a survécu \
         au stripping (module de test au nom non canonique ?)"
    );
    assert!(
        !corpus.contains("cfg(all(test"),
        "le corpus ne doit contenir aucun `cfg(all(test…))` — une région gardée-test \
         composite a survécu au stripping"
    );
}

// ---------------------------------------------------------------------------
// La garde F-157
// ---------------------------------------------------------------------------

/// Chaque champ de [`EngineConfig`] atteint un site de consommation, OU est
/// explicitement annoté « not yet wired » et reste non consommé.
///
/// Vérifié en scannant les sources — pas par relecture. Retirer un câblage d'un champ
/// `Wired` (ou son annotation d'un champ `NotWired`) fait rougir ce test en nommant le
/// champ. Ajouter/retirer un champ casse la compilation (déstructuring exhaustif).
#[test]
fn every_engine_config_field_reaches_a_consumer_or_is_annotated() {
    let sample =
        EngineConfig::from_toml("[engine]\nmodel_path=\"x\"\nmodel_kind=\"chat\"\nport=1\n")
            .expect("la config engine minimale doit se parser");

    let registry = engine_registry(&sample);
    let corpus = production_corpus();
    let config_src = config_source();

    let mut orphans: Vec<String> = Vec::new();

    for e in &registry {
        match &e.wiring {
            Wiring::Wired { proof } => {
                if !reads(&corpus, proof) {
                    orphans.push(format!(
                        "champ `{}` classé Wired mais son jeton de consommation `{}` \
                         est absent des sources de production — câblage retiré ?",
                        e.name, proof
                    ));
                }
            }
            Wiring::NotWired => {
                // (a) le marqueur d'annotation doit être présent près de la déclaration.
                if !has_not_wired_marker(&config_src, e.name) {
                    orphans.push(format!(
                        "champ `{}` classé NotWired mais le marqueur `// not yet wired` \
                         a disparu près de sa déclaration dans config.rs",
                        e.name
                    ));
                }
                // (b) il ne doit PAS être consommé (sinon : câblé sans reclassement).
                let read_token = format!(".{}", e.name);
                if reads(&corpus, &read_token) {
                    orphans.push(format!(
                        "champ `{}` classé NotWired mais il est LU en production (`{}`) — \
                         il a été câblé : le reclasser en Wired",
                        e.name, read_token
                    ));
                }
            }
        }
    }

    assert!(
        orphans.is_empty(),
        "F-157 — {} champ(s) de EngineConfig en défaut de câblage :\n  - {}",
        orphans.len(),
        orphans.join("\n  - ")
    );
}
