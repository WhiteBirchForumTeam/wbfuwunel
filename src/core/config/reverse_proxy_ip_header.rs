//! Names the header a reverse proxy writes the client's address into.
//!
//! [`ReverseProxyIpHeader`] enumerates the forwarding headers the server knows
//! how to read, plus [`ReverseProxyIpHeader::ConnectInfo`] for "no header at
//! all, the transport peer is the answer".

use serde::Deserialize;

/// Selects where the connecting client's address is read from.
///
/// 🚨 Every variant but `ConnectInfo` names a header, and a header is only
/// worth believing when something in front of the server overwrites it. See
/// `api/router/client_ip.rs` for when it is consulted at all.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ReverseProxyIpHeader {
	/// No header: the transport peer address. Safe default; no proxy required.
	#[default]
	ConnectInfo,

	/// Rightmost value of `X-Forwarded-For`.
	RightmostXForwardedFor,

	/// Rightmost value of RFC 7239 `Forwarded`.
	RightmostForwarded,

	/// `X-Real-IP` header (nginx).
	XRealIp,

	/// `CF-Connecting-IP` (Cloudflare / cloudflared).
	CfConnectingIp,

	/// `True-Client-IP` (Akamai, Cloudflare Enterprise).
	TrueClientIp,

	/// `Fly-Client-IP` (Fly.io).
	FlyClientIp,

	/// `CloudFront-Viewer-Address` (AWS CloudFront).
	#[serde(rename = "cloudfront_viewer_address")]
	CloudFrontViewerAddress,
}
