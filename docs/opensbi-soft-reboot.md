# OpenSBI soft-reboot reading notes

Background and load-bearing details for #166 (purgatory/handshake architecture
for guest shutdown/reboot without chip-side reset). Pull this up alongside
the issue when working on Phase 1+.

## Why we need a soft-reboot path

`tt-isa-documentation/BlackholeA0/L2CPUTile/README.md`, Reset section, verbatim:

> Due to a hardware bug, the harts within each L2CPU tile can only be brought
> out of reset once. Once running, putting them back into reset requires
> resetting the entire Blackhole ASIC.

So once a hart's `l2cpu_risc_reset_n` bit (bit `idx+4` of `L2CPU_RESET @
0x80030014`) goes 0→1, we can never put it back into reset cleanly.
`reset_board()` (PCIe LDS reset) works because it resets the ASIC, but
wipes GDDR + invalidates every host mmap on the card.

The only working in-place "re-boot" mechanism is to keep the hart running
in M-mode and hand it a new entry point — kexec/purgatory pattern, but
with the host (over PCIe) playing the role usually filled by the running
OS or an on-chip monitor.

## OpenSBI mainline already has the park-in-M-mode loop

The path in mainline 1.7 `riscv-software-src/opensbi`:

### 1. `lib/sbi/sbi_system.c::sbi_system_reset`

```c
void __noreturn sbi_system_reset(u32 reset_type, u32 reset_reason)
{
    sbi_ipi_send_halt(0, -1UL);          // halt every other hart via IPI
    sbi_hsm_hart_stop(scratch, false);   // mark current hart STOP_PENDING
    if (dom->system_reset_allowed) {
        const struct sbi_system_reset_device *dev =
            sbi_system_reset_get_device(reset_type, reset_reason);
        if (dev) dev->system_reset(reset_type, reset_reason);
    }
    sbi_exit(scratch);                   // <-- the fallback
}
```

If no `sbi_system_reset_device` is registered (or its `system_reset_check`
returns `SBI_ENOTSUPP` for the requested reset type), the fallthrough is
`sbi_exit(scratch)` → `sbi_hsm_exit`.

**For bhx**: don't register a `sbi_system_reset_device` in our patched
firmware. The generic platform on mainline 1.7 doesn't register one by
default unless DTB provides a `syscon-poweroff` / `syscon-reboot`
node — which bhx's `modify_dtb` adds today for the SBI SRST path
(see `src/boot.rs` and #94). To get the soft-reboot fallback we'll
either:

  - Drop the `syscon-poweroff` / `syscon-reboot` node injection from
    `modify_dtb` (cleanest), or
  - Have OpenSBI's syscon-reset driver report `SBI_ENOTSUPP` for our
    use case.

### 2. `lib/sbi/sbi_hsm.c::sbi_hsm_exit`

```c
void __noreturn sbi_hsm_exit(struct sbi_scratch *scratch)
{
    /* As platform is lacking support for hotplug, directly jump to warmboot
     * and wait for interrupts in warmboot. We do it preemptively in order
     * preserve the hart states and reuse the code path for hotplug. */
    jump_warmboot();
}
```

`jump_warmboot()` is a function pointer to `_start_warm`, saved in
`scratch->warmboot_addr` at first cold boot (`firmware/fw_base.S`).

### 3. `firmware/fw_base.S::_start_warm`

The standard OpenSBI warmboot entry. Re-runs `sbi_init` for the hart (with
`COLD_BOOT=0`), eventually landing in `sbi_hsm_hart_wait` for harts that
are not the cold-boot hart, or in `init_warmboot_run` for HSM-started
harts.

### 4. `lib/sbi/sbi_hsm.c::sbi_hsm_hart_wait`

```c
while (atomic_read(&hdata->state) != SBI_HSM_STATE_START_PENDING) {
    if (--max_wait_iter == 0) {
        sbi_revert_entry_count(scratch);
        hsm_device_hart_stop();
    }
    wfi();
}
```

The wait loop. State preserved across the loop:
- `sbi_scratch` (per-hart) — in DRAM, reachable from PCIe via the L2CPU's
  persistent TLB windows
- `warmboot_addr` — function pointer to `_start_warm`
- `next_addr`, `next_mode`, `next_arg1` — set by `sbi_hsm_hart_start` to
  tell the parked hart where to jump on wake
- HSM state machine — atomic per-hart `state` field

Exit conditions: state flipping to `START_PENDING` (normal HSM-resume
path), or `max_wait_iter` exhausting (calls `hsm_device_hart_stop` to do
a platform-specific final stop — bhx's generic platform doesn't define
one, so this just returns).

### 5. Wake-up sequence (mainline)

`sbi_hsm.c::sbi_hsm_hart_start`:

1. Caller (an S-mode kernel issuing SBI HSM_HART_START) provides
   `hartid`, `next_addr`, `next_mode`, `next_arg1`.
2. `sbi_hsm_hart_start` writes those into the target hart's `sbi_scratch`.
3. CASes the target's HSM state from `STOPPED` to `START_PENDING`.
4. Sends an IPI (CLINT MSIP) to the target.
5. Target's `wfi` returns, while-loop sees `state == START_PENDING`,
   exits the loop.
6. Target's `_start_warm` continues into `init_warmboot_run` →
   `sbi_hart_switch_mode` → jumps to `next_addr` in `next_mode`.

**For bhx**: the host plays the role of the S-mode kernel. The host writes
`next_addr` etc. into the target hart's `sbi_scratch` over PCIe NoC,
flips the HSM state atomically (write-with-correct-ordering), and triggers
an MSIP write to wake the parked hart.

## State to consider before / across SRST → re-entry

| State | Carries? | Concern | Action |
|---|---|---|---|
| `sbi_scratch` (per-hart) | yes | Lives in DRAM. Host writes `next_addr` into it before release. | Carve `sbi_scratch` region out of `/memory` in patched DTB so kernel doesn't allocate there. |
| `sbi_domain` table | yes | Built once at cold init. | Reuse on warmboot; new image's `_start` rebuilds if we ever load fresh OpenSBI. |
| **PMP entries** | sticky if `L=1` | Mainline 1.7's `sbi_hart_pmp_configure` does NOT set the lock bit on the generic platform. | Confirm in Phase 1 by reading `pmpcfg*` after SRST. If any entry has `L=1`, we have a hardware-locked region until Blackhole reset. |
| `mtvec`, `mepc`, `mscratch` | overwritten | New `_start_warm` redirects. | No action. |
| `mstatus`, `mie` | live | Parked hart could take spurious traps. | Clear `mie` in `final_exit` hook before `jump_warmboot`. |
| Pending IPI (CLINT MSIP) | live | Parked hart wakes immediately on its old IPI bit. | Clear MSIP for current hart before park. |
| Pending external IRQ (PLIC) | live | Latched claims survive. | Mask `mie.MEIE` before park; new image's `_start` re-inits PLIC. |
| `mtimecmp` | live | Timer interrupt fires immediately after park. | Set `mtimecmp = -1` (never), or `mie.MTIE = 0`. |
| L1 icache | stale post-re-image | New image bytes hidden. | `fence.i` on every hart before jumping. |
| L1/L2 dcache | stale post-re-image | Same. | `cbo.flush` walk over rewritten regions, or chip-wide L3 flush if available. |
| L3 cache (chip-wide) | stale post-re-image | Same. | Platform-specific. SiFive X280 cbo.flush walks per-line; chip-wide flush via tile control register if exposed. |
| PLL state | live | Daemon-owned, fine. | No action. |
| TLBs | live | M-mode unused, S-mode dirty. | `sfence.vma` in new `_start` (mainline already does this). |

## Daemon-side mechanics: what happens over PCIe

Locating the host's read/write targets:

- **`sbi_scratch` for each hart**: mainline OpenSBI puts scratch at the
  end of the firmware's reserved region, growing downward. For
  fw_jump.bin on the generic platform, scratch is at `_fw_end -
  scratch_size_per_hart * num_harts`. The patched DTB needs to reserve
  `[_fw_start, _fw_end)` from `/memory` so kernel doesn't trample.
- **HSM state field**: `&scratch[hartid].hartid_to_state` (atomic u8 in
  scratch).
- **IPI path**: write `1` to `clint_base + MSIP_offset_for_target` to set
  pending. CLINT lives on the L2CPU's local bus; the daemon already has
  the L2CPU TLB programmed to reach it via the L2Cpu's persistent 4 GB
  windows.

## Prior art

- **Microchip PolarFire HSS** (`polarfire-soc/hart-software-services`):
  closest production-quality soft-reboot implementation. The warm path
  in `services/reboot/reboot_service.c::HSS_reboot` uses an
  IPI-with-function-pointer trick:

      IPI_Send(peer, IPI_MSG_GOTO, 0u, PRV_M, do_srst_ecall, NULL);

  The receiving hart `ecall`s into OpenSBI to land in the warmboot path
  with a freshly-loaded payload from M-mode HSS DRAM. The on-chip E51
  monitor plays the host role — directly translatable to "PCIe host
  writes scratch + IPIs the parked hart" for our shape.

- **Linux RISC-V kexec** (`arch/riscv/kernel/machine_kexec.c` +
  `cpu-hotplug.c`): the S-mode kernel calls SBI HSM_HART_STOP on every
  non-boot hart before jumping the boot hart to the new image. Pattern
  for guest behavior — when our guest issues SBI SRST it should already
  have STOPped its secondaries.

- **Linux RISC-V purgatory** (`arch/riscv/purgatory/`): tiny, just SHA-256
  verifies and jumps. The cache-flush + multi-hart convergence happens in
  `machine_kexec.c` before purgatory runs; the purgatory itself doesn't
  need to coordinate. Mirror the structure.

## What NOT to do

- **Don't write a custom relocate-and-jump purgatory blob.** The HSM
  warmboot loop already handles park-in-M-mode. Adding a relocated blob
  on top is duplicate machinery.
- **Don't toggle `L2CPU_RESET` bit `idx+4` in the soft-reboot path.**
  Hardware bug: undefined behavior on a hart that's been released. Use
  the HSM IPI wake instead.
- **Don't try to reload a fresh OpenSBI binary.** The HSM warmboot path
  re-enters the SAME `_start_warm` of the old OpenSBI. Loading a new
  M-mode binary requires clobbering the warmboot trampoline itself —
  feasible but more invasive. Keep the same OpenSBI across re-images
  unless we hit a concrete reason to change that.

## Phase 1 verification

Land the smallest possible hook that proves the SRST → fall-through →
final_exit path works:

1. Patch OpenSBI: add a `sbi_platform_final_exit` (or equivalent earliest
   hook in the SRST path that runs in M-mode after IPIs are sent and
   peers have acked STOP) that writes a fixed magic (e.g., `"PARKED__"`
   as a u64) at a known DRAM offset.
2. In `boot::modify_dtb`, drop the `syscon-poweroff` injection so SBI
   SRST falls through to the OpenSBI fallback path instead of being
   diverted to BRISC's shutdown register.
3. Boot a buildroot guest, run `poweroff -f`.
4. Read the magic via the daemon's NoC access. Adding a tiny
   `bhx debug read-mem -l <l2cpu> -a <addr>` subcommand if not already
   present.
5. Confirm we see `"PARKED__"`.

That's the minimum viable validation — proves the chain works and lets
us start designing the Phase-2 cache flush + Phase-4 re-image without
fighting hidden control-flow.
