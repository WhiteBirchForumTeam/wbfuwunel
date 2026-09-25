//! Tuwunel's client-IP extractor.
//!
//! One rule, two ways to turn it on (維護者 2026-09-25): **a forwarding header
//! is believed when the operator named it in `reverse_proxy_ip_header`, OR
//! when the peer is in `localhost_ip`. Otherwise the peer address is the only
//! answer and no header can move it.**
//!
//! The two conditions say different things and both are needed:
//!
//! - `reverse_proxy_ip_header` says "a proxy in front of me writes the client
//!   into *this* header". It is believed whoever the peer is, because the
//!   operator has promised nobody reaches the server another way.
//! - `localhost_ip` says "a peer here is not a client, it is something running
//!   beside me" — a proxy on the same host, or the Unix socket, whose peer
//!   address is a synthesised `127.0.0.1` (`router/serve/unix.rs`). There is no
//!   configured header to read, so the default one is used.
//!
//! 🚨 Why the second one exists at all: without it every request from those
//! deployments carries the same address, and every limit keyed on the address —
//! the login and OIDC rate limits, the per-address connection limit — becomes
//! one bucket for the whole server. Rate limiting runs before credentials are
//! checked, so that bucket is drainable by anyone (PR #85 review, cirno).
//!
//! 🚨 What this replaced, and why the rule is not just "scan the headers": the
//! extractor used to scan forwarding headers for *every* peer, taking the
//! leftmost `X-Forwarded-For` ahead of the peer. That made the address
//! client-controlled by default — fine for a log line, not fine for anything
//! that bounds a resource by it (PR #85 review: rumia, cirno, salvia). The
//! difference now is that an unconfigured server believes a header only from a
//! peer that is the machine itself.

use std::{
	fmt,
	net::{IpAddr, SocketAddr},
	sync::Arc,
};

use axum::extract::{ConnectInfo, FromRequestParts};
use http::{Extensions, HeaderMap, StatusCode, request::Parts};
use ipnet::IpNet;
use tuwunel_core::config::ReverseProxyIpHeader;

/// What a local peer's forwarded address is read from when the operator has
/// not named a header. `X-Forwarded-For` is what every proxy writes without
/// being asked; rightmost because only the hop that talked to us can append
/// there.
const LOCAL_PEER_DEFAULT: ReverseProxyIpHeader = ReverseProxyIpHeader::RightmostXForwardedFor;

/// Tuwunel client-IP extractor. See module docs.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ClientIp(pub(crate) IpAddr);

/// Marker wrapper around [`ReverseProxyIpHeader`] placed into request
/// extensions only when an operator has explicitly configured it.
#[derive(Clone, Copy, Debug)]
pub struct ConfiguredIpHeader(pub ReverseProxyIpHeader);

/// The operator's `localhost_ip`: peer ranges that are this machine rather
/// than a client, and may therefore say who the client is.
///
/// 🚨 Absent means trust nobody. A router that does not install this layer —
/// the bridge's, for one — gets no local trust at all, which is the safe end.
#[derive(Clone, Debug)]
pub struct LocalPeerRanges(pub Arc<[IpNet]>);

impl<S> FromRequestParts<S> for ClientIp
where
	S: Sync,
{
	type Rejection = (StatusCode, &'static str);

	async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
		const ERROR: StatusCode = StatusCode::INTERNAL_SERVER_ERROR;

		// A header that is there is believed; one that is missing means this
		// request did not come through the proxy (a health check straight at
		// the socket, say), and then the peer is the honest answer.
		if let Some(source) = header_to_believe(&parts.extensions)
			&& let Some(address) = secure_extract(source, &parts.headers, &parts.extensions)
		{
			return Ok(Self(address));
		}

		peer_address(&parts.extensions)
			.map(Self)
			.ok_or((ERROR, "Can't extract `ClientIp`, provide `axum::extract::ConnectInfo`"))
	}
}

impl fmt::Display for ClientIp {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { fmt::Display::fmt(&self.0, f) }
}

/// Args:
///     extensions: the request's extensions, example: the ones the outer
///         router's layers fill from config
/// Return:
///     Option<ReverseProxyIpHeader>  the source to read, or None when no
///     header may be believed for this request.
fn header_to_believe(extensions: &Extensions) -> Option<ReverseProxyIpHeader> {
	// Configured wins outright: naming one header does not open the others,
	// and it does not matter who the peer is.
	if let Some(&ConfiguredIpHeader(source)) = extensions.get::<ConfiguredIpHeader>() {
		return Some(source);
	}

	is_peer_local(extensions).then_some(LOCAL_PEER_DEFAULT)
}

/// Args:
///     extensions: the request's extensions, example: the ones carrying
///         `ConnectInfo` and `LocalPeerRanges`
/// Return:
///     bool  true only when a peer address is present and falls in a
///     configured range; false when either is missing.
fn is_peer_local(extensions: &Extensions) -> bool {
	let Some(&LocalPeerRanges(ref ranges)) = extensions.get::<LocalPeerRanges>() else {
		return false;
	};
	let Some(peer) = peer_address(extensions) else {
		return false;
	};

	// 🚨 Canonicalized first, and that is load-bearing: a dual-stack listener
	// hands an IPv4 peer over as `::ffff:127.0.0.1`, which `127.0.0.0/8` does
	// not contain. Without this the default would match nothing there — and
	// the same omission in `to_address_group` was a real bug (PR #85 review).
	let peer = peer.to_canonical();
	ranges.iter().any(|range| range.contains(&peer))
}

/// The address the transport itself saw, which no header can move.
///
/// Args:
///     extensions: the request's extensions, example: the ones axum fills with
///         `ConnectInfo` for a TCP listener
/// Return:
///     Option<IpAddr>  None only when nothing put a `ConnectInfo` there, and
///     then the caller has no address to report at all.
fn peer_address(extensions: &Extensions) -> Option<IpAddr> {
	extensions
		.get::<ConnectInfo<SocketAddr>>()
		.map(|ConnectInfo(addr)| addr.ip())
}

fn secure_extract(
	source: ReverseProxyIpHeader,
	headers: &HeaderMap,
	extensions: &Extensions,
) -> Option<IpAddr> {
	match source {
		| ReverseProxyIpHeader::ConnectInfo => peer_address(extensions),
		| ReverseProxyIpHeader::RightmostXForwardedFor => rightmost_x_forwarded_for(headers),
		| ReverseProxyIpHeader::RightmostForwarded => rightmost_forwarded(headers),
		| ReverseProxyIpHeader::XRealIp => single_ip_header(headers, "x-real-ip"),
		| ReverseProxyIpHeader::CfConnectingIp => single_ip_header(headers, "cf-connecting-ip"),
		| ReverseProxyIpHeader::TrueClientIp => single_ip_header(headers, "true-client-ip"),
		| ReverseProxyIpHeader::FlyClientIp => single_ip_header(headers, "fly-client-ip"),
		| ReverseProxyIpHeader::CloudFrontViewerAddress => cloudfront_viewer_address(headers),
	}
}

fn rightmost_x_forwarded_for(headers: &HeaderMap) -> Option<IpAddr> {
	headers
		.get_all("x-forwarded-for")
		.iter()
		.filter_map(|v| v.to_str().ok())
		.flat_map(|s| s.split(','))
		.filter_map(|s| s.trim().parse::<IpAddr>().ok())
		.next_back()
}

fn rightmost_forwarded(headers: &HeaderMap) -> Option<IpAddr> {
	headers
		.get_all("forwarded")
		.iter()
		.filter_map(|v| v.to_str().ok())
		.flat_map(|s| s.split(','))
		.filter_map(parse_forwarded_for)
		.next_back()
}

fn parse_forwarded_for(stanza: &str) -> Option<IpAddr> {
	let for_value = stanza
		.split(';')
		.find_map(|part| {
			let (k, v) = part.split_once('=')?;
			k.trim()
				.eq_ignore_ascii_case("for")
				.then_some(v.trim())
		})?
		.trim_matches('"');

	let bracketed = for_value
		.strip_prefix('[')
		.and_then(|s| s.split_once(']'))
		.map(|(ip, _rest)| ip);

	let candidate = bracketed
		.or_else(|| for_value.rsplit_once(':').map(|(ip, _port)| ip))
		.unwrap_or(for_value);

	candidate.trim().parse::<IpAddr>().ok()
}

fn single_ip_header(headers: &HeaderMap, name: &'static str) -> Option<IpAddr> {
	headers
		.get(name)
		.and_then(|v| v.to_str().ok())
		.and_then(|s| s.trim().parse::<IpAddr>().ok())
}

fn cloudfront_viewer_address(headers: &HeaderMap) -> Option<IpAddr> {
	headers
		.get("cloudfront-viewer-address")
		.and_then(|v| v.to_str().ok())
		.and_then(|s| s.rsplit_once(':').map(|(ip, _port)| ip))
		.and_then(|s| s.trim().parse::<IpAddr>().ok())
}

#[cfg(test)]
mod tests {
	use std::{
		iter,
		net::{Ipv4Addr, SocketAddr},
		sync::Arc,
	};

	use axum::{
		extract::{ConnectInfo, FromRequestParts},
		http::{Request, StatusCode, request::Parts},
	};
	use ipnet::IpNet;
	use tuwunel_core::config::ReverseProxyIpHeader;

	use super::{ClientIp, ConfiguredIpHeader, LocalPeerRanges};

	/// A client out on the internet: never local, whatever it sends.
	const PEER: SocketAddr =
		SocketAddr::new(std::net::IpAddr::V4(Ipv4Addr::new(203, 0, 113, 9)), 4567);
	const LOOPBACK: SocketAddr = SocketAddr::new(std::net::IpAddr::V4(Ipv4Addr::LOCALHOST), 38000);

	/// What `default_localhost_ip()` produces, spelled out here so a change to
	/// the default has to be made in two places on purpose.
	fn default_ranges() -> LocalPeerRanges {
		let ranges: Vec<IpNet> = ["127.0.0.0/8", "::1/128"]
			.iter()
			.map(|range| range.parse().expect("a literal range in a test"))
			.collect();
		LocalPeerRanges(Arc::from(ranges))
	}

	fn parts_from(
		peer: Option<SocketAddr>,
		source: Option<ReverseProxyIpHeader>,
		ranges: Option<LocalPeerRanges>,
		headers: impl IntoIterator<Item = (&'static str, &'static str)>,
	) -> Parts {
		let mut request = Request::builder().uri("/");
		for (name, value) in headers {
			request = request.header(name, value);
		}
		let (mut parts, ()) = request
			.body(())
			.expect("a well-formed request in a test")
			.into_parts();
		if let Some(peer) = peer {
			parts.extensions.insert(ConnectInfo(peer));
		}
		if let Some(source) = source {
			parts.extensions.insert(ConfiguredIpHeader(source));
		}
		if let Some(ranges) = ranges {
			parts.extensions.insert(ranges);
		}
		parts
	}

	async fn extract_client_ip(parts: &mut Parts) -> Result<ClientIp, (StatusCode, &'static str)> {
		ClientIp::from_request_parts(parts, &()).await
	}

	/// 🚨 The whole point of the rule: a client out on the internet cannot move
	/// its own address, whatever it sends and whichever default is in force.
	/// The login rate limiter and the per-address connection limit both key on
	/// this, so a header that moved it would hand out a fresh bucket per
	/// request (PR #85 review).
	#[tokio::test]
	async fn a_remote_peer_cannot_move_its_address_with_any_header() {
		for header in [
			("X-Forwarded-For", "9.9.9.9"),
			("X-Real-Ip", "9.9.9.9"),
			("Forwarded", "for=9.9.9.9"),
			("CF-Connecting-IP", "9.9.9.9"),
			("True-Client-IP", "9.9.9.9"),
			("Fly-Client-IP", "9.9.9.9"),
		] {
			let mut parts = parts_from(Some(PEER), None, Some(default_ranges()), [header]);
			let ClientIp(ip) = extract_client_ip(&mut parts)
				.await
				.expect("a peer address is present");
			assert_eq!(ip, PEER.ip(), "{} must not move the address", header.0);
		}
	}

	/// 🚨 The reason `localhost_ip` exists: a proxy on the same host, and the
	/// Unix socket whose peer address is a synthesised `127.0.0.1`, are not the
	/// client. Without this every such request shares one address, and every
	/// limit keyed on it becomes one bucket for the whole server (PR #85
	/// review, cirno).
	#[tokio::test]
	async fn a_local_peer_names_the_client_without_any_header_configured() {
		let mut parts = parts_from(
			Some(LOOPBACK),
			None,
			Some(default_ranges()),
			[("X-Forwarded-For", "1.1.1.1, 2.2.2.2")],
		);
		let ClientIp(ip) = extract_client_ip(&mut parts)
			.await
			.expect("a peer address is present");
		assert_eq!(ip.to_string(), "2.2.2.2", "the rightmost hop is the one the local proxy added");
	}

	/// A dual-stack listener (`[::]`, which `router/serve.rs` sets up on
	/// purpose) hands an IPv4 peer over mapped. `127.0.0.0/8` does not contain
	/// `::ffff:127.0.0.1`, so without canonicalizing first the default would
	/// match nothing there.
	#[tokio::test]
	async fn a_mapped_loopback_peer_is_local_too() {
		let mapped = SocketAddr::new("::ffff:127.0.0.1".parse().expect("a literal address"), 38000);
		let mut parts = parts_from(
			Some(mapped),
			None,
			Some(default_ranges()),
			[("X-Forwarded-For", "2.2.2.2")],
		);
		let ClientIp(ip) = extract_client_ip(&mut parts)
			.await
			.expect("a peer address is present");
		assert_eq!(ip.to_string(), "2.2.2.2");
	}

	/// Emptying `localhost_ip` is how an operator says "I face clients
	/// directly": then not even loopback may name someone else.
	#[tokio::test]
	async fn with_no_local_ranges_a_loopback_peer_is_just_a_peer() {
		let empty = LocalPeerRanges(Arc::from(Vec::new()));
		let mut parts =
			parts_from(Some(LOOPBACK), None, Some(empty), [("X-Forwarded-For", "9.9.9.9")]);
		let ClientIp(ip) = extract_client_ip(&mut parts)
			.await
			.expect("a peer address is present");
		assert_eq!(ip, LOOPBACK.ip());
	}

	/// 🚨 Fail closed: a router that never installs the layer — the bridge's
	/// inner one — trusts nobody, rather than falling back to some built-in
	/// idea of what is local.
	#[tokio::test]
	async fn without_the_ranges_extension_nothing_is_local() {
		let mut parts = parts_from(Some(LOOPBACK), None, None, [("X-Forwarded-For", "9.9.9.9")]);
		let ClientIp(ip) = extract_client_ip(&mut parts)
			.await
			.expect("a peer address is present");
		assert_eq!(ip, LOOPBACK.ip());
	}

	#[tokio::test]
	async fn a_configured_header_is_believed_from_a_remote_peer() {
		let mut parts = parts_from(
			Some(PEER),
			Some(ReverseProxyIpHeader::RightmostXForwardedFor),
			Some(default_ranges()),
			[("X-Forwarded-For", "1.1.1.1, 2.2.2.2")],
		);
		let ClientIp(ip) = extract_client_ip(&mut parts)
			.await
			.expect("a peer address is present");
		assert_eq!(ip.to_string(), "2.2.2.2", "the rightmost hop is the one the proxy added");
	}

	/// A request that did not come through the proxy has no such header — a
	/// health check straight at the port, say. That is not an error: the peer
	/// is the honest answer (維護者 2026-09-24).
	#[tokio::test]
	async fn a_configured_header_that_is_absent_leaves_the_peer() {
		let mut parts = parts_from(
			Some(PEER),
			Some(ReverseProxyIpHeader::RightmostXForwardedFor),
			Some(default_ranges()),
			iter::empty(),
		);
		let ClientIp(ip) = extract_client_ip(&mut parts)
			.await
			.expect("a peer address is present");
		assert_eq!(ip, PEER.ip());
	}

	#[tokio::test]
	async fn a_configured_header_that_is_unparseable_leaves_the_peer() {
		let mut parts = parts_from(
			Some(PEER),
			Some(ReverseProxyIpHeader::RightmostXForwardedFor),
			Some(default_ranges()),
			[("X-Forwarded-For", "not-an-address")],
		);
		let ClientIp(ip) = extract_client_ip(&mut parts)
			.await
			.expect("a peer address is present");
		assert_eq!(ip, PEER.ip(), "a header that says nothing usable is a header that is not there");
	}

	/// Naming one header does not open the others — including for a local
	/// peer, whose default would otherwise have read `X-Forwarded-For`.
	#[tokio::test]
	async fn a_configured_header_replaces_the_local_default_rather_than_adding_to_it() {
		let mut parts = parts_from(
			Some(LOOPBACK),
			Some(ReverseProxyIpHeader::XRealIp),
			Some(default_ranges()),
			[("X-Forwarded-For", "9.9.9.9")],
		);
		let ClientIp(ip) = extract_client_ip(&mut parts)
			.await
			.expect("a peer address is present");
		assert_eq!(ip, LOOPBACK.ip());
	}

	#[tokio::test]
	async fn connect_info_is_the_peer_whatever_the_headers_say() {
		let mut parts = parts_from(
			Some(LOOPBACK),
			Some(ReverseProxyIpHeader::ConnectInfo),
			Some(default_ranges()),
			[("X-Forwarded-For", "9.9.9.9")],
		);
		let ClientIp(ip) = extract_client_ip(&mut parts)
			.await
			.expect("a peer address is present");
		assert_eq!(ip, LOOPBACK.ip(), "connect_info names no header, so none is read");
	}

	#[tokio::test]
	async fn ipv6_forwarded_and_cloudfront_sources_parse() {
		let mut forwarded = parts_from(
			Some(PEER),
			Some(ReverseProxyIpHeader::RightmostForwarded),
			None,
			[("Forwarded", "for=1.1.1.1, for=\"[2001:db8::2]:443\"")],
		);
		let ClientIp(ip) = extract_client_ip(&mut forwarded)
			.await
			.expect("a peer address is present");
		assert_eq!(ip.to_string(), "2001:db8::2");

		let mut cloudfront = parts_from(
			Some(PEER),
			Some(ReverseProxyIpHeader::CloudFrontViewerAddress),
			None,
			[("CloudFront-Viewer-Address", "198.51.100.4:12345")],
		);
		let ClientIp(ip) = extract_client_ip(&mut cloudfront)
			.await
			.expect("a peer address is present");
		assert_eq!(ip.to_string(), "198.51.100.4");
	}

	/// Nothing to report at all. `router/serve/unix.rs` makes sure this does
	/// not happen for a Unix socket by synthesising a peer, but a header alone
	/// is not an address the server may use.
	#[tokio::test]
	async fn without_a_peer_there_is_no_address() {
		let mut parts =
			parts_from(None, None, Some(default_ranges()), [("X-Forwarded-For", "9.9.9.9")]);
		let err = extract_client_ip(&mut parts)
			.await
			.expect_err("no ConnectInfo was inserted");
		assert_eq!(err.0, StatusCode::INTERNAL_SERVER_ERROR);
		assert!(err.1.contains("ConnectInfo"), "{err:?}");
	}
}
