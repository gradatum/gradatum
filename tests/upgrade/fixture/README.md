# Fixture de montée de version 2.4.0 → 2.5.0

Ce dossier est du **texte pur** : un corpus markdown inventé, son manifeste
d'écriture, un gabarit de configuration et un preset ACL. Il ne contient
**aucun binaire**, aucune clé, aucun jeton, aucun identifiant réel.

## Règle R-E10 — aucun artefact binaire commité

Le dépôt ne reçoit pas de base pré-construite. `index.db`, `queue.sqlite` et le
coffre sont **produits à la demande** par le binaire 2.4.0, dans un répertoire
hors dépôt (`--out`).

## Provenance — ce qui fait foi

La provenance de cet état est le **`build_sha` 2.4.0 relevé en direct sur
`/health`** de l'instance d'essai par `gen-fixture.sh` (le script échoue si le
champ est absent, donc la valeur est lue, jamais déduite). Pour la fixture
livrée : **`build_sha=2fabf9bc`**, qui est le commit
`2fabf9bc7da73b909d2c41143bb6ce049b693ce7`, porté par le tag `v2.4.0`.

`build_sha` vaut aussi pour les trois binaires :

```
gradatum-server 2.4.0 (build_sha 2fabf9bc)
gradatum-admin  2.4.0 (build_sha 2fabf9bc)
gradatum-worker 2.4.0 (build_sha 2fabf9bc)
```

Phrase normative, verbatim : la provenance de cette fixture est le `build_sha`
2.4.0 consigné au rapport, pas le hash d'une archive. La valeur est de toute
façon **relue** par le harnais sur le `/health` du serveur de l'état, jamais
reprise de ce document.

## Contenu

| Chemin | Rôle |
|---|---|
| `notes/*.md` | 43 corps markdown, un par note. Aucune métadonnée : le fichier *est* le corps. |
| `feature-cards/*.md` | 2 corps markdown de cartes de fonctionnalité, posés **par le serveur** en cours de génération (voir « Les cartes de fonctionnalité »). |
| `pending/*.md` | 5 corps markdown posés **après** l'arrêt du travailleur : ils restent `Pending` en file. |
| `manifest.jsonl` | Ordre d'écriture + attendus de comptes. Voir ci-dessous. |
| `acl-preset.toml` | Preset ACL à passer à `gradatum-admin init --preset`. |
| `server.toml.tmpl` | Gabarit de configuration, à rendre par `sed` (voir `gen-fixture.sh`). |

Le corpus couvre **11 sections canoniques** (`decisions`, `architecture`,
`debug`, `reasoning`, `lessons-learned`, `retrospectives`, `feedback`,
`experiments`, `reference`, `agent-issues`, `project-map`) et **5 cartes
`project-map`** : 1 `ROADMAP` portant `[[version:gradatum/2.5.0]]`, 2 cartes de
travail portant `[[track:gradatum/2.5.0]]` — c'est ce lien de piste qui exerce
la dérivation de `release` — et 2 cartes de fonctionnalité dont le numéro est
alloué par le serveur. Le corpus décrit le jalon **en préparation** au moment
où l'état est produit : un état produit par le binaire 2.4.0 porte donc des
cartes de 2.5.0, comme l'état produit par le binaire précédent portait des
cartes de 2.4.0.

Deux auteurs **fictifs** (`agent-alpha`, `agent-beta`) signent les notes. Ils
sont nécessaires : la règle R2 interdit à un client d'auto-déclarer son auteur
(l'auteur d'une note est le sujet du porteur), donc deux auteurs distincts dans
l'état produit exigent deux identités réellement déclarées dans le preset.

## Les cartes de fonctionnalité, et pourquoi elles sont posées par le serveur

Le rôle `[[feature:F-XX]]` est l'*identité* d'une carte : il est **alloué par le
serveur**, jamais écrit par un client. Une écriture qui tenterait de l'introduire
est refusée en 400. Une carte de fonctionnalité naît donc d'un appel à
`POST /api/v1/project-map/create-feature`, qui alloue le numéro, l'injecte dans
le corps et délègue l'écriture au chemin normal ; la réponse porte le numéro
alloué, l'identifiant de note et celui du travail.

C'est ce que fait `gen-fixture.sh` pour les 2 corps de `feature-cards/`, **après**
la pose du corpus et **avant** les travaux qui doivent rester `Pending`, le
travailleur étant encore en marche : chaque carte est attendue à l'état terminal
de son travail avant que la suite ne continue. Les 2 cartes restent dans le
coffre `main`.

Cette étape n'est pas un ornement. La fixture doit peupler les cartes, faute de
quoi l'axe « export de cartes » du harnais — une comparaison octet à octet de
`project-map export-features` avant et après la montée — serait vraie des deux
côtés sur un export **vide**. Une parité vraie, mais qui ne couvrirait rien : le
harnais refuse désormais ce cas (`export_features_releves_non_conformes`), et la
génération vérifie elle aussi, sur son propre état, que l'export rend au moins
autant de cartes qu'elle en a créées.

Les 2 cartes de travail, elles, **ne portent pas** de rôle d'identité : elles
exercent la dérivation de `release` par le lien de piste, pas l'axe
« fonctionnalité ».

## La clé morte que la montée doit porter

`server.toml.tmpl` déclare un bloc `[curator.llm]` **inerte** (le curateur reste
`backend = "heuristic"`) mais **présent**, et il y porte `max_tokens = 2048`.
C'est délibéré : cette clé est celle que 2.5.0 **retire du schéma**. Un
`server.toml` d'opérateur produit par 2.4.0 la porte donc légitimement, et la
montée doit prouver qu'un tel fichier **continue de charger** — la section ne
déclare pas `deny_unknown_fields`, une clé retirée est ignorée comme n'importe
quelle clé inconnue. Voir `docs/UPGRADING-2.4.0-to-2.5.0.md`.

Sans cette clé, le harnais monterait une configuration **déjà** au dialecte
cible : il prouverait la parité des données, mais pas la propriété que ce couple
de versions met réellement en jeu.

## `manifest.jsonl`

Une ligne JSON par entrée, dans l'ordre d'écriture. Types de ligne :

- `write` — une note du corpus (`notes/`). Porte `seq`, `file`, `section`,
  `note_id` (ULID **fixé** par le manifeste), `title`, `author`, `tags`.
- `feature_card` — une carte de fonctionnalité (`feature-cards/`), posée par
  `create-feature`. Porte `seq`, `file`, `section`, `title`, `author`, `tags` ;
  **pas** de `note_id` : le numéro de carte comme l'identifiant de note sont
  alloués par le serveur.
- `pending_job` — un travail `Curate` laissé en file (`pending/`).
- `dlq_job` — le travail voué à la file morte (un `Embed` sur un identifiant
  absent : la lecture échoue en erreur métier terminale, puis le balayage
  périodique du travailleur promeut la ligne en DLQ).
- `expected_counts` — dernière ligne, les attendus : notes par section, cartes
  (`project_map_cards`), cartes de fonctionnalité (`feature_cards`), nombre de
  travaux en file et en file morte.

Les ULID des notes du corpus sont **fixés par le manifeste** (et non alloués à
l'écriture) pour que deux exécutions produisent le même ensemble d'identifiants.
Les 2 cartes de fonctionnalité font exception : leur identifiant de note est
alloué à l'écriture, comme leur numéro de carte.

## Régénérer l'état

`gen-fixture.sh` (dans `tests/upgrade/`) fait tout : `init` du répertoire
racine, rendu du gabarit, démarrage du serveur et du travailleur 2.4.0, pose du
corpus, **création des 2 cartes de fonctionnalité par le serveur** (numéro alloué
côté serveur, chaque travail attendu à l'état terminal), attente du vidage de la
file, dépôt de l'entrée en file morte, puis arrêt du travailleur **avant** la
pose des 5 travaux qui doivent rester `Pending`. Il tue ses processus par le PID
capturé au lancement.

```sh
cd <copie du dépôt>
B=/chemin/vers/2.4.0/target/release            # binaires 2.4.0, hors dépôt
bash tests/upgrade/gen-fixture.sh \
  --admin-bin  "$B/gradatum-admin" \
  --server-bin "$B/gradatum-server" \
  --worker-bin "$B/gradatum-worker" \
  --out /chemin/hors/depot/fixture-240 \
  --port 19290
```

Le répertoire `--out` doit être **hors du dépôt** (le script refuse le
contraire) et les binaires ne peuvent pas être ceux installés dans
`/usr/bin` &co (le script refuse aussi : ce sont les binaires LIVE).

Codes de sortie : `0` succès · `1` erreur d'exécution · `2` **non exécuté**
(port déjà pris — refus avant tout démarrage) · `3` argument refusé.

Le script **n'écrit aucun fichier de provenance** dans `<out>` : il relève le
`build_sha` sur `/health` et n'en rend compte que sur la **dernière ligne du
journal** — `gen-fixture: build_sha=… notes=… cartes=… pending=… dlq=… out=…`
(`notes` compte le corpus **et** les 2 cartes créées par le serveur, `cartes`
leur nombre). C'est
cette ligne, et elle seule, qui porte la provenance de l'état produit ; ce qui
est écrit sous `<out>` est l'état lui-même (`db/`, `vault/`, `md/`, `config/`,
`audit/`, `logs/`), pas sa provenance.

## Déterminisme

Deux exécutions produisent deux états dont les **comptes** sont identiques
(notes par section, total, auteurs, cartes, liens, travaux par état) et dont les
identifiants des notes du corpus sont identiques (fixés par le manifeste). Les
**octets** diffèrent : identifiants de travaux alloués à l'exécution,
horodatages, et — pour les 2 cartes de fonctionnalité — identifiant de note
comme numéro de carte, tous deux alloués par le serveur. Ne jamais asserter
l'égalité binaire.

## Ce que la fixture exerce

FTS (recherche lexicale sur état monté), liens typés et graphe de cartes,
dérivation de `release` par le lien de piste, allocation serveur du numéro de
carte et export des cartes (`project-map export-features`, comparé octet à octet
avant/après sur un export **non vide**), file de travaux (vidage complet puis
reprise sur travaux restants) et file morte. Chacun est un chemin que la montée
de version peut casser.
