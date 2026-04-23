// SPDX-FileCopyrightText: © 2025 Tenstorrent AI ULC
// SPDX-License-Identifier: Apache-2.0

//! Client-side terminal pump: bridges the user's tty and the console fd we
//! got back from the daemon via SCM_RIGHTS.
//!
//! Exit sequence is the same Ctrl-A x that the in-process `console_main`
//! used. Exiting here just drops the fd, which causes the daemon's reader
//! thread to see EOF and detach cleanly; the daemon keeps running.

use std::io::{self, Read, Write};
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;

use crate::console::TerminalRawMode;

/// Drive the bidirectional stdin ↔ fd pump until the user hits Ctrl-A x
/// (which flips `exit`) or the fd closes.
pub fn pump(fd: OwnedFd, exit: Arc<AtomicBool>) -> io::Result<()> {
    let _raw = TerminalRawMode::new()?;

    // Wrap the OwnedFd in a UnixStream so we get split read/write easily.
    // from_raw_fd() would take ownership, which is what we want — OwnedFd
    // won't double-close because we leak it into the stream.
    let stream = unsafe {
        use std::os::fd::{FromRawFd, IntoRawFd};
        UnixStream::from_raw_fd(fd.into_raw_fd())
    };

    // Reader thread: stream → stdout.
    let reader_exit = exit.clone();
    let reader_stream = stream.try_clone()?;
    let reader = thread::spawn(move || reader_loop(reader_stream, reader_exit));

    // Main thread: stdin → stream, with Ctrl-A x detection.
    let writer_result = writer_loop(&stream, &exit);

    // Close the stream to unblock the reader on EOF; then join it.
    drop(stream);
    let _ = reader.join();
    writer_result
}

fn reader_loop(stream: UnixStream, exit: Arc<AtomicBool>) {
    let mut stream = stream;
    let mut buf = [0u8; 4096];
    loop {
        match stream.read(&mut buf) {
            Ok(0) => {
                exit.store(true, Ordering::Relaxed);
                return;
            }
            Ok(n) => {
                let _ = io::stdout().write_all(&buf[..n]);
                let _ = io::stdout().flush();
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => {
                exit.store(true, Ordering::Relaxed);
                return;
            }
        }
    }
}

fn writer_loop(stream: &UnixStream, exit: &AtomicBool) -> io::Result<()> {
    let mut stream = stream;
    let mut ctrl_a = false;
    while !exit.load(Ordering::Relaxed) {
        // Poll stdin with 20 ms timeout so we notice `exit` without spinning.
        let mut rfds = unsafe { std::mem::zeroed::<libc::fd_set>() };
        unsafe { libc::FD_SET(libc::STDIN_FILENO, &mut rfds) };
        let mut tv = libc::timeval {
            tv_sec: 0,
            tv_usec: 20_000,
        };
        let rc = unsafe {
            libc::select(
                libc::STDIN_FILENO + 1,
                &mut rfds,
                ptr::null_mut(),
                ptr::null_mut(),
                &mut tv,
            )
        };
        if rc <= 0 {
            continue;
        }
        let mut b = [0u8; 1];
        let n = unsafe {
            libc::read(
                libc::STDIN_FILENO,
                b.as_mut_ptr() as *mut libc::c_void,
                1,
            )
        };
        if n <= 0 {
            break;
        }
        if ctrl_a {
            ctrl_a = false;
            if b[0] == b'x' {
                let _ = io::stdout().write_all(b"\n\n");
                exit.store(true, Ordering::Relaxed);
                return Ok(());
            }
            // Forward both the Ctrl-A and the character.
            stream.write_all(&[1])?;
            stream.write_all(&b)?;
        } else if b[0] == 1 {
            ctrl_a = true;
        } else {
            stream.write_all(&b)?;
        }
    }
    Ok(())
}
