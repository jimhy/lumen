//! Bounded SSH file operations over one independent SFTP subsystem.
//!
//! Nothing in this module shares the terminal command/event queues. A slow
//! transfer or a full file-event queue therefore cannot delay shell input or
//! output. All remote paths use absolute POSIX syntax; local transfer paths
//! remain native [`PathBuf`] values.

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::future::Future;
use std::ops::Deref;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crossbeam_channel::{
    Receiver, Sender, TryRecvError as CrossbeamTryRecvError, TrySendError as CrossbeamTrySendError,
};
use russh::client;
use russh_sftp::client::error::Error as SftpError;
use russh_sftp::client::fs::Metadata;
use russh_sftp::client::{RawSftpSession, SftpSession};
use russh_sftp::protocol::{OpenFlags, Packet, StatusCode};
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;
use tokio::time::{timeout, MissedTickBehavior};

use crate::transport::{DirectoryEntry, DirectoryEntryKind};

pub(super) const COMMAND_CAPACITY: usize = 64;
pub(super) const EVENT_CAPACITY: usize = 128;
pub(super) const PROGRESS_EVENT_CAPACITY: usize = 16;
const COMMAND_POLL_INTERVAL: Duration = Duration::from_millis(10);
const OPEN_TIMEOUT: Duration = Duration::from_secs(8);
const MAX_COMMANDS_PER_TICK: usize = 64;
const MAX_CONCURRENT_OPERATIONS: usize = 4;
const MAX_PATH_BYTES: usize = 4096;
const MAX_NAME_BYTES: usize = 255;
const MAX_QUERY_BYTES: usize = 256;
const MAX_DIRECTORY_ENTRIES: usize = 1000;
const MAX_SEARCH_RESULTS: usize = 1000;
const MAX_SEARCHED_ENTRIES: usize = 100_000;
const MAX_SEARCH_DEPTH: usize = 32;
const MAX_DELETE_ENTRIES: usize = 100_000;
const MAX_DELETE_DEPTH: usize = 64;
const MAX_TEXT_BYTES: usize = 1024 * 1024;
const COPY_BUFFER_BYTES: usize = 64 * 1024;
const PROGRESS_GRANULARITY_BYTES: u64 = 256 * 1024;
const RELIABLE_EVENT_BACKLOG_CAPACITY: usize = COMMAND_CAPACITY + MAX_CONCURRENT_OPERATIONS;
const POSIX_RENAME_EXTENSION: &str = "posix-rename@openssh.com";

pub(super) struct FileSession {
    standard: SftpSession,
    posix_rename: Option<RawSftpSession>,
}

impl Deref for FileSession {
    type Target = SftpSession;

    fn deref(&self) -> &Self::Target {
        &self.standard
    }
}

impl FileSession {
    async fn atomic_replace(&self, from: &str, to: &str) -> Result<(), FileError> {
        let extension = self
            .posix_rename
            .as_ref()
            .ok_or(FileError::AtomicReplaceUnsupported)?;
        let payload = posix_rename_payload(from, to)?;
        let packet = extension
            .extended(POSIX_RENAME_EXTENSION, payload)
            .await
            .map_err(map_sftp_error)?;
        match packet {
            Packet::Status(status) if status.status_code == StatusCode::Ok => Ok(()),
            Packet::Status(status) if status.status_code == StatusCode::OpUnsupported => {
                Err(FileError::AtomicReplaceUnsupported)
            }
            Packet::Status(status) if status.status_code == StatusCode::PermissionDenied => {
                Err(FileError::PermissionDenied)
            }
            Packet::Status(status) if status.status_code == StatusCode::NoSuchFile => {
                Err(FileError::NotFound)
            }
            Packet::Status(_) | Packet::ExtendedReply(_) => Err(FileError::RemoteIo),
            _ => Err(FileError::RemoteIo),
        }
    }

    async fn close(&self) {
        let _ = timeout(Duration::from_secs(1), self.standard.close()).await;
        if let Some(extension) = &self.posix_rename {
            let _ = extension.close_session();
        }
    }
}

pub(super) struct FileLanes {
    pub command_rx: Receiver<FileCommand>,
    pub event_tx: Sender<FileEvent>,
    pub progress_event_tx: Sender<FileEvent>,
}

/// A request for the independent SFTP file service.
///
/// Every command has a non-zero caller token. A token may have only one
/// operation in flight at a time.
pub enum FileCommand {
    ListDirectory {
        token: u64,
        path: String,
        show_hidden: bool,
    },
    Search {
        token: u64,
        root: String,
        query: String,
        maximum_depth: usize,
        maximum_results: usize,
    },
    ReadText {
        token: u64,
        path: String,
    },
    WriteTextAtomic {
        token: u64,
        path: String,
        expected: Option<FileVersion>,
        content: Vec<u8>,
    },
    CreateDirectory {
        token: u64,
        path: String,
    },
    CreateFile {
        token: u64,
        path: String,
    },
    Rename {
        token: u64,
        from: String,
        to: String,
    },
    Delete {
        token: u64,
        path: String,
        recursive: bool,
    },
    Download {
        token: u64,
        remote_path: String,
        local_path: PathBuf,
        overwrite: bool,
    },
    Upload {
        token: u64,
        local_path: PathBuf,
        remote_path: String,
        overwrite: bool,
    },
    Cancel {
        token: u64,
    },
}

impl FileCommand {
    #[must_use]
    pub const fn token(&self) -> u64 {
        match self {
            Self::ListDirectory { token, .. }
            | Self::Search { token, .. }
            | Self::ReadText { token, .. }
            | Self::WriteTextAtomic { token, .. }
            | Self::CreateDirectory { token, .. }
            | Self::CreateFile { token, .. }
            | Self::Rename { token, .. }
            | Self::Delete { token, .. }
            | Self::Download { token, .. }
            | Self::Upload { token, .. }
            | Self::Cancel { token } => *token,
        }
    }
}

impl fmt::Debug for FileCommand {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut debug = formatter.debug_struct(match self {
            Self::ListDirectory { .. } => "ListDirectory",
            Self::Search { .. } => "Search",
            Self::ReadText { .. } => "ReadText",
            Self::WriteTextAtomic { .. } => "WriteTextAtomic",
            Self::CreateDirectory { .. } => "CreateDirectory",
            Self::CreateFile { .. } => "CreateFile",
            Self::Rename { .. } => "Rename",
            Self::Delete { .. } => "Delete",
            Self::Download { .. } => "Download",
            Self::Upload { .. } => "Upload",
            Self::Cancel { .. } => "Cancel",
        });
        debug.field("token", &self.token());
        match self {
            Self::ListDirectory { show_hidden, .. } => {
                debug
                    .field("path", &"<redacted>")
                    .field("show_hidden", show_hidden);
            }
            Self::Search {
                maximum_depth,
                maximum_results,
                ..
            } => {
                debug
                    .field("root", &"<redacted>")
                    .field("query", &"<redacted>")
                    .field("maximum_depth", maximum_depth)
                    .field("maximum_results", maximum_results);
            }
            Self::WriteTextAtomic {
                expected, content, ..
            } => {
                debug
                    .field("path", &"<redacted>")
                    .field("expected", expected)
                    .field("content_bytes", &content.len());
            }
            Self::Rename { .. } => {
                debug
                    .field("from", &"<redacted>")
                    .field("to", &"<redacted>");
            }
            Self::Delete { recursive, .. } => {
                debug
                    .field("path", &"<redacted>")
                    .field("recursive", recursive);
            }
            Self::Download { overwrite, .. } => {
                debug
                    .field("remote_path", &"<redacted>")
                    .field("local_path", &"<redacted>")
                    .field("overwrite", overwrite);
            }
            Self::Upload { overwrite, .. } => {
                debug
                    .field("local_path", &"<redacted>")
                    .field("remote_path", &"<redacted>")
                    .field("overwrite", overwrite);
            }
            Self::ReadText { .. } | Self::CreateDirectory { .. } | Self::CreateFile { .. } => {
                debug.field("path", &"<redacted>");
            }
            Self::Cancel { .. } => {}
        }
        debug.finish()
    }
}

/// Version returned with a text read and checked before an atomic write.
#[derive(Clone, PartialEq, Eq)]
pub struct FileVersion {
    pub size: u64,
    pub modified_seconds: Option<u32>,
    pub sha256: [u8; 32],
}

impl fmt::Debug for FileVersion {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("FileVersion")
            .field("size", &self.size)
            .field("modified_seconds", &self.modified_seconds)
            .field("sha256", &"<redacted>")
            .finish()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileOperation {
    WriteText,
    CreateDirectory,
    CreateFile,
    Rename,
    Delete,
    Download,
    Upload,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransferDirection {
    Download,
    Upload,
}

pub enum FileEvent {
    DirectoryListing {
        token: u64,
        entries: Vec<DirectoryEntry>,
        truncated: bool,
    },
    SearchResults {
        token: u64,
        entries: Vec<DirectoryEntry>,
        truncated: bool,
    },
    TextRead {
        token: u64,
        content: Vec<u8>,
        version: FileVersion,
    },
    OperationComplete {
        token: u64,
        operation: FileOperation,
        version: Option<FileVersion>,
    },
    TransferProgress {
        token: u64,
        direction: TransferDirection,
        transferred: u64,
        total: Option<u64>,
    },
    Error {
        token: u64,
        error: FileError,
    },
}

impl FileEvent {
    #[must_use]
    pub const fn token(&self) -> u64 {
        match self {
            Self::DirectoryListing { token, .. }
            | Self::SearchResults { token, .. }
            | Self::TextRead { token, .. }
            | Self::OperationComplete { token, .. }
            | Self::TransferProgress { token, .. }
            | Self::Error { token, .. } => *token,
        }
    }
}

impl fmt::Debug for FileEvent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DirectoryListing {
                token,
                entries,
                truncated,
            } => formatter
                .debug_struct("DirectoryListing")
                .field("token", token)
                .field("entries", &entries.len())
                .field("truncated", truncated)
                .finish(),
            Self::SearchResults {
                token,
                entries,
                truncated,
            } => formatter
                .debug_struct("SearchResults")
                .field("token", token)
                .field("entries", &entries.len())
                .field("truncated", truncated)
                .finish(),
            Self::TextRead {
                token,
                content,
                version,
            } => formatter
                .debug_struct("TextRead")
                .field("token", token)
                .field("content_bytes", &content.len())
                .field("version", version)
                .finish(),
            Self::OperationComplete {
                token,
                operation,
                version,
            } => formatter
                .debug_struct("OperationComplete")
                .field("token", token)
                .field("operation", operation)
                .field("version", version)
                .finish(),
            Self::TransferProgress {
                token,
                direction,
                transferred,
                total,
            } => formatter
                .debug_struct("TransferProgress")
                .field("token", token)
                .field("direction", direction)
                .field("transferred", transferred)
                .field("total", total)
                .finish(),
            Self::Error { token, error } => formatter
                .debug_struct("Error")
                .field("token", token)
                .field("error", error)
                .finish(),
        }
    }
}

#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum FileError {
    #[error("invalid SSH file request")]
    InvalidRequest,
    #[error("the SSH server does not provide an SFTP file service")]
    Unavailable,
    #[error("the SSH file request token is already active")]
    DuplicateToken,
    #[error("the SSH file service is busy")]
    Busy,
    #[error("the SSH file request was cancelled")]
    Cancelled,
    #[error("the remote path does not exist")]
    NotFound,
    #[error("permission was denied")]
    PermissionDenied,
    #[error("the target already exists or changed")]
    Conflict,
    #[error("the remote path is not a directory")]
    NotDirectory,
    #[error("the remote path is not a regular file")]
    NotFile,
    #[error("the file exceeds the supported size boundary")]
    TooLarge,
    #[error("the file is not valid UTF-8 text")]
    InvalidText,
    #[error("the operation exceeded a traversal boundary")]
    LimitExceeded,
    #[error("the server could not atomically replace the destination")]
    AtomicReplaceUnsupported,
    #[error("a local file operation failed")]
    LocalIo,
    #[error("a remote file operation failed")]
    RemoteIo,
}

#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum FileCommandSendError {
    #[error("SSH file command queue is full")]
    Full,
    #[error("SSH file service is closed")]
    Closed,
    #[error("invalid SSH file request")]
    InvalidRequest,
}

#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum FileEventReceiveError {
    #[error("no SSH file event is ready")]
    Empty,
    #[error("SSH file event stream is closed")]
    Closed,
}

pub(super) async fn open<H>(session: &client::Handle<H>) -> Result<FileSession, FileError>
where
    H: client::Handler,
{
    let open_standard = async {
        let channel = session
            .channel_open_session()
            .await
            .map_err(|_| FileError::Unavailable)?;
        channel
            .request_subsystem(true, "sftp")
            .await
            .map_err(|_| FileError::Unavailable)?;
        SftpSession::new(channel.into_stream())
            .await
            .map_err(|_| FileError::Unavailable)
    };
    let open_posix_rename = async {
        let channel = session.channel_open_session().await.ok()?;
        channel.request_subsystem(true, "sftp").await.ok()?;
        let raw = RawSftpSession::new(channel.into_stream());
        raw.set_timeout(10);
        let version = raw.init().await.ok()?;
        if version.extensions.contains_key(POSIX_RENAME_EXTENSION) {
            Some(raw)
        } else {
            let _ = raw.close_session();
            None
        }
    };
    let (standard, posix_rename) = timeout(OPEN_TIMEOUT, async {
        tokio::join!(open_standard, open_posix_rename)
    })
    .await
    .map_err(|_| FileError::Unavailable)?;
    let standard = standard?;
    standard.set_timeout(10);
    Ok(FileSession {
        standard,
        posix_rename,
    })
}

pub(super) fn spawn_service(
    session: Result<FileSession, FileError>,
    lanes: FileLanes,
    connection_cancelled: Arc<AtomicBool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        run_service(session.map(Arc::new), lanes, connection_cancelled).await;
    })
}

pub(super) fn try_send(
    sender: &Sender<FileCommand>,
    command: FileCommand,
) -> Result<(), FileCommandSendError> {
    validate_command(&command).map_err(|_| FileCommandSendError::InvalidRequest)?;
    sender.try_send(command).map_err(|error| match error {
        CrossbeamTrySendError::Full(_) => FileCommandSendError::Full,
        CrossbeamTrySendError::Disconnected(_) => FileCommandSendError::Closed,
    })
}

/// Bounded staging queue for terminal file events.
///
/// The crossbeam receiver is polled by the UI, so blocking `send` here would
/// freeze the single-thread SSH runtime (including terminal I/O). Instead,
/// terminal events wait in this bounded queue until `try_send` succeeds.
/// Command admission stops before the queue plus active operations can exceed
/// the bound. Progress uses a different lossy lane and cannot consume these
/// reliable-event slots.
struct ReliableEventQueue {
    pending: VecDeque<FileEvent>,
}

impl ReliableEventQueue {
    fn new() -> Self {
        Self {
            pending: VecDeque::with_capacity(RELIABLE_EVENT_BACKLOG_CAPACITY),
        }
    }

    fn can_accept_operation(&self, active_operations: usize) -> bool {
        self.pending.len().saturating_add(active_operations) < RELIABLE_EVENT_BACKLOG_CAPACITY
    }

    fn push(&mut self, event: FileEvent) {
        debug_assert!(self.pending.len() < RELIABLE_EVENT_BACKLOG_CAPACITY);
        self.pending.push_back(event);
    }

    /// Returns false only when the UI-side receiver has been dropped.
    fn flush(&mut self, sender: &Sender<FileEvent>) -> bool {
        while let Some(event) = self.pending.pop_front() {
            match sender.try_send(event) {
                Ok(()) => {}
                Err(CrossbeamTrySendError::Full(event)) => {
                    self.pending.push_front(event);
                    return true;
                }
                Err(CrossbeamTrySendError::Disconnected(_)) => return false,
            }
        }
        true
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.pending.len()
    }
}

async fn run_service(
    session: Result<Arc<FileSession>, FileError>,
    lanes: FileLanes,
    connection_cancelled: Arc<AtomicBool>,
) {
    let (done_tx, mut done_rx) = mpsc::channel::<(u64, FileEvent)>(MAX_CONCURRENT_OPERATIONS);
    let mut active = HashMap::<u64, Arc<AtomicBool>>::new();
    let mut reliable_events = ReliableEventQueue::new();
    let mut tick = tokio::time::interval(COMMAND_POLL_INTERVAL);
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            _ = tick.tick() => {
                if !reliable_events.flush(&lanes.event_tx) {
                    return;
                }
                if connection_cancelled.load(Ordering::Acquire) {
                    for cancelled in active.values() {
                        cancelled.store(true, Ordering::Release);
                    }
                    break;
                }

                for _ in 0..MAX_COMMANDS_PER_TICK {
                    if !reliable_events.can_accept_operation(active.len()) {
                        break;
                    }
                    let command = match lanes.command_rx.try_recv() {
                        Ok(command) => command,
                        Err(CrossbeamTryRecvError::Empty) => break,
                        Err(CrossbeamTryRecvError::Disconnected) => return,
                    };
                    let token = command.token();
                    if let FileCommand::Cancel { .. } = command {
                        if let Some(cancelled) = active.get(&token) {
                            cancelled.store(true, Ordering::Release);
                        }
                        continue;
                    }
                    if validate_command(&command).is_err() {
                        reliable_events.push(FileEvent::Error {
                            token,
                            error: FileError::InvalidRequest,
                        });
                        continue;
                    }
                    let sftp = match &session {
                        Ok(sftp) => Arc::clone(sftp),
                        Err(error) => {
                            reliable_events.push(FileEvent::Error {
                                token,
                                error: *error,
                            });
                            continue;
                        }
                    };
                    if active.contains_key(&token) {
                        reliable_events.push(FileEvent::Error {
                            token,
                            error: FileError::DuplicateToken,
                        });
                        continue;
                    }
                    if active.len() >= MAX_CONCURRENT_OPERATIONS {
                        reliable_events.push(FileEvent::Error {
                            token,
                            error: FileError::Busy,
                        });
                        continue;
                    }

                    let cancelled = Arc::new(AtomicBool::new(false));
                    active.insert(token, Arc::clone(&cancelled));
                    let sender = done_tx.clone();
                    let progress = lanes.progress_event_tx.clone();
                    tokio::spawn(async move {
                        let result = run_operation(sftp, command, Arc::clone(&cancelled), progress).await;
                        let event = match result {
                            Ok(event) => event,
                            Err(error) => FileEvent::Error { token, error },
                        };
                        let _ = sender.send((token, event)).await;
                    });
                }
                if !reliable_events.flush(&lanes.event_tx) {
                    return;
                }
            }
            completed = done_rx.recv(), if !active.is_empty() => {
                if let Some((token, event)) = completed {
                    active.remove(&token);
                    reliable_events.push(event);
                    if !reliable_events.flush(&lanes.event_tx) {
                        return;
                    }
                }
            }
        }
    }

    if let Ok(session) = session {
        session.close().await;
    }
}

async fn run_operation(
    sftp: Arc<FileSession>,
    command: FileCommand,
    cancelled: Arc<AtomicBool>,
    progress: Sender<FileEvent>,
) -> Result<FileEvent, FileError> {
    let token = command.token();
    match command {
        FileCommand::ListDirectory {
            path, show_hidden, ..
        } => {
            let (entries, truncated) =
                list_directory(&sftp, &path, show_hidden, &cancelled).await?;
            Ok(FileEvent::DirectoryListing {
                token,
                entries,
                truncated,
            })
        }
        FileCommand::Search {
            root,
            query,
            maximum_depth,
            maximum_results,
            ..
        } => {
            let (entries, truncated) = search(
                &sftp,
                &root,
                &query,
                maximum_depth,
                maximum_results,
                &cancelled,
            )
            .await?;
            Ok(FileEvent::SearchResults {
                token,
                entries,
                truncated,
            })
        }
        FileCommand::ReadText { path, .. } => {
            let (bytes, metadata) =
                read_remote_bytes(&sftp, &path, MAX_TEXT_BYTES, &cancelled).await?;
            std::str::from_utf8(&bytes).map_err(|_| FileError::InvalidText)?;
            let version = version_from(&metadata, &bytes);
            Ok(FileEvent::TextRead {
                token,
                content: bytes,
                version,
            })
        }
        FileCommand::WriteTextAtomic {
            path,
            expected,
            content,
            ..
        } => {
            let version =
                write_text_atomic(&sftp, token, &path, expected.as_ref(), &content, &cancelled)
                    .await?;
            Ok(FileEvent::OperationComplete {
                token,
                operation: FileOperation::WriteText,
                version: Some(version),
            })
        }
        FileCommand::CreateDirectory { path, .. } => {
            check_cancelled(&cancelled)?;
            sftp.create_dir(path).await.map_err(map_create_error)?;
            Ok(FileEvent::OperationComplete {
                token,
                operation: FileOperation::CreateDirectory,
                version: None,
            })
        }
        FileCommand::CreateFile { path, .. } => {
            check_cancelled(&cancelled)?;
            let mut file = sftp
                .open_with_flags(
                    path,
                    OpenFlags::CREATE | OpenFlags::EXCLUDE | OpenFlags::WRITE,
                )
                .await
                .map_err(map_create_error)?;
            file.shutdown().await.map_err(|_| FileError::RemoteIo)?;
            Ok(FileEvent::OperationComplete {
                token,
                operation: FileOperation::CreateFile,
                version: None,
            })
        }
        FileCommand::Rename { from, to, .. } => {
            check_cancelled(&cancelled)?;
            if sftp.try_exists(to.clone()).await.map_err(map_sftp_error)? {
                return Err(FileError::Conflict);
            }
            sftp.rename(from, to).await.map_err(map_sftp_error)?;
            Ok(FileEvent::OperationComplete {
                token,
                operation: FileOperation::Rename,
                version: None,
            })
        }
        FileCommand::Delete {
            path, recursive, ..
        } => {
            delete_path(&sftp, &path, recursive, &cancelled).await?;
            Ok(FileEvent::OperationComplete {
                token,
                operation: FileOperation::Delete,
                version: None,
            })
        }
        FileCommand::Download {
            remote_path,
            local_path,
            overwrite,
            ..
        } => {
            download(
                &sftp,
                token,
                &remote_path,
                &local_path,
                overwrite,
                &cancelled,
                &progress,
            )
            .await?;
            Ok(FileEvent::OperationComplete {
                token,
                operation: FileOperation::Download,
                version: None,
            })
        }
        FileCommand::Upload {
            local_path,
            remote_path,
            overwrite,
            ..
        } => {
            let version = upload(
                &sftp,
                token,
                &local_path,
                &remote_path,
                overwrite,
                &cancelled,
                &progress,
            )
            .await?;
            Ok(FileEvent::OperationComplete {
                token,
                operation: FileOperation::Upload,
                version: Some(version),
            })
        }
        FileCommand::Cancel { .. } => Err(FileError::Cancelled),
    }
}

async fn list_directory(
    sftp: &SftpSession,
    path: &str,
    show_hidden: bool,
    cancelled: &AtomicBool,
) -> Result<(Vec<DirectoryEntry>, bool), FileError> {
    check_cancelled(cancelled)?;
    let read_dir = sftp
        .read_dir(path.to_owned())
        .await
        .map_err(map_sftp_error)?;
    let mut entries = Vec::new();
    let mut truncated = false;
    for entry in read_dir {
        check_cancelled(cancelled)?;
        let name = entry.file_name();
        if !valid_remote_name(&name) || (!show_hidden && name.starts_with('.')) {
            continue;
        }
        if entries.len() >= MAX_DIRECTORY_ENTRIES {
            truncated = true;
            break;
        }
        let full_path = entry.path();
        if validate_remote_path(&full_path).is_err() {
            continue;
        }
        entries.push(directory_entry(name, full_path, entry.metadata()));
    }
    sort_entries(&mut entries);
    Ok((entries, truncated))
}

async fn search(
    sftp: &SftpSession,
    root: &str,
    query: &str,
    maximum_depth: usize,
    maximum_results: usize,
    cancelled: &AtomicBool,
) -> Result<(Vec<DirectoryEntry>, bool), FileError> {
    let query = query.to_lowercase();
    let mut pending = VecDeque::from([(root.to_owned(), 0usize)]);
    let mut results = Vec::new();
    let mut scanned = 0usize;
    let mut truncated = false;

    while let Some((path, depth)) = pending.pop_front() {
        check_cancelled(cancelled)?;
        let read_dir = sftp.read_dir(path).await.map_err(map_sftp_error)?;
        for entry in read_dir {
            check_cancelled(cancelled)?;
            scanned = scanned.saturating_add(1);
            if scanned > MAX_SEARCHED_ENTRIES {
                return Err(FileError::LimitExceeded);
            }
            let name = entry.file_name();
            if !valid_remote_name(&name) {
                continue;
            }
            let metadata = entry.metadata();
            let full_path = entry.path();
            if validate_remote_path(&full_path).is_err() {
                continue;
            }
            if name.to_lowercase().contains(&query) {
                if results.len() >= maximum_results {
                    truncated = true;
                } else {
                    results.push(directory_entry(
                        name.clone(),
                        full_path.clone(),
                        metadata.clone(),
                    ));
                }
            }
            if metadata.is_dir() && depth < maximum_depth {
                pending.push_back((full_path, depth + 1));
            }
        }
        if truncated {
            break;
        }
    }
    sort_entries(&mut results);
    Ok((results, truncated))
}

async fn read_remote_bytes(
    sftp: &SftpSession,
    path: &str,
    maximum_bytes: usize,
    cancelled: &AtomicBool,
) -> Result<(Vec<u8>, Metadata), FileError> {
    check_cancelled(cancelled)?;
    let metadata = sftp
        .metadata(path.to_owned())
        .await
        .map_err(map_sftp_error)?;
    if !metadata.is_regular() {
        return Err(FileError::NotFile);
    }
    if metadata.len() > maximum_bytes as u64 {
        return Err(FileError::TooLarge);
    }
    let mut file = sftp.open(path.to_owned()).await.map_err(map_sftp_error)?;
    let mut bytes = Vec::with_capacity((metadata.len() as usize).min(maximum_bytes));
    let mut buffer = vec![0_u8; COPY_BUFFER_BYTES];
    loop {
        check_cancelled(cancelled)?;
        let read = file
            .read(&mut buffer)
            .await
            .map_err(|_| FileError::RemoteIo)?;
        if read == 0 {
            break;
        }
        if bytes.len().saturating_add(read) > maximum_bytes {
            return Err(FileError::TooLarge);
        }
        bytes.extend_from_slice(&buffer[..read]);
    }
    Ok((bytes, metadata))
}

async fn write_text_atomic(
    sftp: &FileSession,
    token: u64,
    path: &str,
    expected: Option<&FileVersion>,
    bytes: &[u8],
    cancelled: &AtomicBool,
) -> Result<FileVersion, FileError> {
    if bytes.len() > MAX_TEXT_BYTES {
        return Err(FileError::TooLarge);
    }
    let current_metadata = sftp
        .metadata(path.to_owned())
        .await
        .map_err(map_sftp_error)?;
    if !current_metadata.is_regular() {
        return Err(FileError::NotFile);
    }
    if let Some(expected) = expected {
        verify_expected_version(sftp, path, expected, cancelled).await?;
    }

    let temporary = temporary_remote_path(path, token)?;
    let mut file = sftp
        .open_with_flags(
            temporary.clone(),
            OpenFlags::CREATE | OpenFlags::EXCLUDE | OpenFlags::WRITE,
        )
        .await
        .map_err(map_create_error)?;
    let write_result = async {
        for chunk in bytes.chunks(COPY_BUFFER_BYTES) {
            check_cancelled(cancelled)?;
            file.write_all(chunk)
                .await
                .map_err(|_| FileError::RemoteIo)?;
        }
        file.sync_all().await.map_err(map_sftp_error)?;
        file.shutdown().await.map_err(|_| FileError::RemoteIo)?;
        let mut permissions = Metadata::empty();
        permissions.permissions = current_metadata.permissions;
        sftp.set_metadata(temporary.clone(), permissions)
            .await
            .map_err(map_sftp_error)?;
        check_cancelled(cancelled)?;
        if let Some(expected) = expected {
            // The first check protects us from starting work on an already
            // stale editor buffer. This second check is the commit-time CAS:
            // no destination replacement is attempted if the target changed
            // while the temporary file was being written.
            verify_expected_version(sftp, path, expected, cancelled).await?;
        }
        check_cancelled(cancelled)?;
        // OpenSSH's extension is the only overwrite operation here with
        // atomic replacement semantics. Never emulate it by deleting the
        // destination first: unsupported servers must fail without risking
        // the original file.
        sftp.atomic_replace(&temporary, path).await
    }
    .await;
    cleanup_on_error(write_result, || async {
        let _ = sftp.remove_file(temporary).await;
    })
    .await?;

    let metadata = sftp
        .metadata(path.to_owned())
        .await
        .map_err(map_sftp_error)?;
    Ok(version_from(&metadata, bytes))
}

fn posix_rename_payload(from: &str, to: &str) -> Result<Vec<u8>, FileError> {
    let from_len = u32::try_from(from.len()).map_err(|_| FileError::InvalidRequest)?;
    let to_len = u32::try_from(to.len()).map_err(|_| FileError::InvalidRequest)?;
    let mut payload = Vec::with_capacity(
        from.len()
            .saturating_add(to.len())
            .saturating_add(2 * std::mem::size_of::<u32>()),
    );
    payload.extend_from_slice(&from_len.to_be_bytes());
    payload.extend_from_slice(from.as_bytes());
    payload.extend_from_slice(&to_len.to_be_bytes());
    payload.extend_from_slice(to.as_bytes());
    Ok(payload)
}

async fn verify_expected_version(
    sftp: &SftpSession,
    path: &str,
    expected: &FileVersion,
    cancelled: &AtomicBool,
) -> Result<(), FileError> {
    check_cancelled(cancelled)?;
    let actual = async {
        let link_metadata = sftp
            .symlink_metadata(path.to_owned())
            .await
            .map_err(map_sftp_error)?;
        if !link_metadata.is_regular() {
            return Err(FileError::NotFile);
        }
        let (bytes, metadata) = read_remote_bytes(sftp, path, MAX_TEXT_BYTES, cancelled).await?;
        Ok(version_from(&metadata, &bytes))
    }
    .await;
    compare_expected_version(expected, actual)
}

fn compare_expected_version(
    expected: &FileVersion,
    actual: Result<FileVersion, FileError>,
) -> Result<(), FileError> {
    match actual {
        Ok(actual) if actual == *expected => Ok(()),
        Ok(_) | Err(FileError::NotFound | FileError::NotFile | FileError::TooLarge) => {
            Err(FileError::Conflict)
        }
        Err(error) => Err(error),
    }
}

async fn cleanup_on_error<T, Cleanup, CleanupFuture>(
    result: Result<T, FileError>,
    cleanup: Cleanup,
) -> Result<T, FileError>
where
    Cleanup: FnOnce() -> CleanupFuture,
    CleanupFuture: Future<Output = ()>,
{
    if result.is_err() {
        cleanup().await;
    }
    result
}

async fn delete_path(
    sftp: &SftpSession,
    path: &str,
    recursive: bool,
    cancelled: &AtomicBool,
) -> Result<(), FileError> {
    if path == "/" {
        return Err(FileError::InvalidRequest);
    }
    let metadata = sftp
        .symlink_metadata(path.to_owned())
        .await
        .map_err(map_sftp_error)?;
    if !metadata.is_dir() {
        check_cancelled(cancelled)?;
        return sftp
            .remove_file(path.to_owned())
            .await
            .map_err(map_sftp_error);
    }
    if !recursive {
        check_cancelled(cancelled)?;
        return sftp
            .remove_dir(path.to_owned())
            .await
            .map_err(map_sftp_error);
    }

    let mut pending = vec![(path.to_owned(), 0usize, false)];
    let mut visited = 0usize;
    while let Some((current, depth, expanded)) = pending.pop() {
        check_cancelled(cancelled)?;
        visited = visited.saturating_add(1);
        if visited > MAX_DELETE_ENTRIES || depth > MAX_DELETE_DEPTH {
            return Err(FileError::LimitExceeded);
        }
        let metadata = sftp
            .symlink_metadata(current.clone())
            .await
            .map_err(map_sftp_error)?;
        if !metadata.is_dir() {
            sftp.remove_file(current).await.map_err(map_sftp_error)?;
            continue;
        }
        if expanded {
            sftp.remove_dir(current).await.map_err(map_sftp_error)?;
            continue;
        }

        let children = sftp
            .read_dir(current.clone())
            .await
            .map_err(map_sftp_error)?;
        pending.push((current, depth, true));
        for child in children {
            let name = child.file_name();
            if !valid_remote_name(&name) {
                return Err(FileError::RemoteIo);
            }
            let child_path = child.path();
            if validate_remote_path(&child_path).is_err() {
                return Err(FileError::RemoteIo);
            }
            pending.push((child_path, depth + 1, false));
        }
    }
    Ok(())
}

async fn download(
    sftp: &SftpSession,
    token: u64,
    remote_path: &str,
    local_path: &Path,
    overwrite: bool,
    cancelled: &AtomicBool,
    progress: &Sender<FileEvent>,
) -> Result<(), FileError> {
    let metadata = sftp
        .symlink_metadata(remote_path.to_owned())
        .await
        .map_err(map_sftp_error)?;
    if metadata.is_symlink() {
        return Err(FileError::NotFile);
    }
    if metadata.is_regular() {
        return download_regular(
            sftp,
            token,
            remote_path,
            local_path,
            overwrite,
            cancelled,
            progress,
        )
        .await;
    }
    if !metadata.is_dir() {
        return Err(FileError::NotFile);
    }
    download_directory(
        sftp,
        token,
        remote_path,
        local_path,
        overwrite,
        cancelled,
        progress,
    )
    .await
}

async fn download_regular(
    sftp: &SftpSession,
    token: u64,
    remote_path: &str,
    local_path: &Path,
    overwrite: bool,
    cancelled: &AtomicBool,
    progress: &Sender<FileEvent>,
) -> Result<(), FileError> {
    if !overwrite
        && tokio::fs::try_exists(local_path)
            .await
            .map_err(|_| FileError::LocalIo)?
    {
        return Err(FileError::Conflict);
    }
    let metadata = sftp
        .metadata(remote_path.to_owned())
        .await
        .map_err(map_sftp_error)?;
    if !metadata.is_regular() {
        return Err(FileError::NotFile);
    }
    let temporary = temporary_local_path(local_path, token)?;
    let mut local = tokio::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temporary)
        .await
        .map_err(|_| FileError::LocalIo)?;
    let mut remote = sftp
        .open(remote_path.to_owned())
        .await
        .map_err(map_sftp_error)?;
    let result = async {
        let mut buffer = vec![0_u8; COPY_BUFFER_BYTES];
        let mut transferred = 0_u64;
        let mut next_progress = 0_u64;
        loop {
            check_cancelled(cancelled)?;
            let read = remote
                .read(&mut buffer)
                .await
                .map_err(|_| FileError::RemoteIo)?;
            if read == 0 {
                break;
            }
            local
                .write_all(&buffer[..read])
                .await
                .map_err(|_| FileError::LocalIo)?;
            transferred = transferred.saturating_add(read as u64);
            emit_progress(
                progress,
                token,
                TransferDirection::Download,
                transferred,
                Some(metadata.len()),
                &mut next_progress,
            );
        }
        local.sync_all().await.map_err(|_| FileError::LocalIo)?;
        drop(local);
        check_cancelled(cancelled)?;
        tokio::fs::rename(&temporary, local_path)
            .await
            .map_err(|_| FileError::LocalIo)
    }
    .await;
    if let Err(error) = result {
        let _ = tokio::fs::remove_file(temporary).await;
        return Err(error);
    }
    Ok(())
}

async fn download_directory(
    sftp: &SftpSession,
    token: u64,
    remote_path: &str,
    local_path: &Path,
    overwrite: bool,
    cancelled: &AtomicBool,
    progress: &Sender<FileEvent>,
) -> Result<(), FileError> {
    if !overwrite
        && tokio::fs::try_exists(local_path)
            .await
            .map_err(|_| FileError::LocalIo)?
    {
        return Err(FileError::Conflict);
    }
    let temporary = temporary_local_path(local_path, token)?;
    tokio::fs::create_dir(&temporary)
        .await
        .map_err(|_| FileError::LocalIo)?;
    let result = async {
        let mut pending = VecDeque::from([(remote_path.to_owned(), temporary.clone(), 0usize)]);
        let mut visited = 0usize;
        while let Some((remote_directory, local_directory, depth)) = pending.pop_front() {
            check_cancelled(cancelled)?;
            if depth > MAX_DELETE_DEPTH {
                return Err(FileError::LimitExceeded);
            }
            let children = sftp
                .read_dir(remote_directory)
                .await
                .map_err(map_sftp_error)?;
            for child in children {
                check_cancelled(cancelled)?;
                visited = visited.saturating_add(1);
                if visited > MAX_DELETE_ENTRIES {
                    return Err(FileError::LimitExceeded);
                }
                let name = child.file_name();
                validate_local_component(&name)?;
                let child_remote = child.path();
                validate_remote_path(&child_remote)?;
                let child_local = local_directory.join(&name);
                let metadata = child.metadata();
                if metadata.is_symlink() {
                    return Err(FileError::NotFile);
                }
                if metadata.is_dir() {
                    tokio::fs::create_dir(&child_local)
                        .await
                        .map_err(|_| FileError::LocalIo)?;
                    pending.push_back((child_remote, child_local, depth + 1));
                } else if metadata.is_regular() {
                    download_regular(
                        sftp,
                        token,
                        &child_remote,
                        &child_local,
                        false,
                        cancelled,
                        progress,
                    )
                    .await?;
                } else {
                    return Err(FileError::NotFile);
                }
            }
        }
        check_cancelled(cancelled)?;
        if overwrite
            && tokio::fs::try_exists(local_path)
                .await
                .map_err(|_| FileError::LocalIo)?
        {
            let metadata = tokio::fs::symlink_metadata(local_path)
                .await
                .map_err(|_| FileError::LocalIo)?;
            if metadata.is_dir() {
                tokio::fs::remove_dir_all(local_path)
                    .await
                    .map_err(|_| FileError::LocalIo)?;
            } else {
                tokio::fs::remove_file(local_path)
                    .await
                    .map_err(|_| FileError::LocalIo)?;
            }
        }
        tokio::fs::rename(&temporary, local_path)
            .await
            .map_err(|_| FileError::LocalIo)
    }
    .await;
    if let Err(error) = result {
        let _ = tokio::fs::remove_dir_all(temporary).await;
        return Err(error);
    }
    Ok(())
}

async fn upload(
    sftp: &SftpSession,
    token: u64,
    local_path: &Path,
    remote_path: &str,
    overwrite: bool,
    cancelled: &AtomicBool,
    progress: &Sender<FileEvent>,
) -> Result<FileVersion, FileError> {
    let metadata = tokio::fs::symlink_metadata(local_path)
        .await
        .map_err(|_| FileError::LocalIo)?;
    if metadata.file_type().is_symlink() {
        return Err(FileError::NotFile);
    }
    if metadata.is_file() {
        return upload_regular(
            sftp,
            token,
            local_path,
            remote_path,
            overwrite,
            cancelled,
            progress,
        )
        .await;
    }
    if !metadata.is_dir() {
        return Err(FileError::NotFile);
    }
    upload_directory(
        sftp,
        token,
        local_path,
        remote_path,
        overwrite,
        cancelled,
        progress,
    )
    .await
}

async fn upload_regular(
    sftp: &SftpSession,
    token: u64,
    local_path: &Path,
    remote_path: &str,
    overwrite: bool,
    cancelled: &AtomicBool,
    progress: &Sender<FileEvent>,
) -> Result<FileVersion, FileError> {
    if !overwrite
        && sftp
            .try_exists(remote_path.to_owned())
            .await
            .map_err(map_sftp_error)?
    {
        return Err(FileError::Conflict);
    }
    let mut local = tokio::fs::File::open(local_path)
        .await
        .map_err(|_| FileError::LocalIo)?;
    let total = local
        .metadata()
        .await
        .map_err(|_| FileError::LocalIo)?
        .len();
    let temporary = temporary_remote_path(remote_path, token)?;
    let mut remote = sftp
        .open_with_flags(
            temporary.clone(),
            OpenFlags::CREATE | OpenFlags::EXCLUDE | OpenFlags::WRITE,
        )
        .await
        .map_err(map_create_error)?;
    let result = async {
        let mut hasher = Sha256::new();
        let mut buffer = vec![0_u8; COPY_BUFFER_BYTES];
        let mut transferred = 0_u64;
        let mut next_progress = 0_u64;
        loop {
            check_cancelled(cancelled)?;
            let read = local
                .read(&mut buffer)
                .await
                .map_err(|_| FileError::LocalIo)?;
            if read == 0 {
                break;
            }
            remote
                .write_all(&buffer[..read])
                .await
                .map_err(|_| FileError::RemoteIo)?;
            hasher.update(&buffer[..read]);
            transferred = transferred.saturating_add(read as u64);
            emit_progress(
                progress,
                token,
                TransferDirection::Upload,
                transferred,
                Some(total),
                &mut next_progress,
            );
        }
        remote.sync_all().await.map_err(map_sftp_error)?;
        remote.shutdown().await.map_err(|_| FileError::RemoteIo)?;
        check_cancelled(cancelled)?;
        sftp.rename(temporary.clone(), remote_path.to_owned())
            .await
            .map_err(|_| FileError::AtomicReplaceUnsupported)?;
        let metadata = sftp
            .metadata(remote_path.to_owned())
            .await
            .map_err(map_sftp_error)?;
        let digest: [u8; 32] = hasher.finalize().into();
        Ok(FileVersion {
            size: transferred,
            modified_seconds: metadata.mtime,
            sha256: digest,
        })
    }
    .await;
    if result.is_err() {
        let _ = sftp.remove_file(temporary).await;
    }
    result
}

async fn upload_directory(
    sftp: &SftpSession,
    token: u64,
    local_path: &Path,
    remote_path: &str,
    overwrite: bool,
    cancelled: &AtomicBool,
    progress: &Sender<FileEvent>,
) -> Result<FileVersion, FileError> {
    if !overwrite
        && sftp
            .try_exists(remote_path.to_owned())
            .await
            .map_err(map_sftp_error)?
    {
        return Err(FileError::Conflict);
    }
    let temporary = temporary_remote_path(remote_path, token)?;
    sftp.create_dir(temporary.clone())
        .await
        .map_err(map_create_error)?;
    let result = async {
        let mut pending = VecDeque::from([(local_path.to_path_buf(), temporary.clone(), 0usize)]);
        let mut visited = 0usize;
        let mut total_size = 0_u64;
        while let Some((local_directory, remote_directory, depth)) = pending.pop_front() {
            check_cancelled(cancelled)?;
            if depth > MAX_DELETE_DEPTH {
                return Err(FileError::LimitExceeded);
            }
            let mut children = tokio::fs::read_dir(&local_directory)
                .await
                .map_err(|_| FileError::LocalIo)?;
            while let Some(child) = children
                .next_entry()
                .await
                .map_err(|_| FileError::LocalIo)?
            {
                check_cancelled(cancelled)?;
                visited = visited.saturating_add(1);
                if visited > MAX_DELETE_ENTRIES {
                    return Err(FileError::LimitExceeded);
                }
                let name = child
                    .file_name()
                    .to_str()
                    .map(str::to_owned)
                    .ok_or(FileError::InvalidRequest)?;
                if !valid_remote_name(&name) {
                    return Err(FileError::InvalidRequest);
                }
                let child_local = child.path();
                let child_remote = if remote_directory == "/" {
                    format!("/{name}")
                } else {
                    format!("{remote_directory}/{name}")
                };
                validate_remote_path(&child_remote)?;
                let metadata = tokio::fs::symlink_metadata(&child_local)
                    .await
                    .map_err(|_| FileError::LocalIo)?;
                if metadata.file_type().is_symlink() {
                    return Err(FileError::NotFile);
                }
                if metadata.is_dir() {
                    sftp.create_dir(child_remote.clone())
                        .await
                        .map_err(map_create_error)?;
                    pending.push_back((child_local, child_remote, depth + 1));
                } else if metadata.is_file() {
                    total_size = total_size.saturating_add(metadata.len());
                    upload_regular(
                        sftp,
                        token,
                        &child_local,
                        &child_remote,
                        false,
                        cancelled,
                        progress,
                    )
                    .await?;
                } else {
                    return Err(FileError::NotFile);
                }
            }
        }
        check_cancelled(cancelled)?;
        if overwrite
            && sftp
                .try_exists(remote_path.to_owned())
                .await
                .map_err(map_sftp_error)?
        {
            let cleanup_cancelled = AtomicBool::new(false);
            delete_path(sftp, remote_path, true, &cleanup_cancelled).await?;
        }
        sftp.rename(temporary.clone(), remote_path.to_owned())
            .await
            .map_err(|_| FileError::AtomicReplaceUnsupported)?;
        let metadata = sftp
            .metadata(remote_path.to_owned())
            .await
            .map_err(map_sftp_error)?;
        Ok(FileVersion {
            size: total_size,
            modified_seconds: metadata.mtime,
            sha256: Sha256::digest([]).into(),
        })
    }
    .await;
    if result.is_err() {
        let cleanup_cancelled = AtomicBool::new(false);
        let _ = delete_path(sftp, &temporary, true, &cleanup_cancelled).await;
    }
    result
}

fn validate_command(command: &FileCommand) -> Result<(), FileError> {
    if command.token() == 0 {
        return Err(FileError::InvalidRequest);
    }
    match command {
        FileCommand::ListDirectory { path, .. }
        | FileCommand::ReadText { path, .. }
        | FileCommand::CreateDirectory { path, .. }
        | FileCommand::CreateFile { path, .. }
        | FileCommand::Delete { path, .. } => validate_remote_path(path),
        FileCommand::WriteTextAtomic { path, content, .. } => {
            validate_remote_path(path)?;
            if content.len() > MAX_TEXT_BYTES || std::str::from_utf8(content).is_err() {
                return Err(FileError::InvalidRequest);
            }
            Ok(())
        }
        FileCommand::Search {
            root,
            query,
            maximum_depth,
            maximum_results,
            ..
        } => {
            validate_remote_path(root)?;
            if query.is_empty()
                || query.len() > MAX_QUERY_BYTES
                || query.chars().any(char::is_control)
                || !(1..=MAX_SEARCH_DEPTH).contains(maximum_depth)
                || !(1..=MAX_SEARCH_RESULTS).contains(maximum_results)
            {
                return Err(FileError::InvalidRequest);
            }
            Ok(())
        }
        FileCommand::Rename { from, to, .. } => {
            validate_remote_path(from)?;
            validate_remote_path(to)?;
            if from == "/" || to == "/" || from == to {
                return Err(FileError::InvalidRequest);
            }
            Ok(())
        }
        FileCommand::Download {
            remote_path,
            local_path,
            ..
        } => {
            validate_remote_path(remote_path)?;
            validate_local_path(local_path)
        }
        FileCommand::Upload {
            local_path,
            remote_path,
            ..
        } => {
            validate_local_path(local_path)?;
            validate_remote_path(remote_path)
        }
        FileCommand::Cancel { .. } => Ok(()),
    }
}

fn validate_remote_path(path: &str) -> Result<(), FileError> {
    if path.is_empty()
        || path.len() > MAX_PATH_BYTES
        || !path.starts_with('/')
        || path.chars().any(char::is_control)
    {
        return Err(FileError::InvalidRequest);
    }
    if path != "/"
        && path
            .split('/')
            .skip(1)
            .any(|component| !valid_remote_name(component))
    {
        return Err(FileError::InvalidRequest);
    }
    Ok(())
}

fn validate_local_path(path: &Path) -> Result<(), FileError> {
    if !path.is_absolute()
        || path.as_os_str().is_empty()
        || path
            .components()
            .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(FileError::InvalidRequest);
    }
    Ok(())
}

fn valid_remote_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_NAME_BYTES
        && name != "."
        && name != ".."
        && !name.contains('/')
        && !name.chars().any(char::is_control)
}

fn validate_local_component(name: &str) -> Result<(), FileError> {
    let stem = name
        .split_once('.')
        .map_or(name, |(stem, _)| stem)
        .to_ascii_uppercase();
    let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || stem
            .strip_prefix("COM")
            .or_else(|| stem.strip_prefix("LPT"))
            .is_some_and(|suffix| suffix.len() == 1 && matches!(suffix.as_bytes()[0], b'1'..=b'9'));
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.len() > 240
        || name.ends_with([' ', '.'])
        || name
            .chars()
            .any(|character| character.is_control() || r#"<>:"/\|?*"#.contains(character))
        || reserved
    {
        Err(FileError::InvalidRequest)
    } else {
        Ok(())
    }
}

fn temporary_remote_path(path: &str, token: u64) -> Result<String, FileError> {
    validate_remote_path(path)?;
    let (parent, name) = path.rsplit_once('/').ok_or(FileError::InvalidRequest)?;
    if name.is_empty() {
        return Err(FileError::InvalidRequest);
    }
    let parent = if parent.is_empty() { "/" } else { parent };
    let temporary_name = format!(".{name}.lumen-{token:x}.tmp");
    if temporary_name.len() > MAX_NAME_BYTES {
        return Err(FileError::InvalidRequest);
    }
    Ok(if parent == "/" {
        format!("/{temporary_name}")
    } else {
        format!("{parent}/{temporary_name}")
    })
}

fn temporary_local_path(path: &Path, token: u64) -> Result<PathBuf, FileError> {
    validate_local_path(path)?;
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or(FileError::InvalidRequest)?;
    Ok(path.with_file_name(format!(".{name}.lumen-{token:x}.tmp")))
}

fn directory_entry(name: String, path: String, metadata: Metadata) -> DirectoryEntry {
    let kind = if metadata.is_dir() {
        DirectoryEntryKind::Directory
    } else if metadata.is_regular() {
        DirectoryEntryKind::File
    } else if metadata.is_symlink() {
        DirectoryEntryKind::Symlink
    } else {
        DirectoryEntryKind::Other
    };
    DirectoryEntry {
        name,
        path,
        kind,
        size: metadata.len(),
    }
}

fn sort_entries(entries: &mut [DirectoryEntry]) {
    entries.sort_by(|left, right| {
        let left_directory = left.kind == DirectoryEntryKind::Directory;
        let right_directory = right.kind == DirectoryEntryKind::Directory;
        right_directory
            .cmp(&left_directory)
            .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
            .then_with(|| left.name.cmp(&right.name))
    });
}

fn version_from(metadata: &Metadata, bytes: &[u8]) -> FileVersion {
    FileVersion {
        size: bytes.len() as u64,
        modified_seconds: metadata.mtime,
        sha256: Sha256::digest(bytes).into(),
    }
}

fn map_create_error(error: SftpError) -> FileError {
    match error {
        SftpError::Status(status)
            if matches!(
                status.status_code,
                StatusCode::Failure | StatusCode::PermissionDenied
            ) =>
        {
            if status.status_code == StatusCode::PermissionDenied {
                FileError::PermissionDenied
            } else {
                FileError::Conflict
            }
        }
        error => map_sftp_error(error),
    }
}

fn map_sftp_error(error: SftpError) -> FileError {
    match error {
        SftpError::Status(status) => match status.status_code {
            StatusCode::NoSuchFile => FileError::NotFound,
            StatusCode::PermissionDenied => FileError::PermissionDenied,
            _ => FileError::RemoteIo,
        },
        SftpError::Limited(_) => FileError::LimitExceeded,
        _ => FileError::RemoteIo,
    }
}

fn check_cancelled(cancelled: &AtomicBool) -> Result<(), FileError> {
    if cancelled.load(Ordering::Acquire) {
        Err(FileError::Cancelled)
    } else {
        Ok(())
    }
}

fn emit(sender: &Sender<FileEvent>, event: FileEvent) {
    let _ = sender.try_send(event);
}

fn emit_progress(
    sender: &Sender<FileEvent>,
    token: u64,
    direction: TransferDirection,
    transferred: u64,
    total: Option<u64>,
    next_progress: &mut u64,
) {
    if transferred < *next_progress && total != Some(transferred) {
        return;
    }
    *next_progress = transferred.saturating_add(PROGRESS_GRANULARITY_BYTES);
    emit(
        sender,
        FileEvent::TransferProgress {
            token,
            direction,
            transferred,
            total,
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossbeam_channel::bounded;

    #[test]
    fn remote_path_boundary_rejects_escape_controls_and_relative_paths() {
        assert_eq!(validate_remote_path("/srv/project"), Ok(()));
        assert_eq!(validate_remote_path("/"), Ok(()));
        assert_eq!(
            validate_remote_path("../etc/passwd"),
            Err(FileError::InvalidRequest)
        );
        assert_eq!(
            validate_remote_path("/srv/../etc"),
            Err(FileError::InvalidRequest)
        );
        assert_eq!(
            validate_remote_path("/srv/\u{1b}secret"),
            Err(FileError::InvalidRequest)
        );
        assert_eq!(
            validate_remote_path(&format!("/{}", "x".repeat(MAX_PATH_BYTES))),
            Err(FileError::InvalidRequest)
        );
    }

    #[test]
    fn posix_rename_payload_encodes_two_ssh_strings() {
        let payload = posix_rename_payload("/tmp/旧", "/srv/new.txt").unwrap();
        let from = "/tmp/旧".as_bytes();
        let to = "/srv/new.txt".as_bytes();
        let split = std::mem::size_of::<u32>() + from.len();

        assert_eq!(
            &payload[..std::mem::size_of::<u32>()],
            &u32::try_from(from.len()).unwrap().to_be_bytes()
        );
        assert_eq!(&payload[std::mem::size_of::<u32>()..split], from);
        assert_eq!(
            &payload[split..split + std::mem::size_of::<u32>()],
            &u32::try_from(to.len()).unwrap().to_be_bytes()
        );
        assert_eq!(&payload[split + std::mem::size_of::<u32>()..], to);
    }

    #[test]
    fn local_component_accepts_portable_file_names() {
        for name in [
            "file.txt",
            "safe_name-1.2.md",
            "文件 01.txt",
            ".gitignore",
            "conifer.txt",
            "COM0.log",
            "COM10.log",
            "LPT10",
        ] {
            assert_eq!(
                validate_local_component(name),
                Ok(()),
                "expected portable local name: {name:?}"
            );
        }
    }

    #[test]
    fn local_component_rejects_windows_reserved_names() {
        for name in [
            "CON", "con.txt", "PRN", "prn.log", "AUX", "aux.json", "NUL", "nul.bin", "COM1",
            "com9.txt", "LPT1", "lpt9.doc",
        ] {
            assert_eq!(
                validate_local_component(name),
                Err(FileError::InvalidRequest),
                "expected Windows reserved name rejection: {name:?}"
            );
        }
    }

    #[test]
    fn local_component_rejects_illegal_characters_trailing_suffixes_and_dot_names() {
        for name in [
            "bad<name",
            "bad>name",
            "bad:name",
            "bad\"name",
            "bad/name",
            "bad\\name",
            "bad|name",
            "bad?name",
            "bad*name",
            "bad\nname",
            "bad\0name",
            "trailing.",
            "trailing ",
            ".",
            "..",
        ] {
            assert_eq!(
                validate_local_component(name),
                Err(FileError::InvalidRequest),
                "expected unsafe local name rejection: {name:?}"
            );
        }
    }

    #[test]
    fn commands_and_events_redact_paths_queries_and_text() {
        let command = FileCommand::WriteTextAtomic {
            token: 9,
            path: "/secret/customer.txt".to_owned(),
            expected: None,
            content: b"do-not-log-this".to_vec(),
        };
        let event = FileEvent::TextRead {
            token: 9,
            content: b"do-not-log-this-either".to_vec(),
            version: FileVersion {
                size: 22,
                modified_seconds: Some(123),
                sha256: [7; 32],
            },
        };
        let command_debug = format!("{command:?}");
        let event_debug = format!("{event:?}");
        assert!(!command_debug.contains("customer"));
        assert!(!command_debug.contains("do-not-log"));
        assert!(!event_debug.contains("do-not-log"));
        assert!(!event_debug.contains("[7"));
        assert!(command_debug.contains("content_bytes"));
    }

    #[test]
    fn file_lane_is_bounded_and_validates_before_enqueue() {
        let (sender, receiver) = bounded(1);
        assert_eq!(
            try_send(
                &sender,
                FileCommand::ReadText {
                    token: 1,
                    path: "/srv/one.txt".to_owned(),
                },
            ),
            Ok(())
        );
        assert_eq!(
            try_send(
                &sender,
                FileCommand::ReadText {
                    token: 2,
                    path: "/srv/two.txt".to_owned(),
                },
            ),
            Err(FileCommandSendError::Full)
        );
        let _ = receiver.try_recv();
        assert_eq!(
            try_send(
                &sender,
                FileCommand::ReadText {
                    token: 0,
                    path: "/srv/three.txt".to_owned(),
                },
            ),
            Err(FileCommandSendError::InvalidRequest)
        );
        assert!(receiver.is_empty());
    }

    #[test]
    fn text_commands_preserve_utf8_bytes_and_enforce_editor_limit() {
        let with_bom_and_crlf = b"\xef\xbb\xbfline one\r\nline two\r\n".to_vec();
        let valid = FileCommand::WriteTextAtomic {
            token: 1,
            path: "/srv/file.txt".to_owned(),
            expected: None,
            content: with_bom_and_crlf.clone(),
        };
        assert_eq!(validate_command(&valid), Ok(()));
        match valid {
            FileCommand::WriteTextAtomic { content, .. } => {
                assert_eq!(content, with_bom_and_crlf);
            }
            _ => unreachable!(),
        }

        let oversized = FileCommand::WriteTextAtomic {
            token: 2,
            path: "/srv/file.txt".to_owned(),
            expected: None,
            content: vec![b'x'; MAX_TEXT_BYTES + 1],
        };
        assert_eq!(validate_command(&oversized), Err(FileError::InvalidRequest));
        let invalid_utf8 = FileCommand::WriteTextAtomic {
            token: 3,
            path: "/srv/file.txt".to_owned(),
            expected: None,
            content: vec![0xff],
        };
        assert_eq!(
            validate_command(&invalid_utf8),
            Err(FileError::InvalidRequest)
        );
    }

    #[test]
    fn temporary_remote_file_stays_in_destination_directory() {
        assert_eq!(
            temporary_remote_path("/srv/project/file.txt", 42),
            Ok("/srv/project/.file.txt.lumen-2a.tmp".to_owned())
        );
        assert_eq!(
            temporary_remote_path("/file.txt", 1),
            Ok("/.file.txt.lumen-1.tmp".to_owned())
        );
    }

    #[test]
    fn version_changes_when_same_sized_content_changes() {
        let metadata = Metadata {
            mtime: Some(10),
            ..Metadata::empty()
        };
        let left = version_from(&metadata, b"left");
        let right = version_from(&metadata, b"rift");
        assert_ne!(left, right);
        assert_eq!(left.size, right.size);
    }

    #[test]
    fn commit_version_check_detects_changes_and_missing_targets() {
        let expected = FileVersion {
            size: 4,
            modified_seconds: Some(10),
            sha256: Sha256::digest(b"left").into(),
        };
        assert_eq!(
            compare_expected_version(&expected, Ok(expected.clone())),
            Ok(())
        );

        let changed = FileVersion {
            size: 4,
            modified_seconds: Some(10),
            sha256: Sha256::digest(b"rift").into(),
        };
        assert_eq!(
            compare_expected_version(&expected, Ok(changed)),
            Err(FileError::Conflict)
        );
        for error in [FileError::NotFound, FileError::NotFile, FileError::TooLarge] {
            assert_eq!(
                compare_expected_version(&expected, Err(error)),
                Err(FileError::Conflict)
            );
        }
        assert_eq!(
            compare_expected_version(&expected, Err(FileError::PermissionDenied)),
            Err(FileError::PermissionDenied)
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn commit_conflict_runs_temporary_file_cleanup() {
        let cleaned = Arc::new(AtomicBool::new(false));
        let cleanup_flag = Arc::clone(&cleaned);
        let result: Result<(), FileError> =
            cleanup_on_error(Err(FileError::Conflict), move || async move {
                cleanup_flag.store(true, Ordering::Release);
            })
            .await;
        assert_eq!(result, Err(FileError::Conflict));
        assert!(cleaned.load(Ordering::Acquire));
    }

    #[test]
    fn reliable_terminal_events_wait_for_capacity_without_loss() {
        let (sender, receiver) = bounded(1);
        sender
            .try_send(FileEvent::Error {
                token: 1,
                error: FileError::Busy,
            })
            .expect("fill public terminal-event lane");

        let mut queue = ReliableEventQueue::new();
        queue.push(FileEvent::Error {
            token: 2,
            error: FileError::Conflict,
        });
        queue.push(FileEvent::OperationComplete {
            token: 3,
            operation: FileOperation::Rename,
            version: None,
        });

        assert!(queue.flush(&sender));
        assert_eq!(queue.len(), 2);
        assert_eq!(receiver.try_recv().expect("first").token(), 1);

        assert!(queue.flush(&sender));
        assert_eq!(queue.len(), 1);
        assert_eq!(receiver.try_recv().expect("second").token(), 2);

        assert!(queue.flush(&sender));
        assert_eq!(queue.len(), 0);
        assert_eq!(receiver.try_recv().expect("third").token(), 3);
    }

    #[test]
    fn reliable_terminal_backlog_reserves_space_for_active_operations() {
        let mut queue = ReliableEventQueue::new();
        for token in 1..=(RELIABLE_EVENT_BACKLOG_CAPACITY - MAX_CONCURRENT_OPERATIONS) as u64 {
            queue.push(FileEvent::Error {
                token,
                error: FileError::Busy,
            });
        }
        assert!(!queue.can_accept_operation(MAX_CONCURRENT_OPERATIONS));
        assert!(queue.can_accept_operation(MAX_CONCURRENT_OPERATIONS - 1));
    }
}
