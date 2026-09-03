//! `GET /_wbf/v1/ws`: the WebSocket channel, one pack per binary message.
//!
//! The connection is authenticated once at the upgrade; after that every
//! binary message is decoded and handed to the same `handle_pack` the HTTP
//! transport uses, and its reply goes back as one binary message. Many
//! uploads may interleave on one connection; the pack header's `id` tells
//! them apart.
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
		ws::{Message, WebSocket, WebSocketUpgrade},
	},
	http::{HeaderMap, StatusCode},
	response::Response,
};
use futures::{SinkExt, StreamExt};
use ruma::OwnedUserId;
use tuwunel_core::{
	debug,
	wbf::{HEADER_LEN, PackError, decode},
};

use super::{authenticate, error_pack, handle_pack, header_id_seq, pack_error_code, pack_response};

/// Bytes a pack carries besides its header, meta and data: `meta_len`,
/// `meta_crc`, `data_len`, `data_crc` (4 each, 16 in all), doubled to leave
/// room for the WebSocket layer's own framing on top.
const FRAME_SLACK: usize = 2 * 4 * 4;

/// # `GET /_wbf/v1/ws`
///
/// Bearer token at the upgrade, as for every other client endpoint; a bad
/// token answers 401 with an `Error` pack in the body, before any upgrade.
pub(crate) async fn ws_route(
	State(services): State<crate::State>,
	headers: HeaderMap,
	upgrade: WebSocketUpgrade,
) -> Response {
	let user = match authenticate(&services, &headers).await {
		| Ok(user) => user,
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
		.on_upgrade(move |socket| serve(services, user, socket))
}

/// Runs one connection to its end: read a message, answer it, repeat.
///
/// Messages are handled one at a time in arrival order, which is what keeps
/// the ordered kinds ordered; a client that wants more in flight sends more
/// without waiting for acks, and gets the acks back in the same order. A
/// connection that stays silent for `wbf_ws_idle_timeout` is closed, so an
/// abandoned socket does not hold its slot forever.
async fn serve(services: crate::State, user: OwnedUserId, socket: WebSocket) {
	let idle_timeout = Duration::from_secs(services.config.wbf_ws_idle_timeout);
	let (mut sink, mut stream) = socket.split();

	loop {
		let message = match tokio::time::timeout(idle_timeout, stream.next()).await {
			| Ok(Some(message)) => message,
			| Ok(None) => break,
			| Err(_elapsed) => {
				debug!(%user, "wbf WebSocket connection idle; closing");
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

		let reply = match decode(&mut bytes) {
			| Ok(view) => handle_pack(&services, &user, view).await,
			| Err(error) => {
				debug!(?error, "Rejected pack on the WebSocket channel");
				let (id, seq) = match error {
					| PackError::DataCrc { .. } => header_id_seq(&bytes),
					| _ => (0, 0),
				};
				error_pack(id, seq, pack_error_code(error), &error.to_string())
			},
		};

		if sink.send(Message::Binary(reply.into())).await.is_err() {
			break;
		}
	}

	let _closed = sink.close().await;
	debug!(%user, "wbf WebSocket connection closed");
}
