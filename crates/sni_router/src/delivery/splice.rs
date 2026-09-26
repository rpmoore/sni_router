// Copyright 2026 Ryan Moore
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Linux fast path for proxying: `splice(2)` moves bytes socket-to-socket
//! through an in-kernel pipe, so payload never crosses into a userspace
//! buffer the way `tokio::io::copy_bidirectional` requires. `proxy.rs`
//! falls back to the userspace copy when [`available`] is false or
//! [`Admission::try_acquire`] finds the fd budget exhausted (tunable via
//! `SNI_ROUTER_MAX_SPLICE_CONNECTIONS`, see [`admission_capacity`]).

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::{Arc, OnceLock};

use tokio::io::Interest;
use tokio::io::unix::AsyncFd;
use tokio::net::TcpStream;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use super::metered::{Activity, ByteCounters};

/// Whether `splice(2)` actually works in this process, probed once and
/// cached. False in sandboxes whose seccomp profile denies it (seen in some
/// container runtimes); every connection then uses the userspace copy
/// instead of failing outright.
///
/// Set `SNI_ROUTER_DISABLE_SPLICE` (to anything) to force this off, e.g. to
/// work around an unexpected kernel splice bug, or to A/B the two copy
/// paths' throughput.
pub(super) fn available() -> bool {
    if std::env::var_os("SNI_ROUTER_DISABLE_SPLICE").is_some() {
        return false;
    }
    static AVAILABLE: OnceLock<bool> = OnceLock::new();
    *AVAILABLE.get_or_init(probe)
}

/// Moves one throwaway byte pipe-to-pipe, touching no sockets, so a
/// genuinely missing or blocked syscall is caught once up front instead of
/// surfacing as a mysterious per-connection I/O error.
fn probe() -> bool {
    try_probe().unwrap_or(false)
}

fn try_probe() -> io::Result<bool> {
    let (source_read, source_write) = raw_pipe()?;
    let (sink_read, sink_write) = raw_pipe()?;
    let byte = 0u8;
    // SAFETY: `source_write` was just created by `pipe2` above and is open
    // for writing; `&byte` is one valid, initialized byte.
    if unsafe { libc::write(source_write.as_raw_fd(), (&byte as *const u8).cast(), 1) } != 1 {
        return Err(io::Error::last_os_error());
    }
    let moved = splice_raw(source_read.as_raw_fd(), sink_write.as_raw_fd(), 1)?;
    drop(sink_read);
    Ok(moved == 1)
}

/// A fresh anonymous pipe as a pair of owned, non-blocking, close-on-exec
/// file descriptors. Non-blocking matters: `splice_raw` always passes
/// `SPLICE_F_NONBLOCK`, which per `splice(2)` only avoids blocking on the
/// *pipe* itself if the pipe end is `O_NONBLOCK` — without it, a splice
/// call that would otherwise return `EAGAIN` can instead block the whole
/// executor thread.
fn raw_pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0i32; 2];
    // SAFETY: `fds` has room for exactly the two file descriptors `pipe2`
    // writes on success.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_NONBLOCK | libc::O_CLOEXEC) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `pipe2` just returned these as fresh, open, uniquely-owned
    // descriptors; each is closed exactly once, when its `OwnedFd` drops.
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

/// `splice`'s intermediate kernel buffer for one direction of one
/// connection: a non-blocking anonymous pipe, grown to hold one full
/// `buffer_size` chunk so a splice-in can never need more room than a
/// splice-out has already made.
pub(super) struct Pipe {
    read: AsyncFd<OwnedFd>,
    write: AsyncFd<OwnedFd>,
    capacity: usize,
}

impl Pipe {
    fn new(buffer_size: usize) -> io::Result<Self> {
        let (read_fd, write_fd) = raw_pipe()?;
        let requested = buffer_size.min(i32::MAX as usize) as i32;
        // Best effort: a splice-in is capped at this pipe's actual
        // capacity below, so an oversized request here only costs
        // throughput (more, smaller splice calls), never correctness. The
        // kernel may refuse or round up (e.g. `fs.pipe-max-size`).
        // SAFETY: `write_fd` is open and valid for the duration of this
        // call; `F_SETPIPE_SZ` takes an `int` argument, not a pointer.
        unsafe { libc::fcntl(write_fd.as_raw_fd(), libc::F_SETPIPE_SZ, requested) };
        // SAFETY: same as above; `F_GETPIPE_SZ` takes no argument.
        let capacity = match unsafe { libc::fcntl(write_fd.as_raw_fd(), libc::F_GETPIPE_SZ) } {
            size if size > 0 => size as usize,
            _ => buffer_size,
        };
        Ok(Self {
            read: AsyncFd::new(read_fd)?,
            write: AsyncFd::new(write_fd)?,
            capacity,
        })
    }

    fn read_fd(&self) -> RawFd {
        self.read.get_ref().as_raw_fd()
    }

    fn write_fd(&self) -> RawFd {
        self.write.get_ref().as_raw_fd()
    }
}

/// At most this fraction of the process's file-descriptor limit may ever
/// be tied up in splice pipes; the rest is guaranteed available for
/// sockets, listeners, and anything else in the process — including, in an
/// embedder, file descriptors this crate knows nothing about.
const MAX_SPLICE_FD_SHARE: usize = 2;

/// Two pipes (a read and a write end each) per connection using the
/// splice fast path.
const FDS_PER_SPLICE_CONNECTION: usize = 4;

/// The process-wide bound on how many connections may use the splice fast
/// path at once, so their pipes can never crowd out the fds every other
/// part of the process needs. One semaphore for the whole process, shared
/// by every `Router::serve` call — sizing it per `Router` (e.g. from that
/// `Router`'s own `max_connections`) would double-count the same
/// `RLIMIT_NOFILE` budget across `Router`s running concurrently in one
/// process, which is exactly the scenario a closed-source embedder may
/// hit: this library is meant to be embedded, and nothing here assumes
/// there's only one `Router` in the process.
pub(super) fn admission() -> &'static Admission {
    static ADMISSION: OnceLock<Admission> = OnceLock::new();
    ADMISSION.get_or_init(Admission::new)
}

pub(super) struct Admission {
    permits: Arc<Semaphore>,
}

impl Admission {
    fn new() -> Self {
        let limit = nofile_soft_limit().unwrap_or(0);
        let override_value = std::env::var("SNI_ROUTER_MAX_SPLICE_CONNECTIONS").ok();
        Self {
            permits: Arc::new(Semaphore::new(admission_capacity(
                override_value.as_deref(),
                limit,
            ))),
        }
    }

    /// A permit for one connection's splice pipes, or `None` if the budget
    /// is exhausted — the caller should use the portable copy instead.
    pub(super) fn try_acquire(&self) -> Option<OwnedSemaphorePermit> {
        Arc::clone(&self.permits).try_acquire_owned().ok()
    }
}

/// `SNI_ROUTER_MAX_SPLICE_CONNECTIONS`, if set to a valid number, replaces
/// the `RLIMIT_NOFILE`-derived heuristic outright. The heuristic only
/// knows the process's fd *limit*, not how much of it is already spoken
/// for elsewhere — an embedder holding a large, fixed share of the
/// process's fds in its own database pool, listeners, or files (this
/// library is meant to be embedded) may need a smaller, explicit budget
/// than half the limit would give it. `0` disables splice entirely, like
/// `SNI_ROUTER_DISABLE_SPLICE`, but scoped to this budget specifically. An
/// unset or unparseable value falls back to the heuristic.
fn admission_capacity(override_value: Option<&str>, nofile_limit: usize) -> usize {
    match override_value.and_then(|value| value.parse().ok()) {
        Some(permits) => permits,
        None => splice_permits(nofile_limit),
    }
}

fn splice_permits(nofile_limit: usize) -> usize {
    (nofile_limit / MAX_SPLICE_FD_SHARE) / FDS_PER_SPLICE_CONNECTION
}

fn nofile_soft_limit() -> Option<usize> {
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: `limit` is a valid, appropriately-sized out-pointer.
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) } != 0 {
        return None;
    }
    usize::try_from(limit.rlim_cur).ok()
}

/// Raw `splice(2)`: moves up to `len` bytes from `from` to `to`, at least
/// one of which must be a pipe. Non-blocking; a `WouldBlock` result means
/// the *other* endpoint (whichever holds the data or has the free space)
/// isn't ready, and the caller must already know which one that is.
fn splice_raw(from: RawFd, to: RawFd, len: usize) -> io::Result<usize> {
    // SAFETY: `from` and `to` are valid, open file descriptors for the
    // duration of this call; no user buffers are involved, only fds and a
    // length, so there's nothing for the kernel to read or write out of
    // bounds.
    let moved = unsafe {
        libc::splice(
            from,
            std::ptr::null_mut(),
            to,
            std::ptr::null_mut(),
            len,
            libc::SPLICE_F_MOVE | libc::SPLICE_F_NONBLOCK,
        )
    };
    if moved < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(moved as usize)
}

/// Half of `shutdown(2)`: closes `stream`'s write side so the peer sees
/// EOF, without touching the read side (the reverse direction may still be
/// flowing).
fn shutdown_write(stream: &TcpStream) -> io::Result<()> {
    // SAFETY: `stream` is open and valid for the duration of this call.
    if unsafe { libc::shutdown(stream.as_raw_fd(), libc::SHUT_WR) } != 0 {
        let error = io::Error::last_os_error();
        // Already shut down, or the peer reset the connection: either way
        // there's nothing left to shut down.
        if error.kind() == io::ErrorKind::NotConnected {
            return Ok(());
        }
        return Err(error);
    }
    Ok(())
}

/// Builds both directions' pipes up front, before either socket is
/// touched. A failure here (fd exhaustion, `pipe2`/registration refused) is
/// therefore always safe to answer by falling back to the portable copy:
/// no client or upstream byte has been consumed yet.
pub(super) fn prepare(buffer_size: usize) -> io::Result<(Pipe, Pipe)> {
    let to_upstream = Pipe::new(buffer_size)?;
    let to_client = Pipe::new(buffer_size)?;
    Ok((to_upstream, to_client))
}

/// Splices `src` into `dst` through `pipe` until `src` hits EOF, then shuts
/// down `dst`'s write half. `on_read` is called with each amount pulled
/// from `src` (before it's necessarily reached `dst`), `on_written` with
/// each amount that has actually landed in `dst` — matching
/// `MeteredStream`'s `poll_read`/`poll_write`, which count on their own
/// side independently of what happens on the other. Never buffers more
/// than one pipe's worth (`buffer_size`, or less if the kernel wouldn't
/// grow the pipe that far) at a time.
async fn splice_direction(
    src: &TcpStream,
    dst: &TcpStream,
    pipe: Pipe,
    on_read: impl Fn(usize),
    on_written: impl Fn(usize),
) -> io::Result<()> {
    let chunk = pipe.capacity;
    loop {
        let read = src
            .async_io(Interest::READABLE, || {
                splice_raw(src.as_raw_fd(), pipe.write_fd(), chunk)
            })
            .await?;
        if read == 0 {
            break;
        }
        on_read(read);
        // Always fully drained before the next splice-in, so the pipe is
        // empty whenever we ask it to hold a fresh chunk: a `WouldBlock`
        // splicing in can therefore only mean `src` isn't readable, and a
        // `WouldBlock` splicing out can only mean `dst` isn't writable —
        // exactly what each `async_io` call below assumes.
        let mut remaining = read;
        while remaining > 0 {
            let written = dst
                .async_io(Interest::WRITABLE, || {
                    splice_raw(pipe.read_fd(), dst.as_raw_fd(), remaining)
                })
                .await?;
            remaining -= written;
            on_written(written);
        }
    }
    shutdown_write(dst)
}

/// Proxies `client` and `upstream` via `splice(2)`, bypassing userspace
/// entirely. Mirrors `tokio::io::copy_bidirectional`'s half-close
/// propagation: EOF on one side shuts down the other's write half while the
/// reverse direction keeps flowing until it finishes too.
///
/// This bypasses `MeteredStream`, so every byte is reported to `counters`
/// (and `activity`, if set) directly instead: client reads as soon as
/// they're pulled off the client socket (regardless of whether upstream
/// ever takes them), client writes as soon as they land on the client
/// socket — the same boundary `MeteredStream` uses, so byte accounting is
/// identical to the portable copy even when a direction fails partway.
pub(super) async fn copy_bidirectional(
    client: &TcpStream,
    upstream: &TcpStream,
    pipes: (Pipe, Pipe),
    counters: &ByteCounters,
    activity: Option<&Activity>,
) -> io::Result<()> {
    let (to_upstream_pipe, to_client_pipe) = pipes;
    let touch = || {
        if let Some(activity) = activity {
            activity.touch();
        }
    };
    let to_upstream = splice_direction(
        client,
        upstream,
        to_upstream_pipe,
        |n| {
            counters.add_read(n as u64);
            touch();
        },
        |_written| {},
    );
    let to_client = splice_direction(
        upstream,
        client,
        to_client_pipe,
        |_read| {},
        |n| {
            counters.add_written(n as u64);
            touch();
        },
    );
    tokio::try_join!(to_upstream, to_client)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permits_reserve_at_least_half_the_limit_for_everything_else() {
        assert_eq!(splice_permits(8_000), (8_000 / 2) / 4);
    }

    #[test]
    fn permits_are_zero_below_one_connections_worth_of_fds() {
        assert_eq!(splice_permits(7), 0);
    }

    #[test]
    fn permits_are_zero_when_the_limit_is_unknown() {
        assert_eq!(splice_permits(0), 0);
    }

    #[test]
    fn override_replaces_the_heuristic_when_set_and_valid() {
        assert_eq!(admission_capacity(Some("3"), 8_000), 3);
    }

    #[test]
    fn override_of_zero_disables_splice_admission() {
        assert_eq!(admission_capacity(Some("0"), 1_000_000), 0);
    }

    #[test]
    fn unset_or_unparseable_override_falls_back_to_the_heuristic() {
        assert_eq!(admission_capacity(None, 8_000), splice_permits(8_000));
        assert_eq!(
            admission_capacity(Some("not-a-number"), 8_000),
            splice_permits(8_000)
        );
    }

    #[test]
    fn admission_is_one_process_wide_instance() {
        // Two `Router::serve` calls (or, in an embedder, two independent
        // `Router`s) must share one budget, not each get their own slice of
        // `RLIMIT_NOFILE` — see `admission`'s doc comment.
        assert!(Arc::ptr_eq(&admission().permits, &admission().permits));
    }
}
