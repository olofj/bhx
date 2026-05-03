# Per-OS notes for guest distros on bhx

What you need to know before booting each distro on an L2CPU. The
`bhx image pull <name>` flow downloads + converts a stock cloud image
into something the L2CPU can boot, but each distro ships with its own
quirks (kernel cmdline, console handling, RVA22 vs RVA23 ABI, …) —
this is the cheat-sheet for what to expect, what's broken, and how
to work around it.

The base assumption is that you've already built bhx, brought up
`/dev/tenstorrent/0`, and have the daemon running. See the top-level
[`README.md`](../README.md) for that.

## Image registry quick reference

`bhx image pull <name>` downloads + post-processes a cloud image and
drops it under `$XDG_DATA_HOME/bhx/images/`. Currently working:

| Distro | `<name>` | Notes |
|---|---|---|
| Debian 13 (Trixie) | `debian-13` | Works out of the box. RVA22-only userspace. |
| Ubuntu 24.04 LTS | `ubuntu-24.04` | Works out of the box. The distro of choice for soak validation today. |
| Fedora 42 Cloud | `fedora-42` | Works out of the box. |
| AlmaLinux Kitten 10 | `almalinux-10-kitten` | Boots, **but no console output** without a one-liner fix; see below. |

Pulling lays down both the disk image and a sibling
`<name>.cidata.img` cloud-init seed (default user `bhx`, password
`bhx`, key-only when you supply `--ssh-key`). `bhx boot --image
<name>` resolves both automatically.

## AlmaLinux Kitten 10

The image boots cleanly but RHEL-derivative kernels ship a default
GRUB cmdline pinned to `console=ttyS0,115200 console=tty0` — neither
of which exists on the L2CPU. Result: cloud-init runs, sshd comes up,
the slirp DHCP forward at `127.0.0.1:2222` works — but the operator
never sees a single line on `bhx connect`. Looks "stuck"; is just
silenced.

Fix from inside the running guest (one-shot, persists to subsequent
boots — grubby rewrites all kernel entries):

```sh
sudo grubby --update-kernel=ALL \
    --remove-args="console=ttyS0,115200" \
    --remove-args="console=tty0" \
    --args="console=hvc0"
```

Reboot to pick up the new cmdline. `bhx connect -l N` should now show
the kernel boot log and a login prompt.

You can land the same fix permanently by snapshotting the post-fix
disk image and pointing `bhx boot --disk` at it; the cidata seed is
unchanged.

### Known kernel bug on AlmaLinux 10 — `dma_atomic_pool_init` splat

Kernel 6.12.0-205.el10.riscv64 produces a page-allocation failure
during early init:

```
swapper/0: page allocation failure: order:7, mode:0xcc4(GFP_KERNEL|GFP_DMA32)
...
DMA: failed to allocate 484 KiB GFP_KERNEL|GFP_DMA32 pool for atomic allocation
```

The L2CPU's DRAM physical addresses start above 4 GiB so the kernel's
`GFP_DMA32` zone is empty; `dma_atomic_pool_init` requests an order-7
contiguous allocation from that empty zone and gets `ENOMEM`. The
kernel WARNs and proceeds (it's not a panic), but the dma-pool isn't
populated and any later `GFP_DMA32` driver request also fails.

Doesn't actually break userspace boot or the slirp networking path —
the splat is noisy but harmless for the workloads we currently soak.
Tracked as [#164](https://github.com/olofj/bhx/issues/164) for an
upstream fix or downstream patch.

## Ubuntu 25.10 / 26.04 LTS

Doesn't boot today. Stock distro builds target the RVA23U64 ABI
profile, which includes Zcb (compressed bit-manipulation). The L2CPU
is a SiFive X280 Gen 1 — RVA22 + V; no Zcb. Userspace SIGILLs in
glibc startup before init runs.

Tracked as [#163](https://github.com/olofj/bhx/issues/163) — the path
forward is integrating Benedikt Freisen's
[trap-based ISA emulation OpenSBI patches](https://lore.kernel.org/all/20251227121802.15703-1-b.freisen@gmx.net/),
which trap-and-emulate Zcb in M-mode. Not performant, but lets stock
RVA23 distro builds boot. Validated by the patch author specifically
on Blackhole + X280.

If you need a 25.10 / 26.04 booting today: pin to a custom kernel
build with `CONFIG_RISCV_ISA_ZCB=n` and a custom rootfs whose libc
was built without Zcb. Out of scope for the registry.

## Fedora 42

Works out of the box, including console. Same RVA22-only userspace as
Debian. Largest image of the supported set (~10 GiB for the cloud
spin) so `bhx image pull` takes a minute.

## Debian 13 (Trixie)

Works out of the box. The U-Boot path is the most thoroughly soaked
for this distro — it's what the `tt-bh-linux` kernel team validates
against. RVA22-only userspace.

## Buildroot

For test-rig use rather than operator-facing distro work. The
in-tree [`third_party/buildroot/`](../third_party/buildroot/README.md)
builds a small `rootfs.ext4` with auto-login, `fio`, `iperf3`, and a
test helper. Used by the soak scripts under `scripts/`. Not a
"daily-driver" guest — no package manager, no cloud-init, no DHCP
hostname.

## Common gotchas across distros

- **Ubuntu's cloud-init state is sticky** even after you swap the
  cidata seed. After first boot, the guest writes
  `/var/lib/cloud/data/instance-id`; cloud-init compares against the
  seed's `instance-id` on subsequent boots and skips re-running its
  config modules if they match. Bumping `instance-id` alone in the
  seed isn't always enough — wipe `/var/lib/cloud` or re-pull the
  disk if a seed change isn't taking effect.
- **`bhx connect`** uses Ctrl-A x to detach, like screen / tmux.
  Scripts that can't send Ctrl-A x should always run `connect` under
  `timeout`: `timeout 5 bhx connect -l 0 </dev/null 2>err.log`.
- **First-boot SSH banner timing** varies by distro. Ubuntu 24.04
  reaches `STATUS_DRIVER_OK 0x0f` on all virtio slots within ~15 s
  of `bhx boot` returning, but the kernel-side virtio probe finishes
  *just* around that 15-second mark on the L2CPU that boots second
  in a concurrent multi-L2CPU scenario. Soak scripts that check
  "did virtio probe finish?" should use a 60 s settle window.
- **`bhx daemon stop` is mandatory before `tt-smi -r`.** The kmd
  reset re-enumerates the chardev under the daemon's still-mapped
  windows; if the daemon survives, the per-card chip-fault handler
  catches the resulting SIGBUS and `_exit`s with 134, which is the
  right behavior but easier to read in the log if you orchestrate
  the stop yourself first.
