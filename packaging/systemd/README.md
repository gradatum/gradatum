# gradatum systemd packaging

Fichiers unit systemd pour `gradatum-server` et `gradatum-worker`.

## Fichiers

| Fichier | Rôle |
|---|---|
| `gradatum-server.service` | Façade HTTP/MCP `:19090` — `Type=notify`, `MemoryMax=512M` |
| `gradatum-worker.service` | Worker background curator cascade + job queue — `Type=simple`, `MemoryMax=1G` |

## Installation via script (recommandé)

Le script `scripts/install-gradatum-services.sh` automatise l'intégralité des étapes ci-dessous.
Il est idempotent (relançable), supporte `--clean` pour une réinstallation complète.

```bash
# Depuis la racine du workspace Gradatum :

# Installation standard (binaires déjà compilés)
sudo bash scripts/install-gradatum-services.sh

# Installation avec compilation + wipe préalable
sudo bash scripts/install-gradatum-services.sh --build --clean --yes

# Options disponibles :
#   --build       Forcer cargo build --release avant installation
#   --clean       Wiper /var/lib/gradatum avant init (destructeur)
#   --yes         Non-interactif (skip confirmations)
#   --root DIR    Répertoire racine (défaut : /var/lib/gradatum)
#   --preset NAME Preset ACL (défaut : hierarchical)
#   --bind ADDR   Adresse d'écoute (défaut : 127.0.0.1:19090)
```

## Installation manuelle (procédure de référence)

L'ordre de démarrage **est critique**. `gradatum-admin init` doit s'exécuter
avant `systemctl start gradatum-worker` car le worker tente d'ouvrir
`db/queue.sqlite` — fichier créé par l'init, non par le server.

```bash
# 1. Installer les binaires (paquet Debian/Docker installe en /usr/bin)
sudo install -m 755 target/release/gradatum-server /usr/bin/
sudo install -m 755 target/release/gradatum-worker /usr/bin/
sudo install -m 755 target/release/gradatum-admin /usr/bin/

# 2. Créer l'utilisateur système gradatum (UID 985, GID 985)
sudo cp packaging/sysusers.d/gradatum.conf /usr/lib/sysusers.d/
sudo systemd-sysusers
id gradatum  # MUST: uid=985(gradatum) gid=985(gradatum)

# 3. Installer les unit files
sudo cp packaging/systemd/gradatum-server.service /etc/systemd/system/
sudo cp packaging/systemd/gradatum-worker.service /etc/systemd/system/
sudo systemctl daemon-reload

# 4. Générer les clés, bearer, configs, SQLite — crée tout sous --root
#    NOTE: gradatum-admin init --preset résout le preset embarqué indépendamment du CWD.
#    Un chemin absolu custom est également accepté : --preset /chemin/mon-preset.toml
sudo -u gradatum gradatum-admin init --root /var/lib/gradatum --preset hierarchical --non-interactive
# Crée :
#   /var/lib/gradatum/config/{server.toml, bearer.toml, jwt.public.pem, jwt.private.pem, admin.bearer.txt}
#   /var/lib/gradatum/db/{queue.sqlite, revocation.sqlite, api_keys.sqlite}

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

## Post-deploy validation

After deployment, run the smoke test:

```bash
bash scripts/smoke-alpha-4.sh
```

The script executes 7 acceptance steps.

Measure fastembed peak memory at cold start:

```bash
sudo systemctl show gradatum-worker -p MemoryCurrent
```

Compare against the `MemoryMax=1G` budget in the unit file.

## UID/GID statique

`gradatum` : UID **985**, GID **985**.

Rationale:
- UID 991 initially considered but rejected: already occupied by `sshd` on the reference deployment host (`systemd-sysusers` failure).
- Audit of UID/GID range 980-999 found 985 as first commonly available entry. Retained for stability (margin below `timesync` 990).
- Lesson: any static UID/GID assignment must be validated end-to-end via `systemd-sysusers --check` in the CI release gate before tagging.
