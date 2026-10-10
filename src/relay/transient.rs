// Reply handles for one-shot connections (#511).
//
// A transient dial — `relay send-session`, `relay delegate` — is deliberately
// never put in `PeerRegistry` (#487): registering it under the sender's peer
// id put two connections under one id, and the collision rule closed one
// before its message was read. The listener therefore drops the
// `PeerConnection` and keeps only the reader thread, which is why frames still
// reach the serve loop.
//
// The cost was that nothing could write *back*. A transcript landed, was
// resumable, and the sender was never told: `reg.send_to(from_peer, &ack)`
// looked up a peer that had never been registered, and the ledger row stayed
// at `sent` forever — indistinguishable from a transfer that never arrived.
//
// So this is a map of write handles, kept beside the registry rather than in
// it. Nothing here participates in peer collision, heartbeats, or reconnect;
// an entry lives for one connection and is dropped as soon as it is used or
// the connection ends. The handle is an independent `TcpStream` clone, so the
// reader thread exiting on EOF does not close it.

use std::collections::HashMap;
use std::net::TcpStream;
use std::sync::{Arc, Mutex};

use super::{PeerId, RelayMessage, protocol};

/// Write handles for in-flight one-shot deliveries, keyed by the peer that
/// dialled. Cloneable; every clone shares the one map.
///
/// **Lifetime.** An entry is removed when it is used. A transient connection
/// that delivers something needing no acknowledgement — a delegated task, say
/// — leaves its entry until that peer's next delivery replaces it, so the map
/// is bounded by the number of distinct peers and holds at most one stale
/// handle each. That is a small, fixed number of file descriptors rather than
/// unbounded growth, which is why there is no reaper here: a `forget` with no
/// caller is the kind of item the #465 audit deleted, and the reader thread
/// that would be the natural caller lives inside `PeerConnection` and knows
/// nothing about replies.
#[derive(Clone, Default)]
pub struct TransientReplies {
    inner: Arc<Mutex<HashMap<String, TcpStream>>>,
}

impl TransientReplies {
    pub fn new() -> Self {
        Self::default()
    }

    /// Remember how to answer `peer` for the life of this connection.
    ///
    /// A second delivery from the same peer replaces the first. Two concurrent
    /// one-shot sends from one peer would therefore leave the earlier
    /// unanswerable — acceptable because an ack names its session id, so a
    /// sender can tell the difference, and because the alternative is keying
    /// on something the message channel does not carry.
    pub fn remember(&self, peer: &PeerId, stream: TcpStream) {
        if let Ok(mut map) = self.inner.lock() {
            map.insert(peer.as_str().to_string(), stream);
        }
    }

    /// Answer `peer` and drop the handle, whether or not the write worked.
    ///
    /// Taking the handle out first means a dead socket is removed rather than
    /// retried: an `EPIPE` from a stale handle reads exactly like "the peer is
    /// gone", and keeping it would make the next delivery's ack fail for the
    /// previous connection's reason.
    pub fn reply(&self, peer: &PeerId, msg: &RelayMessage) -> Result<(), String> {
        let Some(mut stream) = self.take(peer) else {
            return Err(format!("no open one-shot connection from {peer}"));
        };
        protocol::write_message(&mut stream, msg).map_err(|e| e.to_string())
    }

    fn take(&self, peer: &PeerId) -> Option<TcpStream> {
        self.inner.lock().ok()?.remove(peer.as_str())
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.inner.lock().map(|m| m.len()).unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{TcpListener, TcpStream as Stream};

    fn a_socket() -> Stream {
        // A real connected socket, so `remember` holds the same kind of handle
        // it does in production. The listener is dropped immediately; nothing
        // here writes.
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let client = Stream::connect(addr).expect("connect");
        let _ = listener.accept();
        client
    }

    #[test]
    fn a_remembered_handle_is_taken_once() {
        let replies = TransientReplies::new();
        let peer = PeerId("sender-1".into());
        replies.remember(&peer, a_socket());
        assert_eq!(replies.len(), 1);

        // The first reply consumes the handle; a second has nothing to use,
        // which is what stops a stale socket being retried.
        let msg = super::super::transfer::build_received_message(
            "sess-1",
            std::path::Path::new("/tmp/sess-1.jsonl"),
            "/tmp",
            "receiver",
        );
        let _ = replies.reply(&peer, &msg);
        assert_eq!(replies.len(), 0);

        let err = replies.reply(&peer, &msg).expect_err("nothing left to use");
        assert!(err.contains("no open one-shot connection"), "got: {err}");
    }

    /// Two deliveries from one peer: the second handle replaces the first, so
    /// the map cannot grow without bound for a peer that keeps sending.
    #[test]
    fn a_second_delivery_from_one_peer_replaces_the_first() {
        let replies = TransientReplies::new();
        let peer = PeerId("sender-3".into());
        replies.remember(&peer, a_socket());
        replies.remember(&peer, a_socket());
        assert_eq!(replies.len(), 1);
    }
}
