//! Join the Stacks peer-to-peer network directly.
//!
//! The production runtime today reaches the chain through one HTTP peer's Stacks
//! RPC, which makes a hosted service's availability, rate limit and *view of the
//! chain* part of nano's liveness. This crate is the transport that replaces it:
//! nano's own wire codec, handshake, liveness and neighbour discovery, plus a
//! peer database that survives a restart.
//!
//! ## What is and is not here
//!
//! [`wire`] is the message codec, [`session`] one authenticated conversation,
//! [`swarm`] a bounded set of them with scoring and neighbour discovery,
//! [`inbound`] the reply side that lets another node handshake *with* nano,
//! [`peers`] the durable peer table, [`relay`] the bounded hand-off between the
//! peers that push data and the loop that can check it, and [`served`] what this
//! node will still tell a peer it has after a restart.
//!
//! ## Blocks come over HTTP, and that is not a shortcut
//!
//! In Nakamoto, stacks-core downloads blocks and tenures over HTTP to each peer's
//! own RPC endpoint (`net/download/nakamoto/tenure_downloader.rs`); there is no
//! p2p message for requesting a block. So what p2p is *for* is the handshake,
//! neighbours, inventories, and pushed blocks and transactions — and the thing it
//! produces for the rest of the node is [`Discovered::endpoints`], the `data_url`
//! of every peer that handshook. Those endpoints replace a hosted API not by
//! speaking a different protocol but by being many, found rather than configured,
//! and nobody's product.
//!
//! ## Why nano's own codec
//!
//! `stackslib` already implements this format, and nano may not link it: the
//! release node is forbidden a reference-node crate in its normal dependency
//! graph, which `nano-conformance/tests/conformance/release_dependencies.rs`
//! enforces from `cargo tree`. That is not a loss of ground truth, because
//! `stackslib` *is* a dev-dependency of `nano-conformance` — so every message
//! here is checked against stacks-core's own encoder in
//! `nano-conformance/tests/conformance/p2p_wire.rs`, both directions, byte for
//! byte, plus signatures each side makes and the other verifies.
//!
//! ## No peer is a consensus input
//!
//! A [`session::Session`] returns authenticated *claims*: this peer says its tip
//! is here, has these tenures, holds these blocks. Nothing here decides what any
//! of it means. Everything a peer says still has to pass the local burnchain,
//! signer, VRF, transaction and state-root checks, and then the fork choice from
//! `tasks/027-choose-a-fork-instead-of-following-a-peer.md`, before it changes
//! this node's view. Authentication proves *who* said something, never that it is
//! true.

pub mod inbound;
pub mod peers;
pub mod relay;
pub mod served;
pub mod session;
pub mod swarm;
pub mod wire;

pub use inbound::{InboundLimits, Listener, Served, Service, serve_peer};
pub use peers::{KnownPeer, MAX_KNOWN_PEERS, PeerDb, PeerDbError};
pub use relay::{MAX_QUEUED_OFFERS, Offer, Pushed, Relay, relayed_by};
pub use served::ServedTenures;
pub use session::{LocalPeer, Protocol, Session, SessionError, nack_is_transient};
pub use swarm::{Discovered, Round, Swarm, SwarmLimits, TenureClaim, assign_tenures};
pub use wire::{
    ChainView, Handshake, Message, NeighborAddress, Payload, PeerAddress, Preamble,
    STABLE_CONFIRMATIONS, WireError,
};

/// Parse a bootstrap peer as stacks-core's configuration spells it:
/// `<33-byte hex node key>@<host>:<port>`.
///
/// The key is optional, and is only ever a hint: a session learns the peer's key
/// from its handshake and authenticates against that, so a configured key that
/// turns out to be wrong changes nothing about what nano accepts. It is kept
/// because it is what operators paste, and because it lets a caller notice that
/// the peer at an address is not the one it was told about.
#[must_use]
pub fn parse_seed(spec: &str) -> Option<(Option<[u8; 33]>, String, u16)> {
    let (key, endpoint) = match spec.split_once('@') {
        Some((key, endpoint)) => {
            let mut bytes = [0; 33];
            hex_into(key, &mut bytes)?;
            (Some(bytes), endpoint)
        }
        None => (None, spec),
    };
    let (host, port) = endpoint.rsplit_once(':')?;
    Some((key, host.to_owned(), port.parse().ok()?))
}

fn hex_into(hex: &str, out: &mut [u8]) -> Option<()> {
    if hex.len() != out.len() * 2 {
        return None;
    }
    for (index, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(hex.get(index * 2..index * 2 + 2)?, 16).ok()?;
    }
    Some(())
}

/// The mainnet bootstrap peers stacks-core ships as its default
/// (`stackslib/src/config/mod.rs`).
///
/// These are a starting point, not an authority: the first successful handshake
/// with any of them yields a `Neighbors` reply, and from then on the peer table
/// is nano's own. A node that has run before should prefer what it remembers.
pub const MAINNET_SEEDS: [&str; 4] = [
    "02196f005965cebe6ddc3901b7b1cc1aa7a88f305bb8c5893456b8f9a605923893@seed.mainnet.hiro.so:20444",
    "02539449ad94e6e6392d8c1deb2b4e61f80ae2a18964349bc14336d8b903c46a8c@cet.stacksnodes.org:20444",
    "02ececc8ce79b8adf813f13a0255f8ae58d4357309ba0cedd523d9f1a306fcfb79@sgt.stacksnodes.org:20444",
    "0303144ba518fe7a0fb56a8a7d488f950307a4330f146e1e1458fc63fb33defe96@est.stacksnodes.org:20444",
];

#[cfg(test)]
mod tests {
    use super::*;

    /// The bootstrap spellings an operator actually pastes.
    #[test]
    fn a_seed_parses_with_or_without_a_key() {
        let (key, host, port) = parse_seed(MAINNET_SEEDS[0]).expect("the default seed parses");
        assert_eq!(host, "seed.mainnet.hiro.so");
        assert_eq!(port, 20444);
        assert_eq!(key.expect("a key")[0], 0x02);
        assert_eq!(
            parse_seed("seed.mainnet.hiro.so:20444"),
            Some((None, "seed.mainnet.hiro.so".to_owned(), 20444))
        );
        // An IPv6 literal keeps its colons: the port is the last one.
        let (_, host, port) = parse_seed("[2001:db8::1]:20444").expect("an IPv6 seed parses");
        assert_eq!((host.as_str(), port), ("[2001:db8::1]", 20444));
        for bad in ["", "no-port", "host:not-a-port", "abcd@host:20444"] {
            assert!(parse_seed(bad).is_none(), "{bad} parsed");
        }
    }

    /// Every default seed is one the code can actually dial.
    #[test]
    fn every_default_seed_parses() {
        for seed in MAINNET_SEEDS {
            let (key, _, port) = parse_seed(seed).unwrap_or_else(|| panic!("{seed} parses"));
            assert!(key.is_some());
            assert_eq!(port, 20444);
        }
    }

    /// A v4 peer survives the round trip through the wire's sixteen bytes.
    ///
    /// The protocol stores every address as IPv6, so a v4 peer is v4-mapped. Not
    /// unmapping it on the way back would make every dial go to a v6 socket that
    /// is not listening.
    #[test]
    fn a_v4_address_comes_back_as_v4() {
        let original: std::net::IpAddr = "203.0.113.9".parse().expect("a v4 address");
        let address = PeerAddress::from_ip(original);
        assert_eq!(address.as_bytes()[..12], [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xff, 0xff]);
        assert_eq!(address.to_ip(), original);
        assert_eq!(address.to_socket_addr(20444).to_string(), "203.0.113.9:20444");

        let v6: std::net::IpAddr = "2001:db8::1".parse().expect("a v6 address");
        assert_eq!(PeerAddress::from_ip(v6).to_ip(), v6);
    }

    /// Only "come back later" nacks are transient.
    #[test]
    fn a_nack_says_not_yet_or_no() {
        assert!(nack_is_transient(wire::nack::THROTTLED));
        assert!(nack_is_transient(wire::nack::STALE_VIEW));
        assert!(!nack_is_transient(wire::nack::INVALID_MESSAGE));
        assert!(!nack_is_transient(wire::nack::NO_SUCH_DB));
        assert!(!nack_is_transient(wire::nack::HANDSHAKE_REQUIRED));
    }
}
