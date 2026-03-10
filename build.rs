use std::process::Command;

fn main() {
    println!("cargo:rustc-link-lib=vdeslirp");
    println!("cargo:rustc-link-lib=slirp");

    // Verify that our opaque SlirpConfig buffer (512 bytes) is large enough
    // for the actual struct. Fail at build time if the library has grown.
    let output = Command::new("sh")
        .arg("-c")
        .arg(concat!(
            "echo '#include <slirp/libslirp.h>\n",
            "#include <stdio.h>\n",
            "int main(){printf(\"%zu\",sizeof(SlirpConfig));return 0;}' ",
            "| cc -x c - -o /tmp/slirp_size_check 2>/dev/null && /tmp/slirp_size_check"
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
    // If the check can't run (no cc, etc.), we proceed anyway — the 512-byte
    // buffer has significant headroom over the known 192-byte struct.
}
