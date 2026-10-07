#[cfg(test)]
mod tests;

use std::{
	convert::Infallible,
	mem::replace,
	sync::Arc,
	task::{Context, Poll},
	time::Duration,
};

use axum::{
	Extension, Router,
	extract::{DefaultBodyLimit, MatchedPath, Request},
	response::{IntoResponse, Response},
};
use futures::{FutureExt, future::Map};
use http::{
	HeaderValue, Method, StatusCode,
	header::{
		self, CONTENT_SECURITY_POLICY, CONTENT_TYPE, ETAG, HeaderName, IF_MATCH, IF_NONE_MATCH,
		X_FRAME_OPTIONS,
	},
	uri::PathAndQuery,
};
use ipnet::IpNet;
use tower::{
	Layer, Service, ServiceBuilder,
	layer::util::Identity,
	util::{Either, MapResponseLayer, option_layer},
};
use tower_http::{
	catch_panic::CatchPanicLayer,
	cors::{AllowOrigin, CorsLayer},
	sensitive_headers::SetSensitiveHeadersLayer,
	set_header::SetResponseHeaderLayer,
	timeout::{RequestBodyTimeoutLayer, ResponseBodyTimeoutLayer, TimeoutLayer},
	trace::{DefaultOnFailure, DefaultOnRequest, DefaultOnResponse, TraceLayer},
};
use tracing::Level;
use tuwunel_api::router::{ConfiguredIpHeader, LocalPeerRanges, catch_panic, state::Guard};
use tuwunel_core::{
	Result, Server, config::ReverseProxyIpHeader, debug,
	utils::content_disposition::content_type_is,
};
use tuwunel_service::Services;

use crate::{request, router};

type Convert = fn(Result<Response, StatusCode>) -> Result<Response, Infallible>;

/// Bespoke `axum::middleware::from_fn`: threading the handler's future type
/// through `F` spares the boxes the generic middleware allocates per request.
#[derive(Clone)]
pub(crate) struct HandleLayer<F> {
	pub(crate) services: Arc<Services>,
	pub(crate) handler: F,
}

#[derive(Clone)]
pub(crate) struct Handle<S, F> {
	services: Arc<Services>,
	handler: F,
	inner: S,
}

const TUWUNEL_CSP: &[&str] = &[
	"default-src 'none'",
	"script-src 'self'",
	"style-src 'self'",
	"frame-ancestors 'none'",
	"form-action 'self'",
	"base-uri 'none'",
];

/// 🚨 **The layer added first is the outermost one.** `ServiceBuilder::layer`
/// stacks `Stack::new(new, previous)` and `Stack` applies its inner first, so
/// `builder.layer(A).layer(B)` becomes `A(B(service))`.
///
/// ⚠️ That makes `CatchPanicLayer` below — the **last** one added — the
/// innermost, wrapping the router and nothing else. A panic inside any layer
/// above it, `HandleLayer`'s `request::handle` included, still has nowhere to go
/// (external review 2026-09-29 / PR #109 review; the bridge's own router gets
/// the layer in `build_bridge_router`, see `/docs/design/wire/api-bridge.md`
/// §2.1). 📎 Easy to read backwards, which is why it is written down here rather
/// than left to whoever reads the chain next.
pub(crate) fn build(services: &Arc<Services>) -> Result<(Router, Guard)> {
	let server = &services.server;
	let layers = ServiceBuilder::new();

	#[cfg(feature = "sentry_telemetry")]
	let layers = layers.layer(sentry_tower::NewSentryLayer::<http::Request<_>>::new_from_top());

	#[cfg(any(
		feature = "zstd_compression",
		feature = "gzip_compression",
		feature = "brotli_compression"
	))]
	let layers = layers.layer(compression_layer(server));

	let services_ = services.clone();
	let layers = layers
		.layer(SetSensitiveHeadersLayer::new([header::AUTHORIZATION]))
		.layer(
			TraceLayer::new_for_http()
				.make_span_with(tracing_span::<_>)
				.on_failure(DefaultOnFailure::new().level(Level::ERROR))
				.on_request(DefaultOnRequest::new().level(Level::TRACE))
				.on_response(DefaultOnResponse::new().level(Level::DEBUG)),
		)
		.layer(HandleLayer {
			services: Arc::clone(services),
			handler: request::handle,
		})
		.layer(local_peer_ranges_layer(&server.config.localhost_ip))
		.layer(reverse_proxy_ip_header_layer(server.config.reverse_proxy_ip_header))
		.layer(ResponseBodyTimeoutLayer::new(Duration::from_secs(
			server.config.client_response_timeout,
		)))
		.layer(RequestBodyTimeoutLayer::new(Duration::from_secs(
			server.config.client_receive_timeout,
		)))
		.layer(TimeoutLayer::with_status_code(
			StatusCode::REQUEST_TIMEOUT,
			Duration::from_secs(server.config.client_request_timeout),
		))
		.layer(SetResponseHeaderLayer::if_not_present(
			header::X_CONTENT_TYPE_OPTIONS,
			HeaderValue::from_static("nosniff"),
		))
		.layer(html_layer())
		.layer(cors_layer(server))
		.layer(body_limit_layer(server))
		.layer(CatchPanicLayer::custom(move |panic| catch_panic(panic, services_.clone())));

	let (router, guard) = router::build(services);
	Ok((router.layer(layers), guard))
}

impl<S, F: Clone> Layer<S> for HandleLayer<F> {
	type Service = Handle<S, F>;

	fn layer(&self, inner: S) -> Self::Service {
		Handle {
			services: self.services.clone(),
			handler: self.handler.clone(),
			inner,
		}
	}
}

impl<S, F, Fut> Service<Request> for Handle<S, F>
where
	S: Service<Request, Error = Infallible> + Clone,
	F: FnMut(Arc<Services>, Request, S) -> Fut,
	Fut: Future<Output = Result<Response, StatusCode>>,
{
	type Error = Infallible;
	type Future = Map<Fut, Convert>;
	type Response = Response;

	fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
		self.inner.poll_ready(cx)
	}

	fn call(&mut self, req: Request) -> Self::Future {
		let convert: Convert = |result| Ok(result.into_response());
		let unready = self.inner.clone();
		let inner = replace(&mut self.inner, unready);

		(self.handler)(self.services.clone(), req, inner).map(convert)
	}
}

#[cfg(any(
	feature = "zstd_compression",
	feature = "gzip_compression",
	feature = "brotli_compression"
))]
fn compression_layer(server: &Server) -> tower_http::compression::CompressionLayer {
	let mut compression_layer = tower_http::compression::CompressionLayer::new();

	#[cfg(feature = "zstd_compression")]
	{
		compression_layer = if server.config.zstd_compression {
			compression_layer.zstd(true)
		} else {
			compression_layer.no_zstd()
		};
	};

	#[cfg(feature = "gzip_compression")]
	{
		compression_layer = if server.config.gzip_compression {
			compression_layer.gzip(true)
		} else {
			compression_layer.no_gzip()
		};
	};

	#[cfg(feature = "brotli_compression")]
	{
		compression_layer = if server.config.brotli_compression {
			compression_layer.br(true)
		} else {
			compression_layer.no_br()
		};
	};

	compression_layer
}

fn cors_layer(server: &Server) -> CorsLayer {
	const METHODS: [Method; 7] = [
		Method::DELETE,
		Method::GET,
		Method::HEAD,
		Method::OPTIONS,
		Method::PATCH,
		Method::POST,
		Method::PUT,
	];

	let headers: [HeaderName; 7] = [
		header::ACCEPT,
		header::AUTHORIZATION,
		CONTENT_TYPE,
		IF_MATCH,
		IF_NONE_MATCH,
		header::ORIGIN,
		HeaderName::from_static("x-requested-with"),
	];

	let allow_origin_list = server
		.config
		.access_control_allow_origin
		.iter()
		.map(AsRef::as_ref)
		.map(HeaderValue::from_str)
		.filter_map(Result::ok);

	let allow_origin = if !server
		.config
		.access_control_allow_origin
		.is_empty()
	{
		AllowOrigin::list(allow_origin_list)
	} else {
		AllowOrigin::any()
	};

	CorsLayer::new()
		.max_age(Duration::from_hours(24))
		.allow_methods(METHODS)
		.allow_headers(headers)
		.expose_headers([ETAG])
		.allow_origin(allow_origin)
}

fn body_limit_layer(server: &Server) -> DefaultBodyLimit {
	DefaultBodyLimit::max(server.config.max_request_size)
}

/// 📎 An empty `localhost_ip` installs no layer at all, which the extractor
/// reads as "trust nobody" — the same answer, one fewer extension per request.
fn local_peer_ranges_layer(ranges: &[IpNet]) -> Either<Extension<LocalPeerRanges>, Identity> {
	option_layer((!ranges.is_empty()).then(|| Extension(LocalPeerRanges(Arc::from(ranges)))))
}

fn reverse_proxy_ip_header_layer(
	header: Option<ReverseProxyIpHeader>,
) -> Either<Extension<ConfiguredIpHeader>, Identity> {
	option_layer(header.map(|header| Extension(ConfiguredIpHeader(header))))
}

fn html_layer<T>() -> MapResponseLayer<impl Fn(http::Response<T>) -> http::Response<T> + Clone> {
	MapResponseLayer::new(set_html_headers)
}

/// Denies framing and foreign scripts for a response that is HTML.
///
/// The Content-Type is echoed from whoever uploaded the media, so the media
/// type decides on its own and regardless of case: a parameter that merely
/// mentions HTML leaves a response that is not HTML alone.
fn set_html_headers<T>(mut response: http::Response<T>) -> http::Response<T> {
	let headers = response.headers_mut();

	let content_type = headers
		.get(CONTENT_TYPE)
		.map(HeaderValue::to_str)
		.and_then(Result::ok);

	if content_type_is(content_type, "text/html") {
		headers
			.entry(CONTENT_SECURITY_POLICY)
			.or_insert(HeaderValue::from_static(const_str::join!(TUWUNEL_CSP, ";")));

		headers
			.entry(X_FRAME_OPTIONS)
			.or_insert(HeaderValue::from_static("DENY"));
	}

	response
}

fn tracing_span<T>(request: &http::Request<T>) -> tracing::Span {
	let path = request
		.extensions()
		.get::<MatchedPath>()
		.map_or_else(|| request_path_str(request), truncated_matched_path);

	tracing::span! {
		parent: None,
		debug::INFO_SPAN_LEVEL,
		"router",
		method = %request.method(),
		%path,
	}
}

fn request_path_str<T>(request: &http::Request<T>) -> &str {
	request
		.uri()
		.path_and_query()
		.map(PathAndQuery::as_str)
		.unwrap_or("/")
}

fn truncated_matched_path(path: &MatchedPath) -> &str {
	path.as_str()
		.rsplit_once('{')
		.map_or(path.as_str(), |path| path.0.strip_suffix('/').unwrap_or(path.0))
}
