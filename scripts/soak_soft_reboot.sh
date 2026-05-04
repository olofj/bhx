#!/usr/bin/env bash
# SPDX-FileCopyrightText: © 2026 Olof Johansson
# SPDX-License-Identifier: MIT
#
# 3-guest 100-cycle boot / in-guest poweroff / re-boot soak for the
# OpenSBI-purgatory soft-reboot path (#166).
#
# What each iteration does, per L2CPU [0, 1, 2] in parallel:
#   1. Drive an in-guest `poweroff -f` (SBI SRST_SHUTDOWN).
#   2. Wait until `bhx daemon status` reports the L2CPU's purgatory
#      cell holds the "PARKED__" magic — confirms the SRST fall-through
#      reached our patched `sbi_platform_final_exit` and the harts
#      have entered `sbi_hsm_hart_wait`.
#   3. Trigger a release-from-purgatory: re-write the OpenSBI/kernel/DTB
#      bytes, write `next_addr` into hart 0's scratch over PCIe, IPI
#      hart 0. (Daemon RPC: TBD; placeholder shell hook below.)
#   4. Wait for the slot to report Running again with a fresh kernel up.
#
# Then move to the next iteration.
#
# Implementation phase status (all per #166):
#   - Phase 1 (sbi_platform_final_exit + stub reset device + magic
#     write) is in place — `BHX_SOFT_REBOOT=1` boot, `poweroff -f`,
#     status shows PARKED. Step 2 of the loop above passes today.
#   - Phase 4 (host-side release: scratch write + IPI) is NOT in place
#     yet. Step 3 below is a TODO placeholder; the script will exit
#     cleanly with a "Phase 4 not implemented yet" message at iter 1.
#     When Phase 4 lands, replace the placeholder release_purgatory()
#     body with the real RPC.
#
# Until Phase 4 lands, this script is useful for:
#   - Validating Phase 1 ergonomics: status shows PARKED after a single
#     `poweroff -f` on a single L2CPU.
#   - Soaking the 3-L2CPU parallel-park step (run with ITERATIONS=1):
#     proves three concurrent SRSTs all land in their respective
#     purgatory hooks without disturbing each other.
#
# Env:
#   ITERATIONS   number of boot/poweroff/boot cycles (default 100)
#   CORES        space-separated L2CPU indices (default "0 1 2")
#   BINARY       bhx binary (default ./target/debug/bhx)
#   LOG_FILE     daemon log path (default ./daemon-card0.log)
#   CARD         tt device index (default 0)
#   ROOTFS       rootfs to copy per-core (default buildroot quiet rootfs)
#   PARK_TIMEOUT seconds to wait for PARKED magic per L2CPU (default 30)
#   BOOT_TIMEOUT seconds to wait for guest shell prompt (default 60)

set -euo pipefail

ITERATIONS=${ITERATIONS:-100}
CORES_STR=${CORES:-"0 1 2"}
read -ra CORES <<<"$CORES_STR"
BINARY=${BINARY:-./target/debug/bhx}
LOG_FILE=${LOG_FILE:-./daemon-card0.log}
CARD=${CARD:-0}
PARK_TIMEOUT=${PARK_TIMEOUT:-30}
BOOT_TIMEOUT=${BOOT_TIMEOUT:-60}
PARKED_MAGIC="0x5f5f44454b524150"

fail() { echo "FAIL: $*" >&2; exit 1; }
note() { echo "[soak-soft-reboot] $*"; }

cleanup() {
    "$BINARY" daemon stop -t "$CARD" >/dev/null 2>&1 || true
}
trap cleanup EXIT

[ -x "$BINARY" ] || fail "binary $BINARY not executable (cargo build first)"

if [ -z "${ROOTFS:-}" ]; then
    if [ -e third_party/buildroot/rootfs-l2cpu1-quiet.ext2 ]; then
        ROOTFS=third_party/buildroot/rootfs-l2cpu1-quiet.ext2
    elif [ -e third_party/buildroot/rootfs.ext4 ]; then
        ROOTFS=third_party/buildroot/rootfs.ext4
    elif [ -e rootfs.ext4 ]; then
        ROOTFS=rootfs.ext4
    fi
fi
[ -n "${ROOTFS:-}" ] && [ -e "$ROOTFS" ] \
    || fail "no rootfs; set ROOTFS or build third_party/buildroot"
[ -e fw_jump.bin ] || fail "fw_jump.bin missing"
[ -e Image ] || fail "Image missing"
[ -e blackhole-card.dtb ] || fail "blackhole-card.dtb missing"

# Per-core rootfs copies: concurrent disk workers can't share an mmap'd
# backing file.
for i in "${CORES[@]}"; do
    if [ ! -e "rootfs-soft-${i}.ext2" ]; then
        note "copying $ROOTFS -> rootfs-soft-${i}.ext2"
        cp --reflink=auto "$ROOTFS" "rootfs-soft-${i}.ext2"
    fi
done

note "tt-smi -r (cold chip)"
(. ~/.tenstorrent-venv/bin/activate && tt-smi -r) >/dev/null 2>&1

rm -f "$LOG_FILE"
note "daemon start (BHX_SOFT_REBOOT=1)"
BHX_SOFT_REBOOT=1 "$BINARY" daemon start -t "$CARD" --log-file "$LOG_FILE" >/dev/null
sleep 0.3

# Cold boot all three L2CPUs once. Subsequent iterations re-use this
# state; the soft-reboot release path keeps the slot's L2Cpu alive.
note "cold boot of L2CPUs ${CORES[*]}"
for i in "${CORES[@]}"; do
    BHX_SOFT_REBOOT=1 "$BINARY" boot -t "$CARD" -l "$i" \
        -d "rootfs-soft-${i}.ext2" >/dev/null \
        || fail "cold boot l2cpu $i failed"
done

# Wait for all three guests to be at a shell prompt before the soak.
note "waiting for guests to reach shell prompt"
for i in "${CORES[@]}"; do
    if ! timeout "$BOOT_TIMEOUT" python3 - "$i" <<'EOF'
import sys, pexpect
idx = sys.argv[1]
child = pexpect.spawn(f"./target/debug/bhx connect -l {idx} --mode ro",
                     encoding="utf-8", timeout=30)
child.expect([r"# $", r"buildroot login:", pexpect.EOF, pexpect.TIMEOUT], timeout=60)
EOF
    then
        fail "guest $i didn't reach prompt within ${BOOT_TIMEOUT}s"
    fi
done

# ---- Helpers ----

# Read a single L2CPU's purgatory cell as an integer (decoded from
# `bhx daemon status` output's `purgatory: <label> (0x...)` line).
read_purgatory() {
    local idx=$1
    "$BINARY" daemon status -t "$CARD" 2>/dev/null \
      | awk -v idx="$idx" '
          $0 ~ "^  l2cpu " idx ":"           { in_block=1; next }
          in_block && /^  l2cpu /             { in_block=0 }
          in_block && /^    purgatory: /      {
              gsub(/[()]/, "", $4); print $4; exit
          }'
}

# Send `poweroff -f` to L2CPU N's guest. Spawns pexpect, returns when
# the connect socket EOFs or 30s timeout.
trigger_poweroff() {
    local idx=$1
    timeout 60 python3 - "$idx" <<'EOF'
import sys, pexpect
idx = sys.argv[1]
child = pexpect.spawn(f"./target/debug/bhx connect -l {idx} --mode rw",
                     encoding="utf-8", timeout=60)
child.sendline("")
i = child.expect([r"# $", r"buildroot login:", pexpect.TIMEOUT], timeout=30)
if i == 1:
    child.sendline("root")
    child.expect([r"# $", pexpect.TIMEOUT], timeout=30)
elif i != 0:
    sys.exit(f"l2cpu {idx}: no prompt")
child.sendline("poweroff -f")
child.expect([r"Power down", pexpect.EOF, pexpect.TIMEOUT], timeout=30)
EOF
}

# Wait for L2CPU N's purgatory cell to read PARKED, polling once per
# second up to PARK_TIMEOUT.
wait_for_parked() {
    local idx=$1
    local deadline=$(( $(date +%s) + PARK_TIMEOUT ))
    while [ "$(date +%s)" -lt "$deadline" ]; do
        local v
        v=$(read_purgatory "$idx" || true)
        if [ "$v" = "$PARKED_MAGIC" ]; then
            return 0
        fi
        sleep 1
    done
    return 1
}

# Phase 4 placeholder. When the host-side release path lands, this
# function should:
#   1. Re-write OpenSBI / kernel / DTB into L2CPU DRAM via the daemon
#      (existing `boot_l2cpu` path).
#   2. Write `next_addr` (= OpenSBI _start), `next_mode` (=M-mode), and
#      `next_arg1` (=DTB PA) into hart 0's `sbi_scratch`.
#   3. Atomically flip hart 0's HSM state to START_PENDING.
#   4. IPI hart 0 (write 1 to CLINT MSIP[0]).
#   5. Poll for the slot to report Running with a fresh kernel up.
#
# Until that lands, fail loud.
release_purgatory() {
    local idx=$1
    echo "[soak-soft-reboot] release_purgatory($idx): NOT IMPLEMENTED (Phase 4 of #166)" >&2
    return 99
}

# ---- Main loop ----

note "starting $ITERATIONS-cycle soak across L2CPUs ${CORES[*]}"
for iter in $(seq 1 "$ITERATIONS"); do
    note "iter $iter/$ITERATIONS: in-guest poweroff -f on L2CPUs ${CORES[*]}"

    # 1. Send poweroff -f to all 3 in parallel.
    pids=()
    for i in "${CORES[@]}"; do
        trigger_poweroff "$i" >"/tmp/soak-$i-poweroff.log" 2>&1 &
        pids+=($!)
    done
    for p in "${pids[@]}"; do
        wait "$p" || note "  warning: poweroff subprocess $p exited non-zero"
    done

    # 2. Wait for all 3 to reach PARKED.
    note "iter $iter: waiting for PARKED on all cores"
    for i in "${CORES[@]}"; do
        if wait_for_parked "$i"; then
            note "  l2cpu $i: PARKED"
        else
            fail "iter $iter: l2cpu $i did not reach PARKED within ${PARK_TIMEOUT}s"
        fi
    done

    # 3. Release each L2CPU back into a fresh boot.
    note "iter $iter: releasing all cores from purgatory"
    for i in "${CORES[@]}"; do
        if ! release_purgatory "$i"; then
            note "iter $iter: release_purgatory($i) failed (expected until Phase 4 lands)"
            note "stopping soak; PASSED iters: $((iter - 1))"
            exit 1
        fi
    done
done

note "PASS: $ITERATIONS iterations completed across L2CPUs ${CORES[*]}"
