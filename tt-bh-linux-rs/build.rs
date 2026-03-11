use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");

    // Only link slirp libraries when the "slirp" feature is enabled.
    // This allows building without libvdeslirp/libslirp for users who
    // only need image/kernel/ramdisk management or console+disk support.
    if std::env::var("CARGO_FEATURE_SLIRP").is_ok() {
        println!("cargo:rustc-link-lib=vdeslirp");
        println!("cargo:rustc-link-lib=slirp");

        // Verify that our opaque SlirpConfig buffer (512 bytes) is large enough
        // for the actual struct. Fail at build time if the library has grown.
        // Use a unique temp path based on PID to avoid race conditions with
        // parallel builds.
        let pid = std::process::id();
        let tmp_bin = format!("/tmp/slirp_size_check_{}", pid);
        let output = Command::new("sh")
            .arg("-c")
            .arg(format!(
                concat!(
                    "echo '#include <slirp/libslirp.h>\n",
                    "#include <stdio.h>\n",
                    "int main(){{printf(\"%zu\",sizeof(SlirpConfig));return 0;}}' ",
                    "| cc -x c - -o {0} 2>/dev/null && {0}; rm -f {0}"
                ),
                tmp_bin
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
