//! The bridge: a pack flagged `IS_BRIDGED` is a Matrix endpoint call. It is
//! turned into an internal HTTP request — kind and subtype pick the endpoint,
//! meta fills the template's variables, data is the body — and handed to the
//! same routing table the HTTP listener serves, so authentication, the gates
//! and the route itself are the HTTP road, not a copy of it
//! (`docs/design/wbf-api-bridge.md`; numbers in `docs/bridge-specs/index.md`).
//!
//! 🚨 The table below is an allowlist. A pack never carries a method or a
//! path: it names a row, and a row that is not here is `UnknownKind`. Letting
//! a pack name its own path would open every endpoint on the channel —
//! `/sync`, legacy media, and the kinds the protocol keeps off HTTP.

use std::net::{IpAddr, SocketAddr};

use axum::{
	body::{Body, to_bytes},
	extract::ConnectInfo,
};
use http::{HeaderValue, Method, Request, StatusCode, header};
use ruma::api::{
	IncomingRequest,
	client::{
		account, alias, config, context, device, membership, profile, read_marker, receipt, redact, room, state,
		tag, typing,
	},
	path_builder::PathBuilder,
};
use serde_json::{Map, Value, json};
use tower::ServiceExt;
use tuwunel_core::wbf::{Flags, IdType, Kind, PackBuilder, PackView, RejectCode};
use tuwunel_service::Services;
use url::{Url, form_urlencoded};

use super::{
	Failure, PackContext, Reject, Reply, control, matrix_error_fields, refuse_wrong_id_type, reject_code_for_status,
};

/// The path every bridged endpoint is reached by. ruma lists each endpoint's
/// paths across versions; the bridge always uses the stable v3 one.
const V3_PREFIX: &str = "/_matrix/client/v3/";

/// One Matrix endpoint reachable through the bridge: one row of
/// `docs/bridge-specs/index.md`.
pub(super) struct BridgedEndpoint {
	pub(super) kind: Kind,
	/// In `0x20`–`0x9F` (`docs/bridge-specs/index.md` §1.4).
	pub(super) subtype: u8,
	/// The name the specs index gives it, for errors and logs.
	pub(super) name: &'static str,
	/// Method and v3 path template, read from the ruma request type so that
	/// an upstream rename breaks the build instead of the endpoint.
	pub(super) shape: fn() -> Option<EndpointShape>,
	/// The query variables it takes, by ruma's names. Path variables come
	/// from the template and are not listed.
	pub(super) query: &'static [&'static str],
}

/// What `shape` reads off a ruma request type.
pub(super) struct EndpointShape {
	pub(super) method: Method,
	/// Example: `/_matrix/client/v3/rooms/{room_id}/leave`
	pub(super) path_template: &'static str,
}

/// Every bridged endpoint, in the order of `docs/bridge-specs/index.md` §2.
/// ⚠️ That table is the authority for the numbers; this one must agree with
/// it row for row. Numbers are never reused.
static BRIDGED_ENDPOINTS: &[BridgedEndpoint] = &[
	// 0x11 Account
	row(Kind::Account, 0x20, "WhoAmI", shape_of::<account::whoami::v3::Request>, NO_QUERY),
	row(Kind::Account, 0x21, "GetProfile", shape_of::<profile::get_profile::v3::Request>, NO_QUERY),
	row(Kind::Account, 0x22, "GetProfileField", shape_of::<profile::get_profile_field::v3::Request>, NO_QUERY),
	row(Kind::Account, 0x23, "SetProfileField", shape_of::<profile::set_profile_field::v3::Request>, NO_QUERY),
	row(Kind::Account, 0x24, "DeleteProfileField", shape_of::<profile::delete_profile_field::v3::Request>, NO_QUERY),
	row(Kind::Account, 0x25, "GetAccountData", shape_of::<config::get_global_account_data::v3::Request>, NO_QUERY),
	row(Kind::Account, 0x26, "SetAccountData", shape_of::<config::set_global_account_data::v3::Request>, NO_QUERY),
	row(Kind::Account, 0x27, "GetRoomAccountData", shape_of::<config::get_room_account_data::v3::Request>, NO_QUERY),
	row(Kind::Account, 0x28, "SetRoomAccountData", shape_of::<config::set_room_account_data::v3::Request>, NO_QUERY),
	row(Kind::Account, 0x29, "GetTags", shape_of::<tag::get_tags::v3::Request>, NO_QUERY),
	row(Kind::Account, 0x2A, "SetTag", shape_of::<tag::create_tag::v3::Request>, NO_QUERY),
	row(Kind::Account, 0x2B, "DeleteTag", shape_of::<tag::delete_tag::v3::Request>, NO_QUERY),
	// 0x13 Room
	row(Kind::Room, 0x20, "CreateRoom", shape_of::<room::create_room::v3::Request>, NO_QUERY),
	row(Kind::Room, 0x21, "Join", shape_of::<membership::join_room_by_id_or_alias::v3::Request>, &["via", "server_name"]),
	row(Kind::Room, 0x22, "Leave", shape_of::<membership::leave_room::v3::Request>, NO_QUERY),
	row(Kind::Room, 0x23, "Forget", shape_of::<membership::forget_room::v3::Request>, NO_QUERY),
	row(Kind::Room, 0x24, "Invite", shape_of::<membership::invite_user::v3::Request>, NO_QUERY),
	row(Kind::Room, 0x25, "Kick", shape_of::<membership::kick_user::v3::Request>, NO_QUERY),
	row(Kind::Room, 0x26, "Ban", shape_of::<membership::ban_user::v3::Request>, NO_QUERY),
	row(Kind::Room, 0x27, "Unban", shape_of::<membership::unban_user::v3::Request>, NO_QUERY),
	row(Kind::Room, 0x28, "JoinedRooms", shape_of::<membership::joined_rooms::v3::Request>, NO_QUERY),
	row(Kind::Room, 0x29, "Members", shape_of::<membership::get_member_events::v3::Request>, &["at", "membership", "not_membership"]),
	row(Kind::Room, 0x2A, "GetAlias", shape_of::<alias::get_alias::v3::Request>, NO_QUERY),
	row(Kind::Room, 0x2B, "SetAlias", shape_of::<alias::create_alias::v3::Request>, NO_QUERY),
	row(Kind::Room, 0x2C, "DeleteAlias", shape_of::<alias::delete_alias::v3::Request>, NO_QUERY),
	// 0x14 Event
	row(Kind::Event, 0x20, "GetEvent", shape_of::<room::get_room_event::v3::Request>, NO_QUERY),
	row(Kind::Event, 0x21, "GetState", shape_of::<state::get_state_events::v3::Request>, NO_QUERY),
	row(Kind::Event, 0x22, "GetStateEvent", shape_of::<state::get_state_event_for_key::v3::Request>, NO_QUERY),
	row(Kind::Event, 0x23, "SetStateEvent", shape_of::<state::send_state_event::v3::Request>, NO_QUERY),
	row(Kind::Event, 0x24, "Redact", shape_of::<redact::redact_event::v3::Request>, NO_QUERY),
	row(Kind::Event, 0x25, "Context", shape_of::<context::get_context::v3::Request>, &["limit", "filter"]),
	// 0x15 Receipt
	row(Kind::Receipt, 0x20, "Typing", shape_of::<typing::create_typing_event::v3::Request>, NO_QUERY),
	row(Kind::Receipt, 0x21, "ReadMarkers", shape_of::<read_marker::set_read_marker::v3::Request>, NO_QUERY),
	row(Kind::Receipt, 0x22, "Receipt", shape_of::<receipt::create_receipt::v3::Request>, NO_QUERY),
	// 0x16 Device
	row(Kind::Device, 0x20, "ListDevices", shape_of::<device::get_devices::v3::Request>, NO_QUERY),
	row(Kind::Device, 0x21, "GetDevice", shape_of::<device::get_device::v3::Request>, NO_QUERY),
	row(Kind::Device, 0x22, "UpdateDevice", shape_of::<device::update_device::v3::Request>, NO_QUERY),
];

const NO_QUERY: &[&str] = &[];

const fn row(
	kind: Kind,
	subtype: u8,
	name: &'static str,
	shape: fn() -> Option<EndpointShape>,
	query: &'static [&'static str],
) -> BridgedEndpoint {
	BridgedEndpoint { kind, subtype, name, shape, query }
}

/// Args:
///     Request: the ruma request type of the endpoint, example: `leave_room::v3::Request`
/// Return:
///     Option<EndpointShape>  None when the type has no v3 path.
fn shape_of<Request: IncomingRequest>() -> Option<EndpointShape> {
	let path_template = Request::PATH_BUILDER
		.all_paths()
		.find(|path| path.starts_with(V3_PREFIX))?;

	Some(EndpointShape { method: Request::METHOD, path_template })
}

/// Args:
///     kind: example: Kind::Room
///     subtype: example: 0x22
/// Return:
///     Option<&BridgedEndpoint>  None when no row has this pair.
fn find_bridged_endpoint(kind: Kind, subtype: u8) -> Option<&'static BridgedEndpoint> {
	BRIDGED_ENDPOINTS
		.iter()
		.find(|endpoint| endpoint.kind == kind && endpoint.subtype == subtype)
}

/// Runs one bridge call and sends its one reply.
///
/// Args:
///     ctx: the session whose token the internal request carries, the peer
///         address, and the router to hand the request to
///     view: a pack with `IS_BRIDGED` set
///     reply: where the `Ack` or `Error` goes
/// Return:
///     Result<(), Failure>  Ok once the reply is queued; a Reject for a pack
///     that names no endpoint (`UnknownKind`), whose variables do not fit it
///     (`InvalidRequest`), or whose response does not fit a pack (`TooLarge`).
///     A Matrix error from the endpoint is not a Reject: it is the reply.
pub(super) async fn handle(
	services: &Services,
	ctx: &PackContext<'_>,
	view: &PackView<'_>,
	reply: &mut Reply,
) -> Result<(), Failure> {
	let header = view.header;
	// The split in `dispatch` already sent only bridge calls here; this is
	// the road asking again (docs/design/wbf-api-bridge.md §2.3).
	if !header.flags.is_bridged() {
		return Err(Reject::code(RejectCode::Unsupported, "a native pack reached the bridge").into());
	}
	let endpoint = find_bridged_endpoint(header.kind, header.subtype)
		.ok_or_else(|| Reject::code(RejectCode::UnknownKind, "no bridged endpoint for this kind and subtype"))?;
	// One request, one reply, paired by seq: no conversation to name.
	refuse_wrong_id_type(header.id, IdType::None)?;
	if header.flags.is_meta_encrypted() {
		return Err(Reject::code(
			RejectCode::InvalidRequest,
			"a bridge call's meta is the endpoint's variables in plain JSON; META_ENCRYPTED (flags bit0) must be unset",
		)
		.into());
	}

	let router = ctx
		.bridge
		.ok_or_else(|| Reject::code(RejectCode::Internal, "this server was built without the bridge"))?;
	let shape = (endpoint.shape)().ok_or_else(|| {
		Reject::code(RejectCode::Internal, format!("{} has no v3 path to bridge to", endpoint.name))
	})?;

	let token = ctx.session.map(|session| session.token.as_str());
	let request = build_request(&shape, endpoint.query, view.meta, view.data, token, ctx.client)?;
	let response = router
		.0
		.clone()
		.oneshot(request)
		.await
		.unwrap_or_else(|never| match never {});

	let status = response.status();
	let content_type = response.headers().get(header::CONTENT_TYPE).cloned();
	let body = to_bytes(response.into_body(), services.config.wbf_data_max_bytes)
		.await
		.map_err(|_| {
			Reject::code(
				RejectCode::TooLarge,
				format!("{}'s response does not fit one pack (wbf_data_max_bytes)", endpoint.name),
			)
		})?;

	reply
		.send(build_reply_pack(header.id, header.seq, status, content_type.as_ref(), &body)?)
		.await?;

	Ok(())
}

/// Turns a bridge call into the HTTP request the router would have received.
///
/// Args:
///     shape: example: POST `/_matrix/client/v3/rooms/{room_id}/leave`
///     query_names: the endpoint's declared query variables, example: `["via"]`
///     meta: the variables as a JSON object, example: `{"room_id":"!r:localhost"}`;
///         empty means no variables
///     data: the body's bytes, example: `{"reason":"bye"}`
///     token: the session's access token; None on a connection not logged in
///     client: the peer address the router's throttles see
/// Return:
///     Result<Request<Body>, Reject>  `InvalidRequest` when meta is not an
///     object, names a variable the endpoint does not take, or leaves a path
///     variable missing or not a string, or gives a query variable a value
///     that is not a string, number, boolean or array of those.
fn build_request(
	shape: &EndpointShape,
	query_names: &[&str],
	meta: &[u8],
	data: &[u8],
	token: Option<&str>,
	client: IpAddr,
) -> Result<Request<Body>, Reject> {
	let variables = parse_variables(meta)?;
	let path_names = list_path_variables(shape.path_template);
	refuse_undeclared_variables(&variables, &path_names, query_names)?;

	let path = fill_path(shape.path_template, &variables)?;
	let query = fill_query(query_names, &variables)?;
	let uri = if query.is_empty() { path } else { format!("{path}?{query}") };

	let mut builder = Request::builder()
		.method(shape.method.clone())
		.uri(uri)
		.extension(ConnectInfo(SocketAddr::new(client, 0)));
	// 🚨 Only the headers the bridge itself decides. Nothing a client sends
	// becomes a header: an `X-Forwarded-For` would move the client past the
	// throttles, and `Authorization` is the session's, not the pack's.
	if let Some(token) = token {
		builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
	}
	if !data.is_empty() {
		builder = builder.header(header::CONTENT_TYPE, "application/json");
	}

	builder
		.body(Body::from(data.to_vec()))
		.map_err(|error| Reject::code(RejectCode::Internal, format!("could not frame the internal request: {error}")))
}

/// Args:
///     meta: example: `{"room_id":"!r:localhost"}`; empty is no variables
/// Return:
///     Result<Map<String, Value>, Reject>  `InvalidRequest` when meta is not
///     a JSON object.
fn parse_variables(meta: &[u8]) -> Result<Map<String, Value>, Reject> {
	if meta.is_empty() {
		return Ok(Map::new());
	}

	match serde_json::from_slice::<Value>(meta) {
		| Ok(Value::Object(variables)) => Ok(variables),
		| _ => Err(Reject::code(
			RejectCode::InvalidRequest,
			"a bridge call's meta is a JSON object of the endpoint's variables",
		)),
	}
}

/// Args:
///     path_template: example: `/_matrix/client/v3/rooms/{room_id}/state/{event_type}/{state_key}`
/// Return:
///     Vec<&str>  the variable names in order, example: `["room_id", "event_type", "state_key"]`;
///     empty for a template with none.
fn list_path_variables(path_template: &str) -> Vec<&str> {
	path_template
		.split('/')
		.filter_map(find_segment_variable)
		.collect()
}

/// Args:
///     segment: one path segment, example: `{room_id}` or `rooms`
/// Return:
///     Option<&str>  the variable's name, example: Some("room_id"); None for a
///     literal segment.
fn find_segment_variable(segment: &str) -> Option<&str> {
	segment
		.strip_prefix('{')
		.and_then(|rest| rest.strip_suffix('}'))
}

/// 🚨 Refused, not ignored: ignoring turns a client's misspelt variable into
/// "the server quietly used the default".
fn refuse_undeclared_variables(
	variables: &Map<String, Value>,
	path_names: &[&str],
	query_names: &[&str],
) -> Result<(), Reject> {
	let undeclared: Vec<&str> = variables
		.keys()
		.map(String::as_str)
		.filter(|name| !path_names.contains(name) && !query_names.contains(name))
		.collect();
	if undeclared.is_empty() {
		return Ok(());
	}

	Err(Reject::code(
		RejectCode::InvalidRequest,
		format!("this endpoint takes no variable named {}", undeclared.join(", ")),
	))
}

/// Args:
///     path_template: example: `/_matrix/client/v3/directory/room/{room_alias}`
///     variables: example: `{"room_alias":"#lobby:localhost"}`
/// Return:
///     Result<String, Reject>  the percent-encoded path, example:
///     `/_matrix/client/v3/directory/room/%23lobby:localhost`. An empty string
///     is a value (a `state_key` of `""` leaves a trailing `/`); a variable
///     that is missing or not a string is `InvalidRequest`.
fn fill_path(path_template: &str, variables: &Map<String, Value>) -> Result<String, Reject> {
	let mut url = Url::parse("http://bridge.invalid/").expect("a constant URL parses");
	{
		let mut segments = url
			.path_segments_mut()
			.expect("an http URL has path segments");
		segments.clear();
		for segment in path_template.trim_start_matches('/').split('/') {
			let Some(name) = find_segment_variable(segment) else {
				segments.push(segment);
				continue;
			};
			match variables.get(name) {
				| Some(Value::String(value)) => {
					segments.push(value);
				},
				| Some(_) => {
					return Err(Reject::code(RejectCode::InvalidRequest, format!("`{name}` must be a string")));
				},
				| None => {
					return Err(Reject::code(RejectCode::InvalidRequest, format!("`{name}` is missing")));
				},
			}
		}
	}

	Ok(url.path().to_owned())
}

/// Args:
///     query_names: example: `["via", "server_name"]`
///     variables: example: `{"via":["a.org","b.org"]}`
/// Return:
///     Result<String, Reject>  the query string without its `?`, example:
///     `via=a.org&via=b.org`; empty when no query variable was given. A query
///     variable that is absent is left out entirely, never sent empty.
fn fill_query(query_names: &[&str], variables: &Map<String, Value>) -> Result<String, Reject> {
	let mut query = form_urlencoded::Serializer::new(String::new());
	let mut is_empty = true;
	for name in query_names {
		let values: Vec<&Value> = match variables.get(*name) {
			| None => continue,
			| Some(Value::Array(items)) => items.iter().collect(),
			| Some(value) => vec![value],
		};
		for value in values {
			query.append_pair(name, &to_query_text(name, value)?);
			is_empty = false;
		}
	}

	Ok(if is_empty { String::new() } else { query.finish() })
}

/// Args:
///     name: for the error, example: "limit"
///     value: example: 10
/// Return:
///     Result<String, Reject>  example: "10"; `InvalidRequest` for null,
///     objects and nested arrays.
fn to_query_text(name: &str, value: &Value) -> Result<String, Reject> {
	match value {
		| Value::String(text) => Ok(text.clone()),
		| Value::Number(number) => Ok(number.to_string()),
		| Value::Bool(flag) => Ok(flag.to_string()),
		| _ => Err(Reject::code(
			RejectCode::InvalidRequest,
			format!("`{name}` must be a string, number, boolean or an array of those"),
		)),
	}
}

/// The one reply to a bridge call: `Ack` for a 2xx, `Error` otherwise. Both
/// carry the endpoint's body in data, byte for byte, and `IS_BRIDGED`.
///
/// Args:
///     id, seq: copied from the request
///     status: example: StatusCode::FORBIDDEN
///     content_type: the response's `Content-Type`, the only header forwarded
///     body: example: `{"errcode":"M_FORBIDDEN","error":"You are not invited"}`
/// Return:
///     Result<Vec<u8>, Reject>  the pack. An `Ack`'s meta is `{status,
///     headers}`; an `Error`'s is `{code_id, code, message}` plus
///     `matrix_error_fields` (`status`, and `errcode`, `retry_after_ms`,
///     `soft_logout` when the body has them).
fn build_reply_pack(
	id: u64,
	seq: u32,
	status: StatusCode,
	content_type: Option<&HeaderValue>,
	body: &[u8],
) -> Result<Vec<u8>, Reject> {
	let flags = Flags::IS_RESPONSE.union(Flags::IS_BRIDGED);

	if status.is_success() {
		let mut headers = Map::new();
		if let Some(content_type) = content_type.and_then(|value| value.to_str().ok()) {
			headers.insert("content-type".into(), Value::String(content_type.to_owned()));
		}
		let meta = json!({ "status": status.as_u16(), "headers": headers });

		return Ok(PackBuilder::new(Kind::Control, control::ACK, flags, id, seq)
			.json_meta(&meta)?
			.data(body)?
			.finish());
	}

	let code = reject_code_for_status(status);
	let message = serde_json::from_slice::<Value>(body)
		.ok()
		.and_then(|error| error.get("error").and_then(Value::as_str).map(to_bounded_message))
		.unwrap_or_else(|| format!("the endpoint answered {status}"));
	let mut meta = Map::new();
	meta.insert("code_id".into(), json!(code.id()));
	meta.insert("code".into(), json!(code.name()));
	meta.insert("message".into(), Value::String(message));
	meta.extend(matrix_error_fields(status, body));

	Ok(PackBuilder::new(Kind::Control, control::ERROR, flags, id, seq)
		.json_meta(&Value::Object(meta))?
		.data(body)?
		.finish())
}

/// The most of a Matrix error's text an `Error` pack's `message` carries. The
/// whole body is still in data; this keeps the meta inside its limit however
/// long the text, so the reply never falls back to `Internal` for its size.
const MAX_MESSAGE_BYTES: usize = 1024;

/// Args:
///     text: example: "You don't have permission to post that to the room."
/// Return:
///     String  the text unchanged when it fits in `MAX_MESSAGE_BYTES`;
///     otherwise cut at the last character boundary that fits, with "…"
fn to_bounded_message(text: &str) -> String {
	if text.len() <= MAX_MESSAGE_BYTES {
		return text.to_owned();
	}

	let ellipsis = "…";
	let mut end = MAX_MESSAGE_BYTES - ellipsis.len();
	while !text.is_char_boundary(end) {
		end -= 1;
	}

	format!("{}{ellipsis}", &text[..end])
}

#[cfg(test)]
mod tests {
	use std::net::{IpAddr, Ipv4Addr};

	use axum::extract::ConnectInfo;
	use http::{HeaderValue, Method, StatusCode, header};
	use ruma::api::client::{membership::leave_room, state::send_state_event};
	use tuwunel_core::wbf::{RejectCode, decode};

	use super::{
		BRIDGED_ENDPOINTS, EndpointShape, MAX_MESSAGE_BYTES, build_reply_pack, build_request, list_path_variables,
		shape_of, to_bounded_message,
	};

	const PEER: IpAddr = IpAddr::V4(Ipv4Addr::new(203, 0, 113, 7));

	fn state_shape() -> EndpointShape {
		shape_of::<send_state_event::v3::Request>().expect("send_state_event has a v3 path")
	}

	fn refused_code(result: Result<http::Request<axum::body::Body>, super::Reject>) -> RejectCode {
		result.err().expect("refused").code
	}

	#[test]
	fn the_shape_comes_from_the_ruma_type() {
		let shape = shape_of::<leave_room::v3::Request>().expect("leave has a v3 path");
		assert_eq!(shape.method, Method::POST);
		assert_eq!(shape.path_template, "/_matrix/client/v3/rooms/{room_id}/leave");
		assert_eq!(list_path_variables(shape.path_template), vec!["room_id"]);
	}

	#[test]
	fn path_variables_are_filled_and_percent_encoded() {
		let shape = state_shape();
		let request = build_request(
			&shape,
			&[],
			br##"{"room_id":"!abc:localhost","event_type":"m.room.topic","state_key":"#weird/key"}"##,
			br#"{"topic":"hi"}"#,
			Some("TOKEN"),
			PEER,
		)
		.expect("builds");

		assert_eq!(request.method(), Method::PUT);
		assert_eq!(
			request.uri().path(),
			"/_matrix/client/v3/rooms/!abc:localhost/state/m.room.topic/%23weird%2Fkey",
			"`#` and `/` in a value cannot cut the path"
		);
	}

	#[test]
	fn an_empty_state_key_is_a_value_and_leaves_a_trailing_slash() {
		let request = build_request(
			&state_shape(),
			&[],
			br#"{"room_id":"!abc:localhost","event_type":"m.room.topic","state_key":""}"#,
			b"{}",
			Some("TOKEN"),
			PEER,
		)
		.expect("an empty string is a value, not a missing variable");

		assert_eq!(request.uri().path(), "/_matrix/client/v3/rooms/!abc:localhost/state/m.room.topic/");
	}

	#[test]
	fn a_path_variable_missing_or_not_a_string_is_refused() {
		let missing = build_request(&state_shape(), &[], br#"{"room_id":"!abc:localhost","event_type":"m.room.topic"}"#, b"", None, PEER);
		assert_eq!(refused_code(missing), RejectCode::InvalidRequest);

		let number = build_request(&state_shape(), &[], br#"{"room_id":"!abc:localhost","event_type":"m.room.topic","state_key":7}"#, b"", None, PEER);
		assert_eq!(refused_code(number), RejectCode::InvalidRequest);

		let null = build_request(&state_shape(), &[], br#"{"room_id":null,"event_type":"m.room.topic","state_key":""}"#, b"", None, PEER);
		assert_eq!(refused_code(null), RejectCode::InvalidRequest, "null is not a value");
	}

	#[test]
	fn a_variable_the_endpoint_does_not_take_is_refused_not_ignored() {
		let result = build_request(
			&shape_of::<leave_room::v3::Request>().expect("v3"),
			&[],
			br#"{"room_id":"!abc:localhost","roomId":"!typo:localhost"}"#,
			b"{}",
			None,
			PEER,
		);
		assert_eq!(refused_code(result), RejectCode::InvalidRequest);
	}

	#[test]
	fn meta_that_is_not_an_object_is_refused_and_empty_meta_is_no_variables() {
		let whoami = EndpointShape { method: Method::GET, path_template: "/_matrix/client/v3/account/whoami" };
		assert_eq!(refused_code(build_request(&whoami, &[], b"[1,2]", b"", None, PEER)), RejectCode::InvalidRequest);
		assert!(build_request(&whoami, &[], b"", b"", None, PEER).is_ok());
		assert!(build_request(&whoami, &[], b"{}", b"", None, PEER).is_ok());
	}

	#[test]
	fn query_variables_are_left_out_when_absent_and_repeated_for_arrays() {
		let join = EndpointShape { method: Method::POST, path_template: "/_matrix/client/v3/join/{room_id_or_alias}" };
		let names = ["via", "server_name"];

		let without = build_request(&join, &names, br##"{"room_id_or_alias":"#lobby:localhost"}"##, b"{}", None, PEER).expect("builds");
		assert_eq!(without.uri().query(), None, "no `?`, no empty parameters");
		assert_eq!(without.uri().path(), "/_matrix/client/v3/join/%23lobby:localhost");

		let with = build_request(&join, &names, br#"{"room_id_or_alias":"!r:localhost","via":["a.org","b.org"]}"#, b"{}", None, PEER).expect("builds");
		assert_eq!(with.uri().query(), Some("via=a.org&via=b.org"));

		let object = build_request(&join, &names, br#"{"room_id_or_alias":"!r:localhost","via":{"a":1}}"#, b"{}", None, PEER);
		assert_eq!(refused_code(object), RejectCode::InvalidRequest);
	}

	#[test]
	fn the_bridge_sets_its_own_headers_and_the_peer_address() {
		let shape = state_shape();
		let variables = br#"{"room_id":"!abc:localhost","event_type":"m.room.topic","state_key":""}"#;

		let logged_in = build_request(&shape, &[], variables, br#"{"topic":"hi"}"#, Some("TOKEN"), PEER).expect("builds");
		assert_eq!(logged_in.headers().get(header::AUTHORIZATION).expect("bearer"), "Bearer TOKEN");
		assert_eq!(logged_in.headers().get(header::CONTENT_TYPE).expect("json"), "application/json");
		let ConnectInfo(peer) = logged_in.extensions().get::<ConnectInfo<std::net::SocketAddr>>().expect("peer");
		assert_eq!(peer.ip(), PEER, "the router's throttles see the connection's peer");

		let anonymous = build_request(&shape, &[], variables, b"", None, PEER).expect("builds");
		assert!(anonymous.headers().get(header::AUTHORIZATION).is_none(), "no session, no token");
		assert!(anonymous.headers().get(header::CONTENT_TYPE).is_none(), "no body, no content type");
	}

	#[test]
	fn a_success_is_an_ack_with_the_body_byte_for_byte() {
		let body = br#"{"user_id":"@a:localhost"}"#;
		let mut pack = build_reply_pack(0, 9, StatusCode::OK, Some(&HeaderValue::from_static("application/json")), body).expect("builds");

		assert_eq!(pack[..4], [0x01, 0x01, 0x02, 0x14], "Control/Ack with IS_RESPONSE and IS_BRIDGED");
		let view = decode(&mut pack).expect("decodes");
		assert_eq!(view.header.seq, 9);
		let meta = view.meta_json().expect("meta");
		assert_eq!(meta["status"], 200);
		assert_eq!(meta["headers"]["content-type"], "application/json");
		assert_eq!(view.data, body);
	}

	#[test]
	fn a_matrix_error_is_an_error_carrying_its_errcode_and_its_body() {
		let body = br#"{"errcode":"M_LIMIT_EXCEEDED","error":"Too many requests","retry_after_ms":1500}"#;
		let mut pack = build_reply_pack(0, 3, StatusCode::TOO_MANY_REQUESTS, None, body).expect("builds");

		assert_eq!(pack[..4], [0x01, 0x01, 0x03, 0x14], "Control/Error with IS_RESPONSE and IS_BRIDGED");
		let view = decode(&mut pack).expect("decodes");
		let meta = view.meta_json().expect("meta");
		assert_eq!(meta["code"], "RateLimited");
		assert_eq!(meta["status"], 429);
		assert_eq!(meta["errcode"], "M_LIMIT_EXCEEDED");
		assert_eq!(meta["retry_after_ms"], 1500);
		assert_eq!(meta["message"], "Too many requests");
		assert_eq!(view.data, body, "the whole Matrix error, so UIAA's flows reach the client too");
	}

	#[test]
	fn a_body_that_is_not_a_matrix_error_leaves_errcode_absent() {
		let mut pack = build_reply_pack(0, 1, StatusCode::INTERNAL_SERVER_ERROR, None, b"boom").expect("builds");
		let view = decode(&mut pack).expect("decodes");
		let meta = view.meta_json().expect("meta");
		assert_eq!(meta["code"], "Internal");
		assert_eq!(meta.get("errcode"), None, "absent, not an empty string");
	}

	#[test]
	fn a_long_matrix_error_is_cut_in_the_message_and_whole_in_the_data() {
		// Three bytes a character, so a cut at a byte count lands mid-character
		// unless it looks for the boundary; longer than the whole meta limit.
		let text = "界".repeat(30_000);
		let body = serde_json::to_vec(&serde_json::json!({ "errcode": "M_FORBIDDEN", "error": text })).expect("json");
		let mut pack = build_reply_pack(0, 9, StatusCode::FORBIDDEN, None, &body).expect("builds");

		let view = decode(&mut pack).expect("decodes");
		let meta = view.meta_json().expect("meta");
		assert_eq!(meta["code"], "Forbidden", "not the Internal fallback for an oversized meta");
		assert_eq!(meta["errcode"], "M_FORBIDDEN");
		let message = meta["message"].as_str().expect("a message");
		assert!(message.len() <= MAX_MESSAGE_BYTES, "message is {} bytes", message.len());
		assert!(message.starts_with("界界界") && message.ends_with('…'), "cut on a character, marked as cut");
		assert_eq!(view.data, body.as_slice(), "the data still carries the whole Matrix error");

		assert_eq!(to_bounded_message("short"), "short", "text that fits is untouched");
	}

	/// The golden vectors are what clients test their decoders against, so
	/// the two bridge replies in them must be exactly what this server sends —
	/// not a hand-written example that happens to decode.
	#[test]
	fn the_bridge_reply_vectors_are_what_the_server_builds() {
		const VECTORS: &str = include_str!("../../../../docs/design/wbf-vectors.json");
		let vectors: serde_json::Value = serde_json::from_str(VECTORS).expect("the vectors file is JSON");
		let bytes_of = |name: &str| -> Vec<u8> {
			let hex = vectors["packs"]
				.as_array()
				.expect("a packs list")
				.iter()
				.find(|vector| vector["name"] == name)
				.unwrap_or_else(|| panic!("no vector named {name}"))["bytes_hex"]
				.as_str()
				.expect("hex")
				.to_owned();
			(0..hex.len())
				.step_by(2)
				.map(|at| u8::from_str_radix(&hex[at..at + 2], 16).expect("hex digit"))
				.collect()
		};

		let ack = build_reply_pack(0, 50, StatusCode::OK, Some(&HeaderValue::from_static("application/json")), br#"{"event_id":"$t0p1c:localhost"}"#)
			.expect("builds");
		assert_eq!(ack, bytes_of("bridge_ack"));

		let forbidden = build_reply_pack(
			0,
			51,
			StatusCode::FORBIDDEN,
			None,
			br#"{"errcode":"M_FORBIDDEN","error":"You don't have permission to post that to the room."}"#,
		)
		.expect("builds");
		assert_eq!(forbidden, bytes_of("bridge_error_forbidden"));
	}

	/// 🚨 The specs index is the authority for bridged numbers, and this table
	/// repeats them — two lists of the same facts drift, and nobody is told.
	/// So the index is read here and every row is compared: kind, subtype, the
	/// first four bytes it prints, the name, and the endpoint against the ruma
	/// type's method and path. A failure here is the documentation and the
	/// server disagreeing.
	#[test]
	fn the_specs_index_and_this_table_list_the_same_endpoints() {
		const INDEX: &str = include_str!("../../../../docs/bridge-specs/index.md");

		let mut kind_byte: Option<u8> = None;
		let mut documented = Vec::new();
		for line in INDEX.lines() {
			if let Some(heading) = line.strip_prefix("### `0x") {
				kind_byte = u8::from_str_radix(&heading[..2], 16).ok();
				continue;
			}
			let Some(rest) = line.strip_prefix("| `0x") else {
				continue;
			};
			let cells: Vec<&str> = rest.split(" | ").map(str::trim).collect();
			let subtype = u8::from_str_radix(&cells[0][..2], 16).expect("a subtype cell");
			let kind = kind_byte.expect("a row under a kind heading");
			documented.push((kind, subtype, cells[1].trim_matches('`').to_owned(), cells[2].to_owned(), cells[4].to_owned()));
		}

		assert_eq!(documented.len(), BRIDGED_ENDPOINTS.len(), "the index and the table have different row counts");
		for (endpoint, (kind, subtype, first_bytes, name, endpoint_cell)) in BRIDGED_ENDPOINTS.iter().zip(&documented) {
			assert_eq!((endpoint.kind as u8, endpoint.subtype), (*kind, *subtype), "row order or numbers differ at {}", endpoint.name);
			assert_eq!(*first_bytes, format!("01 {:02X} {:02X} 10", *kind, *subtype), "{} prints the wrong first bytes", endpoint.name);
			assert_eq!(endpoint.name, name, "the index names 0x{kind:02X}/0x{subtype:02X} differently");

			let shape = (endpoint.shape)().expect("a v3 path");
			let expected = format!("`{} {}`", shape.method, shape.path_template.trim_start_matches("/_matrix/client/v3"));
			assert_eq!(*endpoint_cell, expected, "the index gives {} the wrong endpoint", endpoint.name);
		}
	}

	#[test]
	fn the_table_keeps_its_numbers_in_the_bridge_range_and_off_the_native_table() {
		let mut seen = Vec::new();
		for endpoint in BRIDGED_ENDPOINTS {
			assert!((0x20..=0x9F).contains(&endpoint.subtype), "{} is outside 0x20-0x9F", endpoint.name);
			assert!(
				super::super::admission(endpoint.kind, endpoint.subtype).is_none(),
				"{} shares its number with a native handler",
				endpoint.name
			);
			assert!(!seen.contains(&(endpoint.kind as u8, endpoint.subtype)), "{} is listed twice", endpoint.name);
			seen.push((endpoint.kind as u8, endpoint.subtype));

			let shape = (endpoint.shape)().unwrap_or_else(|| panic!("{} has no v3 path", endpoint.name));
			let path_names = list_path_variables(shape.path_template);
			for query_name in endpoint.query {
				assert!(!path_names.contains(query_name), "{}: `{query_name}` is both a path and a query variable", endpoint.name);
			}
		}
	}
}
