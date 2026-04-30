// SPDX-FileCopyrightText: © 2026 Olof Johansson
// SPDX-License-Identifier: MIT

use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");

    // libfdt is always needed for the DTB-patching done by the `boot` subcommand.
    println!("cargo:rustc-link-lib=fdt");

    build_brisc_firmware();

    // Only link slirp libraries when the "slirp" feature is enabled.
    // This allows building without libvdeslirp/libslirp for users who
    // only need image/kernel/ramdisk management or console+disk support.
    build_slirp_size_probe();
}

/// Build the BRISC firmware variants (#67 M1 hello, #69 M3 virtio)
/// by invoking `brisc-firmware/Makefile`. The Rust side embeds the
/// resulting `.bin`s via `include_bytes!` and copies them into a
/// Tensix tile's L1 over the chip-side TLB.
///
/// The toolchain is the sfpi GCC at `/opt/tenstorrent/sfpi/compiler/bin`
/// (RV32 newlib cross-compiler shipped with tt-installer). When it's
/// present we rebuild from source. When it isn't (CI runners,
/// hardware-free dev hosts), we fall back to the prebuilt
/// `brisc-firmware/prebuilt/*.bin` checked into the repo. Anyone
/// modifying firmware source must rerun `make` locally and commit
/// the refreshed prebuilt binaries alongside the change.
fn build_brisc_firmware() {
    let manifest_dir =
        std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR set by cargo");
    let fw_dir = PathBuf::from(&manifest_dir).join("brisc-firmware");

    // Rebuild whenever any source under brisc-firmware/ changes, plus
    // the prebuilt fallbacks (so a refreshed prebuilt commit triggers
    // re-link on toolchain-less hosts).
    for f in [
        "start.S",
        "hello.c",
        "virtio.c",
        "link.ld",
        "Makefile",
        "include/virtio_layout.h",
    ] {
        println!("cargo:rerun-if-changed=brisc-firmware/{}", f);
    }
    for f in ["brisc-hello.bin", "brisc-virtio.bin"] {
        println!("cargo:rerun-if-changed=brisc-firmware/prebuilt/{}", f);
    }

    let toolchain = "/opt/tenstorrent/sfpi/compiler/bin";
    let (hello_bin, virtio_bin) = if std::path::Path::new(toolchain).is_dir() {
        let status = Command::new("make")
            .current_dir(&fw_dir)
            .arg("all")
            .status()
            .expect("invoke make for brisc-firmware");
        if !status.success() {
            panic!("brisc-firmware build failed (exit {:?})", status.code());
        }
        let build = fw_dir.join("build");
        (
            build.join("brisc-hello.bin"),
            build.join("brisc-virtio.bin"),
        )
    } else {
        let prebuilt = fw_dir.join("prebuilt");
        let hello = prebuilt.join("brisc-hello.bin");
        let virtio = prebuilt.join("brisc-virtio.bin");
        for p in [&hello, &virtio] {
            if !p.is_file() {
                panic!(
                    "sfpi toolchain not found at {} and prebuilt firmware missing at {}. \
                     Install the Tenstorrent toolchain or restore the prebuilt binary.",
                    toolchain,
                    p.display()
                );
            }
        }
        (hello, virtio)
    };

    // Surface the artifact paths to Rust via env! so src/tensix.rs can
    // include_bytes!(env!(...)) without hardcoding a relative path.
    println!("cargo:rustc-env=BRISC_HELLO_BIN={}", hello_bin.display());
    println!("cargo:rustc-env=BRISC_VIRTIO_BIN={}", virtio_bin.display());

    // Compute the firmware build_id with the same algorithm the
    // brisc-firmware Makefile uses (clean tree → `git log` short hash;
    // dirty tree or no git → sha256 prefix of source bytes). 24-bit —
    // it occupies the upper 24 bits of `BRISC_VIRTIO_FW_VERSION` on
    // the firmware side (low byte is `TENSIX_PROTOCOL_VERSION`). See
    // src/tensix_engine.rs::adopt_running and #122.
    let build_id = compute_brisc_fw_build_id(&fw_dir) & 0x00ff_ffff;
    let out_dir = std::env::var_os("OUT_DIR")
        .expect("OUT_DIR not set — cargo always sets this for build scripts");
    let out_path = std::path::Path::new(&out_dir).join("fw_build_id.rs");
    std::fs::write(
        &out_path,
        format!("pub const FW_BUILD_ID: u32 = {:#08x};\n", build_id),
    )
    .expect("write fw_build_id.rs");
}

/// Mirror of the Makefile's `FW_BUILD_ID` algorithm. Returns the same
/// 32-bit value that the C firmware bakes into `BRISC_VIRTIO_FW_VERSION`'s
/// upper 24 bits, so the daemon can compare against the chip's running
/// copy. Both sides use:
///   * `git log -1 --pretty=format:%h --abbrev=8 -- <sources>` when the
///     tree is clean against HEAD for the firmware sources.
///   * sha256 of the concatenated source bytes (first 8 hex chars)
///     otherwise — also covers the no-git-checkout case (#122).
fn compute_brisc_fw_build_id(fw_dir: &Path) -> u32 {
    let mut sources: Vec<String> = vec![
        "start.S".into(),
        "virtio.c".into(),
        "hello.c".into(),
        "link.ld".into(),
    ];
    if let Ok(entries) = std::fs::read_dir(fw_dir.join("include")) {
        for e in entries.flatten() {
            if let Some(name) = e.file_name().to_str() {
                if name.ends_with(".h") {
                    sources.push(format!("include/{}", name));
                }
            }
        }
    }
    sources.sort();

    let clean = is_git_tree_clean(fw_dir, &sources);
    let id = if clean {
        git_short_hash(fw_dir, &sources)
    } else {
        None
    }
    .unwrap_or_else(|| sha256_of_sources(fw_dir, &sources));

    let trimmed = id.trim();
    let hex = trimmed
        .chars()
        .filter(|c| c.is_ascii_hexdigit())
        .take(6)
        .collect::<String>();
    u32::from_str_radix(&hex, 16).unwrap_or(0)
}

fn is_git_tree_clean(fw_dir: &Path, sources: &[String]) -> bool {
    let cmd = |args: &[&str]| {
        Command::new("git")
            .current_dir(fw_dir)
            .args(args)
            .status()
            .ok()
            .map(|s| s.success())
            .unwrap_or(false)
    };
    let mut working = vec!["diff", "--quiet", "HEAD", "--"];
    for s in sources {
        working.push(s);
    }
    let mut staged = vec!["diff", "--cached", "--quiet", "HEAD", "--"];
    for s in sources {
        staged.push(s);
    }
    cmd(&working) && cmd(&staged)
}

fn git_short_hash(fw_dir: &Path, sources: &[String]) -> Option<String> {
    let mut args = vec!["log", "-1", "--pretty=format:%h", "--abbrev=6", "--"];
    for s in sources {
        args.push(s);
    }
    let out = Command::new("git")
        .current_dir(fw_dir)
        .args(&args)
        .output()
        .ok()?;
    if !out.status.success() || out.stdout.is_empty() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn sha256_of_sources(fw_dir: &Path, sources: &[String]) -> String {
    // Shell out to `sha256sum` to match the Makefile's algorithm
    // exactly. If it isn't on PATH we'd need to vendor a hash, which
    // is out of scope here — see #122.
    let cmd_str = format!(
        "cd {} && cat {} 2>/dev/null | sha256sum | head -c 6",
        shell_escape(&fw_dir.to_string_lossy()),
        sources
            .iter()
            .map(|s| shell_escape(s))
            .collect::<Vec<_>>()
            .join(" "),
    );
    let out = Command::new("sh")
        .arg("-c")
        .arg(&cmd_str)
        .output()
        .expect("invoke sha256sum");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn shell_escape(s: &str) -> String {
    if s.chars()
        .all(|c| c.is_ascii_alphanumeric() || "/._-".contains(c))
    {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

fn build_slirp_size_probe() {
    if std::env::var("CARGO_FEATURE_SLIRP").is_ok() {
        println!("cargo:rustc-link-lib=vdeslirp");
        println!("cargo:rustc-link-lib=slirp");

        // Compile the C shim that pokes `vhostname` into the libslirp
        // SlirpConfig (#60). Trivially short — one assignment — but
        // needs a real C `#include` of `<slirp/libslirp.h>` so we don't
        // have to mirror the SlirpConfig layout in Rust.
        println!("cargo:rerun-if-changed=src/slirp_set_hostname.c");
        cc::Build::new()
            .file("src/slirp_set_hostname.c")
            .compile("tt_slirp_helpers");

        // Verify that our opaque SlirpConfig buffer (512 bytes) is large
        // enough for the actual struct from libslirp. Fail at build time if
        // the library has grown. Cargo cleans up `OUT_DIR` on `cargo clean`,
        // so leftover artifacts from a crashed build don't accumulate.
        let out_dir = std::env::var_os("OUT_DIR")
            .expect("OUT_DIR not set — cargo always sets this for build scripts");
        let probe_dir = std::path::Path::new(&out_dir).join("slirp_probe");
        std::fs::create_dir_all(&probe_dir).expect("create OUT_DIR/slirp_probe");
        let bin_path = probe_dir.join("size_check");
        let output = Command::new("sh")
            .arg("-c")
            .arg(format!(
                concat!(
                    "echo '#include <slirp/libslirp.h>\n",
                    "#include <stdio.h>\n",
                    "int main(){{printf(\"%zu\",sizeof(SlirpConfig));return 0;}}' ",
                    "| cc -x c - -o {0} 2>/dev/null && {0}"
                ),
                bin_path.display()
            ))
            .output();

        if let Ok(out) = output {
            if out.status.success() {
                let size_str = String::from_utf8_lossy(&out.stdout);
                if let Ok(real_size) = size_str.trim().parse::<usize>() {
                    if real_size > 512 {
                        panic!(
                            "SlirpConfig is {} bytes but our buffer is only 512 — increase _data size in slirp_ffi.rs",
                            real_size
                        );
                    }
                }
            }
        }
    }
}
