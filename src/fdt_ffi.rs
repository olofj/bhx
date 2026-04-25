// SPDX-FileCopyrightText: © 2026 Olof Johansson
// SPDX-License-Identifier: MIT

//! Minimal libfdt FFI bindings — just the functions the boot sequence needs
//! to mirror the DTB patching that `boot.py` performs via pylibfdt.

use std::ffi::CString;
use std::os::raw::{c_char, c_int, c_void};

extern "C" {
    pub fn fdt_open_into(fdt: *const c_void, buf: *mut c_void, bufsize: c_int) -> c_int;
    pub fn fdt_pack(fdt: *mut c_void) -> c_int;
    pub fn fdt_path_offset(fdt: *const c_void, path: *const c_char) -> c_int;
    pub fn fdt_add_subnode(fdt: *mut c_void, parentoffset: c_int, name: *const c_char) -> c_int;
    pub fn fdt_setprop(
        fdt: *mut c_void,
        nodeoffset: c_int,
        name: *const c_char,
        val: *const c_void,
        len: c_int,
    ) -> c_int;
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

fn check(err: c_int, op: &str) -> Result<(), String> {
    if err < 0 {
        Err(format!("{}: {}", op, err_str(err)))
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
    pub fn open_into(src: &[u8], extra_bytes: usize) -> Result<Self, String> {
        let total = src.len() + extra_bytes;
        let words = total.div_ceil(8);
        let mut storage: Vec<u64> = vec![0u64; words];
        // Stage the input into the aligned buffer so the libfdt call
        // sees an aligned source pointer. We then call fdt_open_into
        // with the same buffer for both src and dst — that's a valid
        // libfdt usage (it does a memmove internally if src == dst,
        // and our case is "moves into self" which is a noop).
        unsafe {
            std::ptr::copy_nonoverlapping(
                src.as_ptr(),
                storage.as_mut_ptr() as *mut u8,
                src.len(),
            );
        }
        let ret = unsafe {
            fdt_open_into(
                storage.as_ptr() as *const c_void,
                storage.as_mut_ptr() as *mut c_void,
                total as c_int,
            )
        };
        check(ret, "fdt_open_into")?;
        Ok(Fdt { storage, byte_len: total })
    }

    fn ptr(&self) -> *const c_void { self.storage.as_ptr() as *const c_void }
    fn ptr_mut(&mut self) -> *mut c_void { self.storage.as_mut_ptr() as *mut c_void }
    fn buf_bytes(&self) -> &[u8] {
        unsafe {
            std::slice::from_raw_parts(self.storage.as_ptr() as *const u8, self.byte_len)
        }
    }

    /// Find a node by path. Returns None if not found.
    pub fn path_offset(&self, path: &str) -> Option<c_int> {
        let c_path = CString::new(path).ok()?;
        let ret = unsafe { fdt_path_offset(self.ptr(), c_path.as_ptr()) };
        if ret < 0 {
            None
        } else {
            Some(ret)
        }
    }

    /// Add a subnode under `parent`, return the new node offset.
    pub fn add_subnode(&mut self, parent: c_int, name: &str) -> Result<c_int, String> {
        let c_name = CString::new(name).map_err(|e| e.to_string())?;
        let ret = unsafe { fdt_add_subnode(self.ptr_mut(), parent, c_name.as_ptr()) };
        if ret < 0 {
            Err(format!("fdt_add_subnode({}): {}", name, err_str(ret)))
        } else {
            Ok(ret)
        }
    }

    /// Set property `name` on `node` to raw `value` bytes.
    pub fn setprop(&mut self, node: c_int, name: &str, value: &[u8]) -> Result<(), String> {
        let c_name = CString::new(name).map_err(|e| e.to_string())?;
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

    pub fn setprop_u32(&mut self, node: c_int, name: &str, value: u32) -> Result<(), String> {
        self.setprop(node, name, &value.to_be_bytes())
    }

    pub fn setprop_string(&mut self, node: c_int, name: &str, value: &str) -> Result<(), String> {
        let mut v = value.as_bytes().to_vec();
        v.push(0);
        self.setprop(node, name, &v)
    }

    /// Get property `name` on `node` as raw bytes. Returns None if missing.
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

    pub fn find_max_phandle(&self) -> Result<u32, String> {
        let mut ph: u32 = 0;
        let ret = unsafe { fdt_find_max_phandle(self.ptr(), &mut ph) };
        check(ret, "fdt_find_max_phandle")?;
        Ok(ph)
    }

    /// Compact the DTB and return the packed bytes. `fdt_totalsize` in the
    /// header is a macro, not a function, so we read the totalsize field
    /// directly (offset 4, big-endian u32).
    pub fn pack(mut self) -> Result<Vec<u8>, String> {
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
