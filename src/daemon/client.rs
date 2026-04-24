// SPDX-FileCopyrightText: © 2026 Olof Johansson
// SPDX-License-Identifier: MIT

//! Thin client helpers: connect to the per-card daemon socket and issue the
//! requests from [`crate::daemon::protocol`]. Each helper does the single
//! frame write + response read, so callers can string them together.

use std::io;
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;

use crate::daemon::lifetime;
use crate::daemon::protocol::{
    read_frame, recv_fd, write_frame, ConsoleMode, Request, Response, StatusPayload,
};

/// Open a connection to the running daemon for `card`, or return a helpful
/// error describing how to start one.
pub fn connect(card: u32) -> io::Result<UnixStream> {
    let sock = lifetime::socket_path(card);
    UnixStream::connect(&sock).map_err(|e| {
        io::Error::other(format!(
            "no daemon socket at {} ({}). Start one with: tt-bh-linux daemon start --card {}",
            sock.display(),
            e,
            card
        ))
    })
}

fn expect_ok(resp: Response) -> io::Result<()> {
    match resp {
        Response::Ok => Ok(()),
        Response::Error { error } => Err(io::Error::other(error)),
        other => Err(io::Error::other(format!("unexpected response: {:?}", other))),
    }
}

pub fn status(sock: &mut UnixStream) -> io::Result<StatusPayload> {
    write_frame(&mut *sock, &Request::Status)?;
    match read_frame::<_, Response>(&mut *sock)? {
        Response::Status(s) => Ok(s),
        Response::Error { error } => Err(io::Error::other(error)),
        other => Err(io::Error::other(format!("unexpected response: {:?}", other))),
    }
}

#[allow(clippy::too_many_arguments)]
pub fn boot(
    sock: &mut UnixStream,
    l2cpu: u8,
    opensbi: String,
    kernel: String,
    dtb: String,
    initramfs: Option<String>,
    root_device: String,
    force_reset_pcie: bool,
    disk: Option<String>,
    network: bool,
    force: bool,
) -> io::Result<()> {
    write_frame(
        &mut *sock,
        &Request::Boot {
            l2cpu,
            opensbi,
            kernel,
            dtb,
            initramfs,
            root_device,
            force_reset_pcie,
            disk,
            network,
            force,
        },
    )?;
    expect_ok(read_frame(&mut *sock)?)
}

pub fn add_disk(sock: &mut UnixStream, l2cpu: u8, path: String) -> io::Result<()> {
    write_frame(&mut *sock, &Request::AddDisk { l2cpu, path })?;
    expect_ok(read_frame(&mut *sock)?)
}

pub fn add_net(sock: &mut UnixStream, l2cpu: u8, ssh_port: Option<u16>) -> io::Result<()> {
    write_frame(&mut *sock, &Request::AddNet { l2cpu, ssh_port })?;
    expect_ok(read_frame(&mut *sock)?)
}

pub fn stop_l2cpu(sock: &mut UnixStream, l2cpu: u8) -> io::Result<()> {
    write_frame(&mut *sock, &Request::Stop { l2cpu })?;
    expect_ok(read_frame(&mut *sock)?)
}

pub fn shutdown(sock: &mut UnixStream) -> io::Result<()> {
    write_frame(&mut *sock, &Request::Shutdown)?;
    expect_ok(read_frame(&mut *sock)?)
}

/// Attach a console: returns `(scrollback_bytes, fd)`. The fd is the client
/// end of a socketpair to the daemon; read chip output from it and write
/// keystrokes into it (only honored if `mode` was `Rw` or `Takeover`).
pub fn attach_console(
    sock: &mut UnixStream,
    l2cpu: u8,
    mode: ConsoleMode,
) -> io::Result<(u32, OwnedFd)> {
    write_frame(&mut *sock, &Request::AttachConsole { l2cpu, mode })?;
    match read_frame::<_, Response>(&mut *sock)? {
        Response::Attached { scrollback_bytes } => {
            let fd = recv_fd(sock)?;
            Ok((scrollback_bytes, fd))
        }
        Response::Error { error } => Err(io::Error::other(error)),
        other => Err(io::Error::other(format!("unexpected response: {:?}", other))),
    }
}
