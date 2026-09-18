# Harnais de montée de version 2.4.0 → 2.5.0

Ce répertoire contient les trois pièces du test de montée, et rien d'autre :

| Fichier | Rôle | Écrit par |
|---|---|---|
| `gen-fixture.sh` | génère un état gradatum **2.4.0** à partir d'une fixture **texte** versionnée | lot E, tâche 1 |
| `fixture/` | la fixture texte : 43 notes de corpus + 2 cartes de fonctionnalité créées par le serveur + 5 travaux en attente, `server.toml.tmpl`, preset ACL, manifeste | lot E, tâche 1 |
| `run-upgrade-test.sh` | monte cet état avec le binaire **2.5.0**, mesure, exerce le retour arrière | lot E, tâche 2 (ce fichier) |
| `assert-parity.sh` | compare deux relevés et rend un verdict nommé — **ne connaît aucun binaire** | lot E, tâche 2 |
| `assert-parity-tests.sh` | auto-test de `assert-parity.sh` — banc **hermétique** : il assemble ses deux relevés lui-même, sans binaire, sans port, sans état | lot E, tâche 3 |

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
4. **Export de cartes** — `gradatum-admin … export-features`, comparé **selon le
   sens de la comparaison** :

   - **montée (2.4.0 → 2.5.0), force `additif`** — l'export est un **rapport** :
     entre deux versions, son dialecte peut **gagner** des clés, et c'est une
     évolution voulue, pas une divergence. La règle est donc : chaque entrée du
     relevé AVANT doit être **égale à l'entrée du relevé MONTÉ projetée sur les
     clés du dialecte AVANT** (même nombre de cartes, même ordre) ; une clé du
     dialecte AVANT qui **disparaît** ou dont la **valeur change** est un écart
     **bloquant** ; les clés que le MONTÉ **ajoute** sont **relevées**
     (`cles_ajoutees=…`) et doivent être **citées dans le guide de montée**
     (`docs/UPGRADING-2.4.0-to-2.5.0.md`). Sans guide lisible, la clé (e) n'est
     **pas mesurée du tout** (`2`, jamais un vert) : un ajout de clé **non
     documenté** est précisément ce que cette force existe pour attraper.
   - **retour arrière (2.4.0 → 2.4.0), force `strict`** — c'est le **même binaire
     des deux côtés** : **octet à octet** (sha256 + `cmp`), pas même un champ ne
     bouge. C'est cette comparaison-là qui garde la force maximale.

   Une comparaison d'octets est vraie de deux exports **vides** : elle serait
   alors satisfaite sans rien couvrir. Chaque relevé est donc d'abord **asserté
   non vacue** — lisible, et portant au moins `feature_cards` cartes (compte lu
   dans le manifeste de la fixture). Un relevé qui échoue à cette assertion est
   **nommé** et compté dans `export_features_releves_non_conformes` ; le harnais
   rend `1`, jamais la parité. L'état porte ces cartes parce que `gen-fixture.sh`
   les y crée lui-même par `create-feature` (voir `fixture/README.md`).
5. **Comptes par table** — `notes` et `gradatum_jobs` à tolérance zéro (lettre du
   brief), plus une liste **nommée** de tables dérivées du corpus. Toute autre table
   est **reportée** avec son delta sans bloquer : leur contenu dépend du trafic
   (télémétrie, journaux), pas du corpus.
6. **File de travaux** — digest ordonné `(id, statut, kind)`, plus strict que le seul
   compte.
7. **Retour arrière exercé** — le binaire **2.4.0** est redémarré sur le répertoire
   *monté*. Ce n'est pas une hypothèse : c'est un démarrage réel, dont le
   `build_sha` et la `version` sont **lus sur `/health`**, jamais déduits d'un chemin.

Chaque mesure est **écrite** sous `--out` ; rien n'est seulement affiché.

## Ce que le harnais NE prouve PAS

Il faut le dire aussi nettement, sinon le vert se lit comme une couverture qu'il n'a pas.

- **Il ne teste pas le retour arrière *binaire* au sens du déploiement** — c'est-à-dire
  le fait de remettre l'ancien binaire en production. Il prouve qu'un binaire 2.4.0
  *démarre* et *sert les mêmes mesures* sur l'état monté. La remise en production
  elle-même (ordonnancement, arrêt propre, bascule de trafic) n'est pas ici.
- **Il ne teste aucun dérivé reconstruit** (embeddings, index ANN, résumés). La fixture
  porte ces tables mais le harnais ne les régénère pas.
- **La copie de la base LIVE est un pré-vol opérateur, hors CI.** Le harnais ne va
  jamais lire l'état de production : il ne travaille que sur un état produit par
  `gen-fixture.sh`. Copier la base réelle, la monter à blanc, comparer — c'est un geste
  manuel, documenté dans le runbook de montée, pas une étape CI. Cette copie se fait
  **en local, jamais au travers d'un montage réseau** : un état monté sur un système de
  fichiers distant fausserait les mesures de verrouillage et de durabilité, et le
  harnais lui-même ne saurait pas le distinguer d'un état local.
- **L'axe F-XX est exercé sur la fixture, pas sur le parc.** Voir ruling E4 ci-dessous.
- **Il ne prouve pas la montée d'un `server.toml` réel d'opérateur.** La fixture rend
  un `server.toml` *représentatif* (voir « La clé morte » ci-dessous), pas la
  configuration du parc. Une clé de configuration inconnue **absente** de la fixture ne
  sera pas exercée.
- **`/health` n'atteint jamais `healthy` sur cette fixture** (capacités critiques
  `embed`/`curator`/`worker` injoignables). C'est une **mesure**, pas un échec : le
  harnais consigne `premier_healthy_s=never` après un plancher de 60 s. Le harnais ne
  conclut donc rien de l'état de santé — il conclut sur les relevés.

## Interface

```bash
bash tests/upgrade/run-upgrade-test.sh \
    --state "$GRADATUM_UPGRADE_STATE" \
    --admin-bin-240  <…>/gradatum-admin  \
    --server-bin-240 <…>/gradatum-server \
    --admin-bin-250  <…>/gradatum-admin  \
    --server-bin-250 <…>/gradatum-server \
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

Les **deux forces** de la clé (e) (point 4 ci-dessus) ne sont pas des options de cette
ligne de commande : elles sont fixées par le **sens de la comparaison**. Le harnais passe
`additif` à la montée (`parity-1v3`) et `strict` au retour arrière (`parity-1v4`) ; la
force retenue est écrite dans chaque rapport de parité et dans `verdict.txt`. La ligne CI
est donc inchangée — c'est la **direction** comparée qui décide, jamais un réglage.

### Options hors commande CI

- `--worker-bin-250 <chemin>` — sans lui, l'axe F-XX (ruling E4) est exercé jusqu'à
  l'**enfilement** : l'allocation du numéro et l'injection du rôle sont prouvées sur le
  travail enfilé, mais la **matérialisation** de la carte ne l'est pas (elle exige un
  consommateur de file). Avec lui, la carte est matérialisée et l'assertion porte sur sa
  présence dans `vault_list`. La CI ne construit pas de worker 2.5.0 : la ligne CI reste
  la commande ci-dessus.
- `--fault-inject delete-note` — **diagnostic**, preuve de rouge : retire une note de
  l'index entre le relevé AVANT et le relevé APRÈS. Doit rendre `1` **et nommer la
  note**. Jamais `0` dans ce mode.

### Récapitulatif de durées

`$GRADATUM_UPGRADE_PHASES` (exportée par le job via `$GITHUB_ENV`, relue par le step de
récapitulatif) reçoit une ligne par phase : `phase=<nom> duree=<n>s`. Défaut :
`<out>/phases.txt`.

## Provenance des binaires

Le harnais ne déduit **jamais** la provenance d'un binaire d'un chemin : il lit `version` et
`build_sha` sur `/health`, à chaud, sur le processus qui vient de démarrer, et il les
consigne. Un chemin peut mentir ou se périmer ; une réponse `/health` lue sur le processus
qui sert, non.

- **La référence du harnais est la release INTERNE** : les binaires portent le `build_sha`
  du tag interne, et ce sont **les mêmes** que ceux déployés sur le parc. C'est la seule
  comparaison qui dise quelque chose du parc.
- **L'archive publique porte un autre `build_sha`, par construction.** Son historique n'est
  pas celui du dépôt interne : la même version y est construite à un commit différent, donc
  l'archive publiée et le binaire du parc ne portent pas le même `build_sha`. L'archive
  publique n'est **pas** la référence du parc, et le harnais ne la traite pas comme telle.
- **`scripts/fetch-gradatum-release.sh`** sert à **vérifier l'archive publique** —
  empreinte SHA-256 du fichier téléchargé, et attestation de provenance quand elle est
  disponible — quand c'est bien elle qu'on monte. Une archive ainsi vérifiée est un
  AVANT/APRÈS légitime : les mesures portent alors sur le couple d'archives publiques, et le
  `build_sha` consigné dans les relevés dit lequel des deux couples a été mesuré.

Les binaires passés en `--admin-bin-*`, `--server-bin-*` et `--worker-bin-*` viennent de
l'**extérieur** du harnais : il ne les construit pas, ne les télécharge pas, ne les remplace
pas. Le **profil de compilation** n'est donc pas une propriété du harnais — mais il en est une
de la mesure : deux binaires de profils différents ne se comparent pas, et la montée doit
opposer, de part et d'autre, des binaires **du même profil**. Le harnais ne contrôle pas ce
point ; il est écrit ici pour que le vert ne se lise pas comme une couverture de plus.

## La clé morte que la montée doit porter

`fixture/server.toml.tmpl` déclare **délibérément** un bloc `[curator.llm]` inerte portant
`max_tokens = 2048` — la clé que 2.5.0 **retire du schéma** (`WorkerLlmConfig::max_tokens`
sort de l'API publique ; `[curator] llm_review_max_tokens` porte seul le budget effectif,
et l'a toujours porté). La section ne déclare pas `deny_unknown_fields` : un `server.toml`
d'opérateur produit par 2.4.0 qui la porte **continue de charger**.

Cette propriété n'est pas mesurée par une clé de parité nommée : elle est exercée par le
**fait que la montée démarre**. Un `server.toml` refusé ferait échouer le sous-step 2d
(`refus de démarrage`, nommé, `RUN_RC=1`). La mesure est donc le démarrage lui-même —
c'est la seule que ce couple de versions met réellement en jeu côté configuration, et
c'est la raison d'être de la clé dans la fixture. Sans elle, le harnais monterait une
configuration **déjà** au dialecte cible et ne prouverait rien de ce point.

## Le preset ACL — contrôlé AVANT de monter

Le mode d'échec le plus dangereux du produit est un preset ACL qui **ne se parse pas** :
le serveur démarre quand même, `/health` répond, et le chargeur retombe sur l'**engine
vide = DENY-ALL** — toute requête authentifiée est refusée en `403` (corps vide), sans
jeton en `401`. **Aucun démarrage ne rate.** Une montée paraît réussie et le service est
muet. C'est pourquoi le preset est contrôlé **avant** la montée, en 2a, avec le binaire
qui monte : c'est *son* parseur qui décidera.

`gradatum-admin acl validate --preset <chemin>` existe **depuis 2.4.0** — il est donc
présent sur **les deux** binaires de ce couple. rc `0` = valide ; rc `1` nomme la clé
fautive sur stderr. Le harnais consigne `preset_controle_acl_validate=disponible` et
`preset_chemin=` (voir ci-dessous).

### Le chemin nominal de ce couple : `deja-conforme`

Le champ `sees_personal_classified` a été retiré des entrées de consommateur en **2.4.0**.
La fixture est produite **par** le binaire 2.4.0 : son preset est donc **déjà** au
dialecte cible, et il n'y a rien à réaligner. Le harnais le **dit** — il ne laisse pas
une absence passer pour un succès :

- 2a : contrôle du preset de l'état sous le parseur 2.5.0 → `rc=0` ;
- 2b/2c : **non armés** — rien à mesurer, rien à réécrire ;
- 2d : montée sur le preset de l'état, qui est celui que le Step 3 mesure ;
- `preset_chemin=deja-conforme (chemin nominal de ce couple)` et
  `preset_deny_all_mesure=0` sont écrits dans `verdict.txt`.

### Le garde 2b/2c : pour un état antérieur à 2.4.0

Le bloc reste **armé** : il ne se déclenche que si le preset de l'état porte **encore**
la clé retirée, c'est-à-dire si l'appelant fournit un état **antérieur à 2.4.0**. Il est
alors la preuve de rouge conservée du harnais — le DENY-ALL est *mesuré*, pas supposé :

1. **2b** — montée sur le preset fautif tel quel : le serveur **démarre**, une lecture
   est sondée, les marqueurs du journal sont relevés. Le verdict est **dérivé des faits**
   (marqueurs *et* code HTTP), jamais de l'hypothèse « ce doit être un 403 ».
   `preset_deny_all_mesure=1` + `lecture_http=403` = dégradation confirmée ; un journal
   qui annonce la dégradation sans refuser la lecture est un **écart nommé**, `rc=1`.
2. **2c** — réalignement documenté, puis contrôle du preset réécrit. Le réalignement
   **écrit** dans l'état (`config/bearer.toml`), c'est la seule écriture du harnais hors
   axe F-XX ; l'original est conservé **octet à octet** sous `2-controle-preset/`.
   Si le formulaire du preset n'est pas réécrivable au jugé (`read_patterns` /
   `write_patterns` sur plusieurs lignes), le harnais rend `2` **sans toucher au preset**
   : un preset à moitié migré serait pire qu'une non-exécution, et un `1` ferait lire
   « divergence mesurée » là où aucune mesure n'a eu lieu.

Les deux étapes de la remédiation — retirer la clé ; préfixer les quatre négations
`!personal-classified`, `!personal-classified/**`, `!**/personal-classified`,
`!**/personal-classified/**` à `read_patterns` **et** `write_patterns` pour les
consommateurs qui portaient `false` ou rien (un consommateur à `true` n'en reçoit
aucune) — sont celles du guide `docs/UPGRADING-2.3.1-to-2.4.0.md`
§ « The ACL preset field removal ».

**Ce garde est réversible**, et c'est vérifié par mesure, pas déduit : le champ est
`#[serde(default)]` et la négation `!` existe déjà (deny-wins) en 2.4.0, donc un preset
réaligné **charge en 2.4.0** et les lectures y fonctionnent. Le retour arrière reste
**binaire seul**, sans retour de configuration. Le harnais écrit
`preset_reversibilite_240=compatible (retour binaire seul suffisant)` — ou `INCOMPATIBLE`
si le redémarrage 2.4.0 du Step 4 a échoué.

## Axe F-XX (ruling E4)

Après les quatre relevés — et **après seulement**, parce qu'il **écrit** dans l'état —
le harnais crée une carte projet avec l'`gradatum-admin` 2.5.0, puis :

- asserte la **présence** de la carte (et, avec `--worker-bin-250`, sa matérialisation) ;
- asserte l'**existence du rôle `[[feature:F-N]]`** sur la carte ;
- lit `N` **dans la réponse** et vérifie la cohérence `feature == "F-" + number`,
  **rembourrage ignoré** (le serveur rend `F-01` pour `number: 1`) ;
- asserte que `N` est **strictement supérieur** au plus grand numéro **exporté par la
  fixture**, relu dans le relevé AVANT (`export-features.json`) — jamais postulé.

Cette dernière assertion est la seule qui lie `N` à une valeur, et elle est
**relative**, pas absolue : le numéro est arbitraire sur une base réelle, mais il
est ici la suite de ceux que la fixture a consommés. S'il retombait en dessous, la
montée aurait remis le compteur à zéro — deux cartes porteraient le même numéro,
ce que l'identité d'une carte interdit. Si le plus grand numéro de la fixture est
illisible (relevé vide, par exemple), l'assertion rend `NON CONCLUE` et l'axe est
en **échec** : une référence indisponible ne vaut pas une référence satisfaite.

**Ce que la matérialisation ajoute — et pourquoi elle seule parle de la carte.** Sans
consommateur de file, les assertions portent sur le travail **enfilé** : le numéro alloué et
le rôle `[[feature:F-N]]` injecté dans le corps. C'est la preuve la plus forte disponible
sans consommateur, mais elle ne dit **pas** que la carte existe : l'index ne la contient pas
tant que rien ne consomme la file. Un `gradatum-worker` 2.5.0 passé en `--worker-bin-250`
ferme cette distance — le harnais le démarre en arrière-plan, **attend borné** sur le statut
du travail, puis relit `vault_list` et écrit
`assertion_materialisation=ok|ÉCHEC (project-map/<id> présent/absent de vault_list, statut=…)`.
Sans worker, la ligne écrite est
`assertion_materialisation=NON ÉTABLIE — aucun travailleur 2.5.0 fourni` : la distance est
**déclarée**, jamais laissée implicite, et elle n'est comptée ni comme un vert ni comme un
échec — c'est l'assertion de rôle, elle, qui décide.

La CI ne construit pas de worker 2.5.0 : sa ligne de commande reste celle de la section
*Interface*, et elle mesure donc l'axe jusqu'à l'enfilement. Le rejeu local, lui, fournit le
worker — c'est la variante sous laquelle l'axe va jusqu'à la carte indexée.

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

| Passe | Montage | Attendu |
|---|---|---|
| (i) | binaire 2.4.0 des **deux** côtés | aucun écart de données : la montée ne change rien, le déroulé est celui du chemin `deja-conforme`, et `preset_deny_all_mesure=0` |
| (ii) | `--fault-inject delete-note` | `1`, la note disparue est **nommée** |
| (iii) | port `19290` déjà en écoute | `2`, refus **avant** tout démarrage |

Les passes (i)–(iii) étaient consignées, sortie mesurée à l'appui, pour le couple
2.3.1 → 2.4.0. **Elles n'ont pas été rejouées pour ce couple** : la reprise du harnais a
vérifié que le montage (i) est cohérent avec le nouveau chemin (`deja-conforme`), mais les
lignes « obtenu » ne sont pas reproduites ici — une sortie qu'on n'a pas relevée ne se
recopie pas.

Quatre passes ont en revanche été **relevées** pour ce couple. (iv) et (v) sur la commande
complète (état régénéré, port `19290`, sans `--worker-bin-250`) ; (vi) et (vii) en rejouant
`assert-parity.sh` **seul** sur les relevés conservés de (v), contre un guide muet puis
contre le guide qui cite la clé — c'est précisément ce que ce script permet :

| Passe | État | Attendu | Obtenu |
|---|---|---|---|
| (iv) | état produit **avant** que la fixture ne porte des cartes de fonctionnalité | l'axe « export de cartes » ne peut pas rendre la parité | `1` — les **3** relevés d'export sont nommés (`export_features_releves_non_conformes=3`), l'axe F-XX rend `assertion_numero_superieur=NON CONCLUE (plus grand numéro de la fixture : indisponible)`, et `montage_ok=1` : la montée est **mesurée** au lieu d'être sautée. Le rapport de parité dit encore `VACUE`, mais le verdict, lui, n'est plus vert |
| (v) | état courant (2 cartes de fonctionnalité), clé (e) en force `strict` | deux binaires distincts dont le dialecte d'export gagne une clé : une comparaison d'octets doit refuser | `1` sur la **seule** clé (e) : les 3 relevés portent bien leurs 2 cartes (330 / 378 / 330 octets), la non-vacuité passe (`non_conformes=0`), et l'écart nommé est l'ajout de la clé `"visibility"` à chaque entrée exportée par 2.5.0 |
| (vi) | mêmes relevés, force `additif`, guide **muet** sur la clé | une clé ajoutée non documentée est un écart | `1` — `cles_ajoutees=visibility` relevé, puis `DIVERGENCE` : clé ajoutée « visibility » **ABSENTE** du guide |
| (vii) | mêmes relevés, force `additif`, guide **citant** la clé | parité établie, ajout relevé | `0` — `PARITÉ ÉTABLIE — 33 clé(s) vérifiée(s), 0 écart`, `cles_ajoutees=visibility` |
| (viii) | rejeu **complet** sur un état régénéré, **avec `--worker-bin-250`** | la montée et le retour arrière rendent la parité, et la carte de l'axe F-XX est **matérialisée** | `0` — `PARITÉ PROUVÉE` ; montée : `33 clé(s)`, `0 écart`, `cles_ajoutees=visibility` citée dans le guide ; retour arrière : `31 clé(s)`, `0 écart`, octet à octet (sha256 `edbb0bf4…`, 330 octets) ; axe F-XX : `assertion_numero_superieur=ok (numéro 3 > 2)`, `assertion_role_enfile=ok`, `assertion_materialisation=ok (project-map/… présent dans vault_list, statut=Done)`, export monté à 3 cartes |

La passe (v) est un **résultat de mesure**, pas un défaut du harnais : l'axe (e) tel
que déclaré — octet à octet entre 2.4.0 et 2.5.0 — est rouge dès que l'état porte des
cartes, parce que 2.5.0 a **délibérément** exposé l'axe de visibilité dans la projection
exportée. Tant que l'état était vide, cet écart existait déjà : il ne se voyait pas.

L'arbitrage rendu par les passes (vi) et (vii) laisse la comparaison d'octets là où elle
est **légitime** — le retour arrière, où c'est le **même binaire** des deux côtés et où
rien ne doit bouger — et fait de la montée une comparaison de **rapport** : le dialecte
peut gagner des clés, mais une clé nouvelle doit être **citée dans le guide**. Les trois
cas de cette règle — une valeur modifiée côté MONTÉ, une clé du dialecte AVANT disparue,
une clé ajoutée **seule** et correctement relevée — sont exercés **hors ligne** par
`assert-parity-tests.sh`, sans binaire ni port : c'est le banc qui les rejoue à chaque
exécution, avec les cas de refus (clé non documentée, guide absent, mode inconnu).

## `assert-parity.sh` est rejouable seul

Il ne connaît ni binaire, ni port, ni serveur : il ne lit que des fichiers. Deux relevés
conservés suffisent à rejouer la comparaison, sans rien redémarrer.
Voir l'en-tête du script pour la force exacte de chaque clé (a)–(g) et la raison d'être
de chacune.

`assert-parity-tests.sh` pousse cette propriété à bout : il **assemble lui-même** ses deux
relevés, dans un répertoire temporaire, à partir de contenu littéral, et n'exerce
`assert-parity.sh` que par sa ligne de commande, son code retour et son rapport écrit. Aucun
binaire, aucun port, aucun état produit par `gen-fixture.sh` : le banc est **hermétique**, et
c'est ce qui permet de rejouer la règle de la clé (e) — clé ajoutée, valeur modifiée, clé du
dialecte AVANT disparue, clé ajoutée non documentée, guide absent, mode inconnu — à chaque
exécution, y compris là où aucun binaire 2.4.0 ou 2.5.0 n'est disponible. Il rend `0` si
tous les cas sont conformes, `1` en nommant le cas en écart, `2` si un outil requis manque :
lui non plus ne rend jamais un vert par incapacité.
