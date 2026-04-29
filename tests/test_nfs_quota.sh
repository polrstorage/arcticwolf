#!/usr/bin/env bash
# NFS folder quota smoke test.
#
# Exercises the quota feature end-to-end using a real kernel NFS client:
#   1. Write under the limit  -> succeeds
#   2. Write exceeding limit   -> kernel reports EDQUOT (Disk quota exceeded)
#   3. Remove the file         -> frees quota; subsequent write succeeds
#   4. Truncate down           -> frees quota; subsequent write succeeds
#   5. Statfs (`df`)           -> reports the quota limit, not the real FS
#
# Requires the server to be running with quota enabled and a bootstrap
# entry for the PVC directory. The CI workflow configures this; local
# developers can run it after preparing a matching config.toml.

set -euo pipefail

MOUNT_POINT="${MOUNT_POINT:-/mnt/nfs}"
PVC_NAME="${PVC_NAME:-pvc-quota-test}"
# The bootstrap entry must match this value. 1 MiB keeps the test fast.
QUOTA_BYTES="${QUOTA_BYTES:-1048576}"

PVC_DIR="${MOUNT_POINT}/${PVC_NAME}"
# Per-run scratch file for capturing dd's stderr; mktemp avoids collisions
# when multiple invocations run in parallel (e.g. local + CI on the same
# host) which a fixed path like /tmp/dd_err cannot.
DD_ERR=$(mktemp -t arcticwolf-quota-dd-err.XXXXXX)

log() { echo "== $*" ; }
fail() { echo "FAIL: $*" >&2 ; exit 1 ; }

cleanup() {
    # Leave the directory in a clean state between runs so CI can replay,
    # and remove the per-run dd stderr scratch file.
    rm -rf "${PVC_DIR}"/* 2>/dev/null || true
    rm -f "${DD_ERR}" 2>/dev/null || true
}
trap cleanup EXIT

log "Mount point: ${MOUNT_POINT}"
log "PVC dir:     ${PVC_DIR}"
log "Quota:       ${QUOTA_BYTES} bytes"

mkdir -p "${PVC_DIR}"

log "Test 1: write just under the limit should succeed"
# 512 KiB write — well under the 1 MiB limit.
dd if=/dev/zero of="${PVC_DIR}/small.bin" bs=1024 count=512 status=none \
    || fail "under-limit write unexpectedly failed"

log "Test 2: write past the limit should be rejected with EDQUOT"
# 2 MiB write — larger than the 1 MiB limit.
set +e
dd if=/dev/zero of="${PVC_DIR}/huge.bin" bs=1024 count=2048 status=none 2> "${DD_ERR}"
rc=$?
set -e
if [[ ${rc} -eq 0 ]]; then
    fail "over-limit write unexpectedly succeeded"
fi
if ! grep -qi "disk quota exceeded\|quota exceeded\|EDQUOT" "${DD_ERR}"; then
    cat "${DD_ERR}" >&2
    fail "over-limit write returned an error but not EDQUOT"
fi
log "  -> write rejected as expected: $(head -1 "${DD_ERR}")"
# The partial file that was created before the failure should be cleaned up
# so subsequent tests start from a known baseline.
rm -f "${PVC_DIR}/huge.bin"

log "Test 3: remove then re-write should succeed"
# REMOVE waits for the redb commit before returning, so quota is already
# freed by the time the unlink RPC completes — no settling delay needed.
rm -f "${PVC_DIR}/small.bin"
dd if=/dev/zero of="${PVC_DIR}/small.bin" bs=1024 count=512 status=none \
    || fail "write after remove unexpectedly failed"

log "Test 4: truncate down then re-write should succeed"
# Truncate the file down to 0, which should free all of its quota.
# SETATTR also persists synchronously, so no sleep is needed here either.
: > "${PVC_DIR}/small.bin"
# 900 KiB write — would exceed the limit if the truncate didn't release.
dd if=/dev/zero of="${PVC_DIR}/small.bin" bs=1024 count=900 status=none \
    || fail "write after truncate unexpectedly failed"

log "Test 5: df should report the quota as the filesystem size"
df_out=$(df -B1 --output=size,used,avail "${PVC_DIR}" | tail -1)
total=$(awk '{print $1}' <<<"${df_out}")
echo "  df total=${total} expected=${QUOTA_BYTES}"
if [[ "${total}" != "${QUOTA_BYTES}" ]]; then
    fail "df total-bytes mismatch: got ${total}, expected ${QUOTA_BYTES}"
fi

log "All quota tests passed."
