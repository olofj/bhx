use std::path::PathBuf;
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
}

fn build_slirp_size_probe() {
    if std::env::var("CARGO_FEATURE_SLIRP").is_ok() {
        println!("cargo:rustc-link-lib=vdeslirp");
        println!("cargo:rustc-link-lib=slirp");

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
