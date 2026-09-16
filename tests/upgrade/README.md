# Harnais de montée de version 2.3.1 → 2.4.0

Ce répertoire contient les trois pièces du test de montée, et rien d'autre :

| Fichier | Rôle | Écrit par |
|---|---|---|
| `gen-fixture.sh` | génère un état gradatum **2.3.1** à partir d'une fixture **texte** versionnée | lot E, tâche 1 |
| `fixture/` | la fixture texte : 43 notes + 5 en attente, `server.toml.tmpl`, preset ACL, manifeste | lot E, tâche 1 |
| `run-upgrade-test.sh` | monte cet état avec le binaire **2.4.0**, mesure, exerce le retour arrière | lot E, tâche 2 (ce fichier) |
| `assert-parity.sh` | compare deux relevés et rend un verdict nommé — **ne connaît aucun binaire** | lot E, tâche 2 |

Le harnais est branché en CI par le job `upgrade-test` de `.forgejo/workflows/ci.yml`,
lui-même inscrit dans `validate[]` (commit `a2d0b99d`).

## Ce que le harnais prouve

Quatre relevés à **tolérance zéro**, plus une épreuve de retour arrière :

1. **Migrations** — `_sqlx_migrations` (queue.sqlite) *et* `_schema_migrations`
   (index.db) comparées **ligne à ligne**, `installed_on` et `checksum` compris.
   Comparer `(version, checksum, success)` laisserait passer une **réapplication**
   (qui ne change que `installed_on`) ; la clé retenue la voit. Aucune ligne
   `success != 1` des deux côtés.
2. **Listing** — `vault_list` : identifiants rendus **dans l'ordre rendu**, plus le
   tri. Une montée qui réordonne le listing est une divergence, même à contenu égal.
3. **Recherche** — 5 requêtes littérales versionnées dans le harnais, identifiants
   dans l'ordre rendu, requête par requête.
4. **Export de cartes** — `gradatum-admin … export-features`, **octet à octet**
   (sha256 + `cmp`), pas une égalité sémantique.
5. **Comptes par table** — `notes` et `gradatum_jobs` à tolérance zéro (lettre du
   brief), plus une liste **nommée** de tables dérivées du corpus. Toute autre table
   est **reportée** avec son delta sans bloquer : leur contenu dépend du trafic
   (télémétrie, journaux), pas du corpus.
6. **File de travaux** — digest ordonné `(id, statut, kind)`, plus strict que le seul
   compte.
7. **Retour arrière exercé** — le binaire **2.3.1** est redémarré sur le répertoire
   *monté*. Ce n'est pas une hypothèse : c'est un démarrage réel, dont le
   `build_sha` et la `version` sont **lus sur `/health`**, jamais déduits d'un chemin.

Chaque mesure est **écrite** sous `--out` ; rien n'est seulement affiché.

## Ce que le harnais NE prouve PAS

Il faut le dire aussi nettement, sinon le vert se lit comme une couverture qu'il n'a pas.

- **Il ne teste pas le retour arrière *binaire* au sens du déploiement** — c'est-à-dire
  le fait de remettre l'ancien binaire en production. Il prouve qu'un binaire 2.3.1
  *démarre* et *sert les mêmes mesures* sur l'état monté. La remise en production
  elle-même (ordonnancement, arrêt propre, bascule de trafic) n'est pas ici.
- **Il ne teste aucun dérivé reconstruit** (embeddings, index ANN, résumés). La fixture
  porte ces tables mais le harnais ne les régénère pas.
- **La copie de la base LIVE est un pré-vol opérateur, hors CI.** Le harnais ne va
  jamais lire l'état de production : il ne travaille que sur un état produit par
  `gen-fixture.sh`. Copier la base réelle, la monter à blanc, comparer — c'est un geste
  manuel, documenté dans le runbook de montée, pas une étape CI.
- **L'axe F-XX est exercé sur la fixture, pas sur le parc.** Voir ruling E4 ci-dessous.
- **`/health` n'atteint jamais `healthy` sur cette fixture** (capacités critiques
  `embed`/`curator`/`worker` injoignables). C'est une **mesure**, pas un échec : le
  harnais consigne `premier_healthy_s=never` après un plancher de 60 s. Le harnais ne
  conclut donc rien de l'état de santé — il conclut sur les relevés.

## Interface

```bash
bash tests/upgrade/run-upgrade-test.sh \
    --state "$GRADATUM_UPGRADE_STATE" \
    --admin-bin-231  <…>/gradatum-admin  \
    --server-bin-231 <…>/gradatum-server \
    --admin-bin-240  <…>/gradatum-admin  \
    --server-bin-240 <…>/gradatum-server \
    --out "$GRADATUM_UPGRADE_OUT" \
    --port 19290
```

Codes : `0` parité prouvée · `1` divergence, **chaque écart nommé** · `2` **NON
EXÉCUTÉ** (binaire absent, port occupé, état illisible, outil requis absent,
`--port` en contradiction avec la configuration de l'état, réalignement du preset
ACL refusé). **Un `2` n'est jamais un vert** : c'est une mesure qui n'a pas eu
lieu. Un `2` n'est pas non plus une divergence : le `1` est réservé à un écart
**mesuré** entre l'AVANT et l'APRÈS, jamais à une incapacité de l'outillage.

Le port est sondé **par bind réel** avant toute autre action : si `19290` écoute déjà,
le harnais sort en `2` sans rien démarrer. Toute attente est une boucle `sleep` **en
avant-plan bornée**. Les serveurs sont arrêtés **par PID capturé**, jamais par motif.

### Options hors commande CI

- `--worker-bin-240 <chemin>` — sans lui, l'axe F-XX (ruling E4) est exercé jusqu'à
  l'**enfilement** : l'allocation du numéro et l'injection du rôle sont prouvées sur le
  travail enfilé, mais la **matérialisation** de la carte ne l'est pas (elle exige un
  consommateur de file). Avec lui, la carte est matérialisée et l'assertion porte sur sa
  présence dans `vault_list`. La CI ne construit pas de worker 2.4.0 : la ligne CI reste
  la commande ci-dessus.
- `--fault-inject delete-note` — **diagnostic**, preuve de rouge : retire une note de
  l'index entre le relevé AVANT et le relevé APRÈS. Doit rendre `1` **et nommer la
  note**. Jamais `0` dans ce mode.

### Récapitulatif de durées

`$GRADATUM_UPGRADE_PHASES` (exportée par le job via `$GITHUB_ENV`, relue par le step de
récapitulatif) reçoit une ligne par phase : `phase=<nom> duree=<n>s`. Défaut :
`<out>/phases.txt`.

## Le réalignement du preset ACL — à faire AVANT de déployer

Point le plus important de ce répertoire, et le seul qui puisse coûter une production.

En 2.4.0, le champ `sees_personal_classified` a été **retiré** des entrées de
consommateur du preset ACL (F-297). Un preset qui le porte encore **ne se parse pas**.
Le serveur **démarre quand même**, `/health` répond, et le chargeur retombe sur
l'**engine vide = DENY-ALL** : toute requête authentifiée est refusée en `403` (corps
vide), sans jeton en `401`. **Aucun démarrage ne rate** — c'est ce qui rend le mode
d'échec dangereux : la montée paraît réussie et le service est muet.

Ce n'est pas une hypothèse : c'est **mesuré** par le harnais au sous-step **2b**, qui
démarre volontairement 2.4.0 sur le preset hérité, sonde une lecture et consigne
`preset_deny_all_mesure=1` avec `lecture_http=403` et le verdict `DÉGRADATION CONFIRMÉE`.

La remédiation est celle du guide `docs/UPGRADING-2.3.1-to-2.4.0.md`
§ « The ACL preset field removal (F-297) », et le harnais l'applique **avant** de monter
(sous-steps **2c** puis **2d**), dans cet ordre :

1. retirer la clé de chaque `[[consumer]]` ;
2. pour les consommateurs qui portaient `false` (ou rien, le défaut), **préfixer** quatre
   motifs de négation — `!personal-classified`, `!personal-classified/**`,
   `!**/personal-classified`, `!**/personal-classified/**` — à `read_patterns` **et**
   `write_patterns` ; un consommateur à `true` ne reçoit **aucune** négation ;
3. **contrôler** le preset réaligné, **puis** déployer.

Le contrôle utilise `gradatum-admin acl validate --preset <chemin>`, qui n'existe
**qu'en 2.4.0**. rc `0` = valide ; rc `1` nomme la clé fautive sur stderr. Quand le
binaire fourni ne porte pas la sous-commande (cas du 2.3.1, et de la preuve de rouge (i)),
le harnais consigne `preset_controle_acl_validate=INDISPONIBLE` : c'est un **manque de
capacité nommé**, non bloquant, et le contrôle restant est la lecture du journal au
sous-step **2d** (marqueurs `DENY-ALL`, `ACL preset unreadable`, …).

**Le réalignement est réversible.** Vérifié par mesure (sous-step **3bis-0**), pas déduit :
`sees_personal_classified` est `#[serde(default)]` en 2.3.1 et la négation `!` y existe
déjà (deny-wins) — un preset réaligné **charge en 2.3.1** et les lectures y fonctionnent.
Le retour arrière est donc **binaire seul**, sans retour de configuration. Le harnais
consigne `preset_reversibilite_231=compatible (retour binaire seul suffisant)`.

**Si le réalignement ne peut pas être automatisé, le harnais le DIT et s'arrête.** Un
`read_patterns`/`write_patterns` écrit sur plusieurs lignes n'est pas réécrit au jugé :
le harnais rend `2` (NON EXÉCUTÉ), **sans modifier le preset de l'état**, avec le motif
sur stderr. Un preset à moitié migré serait pire qu'une non-exécution — et un `1` ferait
lire « divergence mesurée » là où aucune mesure n'a eu lieu.

## Axe F-XX (ruling E4)

Après les quatre relevés — et **après seulement**, parce qu'il **écrit** dans l'état —
le harnais crée une carte projet avec l'`gradatum-admin` 2.4.0, puis :

- asserte la **présence** de la carte (et, avec `--worker-bin-240`, sa matérialisation) ;
- asserte l'**existence du rôle `[[feature:F-N]]`** sur la carte ;
- lit `N` **dans la réponse** et vérifie la cohérence `feature == "F-" + number`,
  **rembourrage ignoré** (le serveur rend `F-01` pour `number: 1`) ;
- n'asserte **jamais** la valeur de `N` : sur une base réelle elle est arbitraire.

## Cas nommé `PrivacyRescan` — point d'extension (ruling E6)

Le Step 3bis du brief demande d'enfiler, depuis 2.4.0, un `Job` de la variante nommée
`PrivacyRescan`. **Cette variante n'existe pas dans cet arbre.** Le harnais implémente le
Step 3bis jusqu'à la 4ᵉ mesure et s'arrête là.

Le point d'extension est la fonction `rollback_named_case` (phase `step3ter-cas-nomme`).
Tant que le cas est indisponible, elle rend exactement :

```
cas_nomme_privacy_rescan=NON EXECUTE (point d extension, ruling E6)
```

Ce rendu n'est **compté ni comme un vert, ni comme une divergence** : c'est une
non-exécution déclarée. Le harnais **ne simule pas** le cas. Le jour où la variante
existe dans l'arbre, c'est cette fonction seule qu'il faut remplir.

## Preuves de rouge (hors CI)

Trois passes exécutées à la main, chacune sur un état frais, pour montrer que le harnais
sait **dire non** — un harnais jamais rouge ne prouve rien :

| Passe | Montage | Attendu | Obtenu |
|---|---|---|---|
| (i) | binaire 2.3.1 des **deux** côtés | le contrôle ACL est `INDISPONIBLE`, aucune dégradation `DENY-ALL` mesurée | `0`, `2b … aucune dégradation (lecture HTTP 200)` |
| (ii) | `--fault-inject delete-note` | `1`, la note disparue est **nommée** | `1`, note nommée + 4 écarts nommés |
| (iii) | port `19290` déjà en écoute | `2`, refus **avant** tout démarrage | `2`, `NON EXÉCUTÉ — port 19290 déjà en écoute` |

## `assert-parity.sh` est rejouable seul

Il ne connaît ni binaire, ni port, ni serveur : il ne lit que des fichiers. Deux relevés
conservés suffisent à rejouer la comparaison, sans rien redémarrer.
Voir l'en-tête du script pour la force exacte de chaque clé (a)–(g) et la raison d'être
de chacune.
