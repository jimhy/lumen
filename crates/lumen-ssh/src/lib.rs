//! SSH transport primitives for Lumen.
//!
//! The crate deliberately owns its Tokio runtime on one OS thread per
//! connection. Callers interact through bounded, non-blocking queues and never
//! need to put an async runtime on the UI thread.

mod credential;
mod host_key;
mod metrics;
mod sftp;
mod transport;

pub use credential::{Credential, PrivateKeyCredential, SecretString};
pub use host_key::{decide_host_key, HostKeyDecision, HostKeyIdentity, HostKeyIdentityError};
pub use metrics::{
    CpuCoreMetrics, DiskIoMetrics, MetricsAccumulator, MetricsError, NetworkMetrics,
    ProcessMetrics, ServerMetrics, ServerMonitorDetails, StorageMetrics, SystemMemoryMetrics,
};
pub use sftp::{
    FileCommand, FileCommandSendError, FileError, FileEvent, FileEventReceiveError, FileOperation,
    FileVersion, TransferDirection,
};
pub use transport::{
    Command, CommandSendError, ConnectionConfig, ConnectionMode, DirectoryEntry,
    DirectoryEntryKind, DirectoryError, DisconnectReason, Event, EventError, EventErrorKind,
    EventReceiveError, KeepaliveConfig, MetricsConfig, QueueConfig, SshConnection, StartError,
    TerminalSize,
};
