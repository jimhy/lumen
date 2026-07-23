use std::collections::HashSet;
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{self, ErrorKind, Write};
use std::path::{Path, PathBuf};

use rand_core::{OsRng, RngCore};
use serde::{de::DeserializeOwned, Deserialize, Serialize};

use super::inventory::{InventoryError, SshInventory};
use super::local::{SshLocalBinding, StorageScope};
use super::model::{new_id, GroupId, NewSshProfile, ProfileId};

const CURRENT_FORMAT_VERSION: u32 = 1;
const INVENTORY_FORMAT_VERSION: u32 = 1;
const BINDINGS_FORMAT_VERSION: u32 = 1;
const MAX_CREDENTIAL_REF_CHARS: usize = 512;
const MAX_PRIVATE_KEY_PATH_CHARS: usize = 4_096;

#[derive(Debug)]
pub enum StoreError {
    Io(io::Error),
    Json(serde_json::Error),
    InvalidScope(&'static str),
    UnsupportedVersion {
        file: &'static str,
        version: u32,
    },
    InvalidInventory(InventoryError),
    InvalidBinding(&'static str),
    ConcurrentModification {
        expected: Option<String>,
        actual: Option<String>,
    },
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "SSH 数据文件读写失败: {error}"),
            Self::Json(error) => write!(f, "SSH 数据文件格式错误: {error}"),
            Self::InvalidScope(message) => write!(f, "SSH 存储作用域非法: {message}"),
            Self::UnsupportedVersion { file, version } => {
                write!(f, "{file} 版本 {version} 高于当前支持版本")
            }
            Self::InvalidInventory(error) => write!(f, "SSH 库存数据非法: {error}"),
            Self::InvalidBinding(message) => write!(f, "SSH 本地认证绑定非法: {message}"),
            Self::ConcurrentModification { .. } => {
                f.write_str("SSH 数据已被另一个实例修改，请重新加载后再试")
            }
        }
    }
}

impl std::error::Error for StoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Json(error) => Some(error),
            Self::InvalidInventory(error) => Some(error),
            Self::InvalidScope(_)
            | Self::UnsupportedVersion { .. }
            | Self::InvalidBinding(_)
            | Self::ConcurrentModification { .. } => None,
        }
    }
}

impl From<io::Error> for StoreError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<serde_json::Error> for StoreError {
    fn from(value: serde_json::Error) -> Self {
        Self::Json(value)
    }
}

impl From<InventoryError> for StoreError {
    fn from(value: InventoryError) -> Self {
        Self::InvalidInventory(value)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct InventoryFile {
    version: u32,
    inventory: SshInventory,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct BindingsFile {
    version: u32,
    bindings: Vec<SshLocalBinding>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CurrentFile {
    version: u32,
    generation: String,
}

/// 账号隔离的 SSH 库存存储。每次变更会先把库存与本地绑定写入同一 generation，
/// 再原子切换 `current.json`，因此读者只会看见一组完整文件。
///
/// `current.json` 的 generation 检查用于尽早发现陈旧实例，但检查与切换之间不是
/// 跨进程原子 CAS。UI 与同步 worker 必须由单 owner 串行应用状态变更；同步 worker
/// 只把远端结果发回 owner，不得持有第二个可写 `SshStore`。
pub struct SshStore {
    directory: PathBuf,
    account_id: Option<String>,
    generation: Option<String>,
    inventory: SshInventory,
    bindings: Vec<SshLocalBinding>,
}

impl fmt::Debug for SshStore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SshStore")
            .field("directory", &self.directory)
            .field("account_scoped", &self.account_id.is_some())
            .field("generation", &self.generation)
            .field("inventory", &self.inventory)
            .field(
                "bindings",
                &format_args!("<redacted:{}>", self.bindings.len()),
            )
            .finish()
    }
}

impl SshStore {
    pub fn load(data_root: &Path, scope: StorageScope) -> Result<Self, StoreError> {
        let account_id = scope
            .canonical_account_id()
            .map_err(StoreError::InvalidScope)?;
        let directory = scope
            .directory(data_root)
            .map_err(StoreError::InvalidScope)?;
        let generation = read_current_generation(&directory)?;
        let (mut inventory, bindings) = match generation.as_deref() {
            Some(generation) => read_generation(&directory, generation)?,
            None => read_legacy_state(&directory)?,
        };
        inventory.validate_loaded()?;
        validate_bindings(&inventory, &bindings)?;
        Ok(Self {
            directory,
            account_id,
            generation,
            inventory,
            bindings,
        })
    }

    pub fn account_id(&self) -> Option<&str> {
        self.account_id.as_deref()
    }

    pub fn inventory(&self) -> &SshInventory {
        &self.inventory
    }

    pub fn bindings(&self) -> &[SshLocalBinding] {
        &self.bindings
    }

    pub fn binding(&self, profile_id: &str) -> Option<&SshLocalBinding> {
        self.bindings
            .iter()
            .find(|binding| binding.profile_id == profile_id)
    }

    pub fn create_group(&mut self, name: &str) -> Result<GroupId, StoreError> {
        self.change_inventory(|next| next.create_group(name))
    }

    pub fn rename_group(&mut self, id: &str, name: &str) -> Result<(), StoreError> {
        self.change_inventory(|next| next.rename_group(id, name))
    }

    pub fn delete_group(&mut self, id: &str) -> Result<(), StoreError> {
        self.change_inventory(|next| next.delete_group(id))
    }

    pub fn reorder_groups(&mut self, ids: &[GroupId]) -> Result<(), StoreError> {
        self.change_inventory(|next| next.reorder_groups(ids))
    }

    pub fn create_profile(&mut self, draft: NewSshProfile) -> Result<ProfileId, StoreError> {
        self.change_inventory(|next| next.create_profile(draft))
    }

    pub fn update_profile(&mut self, id: &str, draft: NewSshProfile) -> Result<(), StoreError> {
        self.change_inventory(|next| next.update_profile(id, draft))
    }

    pub fn delete_profile(&mut self, id: &str) -> Result<(), StoreError> {
        let mut next_inventory = self.inventory.clone();
        next_inventory.delete_profile(id)?;
        let mut next_bindings = self.bindings.clone();
        next_bindings.retain(|binding| binding.profile_id != id);

        self.commit_state(next_inventory, next_bindings)
    }

    pub fn move_profile(
        &mut self,
        id: &str,
        target_group_id: Option<&str>,
        target_index: usize,
    ) -> Result<(), StoreError> {
        self.change_inventory(|next| next.move_profile(id, target_group_id, target_index))
    }

    pub fn reorder_profiles_in_group(
        &mut self,
        group_id: Option<&str>,
        ordered_ids: &[ProfileId],
    ) -> Result<(), StoreError> {
        self.change_inventory(|next| next.reorder_profiles_in_group(group_id, ordered_ids))
    }

    pub fn upsert_binding(&mut self, binding: SshLocalBinding) -> Result<(), StoreError> {
        validate_binding(&self.inventory, &binding)?;
        let mut next = self.bindings.clone();
        if let Some(existing) = next
            .iter_mut()
            .find(|existing| existing.profile_id == binding.profile_id)
        {
            *existing = binding;
        } else {
            next.push(binding);
        }
        self.commit_state(self.inventory.clone(), next)
    }

    pub fn remove_binding(&mut self, profile_id: &str) -> Result<(), StoreError> {
        let mut next = self.bindings.clone();
        next.retain(|binding| binding.profile_id != profile_id);
        self.commit_state(self.inventory.clone(), next)
    }

    fn change_inventory<T>(
        &mut self,
        mutate: impl FnOnce(&mut SshInventory) -> Result<T, InventoryError>,
    ) -> Result<T, StoreError> {
        let mut next = self.inventory.clone();
        let result = mutate(&mut next)?;
        self.commit_state(next, self.bindings.clone())?;
        Ok(result)
    }

    fn commit_state(
        &mut self,
        inventory: SshInventory,
        bindings: Vec<SshLocalBinding>,
    ) -> Result<(), StoreError> {
        validate_bindings(&inventory, &bindings)?;
        let actual_generation = read_current_generation(&self.directory)?;
        if actual_generation != self.generation {
            return Err(StoreError::ConcurrentModification {
                expected: self.generation.clone(),
                actual: actual_generation,
            });
        }

        let generation = new_id("gen_");
        let generations_directory = self.directory.join("generations");
        fs::create_dir_all(&generations_directory)?;
        let generation_directory = generations_directory.join(&generation);
        fs::create_dir(&generation_directory)?;

        let inventory_file = InventoryFile {
            version: INVENTORY_FORMAT_VERSION,
            inventory: inventory.clone(),
        };
        let bindings_file = BindingsFile {
            version: BINDINGS_FORMAT_VERSION,
            bindings: bindings.clone(),
        };
        let write_result = (|| {
            write_new_json(
                &generation_directory.join("sync_state.json"),
                &inventory_file,
            )?;
            write_new_json(
                &generation_directory.join("local_bindings.json"),
                &bindings_file,
            )?;
            atomic_write_json(
                &self.directory.join("current.json"),
                &CurrentFile {
                    version: CURRENT_FORMAT_VERSION,
                    generation: generation.clone(),
                },
            )
        })();
        if write_result.is_err() {
            let _ = fs::remove_dir_all(&generation_directory);
            return write_result;
        }

        self.generation = Some(generation);
        self.inventory = inventory;
        self.bindings = bindings;
        Ok(())
    }
}

fn read_current_generation(directory: &Path) -> Result<Option<String>, StoreError> {
    let Some(current) = read_json::<CurrentFile>(&directory.join("current.json"))? else {
        return Ok(None);
    };
    if current.version > CURRENT_FORMAT_VERSION {
        return Err(StoreError::UnsupportedVersion {
            file: "current.json",
            version: current.version,
        });
    }
    if !valid_generation_id(&current.generation) {
        return Err(StoreError::InvalidBinding("current generation ID 非法"));
    }
    Ok(Some(current.generation))
}

fn read_generation(
    directory: &Path,
    generation: &str,
) -> Result<(SshInventory, Vec<SshLocalBinding>), StoreError> {
    if !valid_generation_id(generation) {
        return Err(StoreError::InvalidBinding("current generation ID 非法"));
    }
    let generation_directory = directory.join("generations").join(generation);
    let inventory_file =
        read_required_json::<InventoryFile>(&generation_directory.join("sync_state.json"))?;
    let bindings_file =
        read_required_json::<BindingsFile>(&generation_directory.join("local_bindings.json"))?;
    validate_inventory_version(&inventory_file)?;
    validate_bindings_version(&bindings_file)?;
    Ok((inventory_file.inventory, bindings_file.bindings))
}

/// 未发布的早期构建曾把两个文件直接写在 scope 目录。没有 `current.json` 时只读一次
/// 该布局；下一次成功变更会迁入 generation 布局。孤立 generation 不参与恢复。
fn read_legacy_state(directory: &Path) -> Result<(SshInventory, Vec<SshLocalBinding>), StoreError> {
    let inventory = match read_json::<InventoryFile>(&directory.join("sync_state.json"))? {
        Some(file) => {
            validate_inventory_version(&file)?;
            file.inventory
        }
        None => SshInventory::default(),
    };
    let bindings = match read_json::<BindingsFile>(&directory.join("local_bindings.json"))? {
        Some(file) => {
            validate_bindings_version(&file)?;
            file.bindings
        }
        None => Vec::new(),
    };
    Ok((inventory, bindings))
}

fn validate_inventory_version(file: &InventoryFile) -> Result<(), StoreError> {
    if file.version > INVENTORY_FORMAT_VERSION {
        return Err(StoreError::UnsupportedVersion {
            file: "sync_state.json",
            version: file.version,
        });
    }
    Ok(())
}

fn validate_bindings_version(file: &BindingsFile) -> Result<(), StoreError> {
    if file.version > BINDINGS_FORMAT_VERSION {
        return Err(StoreError::UnsupportedVersion {
            file: "local_bindings.json",
            version: file.version,
        });
    }
    Ok(())
}

fn valid_generation_id(id: &str) -> bool {
    id.len() == "gen_".len() + 32
        && id.starts_with("gen_")
        && id["gen_".len()..]
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn validate_bindings(
    inventory: &SshInventory,
    bindings: &[SshLocalBinding],
) -> Result<(), StoreError> {
    if bindings.len() > inventory.profiles().len() {
        return Err(StoreError::InvalidBinding("绑定数量超过服务器数量"));
    }
    let mut seen = HashSet::with_capacity(bindings.len());
    for binding in bindings {
        validate_binding(inventory, binding)?;
        if !seen.insert(binding.profile_id.as_str()) {
            return Err(StoreError::InvalidBinding("同一服务器存在重复本地绑定"));
        }
    }
    Ok(())
}

fn validate_binding(inventory: &SshInventory, binding: &SshLocalBinding) -> Result<(), StoreError> {
    if binding.profile_id.is_empty() || inventory.profile(&binding.profile_id).is_none() {
        return Err(StoreError::InvalidBinding("绑定引用了不存在的服务器"));
    }
    if let Some(path) = &binding.private_key_path {
        let Some(text) = path.to_str() else {
            return Err(StoreError::InvalidBinding("私钥路径必须是有效 Unicode"));
        };
        if !path.is_absolute()
            || text.is_empty()
            || text.chars().count() > MAX_PRIVATE_KEY_PATH_CHARS
            || text.chars().any(char::is_control)
        {
            return Err(StoreError::InvalidBinding("私钥路径非法或过长"));
        }
    }
    for reference in [
        binding.password_credential_ref.as_deref(),
        binding.key_passphrase_credential_ref.as_deref(),
    ]
    .into_iter()
    .flatten()
    {
        if reference.is_empty()
            || reference.trim() != reference
            || reference.chars().count() > MAX_CREDENTIAL_REF_CHARS
            || reference.chars().any(char::is_control)
        {
            return Err(StoreError::InvalidBinding("系统凭据引用非法或过长"));
        }
    }
    Ok(())
}

fn read_json<T: DeserializeOwned>(path: &Path) -> Result<Option<T>, StoreError> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(Some(serde_json::from_str(
            text.trim_start_matches('\u{feff}'),
        )?)),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn read_required_json<T: DeserializeOwned>(path: &Path) -> Result<T, StoreError> {
    read_json(path)?.ok_or_else(|| {
        StoreError::Io(io::Error::new(
            ErrorKind::NotFound,
            format!("SSH generation 文件缺失: {}", path.display()),
        ))
    })
}

fn write_new_json<T: Serialize>(path: &Path, value: &T) -> Result<(), StoreError> {
    let bytes = serde_json::to_vec_pretty(value)?;
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    Ok(())
}

fn atomic_write_json<T: Serialize>(path: &Path, value: &T) -> Result<(), StoreError> {
    let directory = path
        .parent()
        .ok_or_else(|| io::Error::new(ErrorKind::InvalidInput, "SSH 数据文件路径无父目录"))?;
    fs::create_dir_all(directory)?;
    let bytes = serde_json::to_vec_pretty(value)?;
    let temporary = unique_temporary_path(path);
    let write_result = (|| -> io::Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        drop(file);
        replace_file(&temporary, path)
    })();
    if write_result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    write_result.map_err(StoreError::Io)
}

fn unique_temporary_path(path: &Path) -> PathBuf {
    let mut random = [0_u8; 8];
    OsRng.fill_bytes(&mut random);
    let suffix = random
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("ssh-data.json");
    path.with_file_name(format!(".{name}.{}.{}.tmp", std::process::id(), suffix))
}

#[cfg(not(windows))]
fn replace_file(source: &Path, target: &Path) -> io::Result<()> {
    fs::rename(source, target)
}

#[cfg(windows)]
fn replace_file(source: &Path, target: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;

    const MOVEFILE_REPLACE_EXISTING: u32 = 0x1;
    const MOVEFILE_WRITE_THROUGH: u32 = 0x8;

    #[link(name = "Kernel32")]
    extern "system" {
        fn MoveFileExW(
            existing_file_name: *const u16,
            new_file_name: *const u16,
            flags: u32,
        ) -> i32;
    }

    let source_wide = source
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let target_wide = target
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    // SAFETY: 两个 UTF-16 缓冲区均以 NUL 结尾，且在调用期间保持存活；
    // flags 只要求同卷替换目标并刷盘。
    let succeeded = unsafe {
        MoveFileExW(
            source_wide.as_ptr(),
            target_wide.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if succeeded == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::AuthMethod;
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temporary_root(test_name: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "lumen-ssh-store-{test_name}-{}-{nonce}",
            std::process::id()
        ))
    }

    fn draft(name: &str, group_id: Option<String>) -> NewSshProfile {
        NewSshProfile {
            name: name.to_owned(),
            host: "server.example.com".to_owned(),
            username: "lumen".to_owned(),
            auth_method: AuthMethod::PrivateKey,
            group_id,
            ..Default::default()
        }
    }

    #[test]
    fn 库存与本地绑定分文件且可往返加载() {
        let root = temporary_root("roundtrip");
        let scope = StorageScope::Account("550E8400-E29B-41D4-A716-446655440000".to_owned());
        let mut store = SshStore::load(&root, scope.clone()).unwrap();
        assert_eq!(
            store.account_id(),
            Some("550e8400-e29b-41d4-a716-446655440000")
        );
        let group_id = store.create_group("生产").unwrap();
        let profile_id = store.create_profile(draft("web", Some(group_id))).unwrap();
        let key_path = root.join("keys").join("id_ed25519");
        store
            .upsert_binding(SshLocalBinding {
                profile_id: profile_id.clone(),
                private_key_path: Some(key_path.clone()),
                password_credential_ref: Some("password-secret-ref".to_owned()),
                key_passphrase_credential_ref: Some("passphrase-secret-ref".to_owned()),
            })
            .unwrap();

        let directory = scope.directory(&root).unwrap();
        assert!(directory.join("current.json").is_file());
        let generation = read_current_generation(&directory).unwrap().unwrap();
        assert!(directory
            .join("generations")
            .join(&generation)
            .join("sync_state.json")
            .is_file());
        assert!(directory
            .join("generations")
            .join(generation)
            .join("local_bindings.json")
            .is_file());
        let loaded = SshStore::load(&root, scope).unwrap();
        assert_eq!(loaded.inventory().profiles().len(), 1);
        assert_eq!(
            loaded.binding(&profile_id).unwrap().private_key_path,
            Some(key_path)
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn 可同步dto与库存文件绝不含本机秘密() {
        let root = temporary_root("no-secrets");
        let scope = StorageScope::Local;
        let mut store = SshStore::load(&root, scope.clone()).unwrap();
        let profile_id = store.create_profile(draft("safe", None)).unwrap();
        store
            .upsert_binding(SshLocalBinding {
                profile_id,
                private_key_path: Some(root.join("PRIVATE_KEY_PATH_SENTINEL")),
                password_credential_ref: Some("PASSWORD_REF_SENTINEL".to_owned()),
                key_passphrase_credential_ref: Some("PASSPHRASE_REF_SENTINEL".to_owned()),
            })
            .unwrap();

        let sync_json = serde_json::to_string(&store.inventory().sync_dto()).unwrap();
        let directory = scope.directory(&root).unwrap();
        let generation = read_current_generation(&directory).unwrap().unwrap();
        let inventory_json = fs::read_to_string(
            directory
                .join("generations")
                .join(generation)
                .join("sync_state.json"),
        )
        .unwrap();
        for forbidden in [
            "PRIVATE_KEY_PATH_SENTINEL",
            "PASSWORD_REF_SENTINEL",
            "PASSPHRASE_REF_SENTINEL",
            "private_key_path",
            "credential_ref",
            "password",
            "passphrase",
        ] {
            assert!(!sync_json.contains(forbidden), "{forbidden}");
            assert!(!inventory_json.contains(forbidden), "{forbidden}");
        }
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn 不同账号读写完全隔离且重复保存可替换() {
        let root = temporary_root("isolation");
        let scope_a = StorageScope::Account("550e8400-e29b-41d4-a716-446655440000".to_owned());
        let scope_b = StorageScope::Account("550e8400-e29b-41d4-a716-446655440001".to_owned());
        let mut a = SshStore::load(&root, scope_a.clone()).unwrap();
        a.create_group("A-1").unwrap();
        a.create_group("A-2").unwrap();
        let b = SshStore::load(&root, scope_b.clone()).unwrap();

        assert_eq!(a.inventory().groups().len(), 2);
        assert!(b.inventory().groups().is_empty());
        assert_ne!(
            scope_a.directory(&root).unwrap(),
            scope_b.directory(&root).unwrap()
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn 变更保存失败时不提交内存() {
        let root = temporary_root("transaction");
        fs::write(&root, "not-a-directory").unwrap();
        let mut store = SshStore {
            directory: root.clone(),
            account_id: None,
            generation: None,
            inventory: SshInventory::default(),
            bindings: Vec::new(),
        };
        assert!(store.create_group("不会提交").is_err());
        assert!(store.inventory().groups().is_empty());
        fs::remove_file(root).unwrap();
    }

    #[test]
    fn 陈旧实例保存会被generation检查拒绝() {
        let root = temporary_root("concurrent");
        let scope = StorageScope::Local;
        let mut first = SshStore::load(&root, scope.clone()).unwrap();
        let mut stale = SshStore::load(&root, scope).unwrap();

        first.create_group("first").unwrap();
        assert!(matches!(
            stale.create_group("stale"),
            Err(StoreError::ConcurrentModification { .. })
        ));
        assert!(stale.inventory().groups().is_empty());

        let loaded = SshStore::load(&root, StorageScope::Local).unwrap();
        assert_eq!(loaded.inventory().groups().len(), 1);
        assert_eq!(loaded.inventory().groups()[0].name, "first");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn 加载拒绝重复与孤儿本地绑定() {
        let root = temporary_root("invalid-bindings");
        let directory = StorageScope::Local.directory(&root).unwrap();
        fs::create_dir_all(&directory).unwrap();
        let mut inventory = SshInventory::default();
        let profile_id = inventory.create_profile(draft("safe", None)).unwrap();
        atomic_write_json(
            &directory.join("sync_state.json"),
            &InventoryFile {
                version: INVENTORY_FORMAT_VERSION,
                inventory,
            },
        )
        .unwrap();
        let binding = SshLocalBinding {
            profile_id,
            private_key_path: None,
            password_credential_ref: Some("credential".to_owned()),
            key_passphrase_credential_ref: None,
        };
        atomic_write_json(
            &directory.join("local_bindings.json"),
            &BindingsFile {
                version: BINDINGS_FORMAT_VERSION,
                bindings: vec![binding.clone(), binding],
            },
        )
        .unwrap();
        assert!(matches!(
            SshStore::load(&root, StorageScope::Local),
            Err(StoreError::InvalidBinding(_))
        ));

        let orphan = SshLocalBinding {
            profile_id: "ssh_00000000000000000000000000000000".to_owned(),
            private_key_path: None,
            password_credential_ref: None,
            key_passphrase_credential_ref: None,
        };
        atomic_write_json(
            &directory.join("local_bindings.json"),
            &BindingsFile {
                version: BINDINGS_FORMAT_VERSION,
                bindings: vec![orphan],
            },
        )
        .unwrap();
        assert!(matches!(
            SshStore::load(&root, StorageScope::Local),
            Err(StoreError::InvalidBinding(_))
        ));
        let _ = fs::remove_dir_all(root);
    }
}
