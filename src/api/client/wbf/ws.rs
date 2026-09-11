//! `GET /_wbf/v1/ws`: the WebSocket channel, one pack per binary message.
//!
//! A connection is a queue (`docs/design/wbf-pack-pipeline.md` §1): one
//! receive loop that takes messages in arrival order and runs one handler at
//! a time, and one send task that writes everything queued for the peer, in
//! order, from a bounded channel. Handlers reach the peer only through that
//! channel (`Reply`); the close frame goes through it too, so it always
//! follows the replies queued before it.
//!
//! A connection may be upgraded with a bearer token (then it is that session
//! from the start and takes its device's connection slot before the upgrade)
//! or without one (then it has `wbf_ws_unauthenticated_timeout` seconds to
//! `Login`, and until it does only `Hello`, `Ping`, `Login` and `Refresh` are
//! answered). A logged-in session is checked again before every message
//! (`revalidate`): a token that was logged out, revoked, expired or whose
//! account was locked stops working at the next message, not never. `Login`
//! and `Refresh` replace the connection's session; `Logout` ends it and the
//! connection is closed. See `docs/design/wbf-wire-format.md` §6.1 and §6.3.
//!
//! The task serving a socket outlives the request that upgraded it, so it is
//! spawned through `services.connections`, which `Services::stop` joins:
//! `State` is a raw pointer to `Services` and must not be dereferenced after
//! shutdown. The loop itself ends when the server starts stopping. The send
//! task holds only the socket, never `Services`.
//!
//! The connection keeps no upload state of its own. The database row is the
//! only truth about where an upload stands, so a chunk that arrives out of
//! order is refused by the upload service, from that row, whether it came
//! over this connection, another one, or HTTP. (A first version kept a
//! per-connection `id -> next chunk` table as a shortcut; review showed it
//! could disagree with the row after an idempotent resend or a chunk sent
//! over another transport, and refuse chunks the row would take.)

use std::{net::IpAddr, time::Duration};

use axum::{
	extract::{
		State,
		ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade, close_code},
	},
	http::{HeaderMap, StatusCode, header},
	response::Response,
};
use futures::{SinkExt, StreamExt};
use tokio::{sync::mpsc, time::Instant};
use tuwunel_core::{
	debug,
	wbf::{HEADER_LEN, PackError, RejectCode, decode},
};

use tuwunel_service::streams::{ConnectionId, Outgoing};

use super::{
	CloseReason, PackContext, Reply, Session, SessionChange, Transport, authenticate, error_pack, handle_pack,
	header_id_seq, pack_response, reserve_connection_slot, revalidate,
};
use crate::ClientIp;

/// Bytes a pack carries besides its header, meta and data: `meta_len`,
/// `meta_crc`, `data_len`, `data_crc` (4 each, 16 in all), doubled to leave
/// room for the WebSocket layer's own framing on top.
const FRAME_SLACK: usize = 2 * 4 * 4;

/// How long the receive loop, once it has ended, waits for the send task to
/// write out what is still queued (the last Ack, the close frame) before
/// giving up on a peer that has stopped reading.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(5);

/// # `GET /_wbf/v1/ws`
///
/// With a bearer token the connection is that session from the upgrade on; a
/// bad token answers 401 with an `Error` pack in the body, and a device
/// already at `wbf_ws_max_connections_per_device` answers 429 with
/// `Error(TooManyConnections)`, both before any upgrade. Without an
/// `Authorization` header the connection is upgraded unauthenticated and has
/// to `Login` within `wbf_ws_unauthenticated_timeout`. A server that is
/// shutting down answers 503 the same way.
pub(crate) async fn ws_route(
	State(services): State<crate::State>,
	ClientIp(client): ClientIp,
	headers: HeaderMap,
	upgrade: WebSocketUpgrade,
) -> Response {
	// Checked before the token lookup: a stopping server owes nobody a
	// database read.
	if !services.server.is_running() {
		let reply = error_pack(0, 0, RejectCode::Internal, "server is shutting down");
		return pack_response(StatusCode::SERVICE_UNAVAILABLE, reply);
	}

	// A header that is there must be right; only its absence means "log in
	// over the channel". A wrong token is refused, not downgraded.
	let session = if headers.contains_key(header::AUTHORIZATION) {
		let mut session = match authenticate(&services, &headers).await {
			| Ok(session) => session,
			| Err(error) => {
				let reply = error_pack(0, 0, RejectCode::Unauthorized, &error.to_string());
				return pack_response(StatusCode::UNAUTHORIZED, reply);
			},
		};
		// The device's slot is taken before the upgrade, so a refused
		// connection never exists; the new one is turned away, never an old.
		match reserve_connection_slot(&services, Transport::WebSocket, None, &session.user, &session.device) {
			| Ok(slot) => session.slot = slot,
			| Err(refused) => return pack_response(StatusCode::TOO_MANY_REQUESTS, refused.into_pack(0, 0)),
		}
		Some(session)
	} else {
		None
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
			// socket is then dropped, which closes it (and the session with
			// its slot is dropped with it).
			if !services
				.connections
				.spawn(serve(services, client, session, socket))
			{
				debug!("wbf WebSocket refused: server is shutting down");
			}
		})
}

/// Runs one connection to its end: read a message, check the session, run
/// its handler (whose replies join the send queue), apply what the handler
/// did to the session, repeat.
///
/// Messages are handled one at a time in arrival order, which is what keeps
/// the ordered kinds ordered; a client that wants more in flight sends more
/// without waiting for acks, and gets the acks back in the same order. The
/// loop ends when the client closes, when the connection stays silent for
/// `wbf_ws_idle_timeout`, when an unauthenticated connection has not logged
/// in by `wbf_ws_unauthenticated_timeout` after the upgrade, when the session
/// no longer checks out, when the client logs out, when a `Login` is refused
/// for the device's connection limit, when the peer stops taking replies, or
/// when the server starts shutting down.
async fn serve(services: crate::State, client: IpAddr, session: Option<Session>, socket: WebSocket) {
	let idle_timeout = Duration::from_secs(services.config.wbf_ws_idle_timeout);
	// Counted from the upgrade, not from the last message: a connection that
	// pings but never logs in still goes at the deadline.
	let login_deadline = Instant::now() + Duration::from_secs(services.config.wbf_ws_unauthenticated_timeout);
	let server = services.server.clone();
	let mut session = session;
	let (sink, mut stream) = socket.split();

	// The connection's number, and the guard that unsubscribes it from every
	// channel when this task ends, whichever way (pipeline 2.1 shape: RAII,
	// not a path that remembers to).
	let connection: ConnectionId = services.streams.next_connection_id();
	let _streams_guard = services.streams.connection_guard(connection);

	// The send queue and its task. Bounded: a handler that produces faster
	// than the peer reads waits in `Reply::send`, and with it the receive
	// loop, and with that the peer's own sending. Memory per connection is
	// bounded by the queue length times a pack's size limit.
	let (queue, outgoing) = mpsc::channel::<Outgoing>(services.config.wbf_ws_send_queue_len.max(1));
	let mut send_task = tokio::spawn(send_queued(sink, outgoing));
	let mut reply = Reply::for_websocket(queue.clone());
	let mut health = FrameHealth::new(services.config.wbf_ws_corrupt_budget);

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
					debug!(user = user_label(session.as_ref()), "wbf WebSocket connection idle; closing");
					break;
				},
			},
			() = tokio::time::sleep_until(login_deadline), if session.is_none() => {
				debug!("wbf WebSocket connection did not log in in time; closing");
				enqueue_close(&queue, close(close_code::POLICY, "not logged in in time")).await;
				break;
			},
			() = &mut shutdown => {
				debug!(user = user_label(session.as_ref()), "wbf WebSocket connection closing: server shutting down");
				// 1001 (going away) rather than 1012 (service restart): the
				// former is in RFC 6455 itself and every client library
				// accepts it; .NET's ClientWebSocket, for one, treats 1012 as
				// a protocol error and drops the connection instead.
				enqueue_close(&queue, close(close_code::AWAY, "server shutting down")).await;
				break;
			},
		};

		let mut bytes = match message {
			| Ok(Message::Binary(bytes)) => bytes.to_vec(),
			| Ok(Message::Close(_)) | Err(_) => break,
			// Control frames are answered by the WebSocket layer itself.
			| Ok(Message::Ping(_) | Message::Pong(_)) => continue,
			| Ok(Message::Text(_)) => {
				let refused = error_pack(0, 0, RejectCode::Corrupt, "text frames are not packs; send one pack per binary frame");
				if queue.send(Outgoing::Pack(refused)).await.is_err() {
					break;
				}
				health.record_undecodable_frame();
				if health.is_spent() {
					enqueue_close(&queue, close(close_code::PROTOCOL, "too many frames that are not packs")).await;
					break;
				}
				continue;
			},
		};

		// The token was good when this session began; is it still? Asked
		// before the bytes are even decoded, so a session that is gone cannot
		// keep the connection alive by sending malformed packs either. One
		// point read per message, the same order of cost as the message
		// itself. Not cached by time: a cache would be one more copy of the
		// truth that can go stale. An unauthenticated connection has nothing
		// to check; the admission table in `handle_pack` is its whole guard.
		if let Some(current) = &session
			&& let Err(error) = revalidate(&services, current).await
		{
			debug!(user = %current.user, ?error, "wbf WebSocket session no longer valid; closing");
			// Header fields read without any CRC check: they only address the
			// refusal, they decide nothing.
			let (id, seq) = header_id_seq(&bytes);
			let refused = error_pack(id, seq, RejectCode::Unauthorized, &error.to_string());
			if queue.try_send(Outgoing::Pack(refused)).is_err() {
				// A full queue means a peer that is not reading; the close
				// frame matters more than the explanation.
				debug!("wbf WebSocket send queue full; the Unauthorized reply is dropped");
			}
			enqueue_close(&queue, close(close_code::POLICY, "session no longer valid")).await;
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
				let refused = error_pack(id, seq, RejectCode::for_pack_error(&error), &error.to_string());
				if queue.send(Outgoing::Pack(refused)).await.is_err() {
					break;
				}
				// A peer whose frames stop decoding is not speaking this
				// protocol (wire-format §2.1); one bad frame is not worth a
				// reconnect, a run of them is all this connection is doing.
				// Not every refusal from `decode` is that: a pack whose kind
				// byte is simply unassigned is framed perfectly, and its
				// sender is speaking wbf. The code decides, in one place.
				if RejectCode::for_pack_error(&error).is_undecodable_frame() {
					health.record_undecodable_frame();
					if health.is_spent() {
						enqueue_close(&queue, close(close_code::PROTOCOL, "too many packs that do not decode")).await;
						break;
					}
				} else {
					health.record_decoded_pack();
				}
				continue;
			},
		};
		health.record_decoded_pack();

		let ctx = PackContext { session: session.as_ref(), client, transport: Transport::WebSocket, connection };
		let change = match handle_pack(&services, &ctx, view, &mut reply).await {
			| Ok(change) => change,
			// The send task is gone: the peer closed while a reply was on
			// its way. Nothing left to tell anyone.
			| Err(error) => {
				debug!(user = user_label(session.as_ref()), ?error, "wbf WebSocket reply could not be queued; closing");
				break;
			},
		};

		match change {
			| SessionChange::Keep => {},
			| SessionChange::Replace(mut new_session) => {
				// Same device logging in again keeps its place in the count;
				// a different device brought its own, and the old place is
				// given back when the old session drops here.
				if let Some(old_session) = session.as_mut() {
					new_session.inherit_slot(old_session);
					// Every stream was entered as the old identity, so the
					// new one keeps none of them and subscribes again if it
					// wants to listen. ⚠️ Not the rooms-only unsubscribe:
					// leaving the device queue behind would push another
					// user's to-device items — their Megolm keys — into this
					// connection's queue, which now belongs to somebody else
					// (PR #43 review, found by cirno, rumia and salvia).
					if !old_session.is_same_device(&new_session) {
						services.streams.remove_connection(connection);
					}
				}
				session = Some(new_session);
			},
			| SessionChange::Close(reason) => {
				let frame = match reason {
					// Logged out: the maintainer chose to close rather than
					// fall back to unauthenticated; a client switching accounts
					// opens a new connection (or logs in again without logging
					// out).
					| CloseReason::LoggedOut => close(close_code::NORMAL, "logged out"),
					| CloseReason::Refused => close(close_code::POLICY, "refused"),
				};
				enqueue_close(&queue, frame).await;
				break;
			},
		}
	}

	// Let the send task write out what is still queued: the last reply and
	// the close frame. Dropping every sender is what tells it the queue is
	// complete. A peer that has stopped reading cannot hold this task past
	// the drain timeout; the socket is then dropped, which closes it.
	drop(reply);
	drop(queue);
	if tokio::time::timeout(DRAIN_TIMEOUT, &mut send_task).await.is_err() {
		debug!(user = user_label(session.as_ref()), "wbf WebSocket peer did not take the last frames; dropping the socket");
		send_task.abort();
	}
	debug!(user = user_label(session.as_ref()), "wbf WebSocket connection closed");
}

/// The send task: writes everything queued to the socket, in queue order, and
/// ends after a close frame, when the queue is complete (every sender gone),
/// or when the socket refuses a write. Holds the socket's sink and nothing
/// else, so it may outlive the receive loop for the drain and is never a
/// borrow of `Services`.
async fn send_queued(mut sink: futures::stream::SplitSink<WebSocket, Message>, mut outgoing: mpsc::Receiver<Outgoing>) {
	while let Some(item) = outgoing.recv().await {
		match item {
			| Outgoing::Pack(pack) =>
				if sink.send(Message::Binary(pack.into())).await.is_err() {
					break;
				},
			| Outgoing::Close { code, reason } => {
				let frame = CloseFrame { code, reason: reason.into() };
				let _closing = sink.send(Message::Close(Some(frame))).await;
				break;
			},
		}
	}
	let _closed = sink.close().await;
}

fn close(code: u16, reason: &'static str) -> Outgoing { Outgoing::Close { code, reason } }

/// How much unreadable input a connection may send before it is closed
/// (wire-format §2.1).
///
/// Only the frame counts. A pack that decodes clears the score even when its
/// handler then refuses it: a peer whose packs decode does speak this
/// protocol, and what it asks for is the handler's business. What this
/// watches for is the other thing — a peer that is not speaking wbf at all,
/// or an encoder with a bug — and there the frames do not decode, one after
/// another, until the budget is gone.
///
/// The specification counts down from zero to `-budget`; counting the
/// unreadable frames up to `budget` is the same rule, and reads better here.
struct FrameHealth {
	budget: u32,
	undecodable_in_a_row: u32,
}

impl FrameHealth {
	/// Args:
	///     budget: `wbf_ws_corrupt_budget`, example: 8 (0 is read as 1, so
	///         the connection still gets one frame before it is closed)
	const fn new(budget: u32) -> Self { Self { budget: if budget == 0 { 1 } else { budget }, undecodable_in_a_row: 0 } }

	fn record_undecodable_frame(&mut self) { self.undecodable_in_a_row = self.undecodable_in_a_row.saturating_add(1); }

	fn record_decoded_pack(&mut self) { self.undecodable_in_a_row = 0; }

	/// Return:
	///     bool  true once the budget is gone and the connection is to be
	///     closed with 1002; false while it may still send more.
	const fn is_spent(&self) -> bool { self.undecodable_in_a_row >= self.budget }
}

/// Queues the close frame, waiting at most `DRAIN_TIMEOUT` for room. A peer
/// that has stopped reading keeps the queue full; then the frame is not worth
/// waiting for: the receive loop ends, the drain below times out too, and the
/// socket is dropped, which closes it (review of PR #33, rumia: without this
/// bound a stopped reader could hold the loop, and its queue's memory, for
/// as long as it liked).
async fn enqueue_close(queue: &mpsc::Sender<Outgoing>, frame: Outgoing) {
	if tokio::time::timeout(DRAIN_TIMEOUT, queue.send(frame))
		.await
		.is_err()
	{
		debug!("wbf WebSocket send queue stayed full; closing without a close frame");
	}
}

/// Who a connection is, for its log lines.
fn user_label(session: Option<&Session>) -> &str {
	session.map_or("(not logged in)", |session| session.user.as_str())
}

#[cfg(test)]
mod tests {
	use super::FrameHealth;

	#[test]
	fn a_run_of_undecodable_frames_spends_the_budget() {
		let mut health = FrameHealth::new(8);

		for _ in 0..7 {
			health.record_undecodable_frame();
			assert!(!health.is_spent(), "seven in a row is still inside the budget");
		}
		health.record_undecodable_frame();

		assert!(health.is_spent(), "the eighth closes the connection");
	}

	#[test]
	fn one_pack_that_decodes_clears_the_score() {
		let mut health = FrameHealth::new(8);
		for _ in 0..7 {
			health.record_undecodable_frame();
		}

		// The handler may well refuse this pack; that is not this counter's
		// business. It decoded, so the peer speaks the protocol.
		health.record_decoded_pack();
		for _ in 0..7 {
			health.record_undecodable_frame();
		}

		assert!(!health.is_spent(), "the budget started over");
	}

	#[test]
	fn a_budget_of_zero_still_allows_one_frame() {
		let mut health = FrameHealth::new(0);
		assert!(!health.is_spent(), "nothing has gone wrong yet");

		health.record_undecodable_frame();

		assert!(health.is_spent());
	}
}
