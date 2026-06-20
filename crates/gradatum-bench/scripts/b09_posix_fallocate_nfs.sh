#!/bin/bash
# B9 — posix_fallocate NFS test (P2 — gated GRADATUM_TEST_NFS_PATH)
#
# Vérifie si posix_fallocate(2) est supporté sur le filesystem cible.
# Sur NFS, l'appel retourne souvent EOPNOTSUPP (errno 95) — le guard NFS
# de gradatum-storage doit le détecter via statfs (caveat C11 spec §0.3).
#
# Usage :
#   GRADATUM_TEST_NFS_PATH=/mnt/nfs ./b09_posix_fallocate_nfs.sh
#
# Sans GRADATUM_TEST_NFS_PATH : test sur /tmp (skip NFS check).

set -euo pipefail

NFS_PATH="${GRADATUM_TEST_NFS_PATH:-/tmp}"
TEST_FILE="${NFS_PATH}/.gradatum-bench-fallocate-$$"

echo "B9 — posix_fallocate NFS test"
echo "Path cible : ${NFS_PATH}"

if [[ "${NFS_PATH}" == "/tmp" ]]; then
    echo "GRADATUM_TEST_NFS_PATH non défini — skip (défaut /tmp, pas NFS)"
    echo "STATUS: SKIPPED"
    exit 0
fi

if [[ ! -d "${NFS_PATH}" ]]; then
    echo "ERREUR: ${NFS_PATH} n'existe pas ou n'est pas un répertoire"
    exit 1
fi

# Détection type de filesystem via stat -f.
FS_TYPE=$(stat -f -c '%T' "${NFS_PATH}" 2>/dev/null || echo "unknown")
echo "Type de filesystem : ${FS_TYPE}"

if [[ "${FS_TYPE}" != "nfs"* && "${FS_TYPE}" != "nfs4"* ]]; then
    echo "AVERTISSEMENT: le chemin ne semble pas être un montage NFS (type=${FS_TYPE})"
    echo "Le test continue mais les résultats ne sont pas représentatifs."
fi

# Test posix_fallocate via Python (portable Python 3.x).
python3 - "${TEST_FILE}" << 'PYTHON_SCRIPT'
import os, sys

test_file = sys.argv[1]
fd = os.open(test_file, os.O_WRONLY | os.O_CREAT, 0o644)
try:
    os.posix_fallocate(fd, 0, 4096)
    print("posix_fallocate(4096): OK — supporté sur ce filesystem")
    result = "SUPPORTED"
except OSError as e:
    if e.errno == 95:  # EOPNOTSUPP
        print(f"posix_fallocate: EOPNOTSUPP (errno 95) — non supporté sur NFS")
        print("INFO: gradatum-storage doit désactiver posix_fallocate sur ce chemin")
        result = "EOPNOTSUPP"
    else:
        print(f"posix_fallocate: ERREUR inattendue (errno {e.errno}: {e.strerror})")
        result = f"ERROR_{e.errno}"
finally:
    os.close(fd)
    try:
        os.unlink(test_file)
    except OSError:
        pass

print(f"STATUS: {result}")
PYTHON_SCRIPT
