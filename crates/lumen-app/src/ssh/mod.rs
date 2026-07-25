//! SSH 服务器库存的纯领域模型与本地持久化。
//!
//! 本模块刻意不包含网络连接或云同步实现。可同步的服务器/分组数据与
//! 只应保留在本机的密钥路径、凭据引用分别落盘，避免未来同步代码误把
//! 秘密字段带出设备。

mod credentials;
mod inventory;
mod local;
mod model;
mod store;
mod sync;

#[allow(unused_imports)]
pub use credentials::{
    delete_secret, read_secret, write_secret, write_secret_with_commit, CredentialError,
    CredentialReference, CredentialSlot, CredentialTransactionError, MAX_SECRET_BYTES,
};
#[allow(unused_imports)]
pub use inventory::{
    InventoryError, SshInventory, MAX_CONNECT_TIMEOUT_SECS, MAX_GROUPS, MAX_GROUP_NAME_CHARS,
    MAX_HOST_CHARS, MAX_HOST_KEY_ALGORITHM_CHARS, MAX_HOST_KEY_FINGERPRINT_CHARS,
    MAX_INITIAL_DIRECTORY_CHARS, MAX_KEEP_ALIVE_SECS, MAX_PROFILES, MAX_PROFILE_NAME_CHARS,
    MAX_USERNAME_CHARS,
};
#[allow(unused_imports)]
pub use local::{SshLocalBinding, StorageScope};
#[allow(unused_imports)]
pub use model::{
    AuthMethod, GroupId, HostKeyTrust, NewSshProfile, ProfileId, SshGroup, SshProfile,
    SyncGroupDto, SyncProfileDto, SyncStateDto,
};
#[allow(unused_imports)]
pub use store::{SshStore, SshSyncApplyReport, SshSyncRejection, StoreError};
#[allow(unused_imports)]
pub use sync::{
    SshSyncCompleted, SshSyncEvent, SshSyncFailed, SshSyncNotifier, SshSyncSnapshot, SshSyncWorker,
};
