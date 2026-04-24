# Hardware soak scripts

Scripts that exercise the daemon's new code paths on real hardware,
with per-step assertions. A non-zero exit means regression; zero exit
with a `PASS:` line means it held up.

These aren't part of `cargo test` — they need a live Blackhole card and
the tt-kmd driver, plus a `rootfs.ext4` and the firmware bundle
(`fw_jump.bin` / `Image` / `blackhole-card.dtb`) in the project root.
Run them from `tt-bh-linux-rs/` after `cargo build`.

## Scripts

| Script | What it exercises |
|--------|-------------------|
| `soak_warm_resume.sh` | N cycles of `daemon stop` + `daemon start`; asserts each restart re-adopts L2CPU 0 via warm-resume (probe passes, slot adopted). |
| `soak_add_remove.sh`  | N cycles of `add-disk` / `remove-disk` / `add-net` / `remove-net`; asserts `daemon status` after each step. Also verifies double-remove errors cleanly without mutating the slot. |
| `soak_concurrent.sh`  | Boots all 4 L2CPUs in parallel (each with its own `rootfs-N.ext4`), then runs N iterations of **4-way concurrent** `remove-disk`/`add-disk`/`remove-net`/`add-net` hammering sibling slots in parallel, with a background `daemon status` poller alongside. Chip-wide AXI ops go through `SharedChip::seq_lock`; per-L2CPU NOC traffic goes through each core's own fd. See [issue #1](https://github.com/olofj/tt-bh-rust/issues/1). |
| `console_roundtrip.py` | End-to-end console I/O stress — logs into the guest via `connect`, puts the tty in raw mode, then roundtrips 64 KiB of base64 text in each direction (guest→host and host→guest) and compares sha256. Validates `chip_console`'s `push_char` / `pop_char` + `ConsoleHub` fan-out under sustained transfers. Caller is responsible for having the daemon up with at least one L2CPU booted + at a login prompt. See "Concurrent console roundtrip" below for the 4-way stress form. |

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

Each `soak_*.sh` cleans up on exit (daemon stopped via trap), so Ctrl-C
or an assertion failure leaves the host in a recoverable state. If you
do wedge the chip,
`(. ~/.tenstorrent-venv/bin/activate && tt-smi -r)` resets it.

### Concurrent console roundtrip

`console_roundtrip.py` targets one L2CPU per invocation (`--l2cpu N`);
the caller wires up however many parallel instances they want. A
typical 4-way stress run:

```bash
# Fresh chip, boot all 4 L2CPUs serially, wait for each login prompt.
(. ~/.tenstorrent-venv/bin/activate && tt-smi -r) >/dev/null
./target/debug/tt-bh-linux daemon start -t 0 --log-file ./daemon-card0.log
for i in 0 1 2 3; do
    cp --reflink=auto rootfs.ext4 rootfs-$i.ext4
    ./target/debug/tt-bh-linux boot -l $i --no-console -d rootfs-$i.ext4
    until timeout 5 ./target/debug/tt-bh-linux connect -l $i </dev/null 2>/dev/null \
            | grep -q "login:"; do sleep 3; done
done

# Fire 4 roundtrips in parallel.
for i in 0 1 2 3; do
    (python3 scripts/console_roundtrip.py --l2cpu $i \
         > /tmp/rt_$i.log 2>&1; echo "l2cpu $i exit=$?") &
done
wait

# Each /tmp/rt_$i.log should end with "ALL PASS".
```

The guest needs a `debian` user with a passwordless console login for
the scripted login to work — that's the default in the rootfs image
served by this repo's pipeline.

## What isn't covered here

These only hammer the happy paths with expected values. Things left for
a separate coverage pass:

- crash injection (SIGKILL the daemon mid-RPC)
- long-running guest with I/O pressure during `remove-disk`
- libvdeslirp TCP session loss on `remove-net`
