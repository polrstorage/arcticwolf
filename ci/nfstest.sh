#!/usr/bin/env bash
# NFS integration test steps shared by CI and local runs.
#
# Runs on a Linux host with docker and passwordless sudo, such as the
# GitHub ubuntu-latest runner. The arcticwolf image ($IMAGE) must
# already be in the local docker image store (`make image` +
# `docker tag`).
#
# Usage: ci/nfstest.sh <setup|deploy|verify|posix|logs|cleanup|all>

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
IMAGE="${IMAGE:-arcticwolf:test}"
NFSTESTER_IMAGE="${NFSTESTER_IMAGE:-registry.opensuse.org/home/vcheng/containers/containers/nfstester:latest}"
NFSTEST_CASES="${NFSTEST_CASES:-read,write}"
MANIFEST="${REPO_ROOT}/deploy/k8s/deployment.yaml"

# kubectl runs as the invoking user against a private 0600 copy of the
# k3s admin kubeconfig; the system file keeps its own mode.
K3S_KUBECONFIG=/etc/rancher/k3s/k3s.yaml
export KUBECONFIG="${NFSTEST_KUBECONFIG:-${HOME}/.kube/arcticwolf-nfstest.yaml}"
# Everything the test deploys lives in its own namespace, so cleanup never
# touches a deployment of the regular manifest on the same cluster.
NS="${NFSTEST_NAMESPACE:-arcticwolf-nfstest}"

log() { printf '\n==> %s\n' "$*"; }

ensure_kubeconfig() {
  if [ ! -s "${KUBECONFIG}" ] || [ "${K3S_KUBECONFIG}" -nt "${KUBECONFIG}" ]; then
    mkdir -p "$(dirname "${KUBECONFIG}")"
    sudo install -m 0600 -o "$(id -u)" -g "$(id -g)" \
      "${K3S_KUBECONFIG}" "${KUBECONFIG}"
  fi
}

# kubectl scoped to the test namespace; kc_cluster for the namespace itself.
kc() {
  ensure_kubeconfig
  kubectl -n "${NS}" "$@"
}
kc_cluster() {
  ensure_kubeconfig
  kubectl "$@"
}

node_ip() {
  kc get nodes \
    -o jsonpath='{.items[0].status.addresses[?(@.type=="InternalIP")].address}'
}

# Point the deployment at the locally built image instead of pulling
# from the registry. Rendered to stdout so the checked-in manifest is
# never modified.
render_manifest() {
  sed -e "s|freezevicente/arcticwolf:latest|${IMAGE}|g" \
      -e '/imagePullPolicy/d' \
      -e "s|image: ${IMAGE}|image: ${IMAGE}\n          imagePullPolicy: Never|g" \
      "${MANIFEST}"
}

cmd_setup() {
  if ! command -v k3s >/dev/null 2>&1; then
    log "Install k3s"
    curl -sfL https://get.k3s.io | INSTALL_K3S_EXEC="--disable traefik" sh -
  fi

  log "Wait for k3s to be ready"
  # `kubectl wait` errors out instead of waiting when the node object
  # hasn't registered yet, which is common right after the k3s install.
  local i
  for i in $(seq 60); do
    [ -n "$(kc get nodes -o name 2>/dev/null)" ] && break
    sleep 2
  done
  kc wait --for=condition=Ready nodes --all --timeout=120s
  kc get nodes

  if ! command -v mount.nfs >/dev/null 2>&1; then
    log "Install NFS client tools"
    sudo apt-get update
    sudo apt-get install -y nfs-common
  fi
  # arcticwolf serves portmap on port 111 itself (hostNetwork); the
  # distro rpcbind that nfs-common pulls in would grab the port first.
  sudo systemctl mask --now rpcbind.socket rpcbind.service 2>/dev/null || true
}

cmd_deploy() {
  log "Load image ${IMAGE} into k3s"
  docker save "${IMAGE}" | sudo k3s ctr images import -

  log "Deploy arcticwolf into namespace ${NS}"
  kc_cluster create namespace "${NS}" --dry-run=client -o yaml | kc_cluster apply -f -
  render_manifest | kc apply -f -
  kc rollout status deployment/arcticwolf --timeout=120s
  kc wait --for=condition=Ready pod -l app=arcticwolf --timeout=120s

  kc get pods -o wide
  echo "NFS server available at $(node_ip) (hostNetwork)"
}

cmd_verify() {
  local ip
  ip="$(node_ip)"

  # Phase 4 of #26: MOUNT MNT now routes the client dirpath against
  # ExportRegistry, and MOUNT EXPORT (proc 5) enumerates configured
  # exports. Verify both protocol pieces against the two exports
  # defined in deploy/k8s/deployment.yaml.
  log "Verify MOUNT EXPORT lists both exports"
  showmount -e "${ip}"
  showmount -e "${ip}" | grep -E '^/data\b'
  showmount -e "${ip}" | grep -E '^/backup\b'

  log "Mount /data export"
  sudo mkdir -p /mnt/nfs_data
  sudo mount -t nfs -o nfsvers=3,tcp,nolock "${ip}:/data" /mnt/nfs_data
  ls -la /mnt/nfs_data/

  log "Mount /backup export"
  sudo mkdir -p /mnt/nfs_backup
  sudo mount -t nfs -o nfsvers=3,tcp,nolock "${ip}:/backup" /mnt/nfs_backup
  ls -la /mnt/nfs_backup/

  # Files written through /data must not show up in /backup — the
  # multi-export router dispatches by the uid prefix in each file
  # handle, so /data and /backup own disjoint trees even though they
  # share a single NFS server. /backup is read_only in Phase 5 of #26,
  # so we only seed /data and check that the filename doesn't leak into
  # the other mount.
  log "Verify per-export isolation"
  sudo bash -c 'echo data-only > /mnt/nfs_data/hello-data.txt'
  sudo test -f /mnt/nfs_data/hello-data.txt
  if sudo test -e /mnt/nfs_backup/hello-data.txt; then
    echo "ISOLATION FAILURE: /data file visible in /backup mount" >&2
    exit 1
  fi
  echo "per-export isolation verified"

  # Phase 5 of #26: every write-class NFS procedure short-circuits to
  # NFS3ERR_ROFS when the handle's export is configured read_only=true.
  # The kernel client surfaces that as EROFS, so any write against
  # /mnt/nfs_backup must fail at the syscall layer while reads still
  # succeed.
  log "Verify /backup rejects writes with EROFS"
  local errfile rc
  errfile="$(mktemp)"
  set +e
  sudo bash -c 'echo nope > /mnt/nfs_backup/ro-write.txt' 2> "${errfile}"
  rc=$?
  set -e
  if [ "${rc}" -eq 0 ]; then
    echo "FAILURE: write to read-only /backup unexpectedly succeeded" >&2
    exit 1
  fi
  # NFS3ERR_ROFS → EROFS → "Read-only file system" in shell error.
  # Match loosely so locale variations don't break the assertion.
  if ! grep -qi "read-only" "${errfile}"; then
    echo "FAILURE: expected EROFS, got:" >&2
    cat "${errfile}" >&2
    exit 1
  fi
  rm -f "${errfile}"
  # mkdir against the ro export must also fail.
  if sudo mkdir /mnt/nfs_backup/ro-dir 2>/dev/null; then
    echo "FAILURE: mkdir on read-only /backup unexpectedly succeeded" >&2
    exit 1
  fi
  # Reads must still work — only write-class ops are gated.
  sudo ls /mnt/nfs_backup/ >/dev/null
  echo "read-only export correctly rejects writes"

  # Phase 5 of #26: fsstat now consults statvfs(2) on the export's root
  # rather than returning hardcoded values. df should report numbers
  # consistent with the underlying mount on the pod (a PVC for /data,
  # an emptyDir for /backup); we don't pin exact figures because the
  # host filesystem varies between runs, but the values must be
  # non-zero and df must succeed for both mounts.
  log "Show per-export df stats"
  df -h /mnt/nfs_data /mnt/nfs_backup
  local data_total backup_total
  data_total=$(df --output=size /mnt/nfs_data | tail -n1 | tr -d ' ')
  backup_total=$(df --output=size /mnt/nfs_backup | tail -n1 | tr -d ' ')
  if [ "${data_total}" = "0" ] || [ "${backup_total}" = "0" ]; then
    echo "FAILURE: fsstat reports zero size for an export" >&2
    df --output=source,size,used,avail /mnt/nfs_data /mnt/nfs_backup >&2
    exit 1
  fi
  echo "per-export fsstat stats look sane"

  # A bogus dirpath must fail at mount time — MNT3ERR_NOENT surfaces to
  # the client as ENOENT and mount(8) exits non-zero.
  log "Verify MNT NOENT for unknown dirpath"
  sudo mkdir -p /mnt/nfs_missing
  if sudo mount -t nfs -o nfsvers=3,tcp,nolock "${ip}:/nonexistent" /mnt/nfs_missing 2>&1; then
    sudo umount /mnt/nfs_missing || true
    echo "ERROR: mounting an unknown export should have failed" >&2
    exit 1
  fi
  echo "unknown dirpath correctly rejected"
}

# The nfstester image is amd64-only; on an arm64 host this relies on an
# amd64 binfmt handler.
cmd_posix() {
  log "Run NFS POSIX tests (${NFSTEST_CASES}) against /data"
  docker run --network host --privileged --rm --platform linux/amd64 \
    "${NFSTESTER_IMAGE}" \
    nfstest_posix -s localhost --export=/data --nfsversion=3 \
    -o nolock,noresvport \
    --runtest="${NFSTEST_CASES}"
}

cmd_logs() {
  log "Pod status"
  kc get pods -l app=arcticwolf -o wide || true
  log "Pod logs"
  kc logs -l app=arcticwolf --tail=200 || true
}

cmd_cleanup() {
  log "Cleanup"
  sudo umount /mnt/nfs_data 2>/dev/null || true
  sudo umount /mnt/nfs_backup 2>/dev/null || true
  # Only this run's namespace goes, PVC included; anything deployed from
  # the manifest elsewhere on the cluster is not ours to touch.
  kc_cluster delete namespace "${NS}" --ignore-not-found --wait --timeout=120s || true
}

cmd_all() {
  cmd_setup
  # Start from a clean slate so reruns in a long-lived VM don't reuse
  # the previous run's PVC contents or pod.
  cmd_cleanup
  trap 'cmd_logs; cmd_cleanup' EXIT
  cmd_deploy
  cmd_verify
  cmd_posix
}

case "${1:-}" in
  setup|deploy|verify|posix|logs|cleanup|all) "cmd_$1" ;;
  *)
    echo "usage: $0 <setup|deploy|verify|posix|logs|cleanup|all>" >&2
    exit 2
    ;;
esac
