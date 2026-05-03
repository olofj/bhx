// SPDX-FileCopyrightText: © 2026 Olof Johansson
// SPDX-License-Identifier: MIT

//! Test-only helpers shared across modules.
//!
//! Exists primarily so `std::env::set_var` callers across the crate
//! serialize against the **same** mutex (#146). Per-module mutexes
//! would let two unrelated modules' tests both call `set_var`
//! concurrently — process-global state, no actual exclusion. glibc's
//! environ block is shared and `getenv`/`setenv` race; the symptom
//! is rare segfaults from torn pointer reads.

#![cfg(test)]

use std::sync::{Mutex, MutexGuard, OnceLock};

/// Acquire the process-wide env-var serialization lock. Hold the
/// returned guard for the entire window where the test reads or
/// mutates env vars (the `set_var` itself, plus any subsequent code
/// that observes the environ block).
///
/// Recovers from poisoning so a previous test's panic doesn't wedge
/// every later test in the run.
pub fn env_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|p| p.into_inner())
}
