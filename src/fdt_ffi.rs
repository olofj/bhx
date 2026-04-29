// SPDX-FileCopyrightText: © 2026 Olof Johansson
// SPDX-License-Identifier: MIT

//! Minimal libfdt FFI bindings — just the functions the boot sequence needs
//! to patch the DTB at boot time (memory size, /chosen/bootargs, virtio-mmio
//! nodes, reserved-memory, sbi-console).

use std::ffi::CString;
use std::os::raw::{c_char, c_int, c_void};

use crate::Error;

extern "C" {
    pub fn fdt_open_into(fdt: *const c_void, buf: *mut c_void, bufsize: c_int) -> c_int;
    pub fn fdt_pack(fdt: *mut c_void) -> c_int;
    pub fn fdt_path_offset(fdt: *const c_void, path: *const c_char) -> c_int;
    pub fn fdt_add_subnode(fdt: *mut c_void, parentoffset: c_int, name: *const c_char) -> c_int;
    pub fn fdt_set_name(fdt: *mut c_void, nodeoffset: c_int, name: *const c_char) -> c_int;
    pub fn fdt_get_name(fdt: *const c_void, nodeoffset: c_int, lenp: *mut c_int) -> *const c_char;
    pub fn fdt_setprop(
        fdt: *mut c_void,
        nodeoffset: c_int,
        name: *const c_char,
        val: *const c_void,
        len: c_int,
    ) -> c_int;
    // Used only by `Fdt::getprop`, which is a test-time helper today;
    // declared here so test code reads through the same FFI surface as
    // production calls.
    #[allow(dead_code)]
    pub fn fdt_getprop(
        fdt: *const c_void,
        nodeoffset: c_int,
        name: *const c_char,
        lenp: *mut c_int,
    ) -> *const c_void;
    pub fn fdt_get_phandle(fdt: *const c_void, nodeoffset: c_int) -> u32;
    pub fn fdt_find_max_phandle(fdt: *const c_void, phandle: *mut u32) -> c_int;
    pub fn fdt_strerror(errval: c_int) -> *const c_char;
}

fn err_str(err: c_int) -> String {
    unsafe {
        let p = fdt_strerror(err);
        if p.is_null() {
            format!("fdt error {}", err)
        } else {
            std::ffi::CStr::from_ptr(p).to_string_lossy().into_owned()
        }
    }
}

fn check(err: c_int, op: &str) -> crate::Result<()> {
    if err < 0 {
        Err(Error::fdt(op, err_str(err)))
    } else {
        Ok(())
    }
}

/// Owned, resizable DTB buffer.
///
/// Provides the libfdt operations used by the boot sequence. The backing
/// storage is `Vec<u64>` rather than `Vec<u8>` so the pointer libfdt sees
/// is guaranteed 8-byte aligned — the FDT spec requires the buffer to be
/// 8-byte aligned in memory, and a plain `Vec<u8>` only promises align 1.
/// Production hits this whenever the heap allocator hands back an
/// unaligned block; tests hit it more often because allocator state
/// shifts test-to-test.
///
/// `byte_len` tracks the externally-visible byte length, which can
/// shrink below `storage.len() * 8` after `pack`.
pub struct Fdt {
    storage: Vec<u64>,
    byte_len: usize,
}

impl Fdt {
    /// Open a DTB for modification, growing the buffer by `extra_bytes`.
    ///
    /// libfdt requires *both* the input pointer and the output buffer to
    /// be 8-byte aligned. `src` is typically a `Vec<u8>` from `fs::read`
    /// or a `&'static [u8; N]` from `include_bytes!`, neither of which
    /// promise alignment > 1. We stage through one aligned `Vec<u64>`:
    /// the input is copied into the output buffer first, then libfdt
    /// validates and resizes in place. This avoids two separate aligned
    /// allocations.
    pub fn open_into(src: &[u8], extra_bytes: usize) -> crate::Result<Self> {
        let total = src.len() + extra_bytes;
        let words = total.div_ceil(8);
        let mut storage: Vec<u64> = vec![0u64; words];
        // Stage the input into the aligned buffer so the libfdt call
        // sees an aligned source pointer. We then call fdt_open_into
        // with the same buffer for both src and dst — that's a valid
        // libfdt usage (it does a memmove internally if src == dst,
        // and our case is "moves into self" which is a noop).
        unsafe {
            std::ptr::copy_nonoverlapping(src.as_ptr(), storage.as_mut_ptr() as *mut u8, src.len());
        }
        let ret = unsafe {
            fdt_open_into(
                storage.as_ptr() as *const c_void,
                storage.as_mut_ptr() as *mut c_void,
                total as c_int,
            )
        };
        check(ret, "fdt_open_into")?;
        Ok(Fdt {
            storage,
            byte_len: total,
        })
    }

    fn ptr(&self) -> *const c_void {
        self.storage.as_ptr() as *const c_void
    }
    fn ptr_mut(&mut self) -> *mut c_void {
        self.storage.as_mut_ptr() as *mut c_void
    }
    fn buf_bytes(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.storage.as_ptr() as *const u8, self.byte_len) }
    }

    /// Find a node by path. Three outcomes:
    /// - `Ok(Some(idx))` — node found at byte offset `idx`.
    /// - `Ok(None)` — node not present (libfdt returned `-FDT_ERR_NOTFOUND`).
    ///   Callers that build-on-demand match on this branch.
    /// - `Err(msg)` — embedded NUL in `path`, or some other libfdt
    ///   error (badmagic, truncated, internal). Today every caller
    ///   passes a static string so the NUL case isn't reachable, but
    ///   surfacing it lets a future caller forward guest-supplied
    ///   paths safely (security finding from #17).
    pub fn path_offset(&self, path: &str) -> crate::Result<Option<c_int>> {
        let c_path = CString::new(path)
            .map_err(|e| Error::fdt(format!("path_offset({})", path), e.to_string()))?;
        let ret = unsafe { fdt_path_offset(self.ptr(), c_path.as_ptr()) };
        // libfdt returns -FDT_ERR_NOTFOUND (-1) for missing nodes; any
        // other negative is a real error worth surfacing.
        const NEG_NOTFOUND: c_int = -1;
        if ret == NEG_NOTFOUND {
            Ok(None)
        } else if ret < 0 {
            Err(Error::fdt(format!("path_offset({})", path), err_str(ret)))
        } else {
            Ok(Some(ret))
        }
    }

    /// Add a subnode under `parent`, return the new node offset.
    pub fn add_subnode(&mut self, parent: c_int, name: &str) -> crate::Result<c_int> {
        let c_name =
            CString::new(name).map_err(|e| Error::fdt("fdt_add_subnode", e.to_string()))?;
        let ret = unsafe { fdt_add_subnode(self.ptr_mut(), parent, c_name.as_ptr()) };
        if ret < 0 {
            Err(Error::fdt(
                format!("fdt_add_subnode({})", name),
                err_str(ret),
            ))
        } else {
            Ok(ret)
        }
    }

    /// Rename `node`'s unit name in place (e.g. `memory@400030000000` ->
    /// `memory@4000b0000000`). Used when patching `reg` shifts the unit
    /// address out of sync with the baked-in node name (#85).
    pub fn set_name(&mut self, node: c_int, name: &str) -> crate::Result<()> {
        let c_name = CString::new(name).map_err(|e| Error::fdt("fdt_set_name", e.to_string()))?;
        let ret = unsafe { fdt_set_name(self.ptr_mut(), node, c_name.as_ptr()) };
        check(ret, &format!("fdt_set_name({})", name))
    }

    /// Get the unit name for `node` (the part after the last `/` of its path).
    pub fn get_name(&self, node: c_int) -> Option<String> {
        let mut len: c_int = 0;
        let p = unsafe { fdt_get_name(self.ptr(), node, &mut len) };
        if p.is_null() || len < 0 {
            return None;
        }
        let bytes = unsafe { std::slice::from_raw_parts(p as *const u8, len as usize) };
        std::str::from_utf8(bytes).ok().map(|s| s.to_string())
    }

    /// Set property `name` on `node` to raw `value` bytes.
    pub fn setprop(&mut self, node: c_int, name: &str, value: &[u8]) -> crate::Result<()> {
        let c_name = CString::new(name).map_err(|e| Error::fdt("fdt_setprop", e.to_string()))?;
        let ret = unsafe {
            fdt_setprop(
                self.ptr_mut(),
                node,
                c_name.as_ptr(),
                value.as_ptr() as *const c_void,
                value.len() as c_int,
            )
        };
        check(ret, &format!("fdt_setprop({})", name))
    }

    pub fn setprop_u32(&mut self, node: c_int, name: &str, value: u32) -> crate::Result<()> {
        self.setprop(node, name, &value.to_be_bytes())
    }

    pub fn setprop_string(&mut self, node: c_int, name: &str, value: &str) -> crate::Result<()> {
        let mut v = value.as_bytes().to_vec();
        v.push(0);
        self.setprop(node, name, &v)
    }

    /// Get property `name` on `node` as raw bytes. Returns None if missing.
    /// Test-time helper today; production code never reads back its own
    /// patches, so it sees this as dead.
    #[allow(dead_code)]
    pub fn getprop<'a>(&'a self, node: c_int, name: &str) -> Option<&'a [u8]> {
        let c_name = CString::new(name).ok()?;
        let mut len: c_int = 0;
        let p = unsafe { fdt_getprop(self.ptr(), node, c_name.as_ptr(), &mut len) };
        if p.is_null() || len < 0 {
            None
        } else {
            Some(unsafe { std::slice::from_raw_parts(p as *const u8, len as usize) })
        }
    }

    pub fn get_phandle(&self, node: c_int) -> u32 {
        unsafe { fdt_get_phandle(self.ptr(), node) }
    }

    pub fn find_max_phandle(&self) -> crate::Result<u32> {
        let mut ph: u32 = 0;
        let ret = unsafe { fdt_find_max_phandle(self.ptr(), &mut ph) };
        check(ret, "fdt_find_max_phandle")?;
        Ok(ph)
    }

    /// Compact the DTB and return the packed bytes. `fdt_totalsize` in the
    /// header is a macro, not a function, so we read the totalsize field
    /// directly (offset 4, big-endian u32).
    pub fn pack(mut self) -> crate::Result<Vec<u8>> {
        let ret = unsafe { fdt_pack(self.ptr_mut()) };
        check(ret, "fdt_pack")?;
        let size = u32::from_be_bytes(self.buf_bytes()[4..8].try_into().unwrap()) as usize;
        // Copy out as Vec<u8> truncated to the packed size. We could
        // hand back the storage's bytes via Vec::from_raw_parts but that
        // breaks Vec's allocator invariant (allocated as u64, freed as
        // u8); a copy is fine for a packed-DTB size that's a few KiB.
        Ok(self.buf_bytes()[..size].to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pinned copy of `blackhole-card.dtb` for hardware-free tests.
    /// Same fixture `boot.rs::tests` uses.
    const FIXTURE_DTB: &[u8] = include_bytes!("../tests/fixtures/blackhole-card.dtb");

    #[test]
    fn path_offset_returns_some_for_existing_node() {
        let fdt = Fdt::open_into(FIXTURE_DTB, 0).unwrap();
        let result = fdt.path_offset("/memory@400030000000").unwrap();
        assert!(result.is_some(), "expected /memory@... to exist in fixture");
    }

    #[test]
    fn path_offset_returns_ok_none_for_missing_node() {
        let fdt = Fdt::open_into(FIXTURE_DTB, 0).unwrap();
        // /chosen does not exist in the input fixture (modify_dtb adds it).
        let result = fdt.path_offset("/this-path-does-not-exist-abc123").unwrap();
        assert!(result.is_none(), "expected None for missing node");
    }

    #[test]
    fn path_offset_returns_err_on_embedded_nul() {
        let fdt = Fdt::open_into(FIXTURE_DTB, 0).unwrap();
        let result = fdt.path_offset("/memory\0/embedded-nul");
        match result {
            Err(e) => {
                let msg = e.to_string();
                assert!(
                    msg.contains("nul") || msg.contains("NUL") || msg.contains("interior"),
                    "expected NUL-related error message, got: {}",
                    msg
                );
                assert!(matches!(e, Error::Fdt { .. }), "expected Fdt variant");
            }
            other => panic!("expected Err for NUL-bearing path, got {:?}", other),
        }
    }
}
