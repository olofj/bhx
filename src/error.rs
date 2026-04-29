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
//! ## `io::Error::other` policy
//!
//! `grep -rn "io::Error::other" src/` should return zero hits in this
//! crate (verified post-#40-Phase-2). All error-construction sites use
//! a `crate::Error` variant; sites that live on `io::Result` chains
//! flow through the `From<crate::Error> for io::Error` bridge below.
//!
//! Bridge semantics:
//!   * `Error::Io { ctx, source }` → `io::Error::new(source.kind(),
//!     "<ctx>: <source>")` — preserves the original `io::ErrorKind`.
//!   * Every other variant → `io::Error::other(variant.to_string())`.
//!     Display strings for `Internal` / `Protocol` / `Fdt` carry their
//!     prefix (e.g. `"internal: ..."`), so error messages that bubble
//!     up to operator-facing surfaces (CLI exit, daemon wire reply)
//!     are slightly more informative than the pre-#40 raw strings.
//!     The wire format is otherwise unchanged: dispatch handlers still
//!     wrap via `crate::Error::Io { ctx: "boot failed", source: e }`
//!     before sending `Response::Error`, so the soak scripts'
//!     `"boot failed: ..."` grep patterns keep matching.
//!
//! Wire-bound error sites (those that surface in `Response::Error`)
//! prefer the no-prefix variants (`BadRequest` / `SlotState` / `Io`)
//! to keep operator-side message stability. Daemon-internal errors
//! (telemetry read failures, BRISC handshake timeouts, etc.) use
//! `Internal` and pick up the `"internal: "` prefix when bridged to
//! `io::Error` — that's a one-time cosmetic shift that no test or
//! soak script asserted on.

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
