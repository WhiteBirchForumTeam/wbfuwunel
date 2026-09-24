//! Tuwunel's client-IP extractor.
//!
//! One rule, and it is the operator's `ip_source` that turns it on (維護者
//! 2026-09-24): **configured means "I am behind a reverse proxy, and it puts
//! the client's address in this header" — so read the header, believe it when
//! it is there, and fall back to the TCP peer when it is not. Unconfigured
//! means there is no proxy, so the TCP peer is the only answer.**
//!
//! 🚨 What this replaced, and why: the extractor used to scan forwarding
//! headers *even with `ip_source` unset*, taking the leftmost `X-Forwarded-For`
//! ahead of the TCP peer. That made the address client-controlled by default —
//! fine for a log line, not fine for anything that bounds a resource by it.
//! Both the login rate limiter and the per-address connection limit key on
//! this value, so a directly-connected client could take a fresh bucket per
//! request just by varying a header (PR #85 review: rumia, cirno, salvia).
//!
//! ⚠️ The remaining trust boundary, stated plainly: with `ip_source` set, a
//! client that can reach the server directly (not through the proxy) can still
//! send that header itself. Closing that needs the peer to be checked against
//! the proxy's address — see the note on `TrustedPeerSubnets`.

use std::{
	fmt,
	net::{IpAddr, SocketAddr},
	sync::Arc,
};

use axum::extract::{ConnectInfo, FromRequestParts};
use http::{Extensions, HeaderMap, StatusCode, request::Parts};
use ipnet::IpNet;
use tuwunel_core::config::IpSource;

/// Tuwunel client-IP extractor. See module docs.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ClientIp(pub(crate) IpAddr);

/// Marker wrapper around [`IpSource`] placed into request extensions
/// only when an operator has explicitly configured `ip_source`.
#[derive(Clone, Copy, Debug)]
pub struct ConfiguredIpSource(pub IpSource);

/// Operator-configured subnets that the reverse proxy connects from.
///
/// 🚧 **Nothing reads this any more.** It used to make a trusted peer *skip*
/// the configured extraction and fall back to scanning headers — the opposite
/// of what the name suggests, and one of the ways the address became
/// client-controlled (PR #85 review, salvia). The rule it would fit is the one
/// still open: with `ip_source` set, believe the header **only when the peer
/// is the proxy**. Until that is decided the server warns at startup that this
/// setting has no effect, rather than letting it look like it does.
#[derive(Clone, Debug)]
pub struct TrustedPeerSubnets(pub Arc<[IpNet]>);

impl<S> FromRequestParts<S> for ClientIp
where
	S: Sync,
{
	type Rejection = (StatusCode, &'static str);

	async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
		const ERROR: StatusCode = StatusCode::INTERNAL_SERVER_ERROR;

		// Configured: the named header is where the proxy writes the client's
		// address. A header that is there is believed; one that is missing
		// means this request did not come through the proxy (a health check on
		// the socket, a direct call), and then the peer is the honest answer.
		if let Some(&ConfiguredIpSource(source)) = parts.extensions.get::<ConfiguredIpSource>()
			&& let Some(address) = secure_extract(source, &parts.headers, &parts.extensions)
		{
			return Ok(Self(address));
		}

		// Unconfigured, or configured but the header was absent: the TCP peer.
		// 🚨 Headers are never consulted here — see the module docs for what
		// scanning them by default used to cost.
		peer_address(&parts.extensions)
			.map(Self)
			.ok_or((ERROR, "Can't extract `ClientIp`, provide `axum::extract::ConnectInfo`"))
	}
}

impl fmt::Display for ClientIp {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { fmt::Display::fmt(&self.0, f) }
}

/// The address the transport itself saw, which no header can move.
///
/// Args:
///     extensions: the request's extensions, example: the ones axum fills with
///         `ConnectInfo` for a TCP listener
/// Return:
///     Option<IpAddr>  None only when nothing put a `ConnectInfo` there (a
///     Unix socket), and then the caller has no address to report at all.
fn peer_address(extensions: &Extensions) -> Option<IpAddr> {
	extensions
		.get::<ConnectInfo<SocketAddr>>()
		.map(|ConnectInfo(addr)| addr.ip())
}

fn secure_extract(
	source: IpSource,
	headers: &HeaderMap,
	extensions: &Extensions,
) -> Option<IpAddr> {
	match source {
		| IpSource::ConnectInfo => extensions
			.get::<ConnectInfo<SocketAddr>>()
			.map(|ConnectInfo(addr)| addr.ip()),
		| IpSource::RightmostXForwardedFor => rightmost_x_forwarded_for(headers),
		| IpSource::RightmostForwarded => rightmost_forwarded(headers),
		| IpSource::XRealIp => single_ip_header(headers, "x-real-ip"),
		| IpSource::CfConnectingIp => single_ip_header(headers, "cf-connecting-ip"),
		| IpSource::TrueClientIp => single_ip_header(headers, "true-client-ip"),
		| IpSource::FlyClientIp => single_ip_header(headers, "fly-client-ip"),
		| IpSource::CloudFrontViewerAddress => cloudfront_viewer_address(headers),
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
	use std::{iter, net::SocketAddr};

	use axum::{
		extract::{ConnectInfo, FromRequestParts},
		http::{Request, StatusCode, request::Parts},
	};
	use tuwunel_core::config::IpSource;

	use super::{ClientIp, ConfiguredIpSource};

	fn parts(headers: impl IntoIterator<Item = (&'static str, &'static str)>) -> Parts {
		let mut request = Request::builder().uri("/");
		for (name, value) in headers {
			request = request.header(name, value);
		}
		let (parts, ()) = request.body(()).unwrap().into_parts();
		parts
	}

	fn parts_from(
		peer: Option<SocketAddr>,
		source: Option<IpSource>,
		headers: impl IntoIterator<Item = (&'static str, &'static str)>,
	) -> Parts {
		let mut parts = parts(headers);
		if let Some(peer) = peer {
			parts.extensions.insert(ConnectInfo(peer));
		}
		if let Some(source) = source {
			parts.extensions.insert(ConfiguredIpSource(source));
		}
		parts
	}

	async fn extract_client_ip(
		parts: &mut Parts,
	) -> Result<ClientIp, (StatusCode, &'static str)> {
		ClientIp::from_request_parts(parts, &()).await
	}

	const PEER: SocketAddr = SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::new(203, 0, 113, 9)), 4567);

	/// 🚨 The whole point of the rule: with no `ip_source` the address is the
	/// socket's, and a client that sends forwarding headers gets nowhere. The
	/// login rate limiter and the per-address connection limit both key on
	/// this, so a header that moved it would hand out a fresh bucket per
	/// request (PR #85 review).
	#[tokio::test]
	async fn without_ip_source_headers_are_ignored_and_the_peer_decides() {
		for header in [
			("X-Forwarded-For", "9.9.9.9"),
			("X-Real-Ip", "9.9.9.9"),
			("Forwarded", "for=9.9.9.9"),
			("CF-Connecting-IP", "9.9.9.9"),
			("True-Client-IP", "9.9.9.9"),
			("Fly-Client-IP", "9.9.9.9"),
		] {
			let mut parts = parts_from(Some(PEER), None, [header]);
			let ClientIp(ip) = extract_client_ip(&mut parts).await.unwrap();
			assert_eq!(ip, PEER.ip(), "{} must not move the address", header.0);
		}
	}

	#[tokio::test]
	async fn with_ip_source_the_named_header_is_believed() {
		let mut parts = parts_from(
			Some(PEER),
			Some(IpSource::RightmostXForwardedFor),
			[("X-Forwarded-For", "1.1.1.1, 2.2.2.2")],
		);
		let ClientIp(ip) = extract_client_ip(&mut parts).await.unwrap();
		assert_eq!(ip.to_string(), "2.2.2.2", "the rightmost hop is the one the proxy added");
	}

	/// A request that did not come through the proxy has no such header — a
	/// health check straight at the port, say. That is not an error: the peer
	/// is the honest answer (維護者 2026-09-24).
	#[tokio::test]
	async fn with_ip_source_but_no_header_the_peer_is_used_rather_than_failing() {
		let mut parts = parts_from(Some(PEER), Some(IpSource::RightmostXForwardedFor), iter::empty());
		let ClientIp(ip) = extract_client_ip(&mut parts).await.unwrap();
		assert_eq!(ip, PEER.ip());
	}

	#[tokio::test]
	async fn with_ip_source_an_unparseable_header_falls_back_to_the_peer() {
		let mut parts = parts_from(
			Some(PEER),
			Some(IpSource::RightmostXForwardedFor),
			[("X-Forwarded-For", "not-an-address")],
		);
		let ClientIp(ip) = extract_client_ip(&mut parts).await.unwrap();
		assert_eq!(ip, PEER.ip(), "a header that says nothing usable is a header that is not there");
	}

	/// Only the configured header counts: naming one does not open the others.
	#[tokio::test]
	async fn with_ip_source_a_different_header_is_not_consulted() {
		let mut parts =
			parts_from(Some(PEER), Some(IpSource::XRealIp), [("X-Forwarded-For", "9.9.9.9")]);
		let ClientIp(ip) = extract_client_ip(&mut parts).await.unwrap();
		assert_eq!(ip, PEER.ip());
	}

	#[tokio::test]
	async fn ip_source_connect_info_is_the_peer_whatever_the_headers_say() {
		let mut parts =
			parts_from(Some(PEER), Some(IpSource::ConnectInfo), [("X-Forwarded-For", "9.9.9.9")]);
		let ClientIp(ip) = extract_client_ip(&mut parts).await.unwrap();
		assert_eq!(ip, PEER.ip());
	}

	/// 📎 Loopback used to be special-cased into the header scan. It is not any
	/// more: the rule keys on the configuration, not on who the peer happens
	/// to be (PR #85 review, salvia — that bypass also applied with
	/// `ip_source` set, which made the gate meaningless from loopback).
	#[tokio::test]
	async fn a_loopback_peer_gets_no_special_treatment() {
		let loopback = SocketAddr::from(([127, 0, 0, 1], 38000));
		let mut unconfigured = parts_from(Some(loopback), None, [("X-Forwarded-For", "9.9.9.9")]);
		let ClientIp(ip) = extract_client_ip(&mut unconfigured).await.unwrap();
		assert_eq!(ip, loopback.ip(), "no ip_source: the header is ignored here too");

		let mut configured = parts_from(
			Some(loopback),
			Some(IpSource::RightmostXForwardedFor),
			[("X-Forwarded-For", "9.9.9.9")],
		);
		let ClientIp(ip) = extract_client_ip(&mut configured).await.unwrap();
		assert_eq!(ip.to_string(), "9.9.9.9", "ip_source set: the named header is read, as anywhere else");
	}

	#[tokio::test]
	async fn ipv6_forwarded_and_cloudfront_sources_parse() {
		let mut forwarded = parts_from(
			Some(PEER),
			Some(IpSource::RightmostForwarded),
			[("Forwarded", "for=1.1.1.1, for=\"[2001:db8::2]:443\"")],
		);
		let ClientIp(ip) = extract_client_ip(&mut forwarded).await.unwrap();
		assert_eq!(ip.to_string(), "2001:db8::2");

		let mut cloudfront = parts_from(
			Some(PEER),
			Some(IpSource::CloudFrontViewerAddress),
			[("CloudFront-Viewer-Address", "198.51.100.4:12345")],
		);
		let ClientIp(ip) = extract_client_ip(&mut cloudfront).await.unwrap();
		assert_eq!(ip.to_string(), "198.51.100.4");
	}

	/// Nothing to report at all: a Unix socket has no peer address, and with
	/// headers no longer consulted there is no second place to look.
	#[tokio::test]
	async fn without_a_peer_there_is_no_address() {
		let mut parts = parts_from(None, None, [("X-Forwarded-For", "9.9.9.9")]);
		let err = extract_client_ip(&mut parts).await.unwrap_err();
		assert_eq!(err.0, StatusCode::INTERNAL_SERVER_ERROR);
		assert!(err.1.contains("ConnectInfo"), "{err:?}");
	}
}
