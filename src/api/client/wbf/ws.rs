//! `GET /_wbf/v1/ws`: the WebSocket channel, one pack per binary message.
//!
//! The connection is authenticated once at the upgrade; after that every
//! binary message is decoded and handed to the same `handle_pack` the HTTP
//! transport uses, and its reply goes back as one binary message. Many
//! uploads may interleave on one connection; the pack header's `id` tells
//! them apart. The connection keeps one small table, `id -> next expected
//! chunk`, so a chunk that is plainly out of order is refused without a
//! database read. The database row stays the truth: a reconnecting client
//! resumes from `Status`, not from anything this connection remembered.

use std::collections::HashMap;

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
	wbf::{HEADER_LEN, Kind, PackError, decode},
};
use tuwunel_service::media::UploadError;

use super::{
	Reject, authenticate, control, error_pack, handle_pack, header_id_seq, pack_error_code,
	pack_response, upload,
};

/// Room for the two CRCs and the two length fields around meta and data.
const FRAME_SLACK: usize = 64;

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
	// buffered in full.
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
/// without waiting for acks, and gets the acks back in the same order.
async fn serve(services: crate::State, user: OwnedUserId, socket: WebSocket) {
	let (mut sink, mut stream) = socket.split();
	let mut next_chunk: HashMap<u64, u32> = HashMap::new();

	while let Some(message) = stream.next().await {
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
			| Ok(view) => {
				let header = view.header;
				let is_chunk = header.kind == Kind::Upload && header.subtype == upload::CHUNK;
				let plainly_out_of_order = is_chunk
					&& next_chunk
						.get(&header.id)
						.is_some_and(|&expected| header.seq > expected);

				if plainly_out_of_order {
					let expected = next_chunk[&header.id];
					Reject::from(UploadError::OutOfOrder { expected }).into_pack(header.id, header.seq)
				} else {
					let reply = handle_pack(&services, &user, view).await;
					remember_order(&mut next_chunk, header.kind, header.subtype, header.id, header.seq, &reply);
					reply
				}
			},
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

	debug!(%user, "wbf WebSocket connection closed");
}

/// Keeps the connection's `id -> next chunk` table in step with what the
/// handler accepted: an acknowledged chunk advances it, a seal or abort
/// forgets the id. Anything else leaves it alone; the database is the truth.
fn remember_order(
	next_chunk: &mut HashMap<u64, u32>,
	kind: Kind,
	subtype: u8,
	id: u64,
	seq: u32,
	reply: &[u8],
) {
	if kind != Kind::Upload {
		return;
	}
	let acknowledged = reply.len() > HEADER_LEN
		&& reply[1] == Kind::Control as u8
		&& reply[2] == control::ACK;
	if !acknowledged {
		return;
	}

	match subtype {
		| upload::CHUNK => {
			next_chunk.insert(id, seq.saturating_add(1));
		},
		| upload::SEAL | upload::ABORT => {
			next_chunk.remove(&id);
		},
		| _ => {},
	}
}
