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

## Env overrides

All scripts honour:

- `ITERATIONS` — soak count (defaults: 5 for warm-resume, 10 for add/remove).
- `BINARY`     — path to `tt-bh-linux` (default `./target/debug/tt-bh-linux`).
- `LOG_FILE`   — daemon log path (default `./daemon-card0.log`).
- `CARD`       — tt device index (default 0).
- `L2CPU`      — core index to exercise (default 0).

## Typical use

```bash
cargo build
bash scripts/soak_warm_resume.sh         # 5 cycles, ~1 min
bash scripts/soak_add_remove.sh          # 10 cycles, ~20 s

ITERATIONS=20 bash scripts/soak_warm_resume.sh   # longer soak
```

Each script cleans up on exit (daemon stopped via trap), so Ctrl-C or
an assertion failure leaves the host in a recoverable state. If you do
wedge the chip, `(. ~/.tenstorrent-venv/bin/activate && tt-smi -r)`
resets it.

## What isn't covered here

These only hammer the happy paths with expected values. Things left for
a separate coverage pass:

- concurrent RPCs on sibling L2CPUs (need a multi-disk setup)
- crash injection (SIGKILL the daemon mid-RPC)
- long-running guest with I/O pressure during `remove-disk`
- libvdeslirp TCP session loss on `remove-net`
