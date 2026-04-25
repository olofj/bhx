use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");

    // libfdt is always needed for the DTB-patching done by the `boot` subcommand.
    println!("cargo:rustc-link-lib=fdt");

    // Only link slirp libraries when the "slirp" feature is enabled.
    // This allows building without libvdeslirp/libslirp for users who
    // only need image/kernel/ramdisk management or console+disk support.
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
