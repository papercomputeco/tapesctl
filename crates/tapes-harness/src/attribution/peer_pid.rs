//! Per-OS "which PID owns this TCP loopback connection" lookup.
//!
//! The proxy listens on `127.0.0.1:51539` and accepts TCP connections;
//! every accept hands back the peer's `(addr, port)` 4-tuple from
//! kernel state. The Claude process on the other end of that connection
//! has the matching socket in its FD table. The lookup answers:
//! "given a candidate PID set (the filenames in `~/.claude/sessions/`)
//! and the peer's loopback address, which candidate owns the socket?"
//!
//! Implementation: [`netsock`] enumerates the TCP socket table; we match
//! on `local_addr`/`local_port` and check whether a candidate PID owns the
//! socket. Ownership resolution is OS-split because attributing a socket to
//! a PID costs differently per platform:
//!
//! * Linux — `NETLINK_INET_DIAG` lists sockets (with inodes and UID) cheaply,
//!   but mapping an inode back to a PID means walking `/proc/<pid>/fd`. paperd
//!   runs unprivileged, so a *system-wide* walk (what
//!   `netsock::get_sockets` does internally) hits `EACCES` on every process
//!   we don't own and floods the log. We instead enumerate sockets with
//!   [`netsock::iter_sockets_without_processes`] (no walk) and read only a
//!   bounded PID set: either the caller's known candidates, or for owner
//!   lookup, processes whose `/proc/<pid>` owner matches the socket UID.
//! * macOS — `proc_pidfdinfo` is already per-PID and there is no
//!   unprivileged-host permission storm, so we keep netsock's
//!   process-attached enumeration as-is.
//!
//! A small per-peer-address memoization sits in front of the netsock
//! scan: Claude's HTTP/2 multiplexes many requests over one TCP
//! connection, and the `(peer_ip, peer_port)` tuple is invariant for the
//! connection's lifetime. Caching the resolved PID for [`CACHE_TTL`]
//! turns "global socket-table scan per request" into "scan once,
//! HashMap lookup after." Cached PIDs are revalidated against the live
//! candidate set on every hit so a dead process whose port gets reused
//! inside the window can't masquerade as the original owner.
//!
//! The cache key is the whole [`SocketAddr`], not just the port. A port
//! number is only unique within an address family and interface
//! address: `127.0.0.1:54321` and `[::1]:54321` are different sockets
//! that can be owned by different processes at the same instant, and
//! the scan below treats them as different (see [`addr_matches`] —
//! native v6 does not match v4). Keying on the port alone let one
//! entry answer for both.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use netsock::family::AddressFamilyFlags;
use netsock::protocol::ProtocolFlags;

/// How long a resolved peer-address → PID mapping stays valid. Chosen to
/// align with the kernel's TIME_WAIT window (≈60 s on Linux/macOS):
/// a closed port can't be reassigned to a different process until
/// TIME_WAIT drains, so an entry that survives TIME_WAIT cannot
/// out-live the connection it described.
const CACHE_TTL: Duration = Duration::from_secs(60);

/// Result of a peer-PID lookup attempt.
pub struct PeerPidLookup {
    /// Matched PID, if any candidate owned the peer socket.
    pub pid: Option<i32>,
    /// How long the lookup took, in microseconds. Reported so callers
    /// (the recorder, future per-request tracing) can characterize
    /// p50/p99 latency on real traffic. Cache hits are measured too —
    /// they're not free, just very cheap.
    pub micros: u64,
}

#[derive(Clone, Copy)]
struct CacheEntry {
    pid: i32,
    when: Instant,
}

/// Process-global memo of peer socket address → owning PID. Keyed by
/// the complete [`SocketAddr`]: the IP is as load-bearing as the port,
/// since a port is only unique per address, and two loopback peers can
/// legitimately hold the same port on different addresses.
fn cache() -> &'static Mutex<HashMap<SocketAddr, CacheEntry>> {
    static C: OnceLock<Mutex<HashMap<SocketAddr, CacheEntry>>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Look up which of `candidates` owns the loopback TCP socket whose
/// peer endpoint is `peer`. The peer is the *agent's* side of the
/// connection (the client socket on the Claude process); paperd's
/// `accept` returns that 4-tuple directly.
///
/// Returns `Self::pid = None` when no candidate matches (caller falls
/// through to the `harness_id: unknown` path) or when the underlying
/// netsock enumeration errors out.
pub fn lookup(candidates: &[i32], peer: SocketAddr) -> PeerPidLookup {
    let started = Instant::now();
    let pid = cached_lookup(candidates, peer);
    let micros = u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX);
    PeerPidLookup { pid, micros }
}

/// Look up the process that owns the accepted loopback peer socket.
///
/// On Linux this still avoids `netsock::get_sockets`: netlink gives us the
/// socket inode and UID, then we inspect only processes owned by that UID. That
/// keeps manual Codex proxy attribution working without crawling root-owned
/// fd tables.
pub fn lookup_owner(peer: SocketAddr) -> PeerPidLookup {
    let started = Instant::now();
    #[cfg(target_os = "linux")]
    let pid = cached_lookup_owner(peer);
    #[cfg(not(target_os = "linux"))]
    let pid = scan_owner(peer);
    let micros = u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX);
    PeerPidLookup { pid, micros }
}

#[cfg(target_os = "linux")]
fn cached_lookup_owner(peer: SocketAddr) -> Option<i32> {
    let candidates = same_uid_pids_for_peer_socket(peer)?;
    cached_lookup(&candidates, peer)
}

fn cached_lookup(candidates: &[i32], peer: SocketAddr) -> Option<i32> {
    let key = peer;

    // Cache-hit path: revalidate against `candidates`. A stale entry
    // for a PID the watcher has since dropped (process died, address
    // reassigned) would mis-attribute fresh connections to a dead
    // owner; checking membership on every hit closes that hole
    // without needing the watcher to notify the cache.
    {
        let mut map = cache()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(entry) = map.get(&key).copied() {
            if entry.when.elapsed() < CACHE_TTL && candidates.contains(&entry.pid) {
                return Some(entry.pid);
            }
            map.remove(&key);
        }
    }

    // Cache miss / stale: scan. Only `Some` results are memoized —
    // `None` from a cold race (UA matched Claude but the watcher
    // hasn't yet parsed the session file) must remain retryable, or
    // the whole connection gets pinned to `unknown` for the full
    // TTL.
    let pid = scan(candidates, peer)?;
    let mut map = cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    map.insert(
        key,
        CacheEntry {
            pid,
            when: Instant::now(),
        },
    );
    Some(pid)
}

fn scan(candidates: &[i32], peer: SocketAddr) -> Option<i32> {
    // Enumerate both address families: a v4 peer can appear as a
    // v4-mapped v6 socket on the harness side (and vice-versa). netsock
    // reports the kernel's `local_addr` verbatim, so `addr_matches`
    // handles both cases without the explicit v4-mapped-v6 dance the old
    // hand-rolled code did.
    let af = AddressFamilyFlags::IPV4 | AddressFamilyFlags::IPV6;

    #[cfg(target_os = "linux")]
    {
        scan_linux(candidates, peer, af)
    }
    #[cfg(not(target_os = "linux"))]
    {
        scan_netsock_attached(candidates, peer, af)
    }
}

#[cfg(not(target_os = "linux"))]
fn scan_owner(peer: SocketAddr) -> Option<i32> {
    let af = AddressFamilyFlags::IPV4 | AddressFamilyFlags::IPV6;
    let sockets = netsock::get_sockets(af, ProtocolFlags::TCP).ok()?;
    let peer_port = peer.port();

    sockets.into_iter().find_map(|s| {
        if s.local_port() != peer_port || !addr_matches(s.local_addr(), peer.ip()) {
            return None;
        }
        s.processes
            .into_iter()
            .find_map(|process| i32::try_from(process.pid).ok())
    })
}

/// Linux: enumerate sockets without the system-wide `/proc` walk, then
/// resolve ownership against only the candidate PIDs.
#[cfg(target_os = "linux")]
fn scan_linux(candidates: &[i32], peer: SocketAddr, af: AddressFamilyFlags) -> Option<i32> {
    let (inode, _uid) = peer_socket_inode_uid(peer, af)?;

    // Ownership: read only the candidate PIDs' fd tables. Candidates are
    // Claude harness children (same uid as paperd), so `/proc/<cand>/fd`
    // is readable — no EACCES, and at most a handful of dirs instead of
    // every process on the host.
    candidates
        .iter()
        .copied()
        .find(|&cand| pid_owns_socket_inode(cand, inode))
}

#[cfg(target_os = "linux")]
fn same_uid_pids_for_peer_socket(peer: SocketAddr) -> Option<Vec<i32>> {
    let af = AddressFamilyFlags::IPV4 | AddressFamilyFlags::IPV6;
    let (_inode, uid) = peer_socket_inode_uid(peer, af)?;
    Some(pids_owned_by_uid(uid))
}

#[cfg(target_os = "linux")]
fn peer_socket_inode_uid(peer: SocketAddr, af: AddressFamilyFlags) -> Option<(u32, u32)> {
    // Netlink-only: lists sockets (with inodes and UID) but attaches no
    // process info, so it never touches `/proc/<pid>/fd` and never emits the
    // permission-denied warnings paperd would hit on processes it can't read.
    let sockets = netsock::iter_sockets_without_processes(af, ProtocolFlags::TCP).ok()?;

    let peer_port = peer.port();
    sockets.into_iter().find_map(|s| {
        let s = s.ok()?;
        (s.local_port() == peer_port && addr_matches(s.local_addr(), peer.ip()))
            .then_some((s.inode, s.uid))
    })
}

#[cfg(target_os = "linux")]
fn pids_owned_by_uid(uid: u32) -> Vec<i32> {
    use std::os::unix::fs::MetadataExt;

    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };

    entries
        .flatten()
        .filter_map(|entry| {
            let pid = entry.file_name().to_str()?.parse::<i32>().ok()?;
            let metadata = entry.metadata().ok()?;
            (metadata.uid() == uid).then_some(pid)
        })
        .collect()
}

/// True if PID `pid` holds an fd pointing at `socket:[inode]`. A PID we
/// can't read (gone, or not ours) simply doesn't match.
#[cfg(target_os = "linux")]
fn pid_owns_socket_inode(pid: i32, inode: u32) -> bool {
    let Ok(entries) = std::fs::read_dir(format!("/proc/{pid}/fd")) else {
        return false;
    };
    let needle = format!("socket:[{inode}]");
    entries.flatten().any(|entry| {
        // Socket fd links are always ASCII `socket:[N]`, so a `to_str`
        // comparison is sufficient and avoids OsStr equality surprises.
        std::fs::read_link(entry.path())
            .ok()
            .is_some_and(|link| link.to_str() == Some(needle.as_str()))
    })
}

/// Non-Linux (macOS): keep netsock's process-attached enumeration —
/// `proc_pidfdinfo` is per-PID and there is no unprivileged-host
/// permission storm to avoid.
#[cfg(not(target_os = "linux"))]
fn scan_netsock_attached(
    candidates: &[i32],
    peer: SocketAddr,
    af: AddressFamilyFlags,
) -> Option<i32> {
    let sockets = netsock::get_sockets(af, ProtocolFlags::TCP).ok()?;

    let peer_port = peer.port();
    for s in sockets {
        if s.local_port() != peer_port {
            continue;
        }
        if !addr_matches(s.local_addr(), peer.ip()) {
            continue;
        }
        for &cand in candidates {
            if let Ok(pid_u32) = u32::try_from(cand)
                && s.is_owned_by_pid(pid_u32)
            {
                return Some(cand);
            }
        }
    }
    None
}

/// Accept either-family loopback equivalence: a v4 peer matches a
/// v4-mapped v6 socket address and vice-versa. netsock returns the
/// kernel-reported `local_addr` verbatim, so the unmap happens here.
fn addr_matches(local: std::net::IpAddr, peer: std::net::IpAddr) -> bool {
    use std::net::IpAddr;
    if local == peer {
        return true;
    }
    match (local, peer) {
        (IpAddr::V6(v6), IpAddr::V4(v4)) | (IpAddr::V4(v4), IpAddr::V6(v6)) => {
            v6.to_ipv4_mapped() == Some(v4)
        }
        _ => false,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, TcpListener, TcpStream};

    /// Drop any cached entry for this peer so tests reusing the same
    /// ephemeral address (rare but possible) don't see leftover state.
    fn clear_cache_for(peer: SocketAddr) {
        let mut map = cache()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        map.remove(&peer);
    }

    fn lookup_until_pid(candidates: &[i32], peer: SocketAddr, expected: i32) -> PeerPidLookup {
        let deadline = Instant::now() + Duration::from_millis(250);
        loop {
            let got = lookup(candidates, peer);
            if got.pid == Some(expected) || Instant::now() >= deadline {
                return got;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn addr_matches_same_family() {
        let v4 = IpAddr::V4(Ipv4Addr::LOCALHOST);
        let v6 = IpAddr::V6(Ipv6Addr::LOCALHOST);
        assert!(addr_matches(v4, v4));
        assert!(addr_matches(v6, v6));
    }

    #[test]
    fn addr_matches_v4_mapped_v6_either_direction() {
        let v4 = IpAddr::V4(Ipv4Addr::LOCALHOST);
        let mapped = IpAddr::V6(Ipv4Addr::LOCALHOST.to_ipv6_mapped());
        assert!(addr_matches(mapped, v4));
        assert!(addr_matches(v4, mapped));
    }

    #[test]
    fn addr_matches_rejects_native_v6_vs_v4() {
        assert!(!addr_matches(
            IpAddr::V6(Ipv6Addr::LOCALHOST),
            IpAddr::V4(Ipv4Addr::LOCALHOST),
        ));
    }

    /// End-to-end behavior: open a real loopback TCP connection,
    /// then ask `lookup` whose PID owns the client side. We're the
    /// owner, so passing our own PID as the sole candidate should
    /// match. Verifies the netsock integration (filter flags, address
    /// matching, ownership predicate) against the live kernel.
    #[test]
    fn lookup_finds_self_pid_for_live_loopback_socket() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let server_addr = listener.local_addr().unwrap();
        let client = TcpStream::connect(server_addr).unwrap();
        let (_server_side, _peer) = listener.accept().unwrap();
        let peer = client.local_addr().unwrap();
        clear_cache_for(peer);
        let me = std::process::id() as i32;

        let got = lookup_until_pid(&[me], peer, me);
        assert_eq!(
            got.pid,
            Some(me),
            "expected self pid {me} to own loopback socket {peer}",
        );
    }

    #[test]
    fn lookup_owner_finds_self_pid_for_live_loopback_socket() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let server_addr = listener.local_addr().unwrap();
        let client = TcpStream::connect(server_addr).unwrap();
        let (_server_side, _peer) = listener.accept().unwrap();
        let peer = client.local_addr().unwrap();
        clear_cache_for(peer);
        let me = std::process::id() as i32;

        let deadline = Instant::now() + Duration::from_millis(250);
        let got = loop {
            let got = lookup_owner(peer);
            if got.pid == Some(me) || Instant::now() >= deadline {
                break got;
            }
            std::thread::sleep(Duration::from_millis(5));
        };
        assert_eq!(
            got.pid,
            Some(me),
            "expected owner lookup to find self pid {me} for loopback socket {peer}",
        );
    }

    /// The second lookup for the same peer must be served from cache:
    /// we tear down the live socket between calls, so a second netsock
    /// scan would no longer find our PID owning anything on that port.
    /// If the call still returns our PID, the cache is doing its job.
    #[test]
    fn second_lookup_hits_cache_after_socket_closes() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let server_addr = listener.local_addr().unwrap();
        let peer = {
            let client = TcpStream::connect(server_addr).unwrap();
            let (_server_side, _peer) = listener.accept().unwrap();
            let peer = client.local_addr().unwrap();
            clear_cache_for(peer);

            let me = std::process::id() as i32;
            assert_eq!(
                lookup_until_pid(&[me], peer, me).pid,
                Some(me),
                "primer scan failed"
            );
            peer
            // `client` drops here; the kernel tears the socket down.
        };

        let me = std::process::id() as i32;
        let got = lookup(&[me], peer);
        assert_eq!(
            got.pid,
            Some(me),
            "expected cache hit to keep returning our pid after socket closed",
        );
    }

    /// A cached PID that's no longer in the candidate set must NOT be
    /// returned — that's how the cache stays correct when a Claude
    /// process dies and its peer port is later reused by some other
    /// process the watcher has not blessed.
    #[test]
    fn cached_pid_dropped_from_candidates_is_invalidated() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let server_addr = listener.local_addr().unwrap();
        let client = TcpStream::connect(server_addr).unwrap();
        let (_server_side, _peer) = listener.accept().unwrap();
        let peer = client.local_addr().unwrap();
        clear_cache_for(peer);

        let me = std::process::id() as i32;
        assert_eq!(
            lookup_until_pid(&[me], peer, me).pid,
            Some(me),
            "primer scan failed"
        );

        // Second call with a candidate set that excludes us: the
        // cached entry must be ignored. We'll get None because the
        // re-scan won't find any of the (bogus) candidates owning
        // the socket either.
        let got = lookup(&[1], peer);
        assert_eq!(
            got.pid, None,
            "stale cached pid was returned despite being absent from candidates",
        );
    }

    /// Two peers can hold the same port on different addresses at the
    /// same instant: `127.0.0.1:N` and `[::1]:N` are distinct sockets,
    /// and [`addr_matches`] deliberately refuses to equate native v6
    /// with v4. Keyed by port alone, the cache collapsed them into one
    /// entry, so a second peer inherited the first peer's PID without
    /// any socket of its own ever having been resolved — silent
    /// mis-attribution, since the candidate-set revalidation cannot
    /// catch it (the PID is live and blessed, just not the owner).
    #[test]
    fn distinct_addresses_sharing_a_port_do_not_share_a_cache_entry() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let server_addr = listener.local_addr().unwrap();
        let client = TcpStream::connect(server_addr).unwrap();
        let (_server_side, _peer) = listener.accept().unwrap();
        let v4_peer = client.local_addr().unwrap();
        // Same port, different address. We own no socket here, so a
        // correct lookup has nothing to resolve and must answer None.
        let v6_peer = SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), v4_peer.port());
        clear_cache_for(v4_peer);
        clear_cache_for(v6_peer);

        let me = std::process::id() as i32;
        assert_eq!(
            lookup_until_pid(&[me], v4_peer, me).pid,
            Some(me),
            "primer scan failed",
        );

        assert_eq!(
            lookup(&[me], v6_peer).pid,
            None,
            "the cache entry for {v4_peer} answered for {v6_peer} — \
             the cache is keyed by port rather than by address",
        );

        // The entry that legitimately exists is untouched by the miss.
        assert_eq!(lookup(&[me], v4_peer).pid, Some(me));

        clear_cache_for(v4_peer);
    }

    /// Structural companion to the behavioural test above: the map
    /// itself holds one entry per address, so the same port on two
    /// addresses carries independent PIDs and independent expiry.
    /// Uses port 9 (discard), outside the ephemeral range, so a real
    /// socket in a concurrently running test cannot collide with it.
    #[test]
    fn cache_holds_one_entry_per_address_not_per_port() {
        let v4 = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 9);
        let v6 = SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), 9);

        {
            let mut map = cache()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            map.insert(
                v4,
                CacheEntry {
                    pid: 111,
                    when: Instant::now(),
                },
            );
            map.insert(
                v6,
                CacheEntry {
                    pid: 222,
                    when: Instant::now(),
                },
            );
            assert_eq!(map.get(&v4).map(|e| e.pid), Some(111));
            assert_eq!(map.get(&v6).map(|e| e.pid), Some(222));
        }

        clear_cache_for(v4);
        clear_cache_for(v6);
    }

    #[test]
    fn lookup_returns_none_when_no_candidate_matches() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let server_addr = listener.local_addr().unwrap();
        let client = TcpStream::connect(server_addr).unwrap();
        let (_server_side, _peer) = listener.accept().unwrap();
        let peer = client.local_addr().unwrap();
        clear_cache_for(peer);

        // PID 1 (init/launchd) won't own a socket we just opened.
        let got = lookup(&[1], peer);
        assert_eq!(got.pid, None);
    }

    /// Manual benchmark. Run with:
    ///
    /// ```text
    /// cargo test -p paper-daemon --release \
    ///     proxy::session::peer_pid::tests::bench_lookup_microseconds \
    ///     -- --ignored --nocapture
    /// ```
    ///
    /// Reports cold (first / cache-miss) vs warm (cache-hit) latency
    /// percentiles. Old baseline (hand-rolled FFI + `/proc/net/tcp`):
    /// macOS p99 ≈ 78 µs, Linux p50 ≈ 25 ms. Cold path on netsock is
    /// expected higher (macOS) or lower (Linux); warm path should be
    /// single-digit µs on both.
    #[test]
    #[ignore = "manual benchmark; opt in with --ignored"]
    fn bench_lookup_microseconds() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let server_addr = listener.local_addr().unwrap();
        let client = TcpStream::connect(server_addr).unwrap();
        let (_server_side, _peer) = listener.accept().unwrap();
        let peer = client.local_addr().unwrap();
        let me = std::process::id() as i32;

        const N: usize = 200;
        let pct = |samples: &[u64], q: f64| {
            samples[((samples.len() as f64 * q) as usize).min(samples.len() - 1)]
        };

        // Cold: clear cache before each iteration so every sample
        // pays for a full netsock scan.
        let mut cold = Vec::with_capacity(N);
        for _ in 0..5 {
            clear_cache_for(peer);
            let _ = lookup(&[me], peer);
        }
        for _ in 0..N {
            clear_cache_for(peer);
            cold.push(lookup(&[me], peer).micros);
        }
        cold.sort_unstable();
        eprintln!(
            "peer_pid::lookup COLD µs over {N} iters: p50={} p90={} p99={} p999={} max={}",
            pct(&cold, 0.50),
            pct(&cold, 0.90),
            pct(&cold, 0.99),
            pct(&cold, 0.999),
            cold.last().copied().unwrap_or(0),
        );

        // Warm: prime once, then hammer the cached path.
        clear_cache_for(peer);
        let _ = lookup(&[me], peer);
        let mut warm = Vec::with_capacity(N);
        for _ in 0..N {
            warm.push(lookup(&[me], peer).micros);
        }
        warm.sort_unstable();
        eprintln!(
            "peer_pid::lookup WARM µs over {N} iters: p50={} p90={} p99={} p999={} max={}",
            pct(&warm, 0.50),
            pct(&warm, 0.90),
            pct(&warm, 0.99),
            pct(&warm, 0.999),
            warm.last().copied().unwrap_or(0),
        );
    }
}
