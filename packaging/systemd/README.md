# gradatum systemd packaging

Fichiers unit systemd pour `gradatum-server`, `gradatum-worker`, `gradatum-engine` et `gradatum-gateway`.

> **Ces fichiers ne sont livrés dans aucune archive de release.** Les deux archives
> binaires du chemin GitHub (`gradatum-server-*.tar.gz`, `gradatum-llm-*.tar.gz` — voir
> [`docs/guides/B-install-binaries.md`](../../docs/guides/B-install-binaries.md))
> ne contiennent que des binaires — ni unit files, ni `sysusers.d`, ni scripts, ni configs
> d'exemple. Même après une installation par archives pré-compilées, il faut récupérer ce
> répertoire séparément : `bash scripts/fetch-gradatum-release.sh` (le récupère en même
> temps que les binaires), `git clone` du dépôt, ou téléchargement HTTPS des fichiers
> individuels depuis le tag de la release. Sans systemd (conteneur, par exemple), voir
> "Running without systemd" dans `docs/DEPLOYMENT.md` §8.

## Fichiers

| Fichier | Rôle |
|---|---|
| `gradatum-server.service` | Façade HTTP/MCP `:19090` — `Type=notify`, `MemoryMax=512M` |
| `gradatum-worker.service` | Worker background curator cascade + job queue — `Type=simple`, `MemoryMax=1G` |
| `gradatum-engine@.service` | Template superviseur `llama-server` — une instance par modèle (`@curator`, `@embed`, …) — `Type=simple` |
| `gradatum-gateway-spike.service` | Routeur LLM (alias → provider + circuit-breaker) — `Type=simple` |
| `gradatum-pre-migration-backup` | Guard ExecStartPre pour `gradatum-server`/`gradatum-worker` — backup atomique des DBs avant toute migration (fail-closed) — installé en `/usr/local/bin/` |
| `wait-for-port-free.sh` | Guard ExecStartPre pour `gradatum-engine@.service` — attend la libération de `child_port` |
| `test-wait-for-port-free.sh` | Tests unitaires pour `wait-for-port-free.sh` |

## Installation via script (recommandé)

Le script `scripts/install-gradatum-services.sh` automatise l'intégralité des étapes ci-dessous.
Il est idempotent (relançable), supporte `--clean` pour une réinstallation complète.

```bash
# Depuis la racine du workspace Gradatum :

# Installation standard (server + worker uniquement)
sudo bash scripts/install-gradatum-services.sh

# Installation avec compilation + wipe préalable
sudo bash scripts/install-gradatum-services.sh --build --clean --yes

# Installation avec engine (superviseur llama-server)
sudo bash scripts/install-gradatum-services.sh --build --with-engine

# Installation avec engine + gateway
sudo bash scripts/install-gradatum-services.sh --build --with-engine --with-gateway

# Options disponibles :
#   --build          Forcer cargo build --release avant installation
#   --clean          Wiper /var/lib/gradatum avant init (destructeur)
#   --yes            Non-interactif (skip confirmations)
#   --root DIR       Répertoire racine (défaut : /var/lib/gradatum)
#   --preset NAME    Preset ACL (défaut : hierarchical)
#   --bind ADDR      Adresse d'écoute (défaut : 127.0.0.1:19090)
#   --with-engine    Installer gradatum-engine + template systemd + configs exemple
#   --with-gateway   Installer gradatum-gateway + service systemd
#   --cleanup-after  Lancer cargo clean après installation réussie
```

## Services installés par défaut (server + worker)

L'ordre de démarrage **est critique**. `gradatum-admin init` doit s'exécuter
avant `systemctl start gradatum-worker` car le worker tente d'ouvrir
`db/queue.sqlite` — fichier créé par l'init, non par le server.

```bash
# 1. Installer les binaires (paquet Debian/Docker installe en /usr/bin)
sudo install -m 755 target/release/gradatum-server /usr/bin/
sudo install -m 755 target/release/gradatum-worker /usr/bin/
sudo install -m 755 target/release/gradatum-admin /usr/bin/

# 2. Créer l'utilisateur système gradatum (UID/GID alloués dynamiquement)
sudo cp packaging/sysusers.d/gradatum.conf /usr/lib/sysusers.d/
sudo systemd-sysusers
id gradatum  # MUST: un user 'gradatum' AVEC un groupe 'gradatum' (numéros alloués dynamiquement)

# 3. Installer les unit files + le helper ExecStartPre (backup pré-migration)
sudo cp packaging/systemd/gradatum-server.service /etc/systemd/system/
sudo cp packaging/systemd/gradatum-worker.service /etc/systemd/system/
# Helper OBLIGATOIRE : les deux units le déclarent en ExecStartPre SANS préfixe `-`.
# Son absence fait échouer le démarrage (status=216/GROUP, « No such process »).
sudo install -m 755 packaging/systemd/gradatum-pre-migration-backup /usr/local/bin/gradatum-pre-migration-backup
sudo systemctl daemon-reload

# 4. Générer bearer, configs, SQLite — crée tout sous --root
#    NOTE: gradatum-admin init --preset résout le preset embarqué indépendamment du CWD.
#    Un chemin absolu custom est également accepté : --preset /chemin/mon-preset.toml
sudo -u gradatum gradatum-admin init --root /var/lib/gradatum --preset hierarchical --non-interactive
# Crée :
#   /var/lib/gradatum/config/{server.toml, bearer.toml, admin.bearer.txt}
#   /var/lib/gradatum/db/{queue.sqlite, revocation.sqlite, api_keys.sqlite}
# NE crée PAS la clé de signature JWT : gradatum-server crée lui-même
#   /var/lib/gradatum/config/jwt-signing-key.secret (mode 0600) au premier boot (étape 5).
#   C'est CE fichier qu'il faut sauvegarder — le perdre invalide tous les jetons émis.
#   init ne génère plus la paire jwt.public.pem / jwt.private.pem : aucun composant
#   runtime ne la lisait.

# 4b. Fichier d'environnement du worker (OBLIGATOIRE) — il porte GRADATUM_INTERNAL_TOKEN.
#     init a déposé ce jeton dans config/internal-worker.token.txt (mode 0600). Le worker lit
#     /etc/gradatum/env (EnvironmentFile SANS `-` : un fichier absent échoue TÔT au niveau
#     systemd, jamais en crash-loop silencieux avec /health vert). Ce fichier est lu par
#     systemd (root), pas par le compte de service → propriétaire root:gradatum, mode 0640.
sudo mkdir -p /etc/gradatum
printf 'GRADATUM_INTERNAL_URL=http://127.0.0.1:19092\nGRADATUM_INTERNAL_TOKEN=%s\n' \
    "$(sudo cat /var/lib/gradatum/config/internal-worker.token.txt)" \
    | sudo tee /etc/gradatum/env >/dev/null
sudo chown root:gradatum /etc/gradatum/env
sudo chmod 0640 /etc/gradatum/env

# 5. Démarrer dans l'ordre : server en premier (matérialise les fichiers SQLite restants)
sudo systemctl enable --now gradatum-server.service
sudo systemctl enable --now gradatum-worker.service

# 6. Vérifier
sudo systemctl status gradatum-server gradatum-worker
sudo journalctl -u gradatum-server -n 20
sudo journalctl -u gradatum-worker -n 20
curl -fsS http://localhost:19090/health
```

> **Si gradatum-worker est démarré avant `gradatum-admin init`**, le worker
> échoue avec `SQLITE_CANTOPEN`. Relancer le worker après que l'init a
> créé `db/queue.sqlite`.

## Engine (`--with-engine`)

`gradatum-engine` est un superviseur de processus pour `llama-server`. Il s'installe
dans `/opt/gradatum/bin/` et utilise un template systemd `gradatum-engine@.service`.

### Fichiers installés

| Fichier | Emplacement |
|---|---|
| `gradatum-engine` (binaire) | `/opt/gradatum/bin/gradatum-engine` |
| `wait-for-port-free.sh` | `/opt/gradatum/bin/wait-for-port-free.sh` |
| `gradatum-engine@.service` (template) | `/etc/systemd/system/gradatum-engine@.service` |
| `70-engine-chat.toml` (exemple) | `/etc/gradatum/conf.d/70-engine-chat.toml` |
| `70-engine-embed.toml` (exemple) | `/etc/gradatum/conf.d/70-engine-embed.toml` |
| `engine-secrets.env` (vide, 0600) | `/etc/gradatum/engine-secrets.env` |

### Convention de nommage des configs

Chaque instance engine correspond à un fichier de configuration :
`/etc/gradatum/conf.d/70-engine-<name>.toml`

Le paramètre `%i` du template systemd est le `<name>`. Exemples :
- `70-engine-curator.toml` → `systemctl start gradatum-engine@curator`
- `70-engine-embed.toml`   → `systemctl start gradatum-engine@embed`

### Créer une instance engine

```bash
# 1. Placer le modèle GGUF dans /opt/gradatum/models/
cp mon-modele.gguf /opt/gradatum/models/

# 2. Copier et adapter la config exemple
sudo cp /etc/gradatum/conf.d/70-engine-chat.toml /etc/gradatum/conf.d/70-engine-curator.toml
sudoedit /etc/gradatum/conf.d/70-engine-curator.toml
# → Modifier model_path, port, child_port

# 3. Démarrer l'instance
sudo systemctl daemon-reload
sudo systemctl enable --now gradatum-engine@curator

# 4. Vérifier
systemctl status gradatum-engine@curator
journalctl -u gradatum-engine@curator -f
curl -s http://127.0.0.1:11435/health | python3 -m json.tool
```

### `wait-for-port-free.sh`

Guard `ExecStartPre` qui attend que le `child_port` (port loopback du child `llama-server`)
soit libre avant de lancer l'engine. Évite les races au port entre un `systemctl restart`
et l'`ExecStopPost` du run précédent (incident 2026-07-08).

Timeout configurable via `WAIT_FOR_PORT_FREE_TIMEOUT_SECS` (défaut : 10s).

### Propriétés de sécurité

Voir `docs/DEPLOYMENT.md` §7 pour le détail complet :
- `bind_addr` : `127.0.0.1` par défaut, rejette `0.0.0.0` (fail-closed)
- `/metrics` : toujours sur loopback quelle que soit `bind_addr`
- `model_path` / `mmproj_path` : doit être sous `/opt/gradatum/models/`
- `llama_server_bin` : doit être sous `/usr/local/bin/` ou `/opt/gradatum/bin/`
- `extra_args` : allow-list stricte (voir docs/DEPLOYMENT.md §6)

## Gateway (`--with-gateway`)

`gradatum-gateway` est un routeur LLM avec circuit-breaker. Il s'installe dans
`/opt/gradatum/bin/` et utilise le service `gradatum-gateway-spike.service`.

### Fichiers installés

| Fichier | Emplacement |
|---|---|
| `gradatum-gateway` (binaire) | `/opt/gradatum/bin/gradatum-gateway` |
| `gradatum-gateway-spike.service` | `/etc/systemd/system/gradatum-gateway-spike.service` |

### Démarrer le gateway

```bash
# 1. Créer la config (obligatoire — le gateway refuse de démarrer sans)
# `examples/configs/curator.toml` n'est pas dans l'archive gradatum-llm (2 binaires
# seulement : gradatum-gateway, gradatum-engine) — provient du dépôt cloné, comme le
# reste de ce répertoire (voir l'avertissement en tête de fichier).
sudo cp examples/configs/curator.toml /etc/gradatum/gateway-spike.toml
sudoedit /etc/gradatum/gateway-spike.toml
# → Configurer les providers et aliases (voir docs/DEPLOYMENT.md §10)

# 2. Démarrer
sudo systemctl enable --now gradatum-gateway-spike

# 3. Vérifier
systemctl status gradatum-gateway-spike
journalctl -u gradatum-gateway-spike -f
```

## Post-deploy validation

After deployment, run the smoke test:

```bash
GRADATUM_API_KEY=<api-key> bash scripts/smoke-alpha-4.sh
```

The script executes 7 acceptance steps. It authenticates through the standard
flow (api-key → `POST /auth/exchange` → JWT), so it needs an api-key: either
`GRADATUM_API_KEY` in the environment, or a readable `API_KEY_FILE`
(default `/etc/gradatum/gradatum-mcp.api-key`, mode `0600` owned by `gradatum`
— an unprivileged account cannot read it, hence the environment variable above).

Exit codes: `0` = PASS (all 7 steps verified), `1` = FAIL, `2` = INCOMPLETE or
gate not executed (missing api-key, unreachable server, skipped step). A run
with a skipped step is never reported as PASS.

Measure fastembed peak memory at cold start:

```bash
sudo systemctl show gradatum-worker -p MemoryCurrent
```

Compare against the `MemoryMax=1G` budget in the unit file.

## Compte de service (allocation dynamique)

`gradatum` : compte système dédié, **UID/GID alloués dynamiquement** par
`systemd-sysusers` à l'installation (`u gradatum -`). Aucune unité ne référence
le compte par son numéro — les quatre units déclarent `User=gradatum` /
`Group=gradatum` **par nom**, résolu à l'exécution.

Rationale:
- A fixed UID/GID buys nothing here: no unit, script or config refers to the
  account by number. Only the name is load-bearing.
- Worse, a fixed number was actively harmful. It was chosen by scanning ID
  availability on a single reference host, then frozen for every installer. On
  any host where that GID is already taken (observed: `forgejo-runner` holding
  the GID), `systemd-sysusers` does not fail — it creates the user, sees the
  numeric group already exists, and silently attaches the account to that
  FOREIGN group instead of creating `gradatum`. The install then dies later on
  `chown gradatum:gradatum` ("invalid group"), half-done.
- Lesson: a static UID/GID cannot be validated for a host you do not control.
  A `systemd-sysusers --dry-run` on the build host proves nothing about the
  operator's machine. Designate the account by NAME and let systemd allocate the
  number. For a non-standard account name, run
  `install-gradatum-services.sh --user <name>`: it creates the account with a
  dynamic UID/GID and applies a systemd drop-in (`User=`/`Group=`) on the units.
