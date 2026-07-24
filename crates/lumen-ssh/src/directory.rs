//! Bounded, read-only Linux directory listing over an SSH exec channel.
//!
//! The command is fixed and the requested path is transported as an
//! ASCII-only octal argument. Directory output is a NUL-framed stream; `ls`
//! output is intentionally never parsed.

use std::fmt::{self, Write as _};
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use russh::client;
use russh::{Channel, ChannelMsg};
use tokio::time::timeout;

use super::StrictHostKeyHandler;

pub(super) const COMMAND_CAPACITY: usize = 32;
pub(super) const EVENT_CAPACITY: usize = 32;
pub(super) const MAX_CONCURRENT_REQUESTS: usize = 4;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(8);
const CANCEL_POLL_INTERVAL: Duration = Duration::from_millis(10);
const MAX_PATH_BYTES: usize = 4096;
const MAX_NAME_BYTES: usize = 255;
const MAX_OUTPUT_BYTES: usize = 4 * 1024 * 1024;
const MAX_ENTRIES: usize = 1000;

const FIND_VISIBLE_SCRIPT: &str = r#"p=$(printf '%bX' "$1"); p=${p%X}; exec /usr/bin/find -P "$p" -mindepth 1 -maxdepth 1 ! -name '.*' -printf '%y\0%s\0%f\0'"#;
const FIND_ALL_SCRIPT: &str = r#"p=$(printf '%bX' "$1"); p=${p%X}; exec /usr/bin/find -P "$p" -mindepth 1 -maxdepth 1 -printf '%y\0%s\0%f\0'"#;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DirectoryEntryKind {
    Directory,
    File,
    Symlink,
    Other,
}

#[derive(Clone, PartialEq, Eq)]
pub struct DirectoryEntry {
    pub name: String,
    pub path: String,
    pub kind: DirectoryEntryKind,
    pub size: u64,
}

impl fmt::Debug for DirectoryEntry {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DirectoryEntry")
            .field("name", &"<redacted>")
            .field("path", &"<redacted>")
            .field("kind", &self.kind)
            .field("size", &self.size)
            .finish()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DirectoryError {
    InvalidRequest,
    DuplicateToken,
    Busy,
    OpenFailed,
    ExecFailed,
    TimedOut,
    OutputTooLarge,
    MalformedOutput,
    CommandFailed,
    Cancelled,
}

#[derive(Clone)]
pub(super) struct Request {
    pub token: u64,
    pub path: String,
    pub show_hidden: bool,
}

pub(super) struct Job {
    pub request: Request,
    pub cancelled: Arc<AtomicBool>,
}

pub(super) struct Opened {
    pub job: Job,
    pub channel: Channel<client::Msg>,
}

pub(super) struct Listing {
    pub entries: Vec<DirectoryEntry>,
    pub truncated: bool,
}

pub(super) fn validate_request(token: u64, path: &str) -> Result<(), DirectoryError> {
    if token == 0
        || path.is_empty()
        || path.len() > MAX_PATH_BYTES
        || !path.starts_with('/')
        || path.chars().any(char::is_control)
    {
        return Err(DirectoryError::InvalidRequest);
    }
    Ok(())
}

pub(super) fn build_command(path: &str, show_hidden: bool) -> Result<String, DirectoryError> {
    validate_request(1, path)?;
    let script = if show_hidden {
        FIND_ALL_SCRIPT
    } else {
        FIND_VISIBLE_SCRIPT
    };
    let encoded_path = encode_octal(path.as_bytes());
    Ok(format!(
        "LC_ALL=C /bin/sh -c {} lumen-directory '{}'",
        shell_quote(script),
        encoded_path
    ))
}

fn encode_octal(input: &[u8]) -> String {
    let mut encoded = String::with_capacity(input.len().saturating_mul(4));
    for byte in input {
        let _ = write!(encoded, "\\{byte:03o}");
    }
    encoded
}

fn shell_quote(input: &str) -> String {
    let mut quoted = String::with_capacity(input.len().saturating_add(2));
    quoted.push('\'');
    for (index, part) in input.split('\'').enumerate() {
        if index > 0 {
            quoted.push_str("'\"'\"'");
        }
        quoted.push_str(part);
    }
    quoted.push('\'');
    quoted
}

pub(super) async fn open_channel(
    session: &client::Handle<StrictHostKeyHandler>,
    job: Job,
    connection_cancelled: Arc<AtomicBool>,
) -> Result<Opened, (Job, DirectoryError)> {
    if request_cancelled(&job.cancelled, &connection_cancelled) {
        return Err((job, DirectoryError::Cancelled));
    }

    let open = cancellable(
        timeout(REQUEST_TIMEOUT, session.channel_open_session()),
        Arc::clone(&job.cancelled),
        Arc::clone(&connection_cancelled),
    )
    .await;
    let channel = match open {
        None => return Err((job, DirectoryError::Cancelled)),
        Some(Err(_)) => return Err((job, DirectoryError::TimedOut)),
        Some(Ok(Err(_))) => return Err((job, DirectoryError::OpenFailed)),
        Some(Ok(Ok(channel))) => channel,
    };

    let command = match build_command(&job.request.path, job.request.show_hidden) {
        Ok(command) => command,
        Err(error) => {
            let _ = channel.close().await;
            return Err((job, error));
        }
    };
    let exec = cancellable(
        timeout(REQUEST_TIMEOUT, channel.exec(true, command)),
        Arc::clone(&job.cancelled),
        connection_cancelled,
    )
    .await;
    match exec {
        None => {
            let _ = channel.close().await;
            Err((job, DirectoryError::Cancelled))
        }
        Some(Err(_)) => {
            let _ = channel.close().await;
            Err((job, DirectoryError::TimedOut))
        }
        Some(Ok(Err(_))) => {
            let _ = channel.close().await;
            Err((job, DirectoryError::ExecFailed))
        }
        Some(Ok(Ok(()))) => Ok(Opened { job, channel }),
    }
}

pub(super) async fn collect(
    mut channel: Channel<client::Msg>,
    parent: String,
    request_cancelled_flag: Arc<AtomicBool>,
    connection_cancelled: Arc<AtomicBool>,
) -> Result<Listing, DirectoryError> {
    let result = cancellable(
        timeout(REQUEST_TIMEOUT, collect_inner(&mut channel, parent)),
        request_cancelled_flag,
        connection_cancelled,
    )
    .await;

    match result {
        None => {
            let _ = channel.close().await;
            Err(DirectoryError::Cancelled)
        }
        Some(Err(_)) => {
            let _ = channel.close().await;
            Err(DirectoryError::TimedOut)
        }
        Some(Ok(Ok(listing))) => Ok(listing),
        Some(Ok(Err(error))) => {
            let _ = channel.close().await;
            Err(error)
        }
    }
}

async fn collect_inner(
    channel: &mut Channel<client::Msg>,
    parent: String,
) -> Result<Listing, DirectoryError> {
    let mut parser = DirectoryParser::new(parent);
    let mut exit_status = None;
    while let Some(message) = channel.wait().await {
        match message {
            ChannelMsg::Data { data } => {
                if parser.push(&data)? {
                    let _ = channel.close().await;
                    return parser.finish(true);
                }
            }
            ChannelMsg::ExtendedData { data, .. } => {
                parser.add_non_stdout_bytes(data.len())?;
            }
            ChannelMsg::ExitStatus {
                exit_status: status,
            } => exit_status = Some(status),
            ChannelMsg::Close => break,
            _ => {}
        }
    }
    match exit_status {
        Some(0) => parser.finish(false),
        Some(_) | None => Err(DirectoryError::CommandFailed),
    }
}

async fn cancellable<F>(
    future: F,
    request_cancelled_flag: Arc<AtomicBool>,
    connection_cancelled: Arc<AtomicBool>,
) -> Option<F::Output>
where
    F: Future,
{
    tokio::pin!(future);
    tokio::select! {
        output = &mut future => Some(output),
        () = wait_until_cancelled(request_cancelled_flag, connection_cancelled) => None,
    }
}

async fn wait_until_cancelled(
    request_cancelled_flag: Arc<AtomicBool>,
    connection_cancelled: Arc<AtomicBool>,
) {
    while !request_cancelled(&request_cancelled_flag, &connection_cancelled) {
        tokio::time::sleep(CANCEL_POLL_INTERVAL).await;
    }
}

fn request_cancelled(request: &AtomicBool, connection: &AtomicBool) -> bool {
    request.load(Ordering::Acquire) || connection.load(Ordering::Acquire)
}

struct DirectoryParser {
    parent: String,
    buffer: Vec<u8>,
    fields: Vec<Vec<u8>>,
    entries: Vec<DirectoryEntry>,
    output_bytes: usize,
}

impl DirectoryParser {
    fn new(parent: String) -> Self {
        Self {
            parent,
            buffer: Vec::new(),
            fields: Vec::with_capacity(3),
            entries: Vec::new(),
            output_bytes: 0,
        }
    }

    /// Returns `true` once the entry limit has been exceeded.
    fn push(&mut self, chunk: &[u8]) -> Result<bool, DirectoryError> {
        self.add_output_bytes(chunk.len())?;
        self.buffer.extend_from_slice(chunk);

        while let Some(position) = self.buffer.iter().position(|byte| *byte == 0) {
            let mut remainder = self.buffer.split_off(position.saturating_add(1));
            self.buffer.truncate(position);
            let field = std::mem::take(&mut self.buffer);
            self.buffer.append(&mut remainder);
            self.fields.push(field);
            if self.fields.len() == 3 {
                let fields = std::mem::take(&mut self.fields);
                let entry = parse_entry(&self.parent, &fields)?;
                if self.entries.len() == MAX_ENTRIES {
                    return Ok(true);
                }
                self.entries.push(entry);
            }
        }
        Ok(false)
    }

    fn add_non_stdout_bytes(&mut self, bytes: usize) -> Result<(), DirectoryError> {
        self.add_output_bytes(bytes)
    }

    fn add_output_bytes(&mut self, bytes: usize) -> Result<(), DirectoryError> {
        self.output_bytes = self.output_bytes.saturating_add(bytes);
        if self.output_bytes > MAX_OUTPUT_BYTES {
            return Err(DirectoryError::OutputTooLarge);
        }
        Ok(())
    }

    fn finish(mut self, truncated: bool) -> Result<Listing, DirectoryError> {
        if !truncated && (!self.buffer.is_empty() || !self.fields.is_empty()) {
            return Err(DirectoryError::MalformedOutput);
        }
        self.entries.sort_by(|left, right| {
            let left_directory = left.kind == DirectoryEntryKind::Directory;
            let right_directory = right.kind == DirectoryEntryKind::Directory;
            right_directory
                .cmp(&left_directory)
                .then_with(|| left.name.as_bytes().cmp(right.name.as_bytes()))
        });
        Ok(Listing {
            entries: self.entries,
            truncated,
        })
    }
}

fn parse_entry(parent: &str, fields: &[Vec<u8>]) -> Result<DirectoryEntry, DirectoryError> {
    let [kind, size, name] = fields else {
        return Err(DirectoryError::MalformedOutput);
    };
    let kind = match kind.as_slice() {
        b"d" => DirectoryEntryKind::Directory,
        b"f" => DirectoryEntryKind::File,
        b"l" => DirectoryEntryKind::Symlink,
        [byte] if byte.is_ascii_graphic() => DirectoryEntryKind::Other,
        _ => return Err(DirectoryError::MalformedOutput),
    };
    let size = std::str::from_utf8(size)
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .ok_or(DirectoryError::MalformedOutput)?;
    let name = std::str::from_utf8(name).map_err(|_| DirectoryError::MalformedOutput)?;
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.len() > MAX_NAME_BYTES
        || name.contains('/')
        || name.chars().any(char::is_control)
    {
        return Err(DirectoryError::MalformedOutput);
    }
    let path = if parent.ends_with('/') {
        format!("{parent}{name}")
    } else {
        format!("{parent}/{name}")
    };
    if path.len() > MAX_PATH_BYTES {
        return Err(DirectoryError::MalformedOutput);
    }
    Ok(DirectoryEntry {
        name: name.to_owned(),
        path,
        kind,
        size,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(kind: u8, size: u64, name: &str) -> Vec<u8> {
        let mut output = vec![kind, 0];
        output.extend_from_slice(size.to_string().as_bytes());
        output.push(0);
        output.extend_from_slice(name.as_bytes());
        output.push(0);
        output
    }

    #[test]
    fn command_never_contains_untrusted_path_text() {
        let corpus = [
            "/tmp/space name",
            "/tmp/'quoted'",
            "/tmp/$(touch PWNED)",
            "/tmp/`touch PWNED`",
            "/tmp/a; touch PWNED",
            "/tmp/-leading",
            "/tmp/用户",
            "/tmp/back\\slash",
        ];
        for path in corpus {
            let command = build_command(path, false).expect("valid safe path");
            assert!(command.is_ascii());
            assert!(command.contains("/usr/bin/find"));
            assert!(command.contains("-printf"));
            assert!(!command.contains(path));
            assert!(!command.contains("PWNED"));
            assert!(!command.contains("ls "));
        }
    }

    #[test]
    fn request_validation_requires_absolute_control_free_bounded_linux_path() {
        for invalid in [
            "",
            "relative",
            "/tmp/\0bad",
            "/tmp/\nbad",
            "/tmp/\rbad",
            "/tmp/\tbad",
        ] {
            assert_eq!(
                validate_request(1, invalid),
                Err(DirectoryError::InvalidRequest)
            );
        }
        assert_eq!(
            validate_request(0, "/tmp"),
            Err(DirectoryError::InvalidRequest)
        );
        assert_eq!(
            validate_request(1, &format!("/{}", "a".repeat(MAX_PATH_BYTES))),
            Err(DirectoryError::InvalidRequest)
        );
        assert!(validate_request(1, "/tmp/back\\slash").is_ok());
    }

    #[test]
    fn parser_accepts_records_split_at_every_chunk_boundary() {
        let mut bytes = record(b'f', 12, "alpha.txt");
        bytes.extend(record(b'd', 0, "folder"));
        for split in 0..=bytes.len() {
            let mut parser = DirectoryParser::new("/srv".to_owned());
            assert!(!parser.push(&bytes[..split]).expect("first chunk"));
            assert!(!parser.push(&bytes[split..]).expect("second chunk"));
            let listing = parser.finish(false).expect("complete listing");
            assert_eq!(listing.entries.len(), 2);
            assert_eq!(listing.entries[0].name, "folder");
            assert_eq!(listing.entries[0].path, "/srv/folder");
            assert_eq!(listing.entries[1].name, "alpha.txt");
        }
    }

    #[test]
    fn parser_rejects_malformed_and_unbounded_output() {
        let mut partial = DirectoryParser::new("/".to_owned());
        partial.push(b"f\0").expect("bounded");
        assert!(matches!(
            partial.finish(false),
            Err(DirectoryError::MalformedOutput)
        ));

        for bytes in [record(b'f', 1, "bad/name"), record(b'f', 1, "bad\nname"), {
            let mut invalid_utf8 = record(b'f', 1, "ok");
            let name = invalid_utf8.len() - 2;
            invalid_utf8[name] = 0xff;
            invalid_utf8
        }] {
            let mut parser = DirectoryParser::new("/".to_owned());
            assert_eq!(parser.push(&bytes), Err(DirectoryError::MalformedOutput));
        }

        let mut oversized = DirectoryParser::new("/".to_owned());
        assert_eq!(
            oversized.push(&vec![b'x'; MAX_OUTPUT_BYTES + 1]),
            Err(DirectoryError::OutputTooLarge)
        );
    }

    #[test]
    fn parser_stops_after_bounded_number_of_entries() {
        let mut parser = DirectoryParser::new("/".to_owned());
        let mut truncated = false;
        for index in 0..=MAX_ENTRIES {
            truncated = parser
                .push(&record(b'f', 1, &format!("item-{index:04}")))
                .expect("valid record");
        }
        assert!(truncated);
        let listing = parser.finish(true).expect("truncated listing");
        assert_eq!(listing.entries.len(), MAX_ENTRIES);
        assert!(listing.truncated);
    }

    #[test]
    fn entry_debug_redacts_remote_names_and_paths() {
        let entry = DirectoryEntry {
            name: "sensitive-name".to_owned(),
            path: "/secret/sensitive-name".to_owned(),
            kind: DirectoryEntryKind::File,
            size: 42,
        };
        let debug = format!("{entry:?}");
        assert!(!debug.contains("sensitive-name"));
        assert!(!debug.contains("/secret"));
        assert!(debug.contains("File"));
    }
}
