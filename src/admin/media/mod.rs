#![expect(rustdoc::broken_intra_doc_links)]
mod delete;
mod delete_all_from_server;
mod delete_all_from_user;
mod delete_by_event;
mod delete_list;
mod delete_range;
mod get_file_info;
mod get_remote_file;
mod get_remote_thumbnail;
mod migrate_references;
mod preview;
mod refcount;

use clap::{ArgGroup, Subcommand};
use ruma::{OwnedEventId, OwnedMxcUri, OwnedServerName};
use tuwunel_core::Result;
use url::Url;

use crate::admin_command_dispatch;

#[admin_command_dispatch]
#[derive(Debug, Subcommand)]
pub(super) enum MediaCommand {
	/// - Deletes a single media file from our database and on the filesystem
	///   via a single MXC URL
	Delete {
		/// The MXC URL to delete
		#[arg(long)]
		mxc: OwnedMxcUri,
	},

	/// - Deletes media files referenced by event
	DeleteByEvent {
		/// - The event ID which contains the media and thumbnail MXC URLs
		#[arg(long)]
		event_id: OwnedEventId,
	},

	/// - Deletes a codeblock list of MXC URLs from our database and on the
	///   filesystem. This will always ignore errors.
	DeleteList,

	/// - Deletes all remote (and optionally local) media created before or
	///   after [duration] time using filesystem metadata last modified at date.
	//    This will always ignore errors by default.
	#[command(group(
		ArgGroup::new("direction")
			.required(true)
			.args(["older_than", "newer_than"]),
	))]
	DeleteRange {
		/// - The relative time (e.g. 30s, 5m, 7d) within which to search
		duration: String,

		/// - Only delete media created before [duration] ago
		#[arg(long, short)]
		older_than: bool,

		/// - Only delete media created after [duration] ago
		#[arg(long, short)]
		newer_than: bool,

		/// - Long argument to additionally delete local media
		#[arg(long)]
		yes_i_want_to_delete_local_media: bool,
	},

	/// - Deletes all the local media from a local user on our server. This will
	///   always ignore errors by default.
	DeleteAllFromUser {
		username: String,
	},

	/// - Deletes all remote media from the specified remote server. This will
	///   always ignore errors by default.
	DeleteAllFromServer {
		server_name: OwnedServerName,

		/// Long argument to delete local media
		#[arg(long)]
		yes_i_want_to_delete_local_media: bool,
	},

	GetFileInfo {
		/// The MXC URL to lookup info for.
		mxc: OwnedMxcUri,
	},

	/// - Reports how many references an MXC URL holds.
	///
	/// Media created before counting existed reports no count, or the
	/// sentinel once something has touched it; neither is zero, and neither
	/// is collected until references are rebuilt.
	Refcount {
		/// The MXC URL to look up.
		mxc: OwnedMxcUri,
	},

	/// - Recounts every media reference from stored events, retained
	///   originals and avatars, then removes local media nothing references.
	///
	/// Offline maintenance: run it in a maintenance window, and run it with
	/// --dry-run first. It replaces every count, so media from before
	/// counting existed becomes collectable afterwards.
	MigrateReferences {
		/// Recount and list what would be removed, without writing counts or
		/// removing anything.
		#[arg(long)]
		dry_run: bool,
	},

	GetRemoteFile {
		/// The MXC URL to fetch
		mxc: OwnedMxcUri,

		#[arg(short, long)]
		server: Option<OwnedServerName>,

		#[arg(short, long, default_value("10000"))]
		timeout: u32,
	},

	GetRemoteThumbnail {
		/// The MXC URL to fetch
		mxc: OwnedMxcUri,

		#[arg(short, long)]
		server: Option<OwnedServerName>,

		#[arg(short, long, default_value("10000"))]
		timeout: u32,

		#[arg(long, default_value("800"))]
		width: u32,

		#[arg(long, default_value("800"))]
		height: u32,
	},

	Preview {
		url: Url,

		/// Bypass cache
		#[arg(short, long)]
		no_cache: bool,
	},
}
