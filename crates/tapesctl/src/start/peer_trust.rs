//! Whether the process on the other end of a connection is the harness this
//! process launched.
//!
//! # Why this exists
//!
//! A harness that stamps its own `X-Tapes-*` envelope hands the proxy a session
//! identity it cannot otherwise discover. Believing that envelope because the
//! *launched* harness was self-attributing is not enough: it says nothing about
//! who sent the request. The proxy listens on loopback, and every process on the
//! machine can reach a loopback port — an ephemeral port number is obscurity,
//! not a boundary. Without this check any local process could stamp two headers
//! and have a turn persisted, and a session link announced, under a session id
//! of its choosing.
//!
//! So the envelope is believed only when the peer socket is owned by the
//! launched harness or by one of its descendants — the same evidence chain the
//! redirected harnesses already attribute through, applied to a different
//! question. The kernel's socket table answers "which PID owns this peer
//! socket", and the process tree answers "is that PID ours".
//!
//! # Why descendants, and not just the launched PID
//!
//! A harness is often reached through a wrapper. The pi builds seen here are a
//! chain of `exec`ing shell wrappers, which preserves the PID all the way down
//! to the `node` process that runs the extension — for those, the launched PID
//! *is* the peer. But a wrapper that runs its payload without `exec` is an
//! equally ordinary packaging choice, and there the socket belongs to a child.
//! Accepting descendants covers both without weakening anything: the tree is
//! walked upward from the peer, so membership still has to terminate at the
//! process this proxy started.
//!
//! # Failing closed
//!
//! Every uncertain answer here is `false`: an unresolvable peer, a harness that
//! has not been spawned yet, a truncated walk. The cost of a false negative is a
//! turn filed under `unknown` — recoverable, and visible in the log. The cost of
//! a false positive is a forged session, which is not.

use std::net::SocketAddr;

use tapes_harnesses::attribution::peer_pid;

/// How far up the process tree to look before giving up.
///
/// A bound rather than a `while` loop: `getppid` chains are shallow in practice,
/// and a cycle or a pathologically deep tree must not become an unbounded loop
/// on a request path.
const MAX_ANCESTRY_HOPS: usize = 32;

/// Does the peer of this connection belong to the launched harness?
///
/// `launched` is the harness's PID, or `None` before it has been spawned.
#[must_use]
pub fn peer_is_launched_harness(peer: SocketAddr, launched: Option<i32>) -> bool {
    let Some(launched) = launched else {
        return false;
    };
    // Whole-table socket scan, so it is deliberately the *second* question:
    // callers check that there is an envelope worth trusting first, and a
    // request without one never pays for this.
    let Some(peer_pid) = peer_pid::lookup_owner(peer).pid else {
        return false;
    };
    is_launched_or_descendant(peer_pid, launched)
}

/// Is `pid` the launched harness, or below it in the process tree?
#[must_use]
pub fn is_launched_or_descendant(pid: i32, launched: i32) -> bool {
    // PID 1 adopts orphans, so every process on the machine descends from it.
    // A launched harness that somehow presented as PID 1 would therefore vouch
    // for the entire machine.
    if launched <= 1 {
        return false;
    }
    let mut current = pid;
    for _ in 0..MAX_ANCESTRY_HOPS {
        if current == launched {
            return true;
        }
        // Stop at the root of the tree rather than reporting a miss as a
        // truncation: `parent_of` returning 0 means "no parent left".
        match parent_of(current) {
            Some(parent) if parent > 0 && parent != current => current = parent,
            _ => return false,
        }
    }
    false
}

/// The parent PID of `pid`, or `None` when it cannot be determined.
///
/// `/proc/<pid>/stat`'s second field is the executable name in parentheses and
/// may itself contain spaces and parentheses, so the fields after it are found
/// from the *last* `)` rather than by splitting the whole line.
#[cfg(target_os = "linux")]
fn parent_of(pid: i32) -> Option<i32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after_comm = stat.rsplit_once(')')?.1;
    // Fields resume at `state`, so the parent is the one after it.
    after_comm.split_whitespace().nth(1)?.parse().ok()
}

/// The parent PID of `pid`, or `None` when it cannot be determined.
///
/// macOS has no `/proc`; `proc_pidinfo` is the supported query, and the shared
/// crate reaches for the same call for its own per-PID lookups.
#[cfg(target_os = "macos")]
fn parent_of(pid: i32) -> Option<i32> {
    use std::mem::{MaybeUninit, size_of};

    // Not exported by `libc`, so it is spelled here the way the shared crate
    // spells the flavours it needs. From `<sys/proc_info.h>`.
    const PROC_PIDTBSDINFO: libc::c_int = 3;

    let mut info = MaybeUninit::<libc::proc_bsdinfo>::zeroed();
    let size = i32::try_from(size_of::<libc::proc_bsdinfo>()).ok()?;
    // SAFETY: the kernel writes at most `size` bytes into a correctly sized,
    // zeroed allocation of exactly this type, and the result is only read after
    // the call reports having filled it.
    let written =
        unsafe { libc::proc_pidinfo(pid, PROC_PIDTBSDINFO, 0, info.as_mut_ptr().cast(), size) };
    if written != size {
        return None;
    }
    // SAFETY: `proc_pidinfo` reported a complete structure.
    let info = unsafe { info.assume_init() };
    i32::try_from(info.pbi_ppid).ok()
}

/// No process-tree source on this platform, so nothing can be vouched for.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn parent_of(_pid: i32) -> Option<i32> {
    None
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    /// A child that lives long enough to be asked about, and is killed with the
    /// test rather than left behind.
    struct Child(std::process::Child);

    impl Child {
        fn spawn() -> Self {
            Self(
                std::process::Command::new("sleep")
                    .arg("30")
                    .spawn()
                    .expect("sleep is available"),
            )
        }

        fn pid(&self) -> i32 {
            i32::try_from(self.0.id()).unwrap()
        }
    }

    impl Drop for Child {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    fn me() -> i32 {
        i32::try_from(std::process::id()).unwrap()
    }

    #[test]
    fn a_process_is_its_own_launched_harness() {
        // The common case on an `exec`ing wrapper chain: the PID this proxy
        // spawned is the PID that opens the socket.
        assert!(is_launched_or_descendant(me(), me()));
    }

    #[test]
    fn a_child_of_the_launched_process_is_trusted() {
        // The other packaging shape: a wrapper that runs its payload as a child
        // rather than `exec`ing it. Read against the live process tree, because
        // that is the thing being trusted.
        let child = Child::spawn();
        assert!(
            is_launched_or_descendant(child.pid(), me()),
            "a real child of this process was not recognised as its descendant",
        );
    }

    #[test]
    fn a_process_that_is_not_below_the_launched_one_is_refused() {
        // The attack: some other local process reaches the proxy port. Our own
        // process is the child's *parent*, so walking up from it can never
        // arrive at the child — the direction of the walk is what makes this
        // hold.
        let child = Child::spawn();
        assert!(
            !is_launched_or_descendant(me(), child.pid()),
            "a process outside the launched harness's subtree was trusted",
        );
    }

    #[test]
    fn nothing_is_trusted_before_the_harness_is_spawned() {
        // Requests can reach the listener between bind and spawn. Until there is
        // a harness to compare against, no envelope is believable.
        let peer = "127.0.0.1:9".parse().unwrap();
        assert!(!peer_is_launched_harness(peer, None));
    }

    #[test]
    fn pid_one_vouches_for_nobody() {
        // Every process descends from the init process, so accepting it as a
        // launched harness would trust the whole machine.
        assert!(!is_launched_or_descendant(me(), 1));
        assert!(!is_launched_or_descendant(me(), 0));
    }

    #[test]
    fn an_unowned_peer_address_is_refused() {
        // Port 9 (discard) is outside the ephemeral range and nothing here holds
        // it, so the owner lookup finds no process — which must read as "not the
        // harness", never as "cannot tell, so allow".
        let peer = "127.0.0.1:9".parse().unwrap();
        assert!(!peer_is_launched_harness(peer, Some(me())));
    }
}
