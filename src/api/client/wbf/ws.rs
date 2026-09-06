//! `GET /_wbf/v1/ws`: the WebSocket channel, one pack per binary message.
//!
//! The connection is authenticated at the upgrade and again before every
//! pack (`revalidate`): a token that was logged out, revoked, expired or
//! whose account was locked stops working at the next message, not never.
//! Each binary message is decoded and handed to the same `handle_pack` the
//! HTTP transport uses, and its reply goes back as one binary message. Many
//! uploads may interleave on one connection; the pack header's `id` tells
//! them apart.
//!
//! The task serving a socket outlives the request that upgraded it, so it is
//! spawned through `services.connections`, which `Services::stop` joins:
//! `State` is a raw pointer to `Services` and must not be dereferenced after
//! shutdown. The loop itself ends when the server starts stopping.
//!
//! The connection keeps no upload state of its own. The database row is the
//! only truth about where an upload stands, so a chunk that arrives out of
//! order is refused by the upload service, from that row, whether it came
//! over this connection, another one, or HTTP. (A first version kept a
//! per-connection `id -> next chunk` table as a shortcut; review showed it
//! could disagree with the row after an idempotent resend or a chunk sent
//! over another transport, and refuse chunks the row would take.)

use std::time::Duration;

use axum::{
	extract::{
		State,
		ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade, close_code},
	},
	http::{HeaderMap, StatusCode},
	response::Response,
};
use futures::{SinkExt, StreamExt};
use tuwunel_core::{
	debug,
	wbf::{HEADER_LEN, PackError, decode},
};

use super::{
	Session, authenticate, error_pack, handle_pack, header_id_seq, pack_error_code, pack_response,
	revalidate,
};

/// Bytes a pack carries besides its header, meta and data: `meta_len`,
/// `meta_crc`, `data_len`, `data_crc` (4 each, 16 in all), doubled to leave
/// room for the WebSocket layer's own framing on top.
const FRAME_SLACK: usize = 2 * 4 * 4;

/// # `GET /_wbf/v1/ws`
///
/// Bearer token at the upgrade, as for every other client endpoint; a bad
/// token answers 401 with an `Error` pack in the body, before any upgrade.
/// A server that is shutting down answers 503 the same way.
pub(crate) async fn ws_route(
	State(services): State<crate::State>,
	headers: HeaderMap,
	upgrade: WebSocketUpgrade,
) -> Response {
	// Checked before the token lookup: a stopping server owes nobody a
	// database read.
	if !services.server.is_running() {
		let reply = error_pack(0, 0, "Internal", "server is shutting down");
		return pack_response(StatusCode::SERVICE_UNAVAILABLE, reply);
	}

	let session = match authenticate(&services, &headers).await {
		| Ok(session) => session,
		| Err(error) => {
			let reply = error_pack(0, 0, "Unauthorized", &error.to_string());
			return pack_response(StatusCode::UNAUTHORIZED, reply);
		},
	};

	// One pack is at most the configured meta and data plus the frame around
	// them; anything larger is refused by the WebSocket layer before it is
	// buffered in full. A pack is one message in one frame, so the frame
	// limit is the message limit: change one, change both.
	let max_message = services
		.config
		.wbf_meta_max_bytes
		.saturating_add(services.config.wbf_data_max_bytes)
		.saturating_add(HEADER_LEN)
		.saturating_add(FRAME_SLACK);

	upgrade
		.max_message_size(max_message)
		.max_frame_size(max_message)
		.on_upgrade(async move |socket| {
			// axum runs this callback on its own task, which ends at once;
			// the serving loop runs on a tracked task instead. Refused only
			// when shutdown began between the check above and here: the
			// socket is then dropped, which closes it.
			if !services
				.connections
				.spawn(serve(services, session, socket))
			{
				debug!("wbf WebSocket refused: server is shutting down");
			}
		})
}

/// Runs one connection to its end: read a message, check the session, answer
/// the message, repeat.
///
/// Messages are handled one at a time in arrival order, which is what keeps
/// the ordered kinds ordered; a client that wants more in flight sends more
/// without waiting for acks, and gets the acks back in the same order. The
/// loop ends when the client closes, when the connection stays silent for
/// `wbf_ws_idle_timeout`, when the session no longer checks out, or when the
/// server starts shutting down.
async fn serve(services: crate::State, session: Session, socket: WebSocket) {
	let idle_timeout = Duration::from_secs(services.config.wbf_ws_idle_timeout);
	let server = services.server.clone();
	let user = &session.user;
	let (mut sink, mut stream) = socket.split();

	// One subscription for the life of the connection, polled from every
	// turn of the loop, rather than a fresh one per message.
	let shutdown = server.until_shutdown();
	futures::pin_mut!(shutdown);

	loop {
		let message = tokio::select! {
			next = tokio::time::timeout(idle_timeout, stream.next()) => match next {
				| Ok(Some(message)) => message,
				| Ok(None) => break,
				| Err(_elapsed) => {
					debug!(%user, "wbf WebSocket connection idle; closing");
					break;
				},
			},
			() = &mut shutdown => {
				debug!(%user, "wbf WebSocket connection closing: server shutting down");
				// 1001 (going away) rather than 1012 (service restart): the
				// former is in RFC 6455 itself and every client library
				// accepts it; .NET's ClientWebSocket, for one, treats 1012 as
				// a protocol error and drops the connection instead.
				let frame = CloseFrame { code: close_code::AWAY, reason: "server shutting down".into() };
				let _closing = sink.send(Message::Close(Some(frame))).await;
				break;
			},
		};

		let mut bytes = match message {
			| Ok(Message::Binary(bytes)) => bytes.to_vec(),
			| Ok(Message::Close(_)) | Err(_) => break,
			// Control frames are answered by the WebSocket layer itself.
			| Ok(Message::Ping(_) | Message::Pong(_)) => continue,
			| Ok(Message::Text(_)) => {
				let reply = error_pack(0, 0, "Corrupt", "text frames are not packs; send one pack per binary frame");
				if sink.send(Message::Binary(reply.into())).await.is_err() {
					break;
				}
				continue;
			},
		};

		// The token was good at the upgrade; is it still? Asked before the
		// bytes are even decoded, so a session that is gone cannot keep the
		// connection alive by sending malformed packs either. One point read
		// per message, the same order of cost as the message itself. Not
		// cached by time: a cache would be one more copy of the truth that
		// can go stale.
		if let Err(error) = revalidate(&services, &session).await {
			debug!(%user, ?error, "wbf WebSocket session no longer valid; closing");
			// Header fields read without any CRC check: they only address the
			// refusal, they decide nothing.
			let (id, seq) = header_id_seq(&bytes);
			let reply = error_pack(id, seq, "Unauthorized", &error.to_string());
			let _refused = sink.send(Message::Binary(reply.into())).await;
			let frame = CloseFrame { code: close_code::POLICY, reason: "session no longer valid".into() };
			let _closing = sink.send(Message::Close(Some(frame))).await;
			break;
		}

		let view = match decode(&mut bytes) {
			| Ok(view) => view,
			| Err(error) => {
				debug!(?error, "Rejected pack on the WebSocket channel");
				let (id, seq) = match error {
					| PackError::DataCrc { .. } => header_id_seq(&bytes),
					| _ => (0, 0),
				};
				let reply = error_pack(id, seq, pack_error_code(error), &error.to_string());
				if sink.send(Message::Binary(reply.into())).await.is_err() {
					break;
				}
				continue;
			},
		};

		let reply = handle_pack(&services, user, view).await;
		if sink.send(Message::Binary(reply.into())).await.is_err() {
			break;
		}
	}

	let _closed = sink.close().await;
	debug!(%user, "wbf WebSocket connection closed");
}
