// SPDX-FileCopyrightText: © 2026 Olof Johansson
// SPDX-License-Identifier: MIT

//! Crate-level structured error type. Replaces the historical
//! `io::Error::other(format!(...))` pattern with named variants so:
//!
//! - dispatch handlers can classify errors (client error vs slot
//!   state vs IO failure vs internal bug) without grepping the
//!   message text;
//! - the underlying `io::Error` chain is preserved for kind-aware
//!   logging — the old `format!("{}: {}", ctx, e)` pattern collapsed
//!   the source error into a string;
//! - libfdt failures get an explicit `op` label so `(fdt: setprop_u32)
//!   NOSPACE` is the format, not a buried-inside-format-string artifact.
//!
//! Convertible via `?` from `serde_json::Error` (auto-converts to
//! `Protocol`). `io::Error` is *not* auto-convertible — every
//! propagation site has to supply context via [`Error::io_ctx`],
//! which is the whole point of the refactor.
//!
//! Wire format unchanged: `dispatch_*` calls `error.to_string()`
//! and stuffs the result into `Response::Error { error }`. Soak
//! scripts grepping for "cannot open disk image" etc. keep matching.
//!
//! ## Where `io::Error::other` is still allowed
//!
//! After #21 the `dispatch_*` boundary, `boot::modify_dtb`, and
//! `fdt_ffi` all use `crate::Result`. The remaining `io::Error::other`
//! call sites live on `io::Result` chains *below* that boundary and
//! intentionally keep `io::Result` for now:
//!
//! - **`src/daemon/protocol.rs`**: `read_frame` / `write_frame` /
//!   `recv_with_fd`. These are wire-framing primitives consumed by both
//!   the daemon (server-side dispatch) and the CLI client; both
//!   chains return `io::Result` to the OS. A `crate::Error::Protocol`
//!   variant exists for the serde_json case, but the framing helpers
//!   themselves stay on `io::Result` so cancellation paths (read EOF,
//!   short-write, EINTR) preserve `io::ErrorKind` end-to-end.
//! - **`src/daemon/client.rs`**: client-side RPC helpers consumed by
//!   `main.rs`'s CLI subcommands. The CLI uses `io::Result<()>` for
//!   exit-code propagation; migrating it would cascade through every
//!   `clap` subcommand. Out of scope for #21.
//! - **`src/daemon/runner.rs`**, **`src/daemon/lifetime.rs`**,
//!   **`src/daemon/console_hub.rs`**: pidfile / log / fan-out helpers
//!   on `io::Result`. Same cascading argument.
//! - **`src/chip.rs`**, **`src/kmd.rs`**, **`src/virtio/network.rs`**:
//!   low-level chip + ioctl + slirp wrappers. Their callers are split
//!   across daemon and CLI debug subcommands; both paths stay on
//!   `io::Result`. The errors are intrinsically OS-level (open(),
//!   ioctl(), vdeslirp_open).
//! - **`src/main.rs`**, **`src/daemon/server.rs::serve`** (3 sites):
//!   the daemon entry point and CLI dispatch top-level. Both return
//!   `io::Result<()>` to the runtime / clap. Sandbox-install /
//!   metrics-bind failures wrap their message via `io::Error::other`
//!   so the daemon refuses to start.
//!
//! These sites benefit from the `From<crate::Error> for io::Error`
//! bridge defined below — code that wants to construct a
//! `crate::Error` for the variant info can still flow through `?`
//! into an `io::Result`, preserving `io::ErrorKind` on the `Io`
//! variant.
//!
//! Migrating these chains to `crate::Result` end-to-end is a separate
//! follow-up. Tracked as the remaining piece of #21.

use thiserror::Error;

/// Crate-wide error.
#[derive(Debug, Error)]
pub enum Error {
    /// Bad request from the client. The wire reply uses the message
    /// verbatim. Programmer errors should NOT use this variant — they
    /// should be `Internal`.
    #[error("{0}")]
    BadRequest(String),

    /// L2CPU index out of range, slot already occupied, "not booted",
    /// etc. — anything that's a state-machine NACK rather than an IO
    /// failure.
    #[error("{0}")]
    SlotState(String),

    /// Filesystem / fd / chip-ioctl IO error. The `ctx` string
    /// describes what the caller was trying to do; `source` carries
    /// the underlying `io::Error` (and its `kind()`).
    ///
    /// Format: `"<ctx>: <source>"` — matches the historical
    /// `format!("<ctx>: {}", e)` shape so existing log-grep patterns
    /// keep working.
    #[error("{ctx}: {source}")]
    Io {
        ctx: String,
        #[source]
        source: std::io::Error,
    },

    /// libfdt operation failed. `op` is the libfdt function name
    /// (`"fdt_setprop"`, `"fdt_add_subnode"`, …); `message` is the
    /// error string libfdt produced.
    #[error("fdt: {op}: {message}")]
    Fdt { op: String, message: String },

    /// JSON wire-format parse / encode failure on the daemon control
    /// socket. Auto-converts via `?`.
    #[error("protocol: {0}")]
    Protocol(#[from] serde_json::Error),

    /// Programmer error: invariant violated, arithmetic overflow on
    /// trusted inputs, etc. The dispatch wrapper logs the full
    /// message and replies a generic "internal daemon error" to the
    /// client — operator triages from the daemon log.
    #[error("internal: {0}")]
    Internal(String),
}

/// Crate-wide `Result` type. Most callers should write `crate::Result<T>`
/// to avoid the verbose `Result<T, crate::Error>` shape.
pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    /// Returns a closure suitable for `.map_err(...)` that wraps a
    /// raw `io::Error` with context. Use:
    ///
    /// ```ignore
    /// std::fs::read(path).map_err(Error::io_ctx(format!("read {}", path.display())))?
    /// ```
    pub fn io_ctx(ctx: impl Into<String>) -> impl FnOnce(std::io::Error) -> Error {
        let ctx = ctx.into();
        move |source| Error::Io { ctx, source }
    }

    pub fn slot_state(msg: impl Into<String>) -> Error {
        Error::SlotState(msg.into())
    }

    pub fn bad_request(msg: impl Into<String>) -> Error {
        Error::BadRequest(msg.into())
    }

    pub fn internal(msg: impl Into<String>) -> Error {
        Error::Internal(msg.into())
    }

    pub fn fdt(op: impl Into<String>, message: impl Into<String>) -> Error {
        Error::Fdt {
            op: op.into(),
            message: message.into(),
        }
    }
}

/// Bridge from `crate::Error` to `std::io::Error` for callers that
/// still live on the `io::Result` boundary (the `daemon::runner`
/// shell and its `serve()` return type, which propagates upward to
/// `main`'s exit code). Avoids forcing the entire boundary to
/// migrate at once.
///
/// `Io` variants pass through their original `io::Error` so the
/// `kind()` is preserved; everything else collapses to
/// `ErrorKind::Other` with the display string.
impl From<Error> for std::io::Error {
    fn from(e: Error) -> Self {
        match e {
            Error::Io { ctx, source } => {
                let kind = source.kind();
                std::io::Error::new(kind, format!("{}: {}", ctx, source))
            }
            other => std::io::Error::other(other.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn io_ctx_preserves_io_error_kind() {
        let raw = std::io::Error::new(std::io::ErrorKind::NotFound, "no such file");
        let wrapped: Error = (Error::io_ctx("opening foo"))(raw);
        // Round-trip back through io::Error: the kind survives.
        let bridged: std::io::Error = wrapped.into();
        assert_eq!(bridged.kind(), std::io::ErrorKind::NotFound);
        assert_eq!(bridged.to_string(), "opening foo: no such file");
    }

    #[test]
    fn display_format_matches_legacy_shape() {
        // The old code emitted `format!("ctx: {}", io_err)`. The new
        // `Error::Io` Display must produce the same string so soak
        // scripts grepping the wire response keep matching.
        let raw = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "Permission denied");
        let e = Error::Io {
            ctx: "cannot open disk image /tmp/foo.img".into(),
            source: raw,
        };
        assert_eq!(
            e.to_string(),
            "cannot open disk image /tmp/foo.img: Permission denied"
        );
    }

    #[test]
    fn display_for_slot_state_is_bare_message() {
        let e = Error::slot_state("l2cpu 2 is already running");
        assert_eq!(e.to_string(), "l2cpu 2 is already running");
    }

    #[test]
    fn display_for_fdt_includes_op_label() {
        let e = Error::fdt("fdt_setprop", "FDT_ERR_NOSPACE");
        assert_eq!(e.to_string(), "fdt: fdt_setprop: FDT_ERR_NOSPACE");
    }

    #[test]
    fn protocol_from_serde_json_auto_converts() {
        let bad = serde_json::from_str::<u32>("not-a-number").unwrap_err();
        let e: Error = bad.into();
        assert!(
            matches!(e, Error::Protocol(_)),
            "expected Protocol variant, got {:?}",
            e
        );
        assert!(e.to_string().starts_with("protocol: "));
    }

    #[test]
    fn internal_collapses_to_other_kind_via_io_bridge() {
        let e = Error::internal("invariant violated");
        let bridged: std::io::Error = e.into();
        assert_eq!(bridged.kind(), std::io::ErrorKind::Other);
        assert_eq!(bridged.to_string(), "internal: invariant violated");
    }
}
