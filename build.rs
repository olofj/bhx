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

/// Build the BRISC hello-world firmware (issue #67, M1) by invoking
/// `brisc-firmware/Makefile`. The Rust side embeds the resulting
/// `.bin` via `include_bytes!` and copies it into Tensix tile L1.
///
/// The toolchain is the sfpi GCC at `/opt/tenstorrent/sfpi/compiler/bin`
/// (RV32 newlib cross-compiler shipped with tt-installer). We assume
/// it's present — the project's runtime requirements already include
/// the Tenstorrent stack, so anyone able to run the daemon has it.
/// If it's missing the build fails with a clear error pointing at
/// the install path.
fn build_brisc_firmware() {
    let manifest_dir =
        std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR set by cargo");
    let fw_dir = PathBuf::from(&manifest_dir).join("brisc-firmware");

    // Rebuild whenever any source under brisc-firmware/ changes.
    for f in ["start.S", "main.c", "link.ld", "Makefile"] {
        println!("cargo:rerun-if-changed=brisc-firmware/{}", f);
    }

    let toolchain = "/opt/tenstorrent/sfpi/compiler/bin";
    if !std::path::Path::new(toolchain).is_dir() {
        panic!(
            "sfpi toolchain not found at {}. Install with the Tenstorrent installer \
             or set the path in brisc-firmware/Makefile (TOOLCHAIN_BIN).",
            toolchain
        );
    }

    let status = Command::new("make")
        .current_dir(&fw_dir)
        .arg("all")
        .status()
        .expect("invoke make for brisc-firmware");
    if !status.success() {
        panic!("brisc-firmware build failed (exit {:?})", status.code());
    }

    // Surface the artifact path to Rust via env! so src/tensix.rs can
    // include_bytes!(env!(...)) without hardcoding a relative path.
    let bin_path = fw_dir.join("build").join("brisc-hello.bin");
    println!("cargo:rustc-env=BRISC_HELLO_BIN={}", bin_path.display());
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
