#!/bin/sh
# SPDX-FileCopyrightText: © 2026 Olof Johansson
# SPDX-License-Identifier: MIT
#
# bhx#166 soak suicide. Each guest sleeps briefly after reaching
# userspace, then issues `poweroff -f` which the kernel translates to
# SBI SRST_SHUTDOWN. With BHX_SOFT_REBOOT=1 on the daemon side
# OpenSBI's stub system_reset_device returns through to sbi_exit →
# sbi_platform_final_exit, the bhx-purgatory hook writes the PARKED
# magic, and the daemon's per-core watcher (scripts/soak_soft_reboot.sh)
# detects it and issues a release-from-purgatory. Cycle repeats.
#
# Inject into a copy of buildroot's quiet rootfs:
#   cp third_party/buildroot/rootfs-l2cpu1-quiet.ext2 rootfs-suicide.ext2
#   e2cp -P 0755 scripts/soak_suicide_init.sh \
#       rootfs-suicide.ext2:/etc/init.d/S99-bhx-suicide
sleep 10
poweroff -f
