# Hardware soak scripts

Bash scripts that exercise the daemon's new code paths on real
hardware, with per-step assertions. A non-zero exit means regression;
zero exit with a `PASS:` line means it held up.

These aren't part of `cargo test` — they need a live Blackhole card and
the tt-kmd driver, plus a `rootfs.ext4` and the firmware bundle
(`fw_jump.bin` / `Image` / `blackhole-card.dtb`) in the project root.
Run them from `tt-bh-linux-rs/` after `cargo build`.

## Scripts

| Script | What it exercises |
|--------|-------------------|
| `soak_warm_resume.sh` | N cycles of `daemon stop` + `daemon start`; asserts each restart re-adopts L2CPU 0 via warm-resume (probe passes, slot adopted). |
| `soak_add_remove.sh`  | N cycles of `add-disk` / `remove-disk` / `add-net` / `remove-net`; asserts `daemon status` after each step. Also verifies double-remove errors cleanly without mutating the slot. |
| `soak_concurrent.sh`  | Cold-boots all 4 L2CPUs (each with its own `rootfs-N.ext4`), then runs N iterations of **4-way concurrent** `remove-disk`/`add-disk`/`remove-net`/`add-net` hammering sibling slots in parallel, with a background `daemon status` poller alongside. Boots themselves are serialized by the daemon's `boot_lock` ([issue #1](https://github.com/olofj/tt-bh-rust/issues/1)); this soak exercises the post-boot concurrent RPC surface. |

## Env overrides

All scripts honour:

- `ITERATIONS`     — soak count (defaults: 5 for warm-resume, 10 for add/remove, 5 for concurrent).
- `BINARY`         — path to `tt-bh-linux` (default `./target/debug/tt-bh-linux`).
- `LOG_FILE`       — daemon log path (default `./daemon-card0.log`).
- `CARD`           — tt device index (default 0).
- `L2CPU`          — core index to exercise (default 0, ignored by `soak_concurrent.sh` which always uses all 4).
- `STATUS_POLL_HZ` — background status poll frequency in `soak_concurrent.sh` (default 20).

## Typical use

```bash
cargo build
bash scripts/soak_warm_resume.sh         # 5 cycles, ~1 min
bash scripts/soak_add_remove.sh          # 10 cycles, ~20 s
bash scripts/soak_concurrent.sh          # 4-core cold boot + 5 concurrent cycles, ~2 min

ITERATIONS=20 bash scripts/soak_warm_resume.sh   # longer soak
```

Each script cleans up on exit (daemon stopped via trap), so Ctrl-C or
an assertion failure leaves the host in a recoverable state. If you do
wedge the chip, `(. ~/.tenstorrent-venv/bin/activate && tt-smi -r)`
resets it.

## What isn't covered here

These only hammer the happy paths with expected values. Things left for
a separate coverage pass:

- 4-way parallel cold boot (currently gated by the daemon's `boot_lock`; see [issue #1](https://github.com/olofj/tt-bh-rust/issues/1))
- crash injection (SIGKILL the daemon mid-RPC)
- long-running guest with I/O pressure during `remove-disk`
- libvdeslirp TCP session loss on `remove-net`
