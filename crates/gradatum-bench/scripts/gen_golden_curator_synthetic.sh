#!/usr/bin/env bash
# gen_golden_curator_synthetic.sh — D2.4 (v0.4.8)
#
# Génère un golden-set curator 100% SYNTHÉTIQUE (≥50 cas, zéro donnée perso/IP)
# pour mesurer la robustesse F1 de la classification de section.
#
# Pourquoi un générateur plutôt qu'un .jsonl commité :
#   La règle .gitignore `crates/gradatum-bench/datasets/*.jsonl` (durcie v0.4.4,
#   anti-leak) bloque tout .jsonl. On commit donc CE script (reproductible,
#   auditable) ; le .jsonl de sortie reste local + gitignored.
#
# Schéma de chaque ligne (GoldenNote, curator_f1.rs) :
#   { "path", "title", "body_preview", "expected_section", "section_hint"? }
#
# Routage : chaque titre porte un préfixe canonique (ex. [DECISIONS], [RETRO])
# qui force le fast-path `heuristic_route` → label déterministe. Quelques cas
# sans préfixe stressent le chemin sémantique par mots-clés.
#
# Usage :
#   bash crates/gradatum-bench/scripts/gen_golden_curator_synthetic.sh [OUT]
#   CURATOR_GOLDEN_PATH=<OUT> cargo run -p gradatum-bench --bin curator_f1
#
# Défaut OUT : crates/gradatum-bench/datasets/golden-set-curator-synthetic-v2.jsonl
set -euo pipefail

OUT="${1:-crates/gradatum-bench/datasets/golden-set-curator-synthetic-v2.jsonl}"
mkdir -p "$(dirname "$OUT")"

# ── Émetteur JSONL : section, préfixe titre, slug path, body, [hint] ──────────
emit() {
  local section="$1" prefix="$2" slug="$3" body="$4" hint="${5:-}"
  local title="${prefix}[demo-project] ${slug}"
  if [[ -n "$hint" ]]; then
    printf '{"path":"synthetic/%s/%s.md","title":%s,"body_preview":%s,"expected_section":"%s","section_hint":"%s"}\n' \
      "$section" "$slug" "$(json_str "$title")" "$(json_str "$body")" "$section" "$hint"
  else
    printf '{"path":"synthetic/%s/%s.md","title":%s,"body_preview":%s,"expected_section":"%s"}\n' \
      "$section" "$slug" "$(json_str "$title")" "$(json_str "$body")" "$section"
  fi
}

# Échappe une chaîne en littéral JSON (guillemets + backslash + retours ligne).
json_str() {
  local s="$1"
  s="${s//\\/\\\\}"
  s="${s//\"/\\\"}"
  printf '"%s"' "$s"
}

{
  # ── decisions (préfixe [DECISIONS]) — 6 cas ───────────────────────────────
  emit decisions "[DECISIONS]" "go-migration-batch-A"   "GO acté sur la migration batch A, trade-off perf vs simplicité tranché."
  emit decisions "[DECISIONS]" "rejet-option-cache-lru" "Option cache LRU rejetée, on garde le ARC actuel (NOK sur la mémoire)."
  emit decisions "[DECIS]"     "choix-runtime-tokio"    "On a choisi le runtime multi-thread, decision validée."
  emit decisions "[DECISIONS]" "format-json-vs-toml"    "Picked TOML pour la config, JSON gardé pour les payloads API."
  emit decisions "[DECISIONS]" "gel-public-maintenu"    "Decision : gel des releases publiques maintenu jusqu'au prochain milestone." "decisions"
  emit decisions "[DECISIONS]" "scope-lot-d2-acte"      "Scope du lot D2 acté : hygiène backend sans bump version."

  # ── council (préfixe [COUNCIL]) — 5 cas ───────────────────────────────────
  emit council "[COUNCIL]" "verdict-art19-prompt"   "Verdict council Art.19 4/4 GO sur le prompt curator v2, caveats intégrés."
  emit council "[COUNCIL]" "art15bis-homelab-flotte" "Délibération multi-experts art15bis sur la flotte engines, GO-CAVEATS."
  emit council "[COUNCIL]" "leader-override-modeles" "Leader-override acté, modèles agents mis à jour pour la fenêtre courante." "council"
  emit council "[COUNCIL]" "art18-amendement-const"  "Council Art.18 sur l'amendement de la constitution, 3 voix requises."
  emit council "[COUNCIL]" "rollback-decision-go"    "Verdict council : rollback armé non déclenché, smoke 5/5 vert."

  # ── architecture (préfixe [ARCH]) — 5 cas ─────────────────────────────────
  emit architecture "[ARCH]"         "trait-index-store"    "Le trait IndexStore découple le module de persistance, pattern decorator."
  emit architecture "[ARCHITECTURE]" "crate-server-layers"  "Architecture en couches du crate server : handlers, state, scoring."
  emit architecture "[ARCH]"         "protocol-rrf-fusion"  "Protocol de fusion RRF entre BM25 et sémantique, module de reranking." "architecture"
  emit architecture "[ARCH]"         "component-circuit-breaker" "Component circuit breaker en decorator autour du backend LLM."
  emit architecture "[ARCH]"         "module-vault-lifecycle"    "Module vault lifecycle : write path CoW, index SQLite, FTS5."

  # ── debug (préfixe [DEBUG]) — 5 cas ───────────────────────────────────────
  emit debug "[DEBUG]" "panic-unwrap-empty-vec"  "Crash sur un unwrap d'un vec vide, fix par un guard de longueur."
  emit debug "[DEBUG]" "oom-batch-ingest"        "OOM pendant l'ingest batch, bug de taille de fenêtre trop large."
  emit debug "[DEBUG]" "fts5-no-such-column"     "Error FTS5 no such column sur un token avec tiret, fix par quoting." "debug"
  emit debug "[DEBUG]" "fail-409-conflict"       "Fail HTTP 409 conflict sur double upsert, fix par content_hash idempotent."
  emit debug "[DEBUG]" "timeout-probe-halfopen"  "Timeout sur la probe halfopen, le circuit ne se fermait jamais."

  # ── reasoning (préfixe [REASONING]) — 4 cas ───────────────────────────────
  emit reasoning "[REASONING]" "why-arc-over-clone"   "Pourquoi Arc plutôt que clone : because la collection est volumineuse."
  emit reasoning "[REASON]"    "tradeoff-cpu-vs-gpu"  "Tradeoff CPU vs GPU pour l'embedding, on considère la latence cible."
  emit reasoning "[REASONING]" "option-libsql-fts5"   "Option libsql FTS5 : hypothèse de gain, à valider par un spike." "reasoning"
  emit reasoning "[REASONING]" "why-deterministic-clock" "Why une horloge déterministe : because les sleeps flakent sous charge."

  # ── feedback (préfixe [FEEDBACK]) — 4 cas ─────────────────────────────────
  emit feedback "[FEEDBACK]" "review-api-naming"      "Review : le naming des endpoints manque de cohérence, comment ajouté."
  emit feedback "[FEEDBACK]" "praise-latence-search"  "Praise : la latence de search est excellente, critic mineure sur le tri."
  emit feedback "[FEEDBACK]" "comment-doc-sparse"     "Comment : la doc des fonctions publiques est trop sparse." "feedback"
  emit feedback "[FEEDBACK]" "critic-error-messages"  "Critic : les messages d'erreur exposent trop de détails internes."

  # ── lessons-learned (préfixe [LESSONS]) — 5 cas ───────────────────────────
  emit lessons-learned "[LESSONS]"         "avoid-self-now-static" "Lesson learned : avoid une fn now_ms statique, préférer une méthode."
  emit lessons-learned "[LESSON]"          "always-backup-before-write" "Takeaway : always faire un backup avant une écriture destructrice."
  emit lessons-learned "[LESSONS-LEARNED]" "gitignore-jsonl-leak"  "Learned : un .jsonl perso peut leaker, le gitignore datasets le bloque." "lessons-learned"
  emit lessons-learned "[LESSONS]"         "avoid-lock-across-await" "Lesson : avoid de tenir un lock à travers un await, scoper le guard."
  emit lessons-learned "[LESSONS]"         "always-validate-input"   "Takeaway : always valider longueur et format de tout input externe."

  # ── retrospectives (préfixe [RETRO]) — 5 cas ──────────────────────────────
  emit retrospectives "[RETRO]"          "sprint-5-what-went-well" "Retro sprint 5 : what went well le pipeline, to improve les flakes."
  emit retrospectives "[RETROS]"         "phase-distillation-bilan" "Retrospective de la phase distillation, bilan des deploys internes."
  emit retrospectives "[RETROSPECTIVE]"  "milestone-v048-bilan"    "Retro milestone v0.4.8 : dette soldée, surface assainie." "retrospectives"
  emit retrospectives "[RETRO]"          "sprint-4-to-improve"     "Retro sprint 4 : to improve la couverture E2E, sprint suivant cadré."
  emit retrospectives "[RETRO]"          "phase-backends-review"   "Phase review backends : worker dyn livré, parity suite verte."

  # ── experiments (préfixe [EXP]) — 4 cas ───────────────────────────────────
  emit experiments "[EXP]"          "spike-libsql-vs-rusqlite" "Spike libsql vs rusqlite, benchmark de l'ouverture FTS5."
  emit experiments "[EXPERIMENTS]"  "poc-clock-injection"      "POC injection horloge sur le circuit breaker, hypothesis validée."
  emit experiments "[EXPE]"         "benchmark-rrf-k-tuning"   "Experiment : tuning du k RRF, benchmark sur le golden-set." "experiments"
  emit experiments "[EXP]"          "spike-fastembed-cpu"      "Spike fastembed CPU, mesure de la latence froid/chaud."

  # ── agent-issues (préfixe [ISSUES]) — 4 cas ───────────────────────────────
  emit agent-issues "[AGENT-ISSUES]" "skill-fail-recall"       "Agent skill fail sur le recall, pipeline error en amont de la classification."
  emit agent-issues "[ISSUES]"       "coord-double-write"      "Coord issue : double-write non formalisé, agent a perdu la trace."
  emit agent-issues "[ISSUE]"        "agent-stale-memory"      "Agent issue : mémoire de session périmée citée comme source." "agent-issues"
  emit agent-issues "[AGENT-ISSUE]"  "pipeline-skip-gate"      "Pipeline error : un gate sauté, l'agent a contourné une règle bloquante."

  # ── reference (préfixe [REF]) — 4 cas ─────────────────────────────────────
  emit reference "[REF]"       "ports-services-table"  "Reference : table des ports des services et de leurs variables env."
  emit reference "[REFERENCE]" "cheatsheet-fts5-quote" "Cheatsheet du quoting FTS5 : tokens, phrases, doublage des guillemets."
  emit reference "[REF]"       "config-trust-decay"    "Reference config : demi-vies de decay-trust par provenance, spec." "reference"
  emit reference "[REF]"       "spec-golden-schema"    "Spec du schéma golden-set : path, title, body, expected_section."

  # ── Cas sémantiques SANS préfixe (chemin keyword) — 5 cas ─────────────────
  # Mots-clés denses (≥3 hits, dominance 1.5x) pour franchir le seuil heuristique.
  emit debug ""    "crash-panic-fix-loop"     "bug crash panic error fail fix : la boucle de restart révèle un panic récurrent."
  emit architecture "" "trait-crate-module-pattern" "architecture component pattern crate trait module protocol : design du module."
  emit experiments ""  "poc-spike-benchmark-hyp"    "experiment spike POC benchmark hypothesis : on teste la latence d'ingest."
  emit retrospectives "" "sprint-retro-phase-review" "retrospective retro sprint phase : what went well, to improve la CI."
  emit council ""  "council-verdict-delib"     "council verdict art19 art18 leader-override multi-experts délibération acté."
} > "$OUT"

COUNT="$(wc -l < "$OUT" | tr -d ' ')"
echo "Golden-set synthétique généré : $OUT ($COUNT cas)"
if [[ "$COUNT" -lt 50 ]]; then
  echo "ERREUR : < 50 cas ($COUNT) — robustesse F1 insuffisante (D2.4)" >&2
  exit 1
fi
echo "OK : ≥50 cas, 11 sections, 100% synthétique (zéro donnée perso)."
