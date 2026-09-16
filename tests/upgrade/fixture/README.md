# Fixture de montée de version 2.3.1 → 2.4.0

Ce dossier est du **texte pur** : un corpus markdown inventé, son manifeste
d'écriture, un gabarit de configuration et un preset ACL. Il ne contient
**aucun binaire**, aucune clé, aucun jeton, aucun identifiant réel.

## Règle R-E10 — aucun artefact binaire commité

Le dépôt ne reçoit pas de base pré-construite. `index.db`, `queue.sqlite` et le
coffre sont **produits à la demande** par le binaire 2.3.1, dans un répertoire
hors dépôt (`--out`).

## Provenance — ce qui fait foi

La provenance de cet état est le **`build_sha` 2.3.1 relevé en direct sur
`/health`** de l'instance d'essai par `gen-fixture.sh` (le script échoue si le
champ est absent, donc la valeur est lue, jamais déduite). Pour la fixture
livrée : **`build_sha=b2cb32b1`**, qui est le commit
`b2cb32b1c18f7044783a8b6ff6537d6c0833db98` de la branche `internal/2.3.1`.

`build_sha` vaut aussi pour les trois binaires :

```
gradatum-server 2.3.1 (build_sha b2cb32b1)
gradatum-admin  2.3.1 (build_sha b2cb32b1)
gradatum-worker 2.3.1 (build_sha b2cb32b1)
```

Phrase normative, verbatim : la provenance de cette fixture est le `build_sha`
2.3.1 consigné au rapport, pas le hash d'une archive — il n'y a pas d'archive.

## Contenu

| Chemin | Rôle |
|---|---|
| `notes/*.md` | 43 corps markdown, un par note. Aucune métadonnée : le fichier *est* le corps. |
| `pending/*.md` | 5 corps markdown posés **après** l'arrêt du travailleur : ils restent `Pending` en file. |
| `manifest.jsonl` | Ordre d'écriture + attendus de comptes. Voir ci-dessous. |
| `acl-preset.toml` | Preset ACL à passer à `gradatum-admin init --preset`. |
| `server.toml.tmpl` | Gabarit de configuration, à rendre par `sed` (voir `gen-fixture.sh`). |

Le corpus couvre **11 sections canoniques** (`decisions`, `architecture`,
`debug`, `reasoning`, `lessons-learned`, `retrospectives`, `feedback`,
`experiments`, `reference`, `agent-issues`, `project-map`) et **3 cartes
`project-map`** : 1 `ROADMAP` portant `[[version:gradatum/2.4.0]]` et 2 cartes
de travail portant `[[track:gradatum/2.4.0]]` — c'est ce lien de piste qui
exerce la dérivation de `release`.

Deux auteurs **fictifs** (`agent-alpha`, `agent-beta`) signent les notes. Ils
sont nécessaires : la règle R2 interdit à un client d'auto-déclarer son auteur
(l'auteur d'une note est le sujet du porteur), donc deux auteurs distincts dans
l'état produit exigent deux identités réellement déclarées dans le preset.

Les deux cartes de travail **ne portent pas** de rôle `[[feature:F-XX]]` : ce
rôle est l'*identité* de la carte, que seul le serveur alloue (une écriture
externe qui tenterait de l'introduire est refusée en 400). La fixture n'exerce
donc pas l'axe « fonctionnalité » ; elle exerce la dérivation de `release` par
le lien de piste, ce que le brief demande.

## `manifest.jsonl`

Une ligne JSON par entrée, dans l'ordre d'écriture. Types de ligne :

- `write` — une note du corpus (`notes/`). Porte `seq`, `file`, `section`,
  `note_id` (ULID **fixé** par le manifeste), `title`, `author`, `tags`.
- `pending_job` — un travail `Curate` laissé en file (`pending/`).
- `dlq_job` — le travail voué à la file morte (un `Embed` sur un identifiant
  absent : la lecture échoue en erreur métier terminale, puis le balayage
  périodique du travailleur promeut la ligne en DLQ).
- `expected_counts` — dernière ligne, les attendus : notes par section, cartes,
  nombre de travaux en file et en file morte.

Les ULID de notes sont **fixés par le manifeste** (et non alloués à l'écriture)
pour que deux exécutions produisent le même ensemble d'identifiants.

## Régénérer l'état

`gen-fixture.sh` (dans `tests/upgrade/`) fait tout : `init` du répertoire
racine, rendu du gabarit, démarrage du serveur et du travailleur 2.3.1, pose du
corpus, attente du vidage de la file, dépôt de l'entrée en file morte, puis
arrêt du travailleur **avant** la pose des 5 travaux qui doivent rester
`Pending`. Il tue ses processus par le PID capturé au lancement.

```sh
cd <copie du dépôt>
B=/chemin/vers/2.3.1/target/release            # binaires 2.3.1, hors dépôt
bash tests/upgrade/gen-fixture.sh \
  --admin-bin  "$B/gradatum-admin" \
  --server-bin "$B/gradatum-server" \
  --worker-bin "$B/gradatum-worker" \
  --out /chemin/hors/depot/fixture-231 \
  --port 19290
```

Le répertoire `--out` doit être **hors du dépôt** (le script refuse le
contraire) et les binaires ne peuvent pas être ceux installés dans
`/usr/bin` &co (le script refuse aussi : ce sont les binaires LIVE).

Codes de sortie : `0` succès · `1` erreur d'exécution · `2` **non exécuté**
(port déjà pris — refus avant tout démarrage) · `3` argument refusé.

Le script **n'écrit aucun fichier de provenance** dans `<out>` : il relève le
`build_sha` sur `/health` et n'en rend compte que sur la **dernière ligne du
journal** — `gen-fixture: build_sha=… notes=… pending=… dlq=… out=…`. C'est
cette ligne, et elle seule, qui porte la provenance de l'état produit ; ce qui
est écrit sous `<out>` est l'état lui-même (`db/`, `vault/`, `md/`, `config/`,
`audit/`, `logs/`), pas sa provenance.

## Déterminisme

Deux exécutions produisent deux états dont les **comptes** sont identiques
(notes par section, total, auteurs, cartes, liens, travaux par état) et dont les
identifiants de notes sont identiques (fixés par le manifeste). Les **octets**
diffèrent : identifiants de travaux alloués à l'exécution, horodatages. Ne
jamais asserter l'égalité binaire.

## Ce que la fixture exerce

FTS (recherche lexicale sur état monté), liens typés et graphe de cartes,
dérivation de `release` par le lien de piste, file de travaux (vidage complet
puis reprise sur travaux restants) et file morte. Chacun est un chemin que la
montée de version peut casser.
