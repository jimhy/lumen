//! SSH transport primitives for Lumen.
//!
//! The crate deliberately owns its Tokio runtime on one OS thread per
//! connection. Callers interact through bounded, non-blocking queues and never
//! need to put an async runtime on the UI thread.

mod credential;
mod host_key;
mod metrics;
mod transport;

pub use credential::{Credential, PrivateKeyCredential, SecretString};
pub use host_key::{decide_host_key, HostKeyDecision, HostKeyIdentity, HostKeyIdentityError};
pub use metrics::{
    MetricsAccumulator, MetricsError, NetworkMetrics, ServerMetrics, StorageMetrics,
    SystemMemoryMetrics,
};
pub use transport::{
    Command, CommandSendError, ConnectionConfig, DisconnectReason, Event, EventError,
    EventErrorKind, EventReceiveError, KeepaliveConfig, MetricsConfig, QueueConfig, SshConnection,
    StartError, TerminalSize,
};
