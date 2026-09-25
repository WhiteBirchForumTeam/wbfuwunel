#![cfg(test)]

use axum::Extension;
use http::{
	HeaderMap, Response,
	header::{CONTENT_SECURITY_POLICY, CONTENT_TYPE, X_FRAME_OPTIONS},
};
use ipnet::IpNet;
use tower::util::Either;
use tuwunel_api::router::{ConfiguredIpHeader, LocalPeerRanges};
use tuwunel_core::config::ReverseProxyIpHeader;

use super::{local_peer_ranges_layer, reverse_proxy_ip_header_layer, set_html_headers};

#[test]
fn reverse_proxy_ip_header_layer_none_returns_identity_branch() {
	let layer = reverse_proxy_ip_header_layer(None);

	assert!(matches!(layer, Either::Right(_)));
}

#[test]
fn reverse_proxy_ip_header_layer_connect_info_returns_extension_branch() {
	let layer = reverse_proxy_ip_header_layer(Some(ReverseProxyIpHeader::ConnectInfo));

	assert!(matches!(layer, Either::Left(Extension(ConfiguredIpHeader(_)))));
}

#[test]
fn local_peer_ranges_layer_empty_returns_identity_branch() {
	let layer = local_peer_ranges_layer(&[]);

	assert!(matches!(layer, Either::Right(_)));
}

#[test]
fn local_peer_ranges_layer_populated_returns_extension_branch() {
	let subnets: Vec<IpNet> =
		vec!["172.18.0.0/16".parse().expect("CIDR"), "fd00::/8".parse().expect("CIDR")];

	let layer = local_peer_ranges_layer(&subnets);

	let nets = match layer {
		| Either::Left(Extension(LocalPeerRanges(nets))) => nets,
		| Either::Right(_) => panic!("expected extension branch"),
	};

	assert_eq!(nets.len(), 2);
}

#[test]
fn html_is_framed_and_constrained_in_any_case() {
	for content_type in ["text/html", "Text/HTML", "text/html; charset=utf-8"] {
		let headers = html_headers_for(content_type);

		assert!(
			headers.contains_key(CONTENT_SECURITY_POLICY),
			"{content_type} gets a content security policy"
		);
		assert!(headers.contains_key(X_FRAME_OPTIONS), "{content_type} is denied framing");
	}
}

#[test]
fn a_parameter_mentioning_html_is_left_alone() {
	for content_type in ["application/json; x=text/html", "application/json"] {
		let headers = html_headers_for(content_type);

		assert!(!headers.contains_key(CONTENT_SECURITY_POLICY), "{content_type} is not html");
		assert!(!headers.contains_key(X_FRAME_OPTIONS), "{content_type} is not html");
	}
}

fn html_headers_for(content_type: &str) -> HeaderMap {
	let response = Response::builder()
		.header(CONTENT_TYPE, content_type)
		.body(())
		.expect("the response builds");

	set_html_headers(response).headers().clone()
}
