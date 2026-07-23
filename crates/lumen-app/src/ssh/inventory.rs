use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use super::model::{
    new_id, GroupId, NewSshProfile, ProfileId, SshGroup, SshProfile, SyncGroupDto, SyncProfileDto,
    SyncStateDto,
};

pub const MAX_GROUP_NAME_CHARS: usize = 50;
pub const MAX_GROUPS: usize = 1_000;
pub const MAX_PROFILES: usize = 10_000;
pub const MAX_PROFILE_NAME_CHARS: usize = 100;
pub const MAX_HOST_CHARS: usize = 255;
pub const MAX_USERNAME_CHARS: usize = 128;
pub const MAX_INITIAL_DIRECTORY_CHARS: usize = 4_096;
pub const MAX_HOST_KEY_ALGORITHM_CHARS: usize = 64;
pub const MAX_HOST_KEY_FINGERPRINT_CHARS: usize = 256;
pub const MAX_CONNECT_TIMEOUT_SECS: u32 = 300;
pub const MAX_KEEP_ALIVE_SECS: u32 = 3_600;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InventoryError {
    EmptyGroupName,
    GroupNameTooLong,
    InvalidGroupName,
    DuplicateGroupName,
    TooManyGroups,
    TooManyProfiles,
    InvalidId,
    GroupNotFound,
    ProfileNotFound,
    EmptyProfileName,
    ProfileNameTooLong,
    EmptyHost,
    InvalidHost,
    EmptyUsername,
    InvalidUsername,
    InvalidPort,
    InvalidInitialDirectory,
    InvalidConnectTimeout,
    InvalidKeepAlive,
    InvalidHostKey,
    InvalidGroup,
    InvalidOrder,
}

impl fmt::Display for InventoryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::EmptyGroupName => "分组名称不能为空",
            Self::GroupNameTooLong => "分组名称不能超过 50 个字符",
            Self::InvalidGroupName => "分组名称包含非法控制字符",
            Self::DuplicateGroupName => "分组名称已存在",
            Self::TooManyGroups => "SSH 分组数量超过上限",
            Self::TooManyProfiles => "SSH 服务器数量超过上限",
            Self::InvalidId => "SSH 数据 ID 非法",
            Self::GroupNotFound => "SSH 分组不存在",
            Self::ProfileNotFound => "SSH 服务器不存在",
            Self::EmptyProfileName => "服务器名称不能为空",
            Self::ProfileNameTooLong => "服务器名称过长",
            Self::EmptyHost => "服务器地址不能为空",
            Self::InvalidHost => "服务器地址非法或过长",
            Self::EmptyUsername => "登录用户名不能为空",
            Self::InvalidUsername => "登录用户名非法或过长",
            Self::InvalidPort => "SSH 端口不能为 0",
            Self::InvalidInitialDirectory => "初始目录非法或过长",
            Self::InvalidConnectTimeout => "连接超时必须在 1 到 300 秒之间",
            Self::InvalidKeepAlive => "保活间隔必须在 1 到 3600 秒之间",
            Self::InvalidHostKey => "主机密钥算法或指纹非法",
            Self::InvalidGroup => "服务器引用了不存在的分组",
            Self::InvalidOrder => "排序列表必须恰好包含目标分组内的全部服务器",
        };
        f.write_str(message)
    }
}

impl std::error::Error for InventoryError {}

/// 可同步的 SSH 库存。未分组服务器由 `group_id = None` 独立表示。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SshInventory {
    groups: Vec<SshGroup>,
    profiles: Vec<SshProfile>,
}

impl SshInventory {
    pub fn groups(&self) -> &[SshGroup] {
        &self.groups
    }

    pub fn profiles(&self) -> &[SshProfile] {
        &self.profiles
    }

    pub fn group(&self, id: &str) -> Option<&SshGroup> {
        self.groups.iter().find(|group| group.id == id)
    }

    pub fn profile(&self, id: &str) -> Option<&SshProfile> {
        self.profiles.iter().find(|profile| profile.id == id)
    }

    pub fn profiles_in_group(&self, group_id: Option<&str>) -> Vec<&SshProfile> {
        let mut profiles: Vec<_> = self
            .profiles
            .iter()
            .filter(|profile| profile.group_id.as_deref() == group_id)
            .collect();
        profiles.sort_by_key(|profile| profile.sort_order);
        profiles
    }

    pub fn create_group(&mut self, name: &str) -> Result<GroupId, InventoryError> {
        if self.groups.len() >= MAX_GROUPS {
            return Err(InventoryError::TooManyGroups);
        }
        let name = self.validate_group_name(name, None)?;
        let now = now_ms();
        let id = new_id("grp_");
        self.groups.push(SshGroup {
            id: id.clone(),
            name,
            sort_order: self.groups.len() as u32,
            created_at_ms: now,
            updated_at_ms: now,
        });
        Ok(id)
    }

    pub fn rename_group(&mut self, id: &str, name: &str) -> Result<(), InventoryError> {
        let name = self.validate_group_name(name, Some(id))?;
        let group = self
            .groups
            .iter_mut()
            .find(|group| group.id == id)
            .ok_or(InventoryError::GroupNotFound)?;
        group.name = name;
        group.updated_at_ms = now_ms();
        Ok(())
    }

    /// 删除分组只解除归属，绝不删除其中的服务器。
    pub fn delete_group(&mut self, id: &str) -> Result<(), InventoryError> {
        let position = self
            .groups
            .iter()
            .position(|group| group.id == id)
            .ok_or(InventoryError::GroupNotFound)?;
        let append_at = self.profiles_in_group(None).len() as u32;
        let moved_ids = self
            .profiles_in_group(Some(id))
            .into_iter()
            .map(|profile| profile.id.clone())
            .collect::<Vec<_>>();
        self.groups.remove(position);
        let now = now_ms();
        for (offset, profile_id) in moved_ids.iter().enumerate() {
            if let Some(profile) = self
                .profiles
                .iter_mut()
                .find(|profile| &profile.id == profile_id)
            {
                profile.group_id = None;
                profile.sort_order = append_at + offset as u32;
                profile.updated_at_ms = now;
            }
        }
        self.normalize_group_order();
        self.normalize_profile_order(None);
        Ok(())
    }

    pub fn reorder_groups(&mut self, ordered_ids: &[GroupId]) -> Result<(), InventoryError> {
        if ordered_ids.len() != self.groups.len()
            || self
                .groups
                .iter()
                .any(|group| !ordered_ids.iter().any(|id| id == &group.id))
            || has_duplicates(ordered_ids)
        {
            return Err(InventoryError::InvalidOrder);
        }
        for (sort_order, id) in ordered_ids.iter().enumerate() {
            let group = self.group_mut(id).ok_or(InventoryError::InvalidOrder)?;
            group.sort_order = sort_order as u32;
            group.updated_at_ms = now_ms();
        }
        self.groups.sort_by_key(|group| group.sort_order);
        Ok(())
    }

    pub fn create_profile(&mut self, draft: NewSshProfile) -> Result<ProfileId, InventoryError> {
        if self.profiles.len() >= MAX_PROFILES {
            return Err(InventoryError::TooManyProfiles);
        }
        let draft = self.validate_profile_draft(draft)?;
        let group_id = draft.group_id.clone();
        let now = now_ms();
        let id = new_id("ssh_");
        let sort_order = self.profiles_in_group(group_id.as_deref()).len() as u32;
        self.profiles.push(SshProfile {
            id: id.clone(),
            name: draft.name,
            host: draft.host,
            port: draft.port,
            username: draft.username,
            auth_method: draft.auth_method,
            group_id,
            sort_order,
            initial_directory: draft.initial_directory,
            connect_timeout_secs: draft.connect_timeout_secs,
            keep_alive_secs: draft.keep_alive_secs,
            monitor_enabled: draft.monitor_enabled,
            trusted_host_key: draft.trusted_host_key,
            created_at_ms: now,
            updated_at_ms: now,
        });
        Ok(id)
    }

    /// 用 draft 替换服务器的可编辑元数据，稳定 ID 与创建时间保持不变。
    pub fn update_profile(&mut self, id: &str, draft: NewSshProfile) -> Result<(), InventoryError> {
        let draft = self.validate_profile_draft(draft)?;
        let old_group = self
            .profile(id)
            .ok_or(InventoryError::ProfileNotFound)?
            .group_id
            .clone();
        let group_changed = old_group != draft.group_id;
        let new_order = if group_changed {
            self.profiles_in_group(draft.group_id.as_deref()).len() as u32
        } else {
            self.profile(id).expect("已确认存在").sort_order
        };
        let profile = self
            .profiles
            .iter_mut()
            .find(|profile| profile.id == id)
            .expect("已确认存在");
        profile.name = draft.name;
        profile.host = draft.host;
        profile.port = draft.port;
        profile.username = draft.username;
        profile.auth_method = draft.auth_method;
        profile.group_id = draft.group_id;
        profile.sort_order = new_order;
        profile.initial_directory = draft.initial_directory;
        profile.connect_timeout_secs = draft.connect_timeout_secs;
        profile.keep_alive_secs = draft.keep_alive_secs;
        profile.monitor_enabled = draft.monitor_enabled;
        profile.trusted_host_key = draft.trusted_host_key;
        profile.updated_at_ms = now_ms();
        if group_changed {
            self.normalize_profile_order(old_group.as_deref());
        }
        Ok(())
    }

    pub fn delete_profile(&mut self, id: &str) -> Result<(), InventoryError> {
        let position = self
            .profiles
            .iter()
            .position(|profile| profile.id == id)
            .ok_or(InventoryError::ProfileNotFound)?;
        let group_id = self.profiles[position].group_id.clone();
        self.profiles.remove(position);
        self.normalize_profile_order(group_id.as_deref());
        Ok(())
    }

    /// 拖入分组、拖出到“未分组”或跨组移动。
    ///
    /// `target_index` 是先从目标列表移除当前服务器后得到的列表中的插入位置；
    /// UI 处理同组向下拖放时不应再把原始列表下标直接传入。
    pub fn move_profile(
        &mut self,
        id: &str,
        target_group_id: Option<&str>,
        target_index: usize,
    ) -> Result<(), InventoryError> {
        if let Some(group_id) = target_group_id {
            if self.group(group_id).is_none() {
                return Err(InventoryError::InvalidGroup);
            }
        }
        let old_group = self
            .profile(id)
            .ok_or(InventoryError::ProfileNotFound)?
            .group_id
            .clone();
        {
            let profile = self
                .profiles
                .iter_mut()
                .find(|profile| profile.id == id)
                .expect("已确认存在");
            profile.group_id = target_group_id.map(ToOwned::to_owned);
            profile.updated_at_ms = now_ms();
        }
        self.normalize_profile_order(old_group.as_deref());

        let target_ids: Vec<_> = self
            .profiles_in_group(target_group_id)
            .into_iter()
            .filter(|profile| profile.id != id)
            .map(|profile| profile.id.clone())
            .collect();
        let insert_at = target_index.min(target_ids.len());
        let mut ordered_ids = target_ids;
        ordered_ids.insert(insert_at, id.to_owned());
        self.reorder_profiles_in_group(target_group_id, &ordered_ids)
    }

    pub fn reorder_profiles_in_group(
        &mut self,
        group_id: Option<&str>,
        ordered_ids: &[ProfileId],
    ) -> Result<(), InventoryError> {
        if let Some(id) = group_id {
            if self.group(id).is_none() {
                return Err(InventoryError::InvalidGroup);
            }
        }
        let current_ids: Vec<_> = self
            .profiles_in_group(group_id)
            .into_iter()
            .map(|profile| profile.id.clone())
            .collect();
        if ordered_ids.len() != current_ids.len()
            || current_ids
                .iter()
                .any(|id| !ordered_ids.iter().any(|ordered| ordered == id))
            || has_duplicates(ordered_ids)
        {
            return Err(InventoryError::InvalidOrder);
        }
        for (sort_order, id) in ordered_ids.iter().enumerate() {
            let profile = self
                .profiles
                .iter_mut()
                .find(|profile| &profile.id == id)
                .ok_or(InventoryError::InvalidOrder)?;
            profile.sort_order = sort_order as u32;
            profile.updated_at_ms = now_ms();
        }
        Ok(())
    }

    pub fn sync_dto(&self) -> SyncStateDto {
        let mut groups: Vec<SyncGroupDto> = self.groups.iter().map(Into::into).collect();
        groups.sort_by_key(|group| group.sort_order);
        let mut profiles: Vec<SyncProfileDto> = self.profiles.iter().map(Into::into).collect();
        profiles.sort_by(|left, right| {
            left.group_id
                .cmp(&right.group_id)
                .then(left.sort_order.cmp(&right.sort_order))
        });
        SyncStateDto { groups, profiles }
    }

    /// 应用服务端权威分组值。服务端 revision 才是同步顺序依据；这里保留线缆上的
    /// 展示时间，不以本机 wall clock 决定胜负。
    pub(crate) fn apply_synced_group(&mut self, group: SshGroup) -> Result<(), InventoryError> {
        let mut next = self.clone();
        if let Some(existing) = next
            .groups
            .iter_mut()
            .find(|existing| existing.id == group.id)
        {
            *existing = group;
        } else {
            next.groups.push(group);
        }
        next.validate_loaded()?;
        *self = next;
        Ok(())
    }

    /// 应用服务端权威服务器值；认证材料不在该类型中，因此本机绑定不受覆盖。
    pub(crate) fn apply_synced_profile(
        &mut self,
        mut profile: SshProfile,
    ) -> Result<(), InventoryError> {
        let mut next = self.clone();
        if let Some(existing) = next
            .profiles
            .iter_mut()
            .find(|existing| existing.id == profile.id)
        {
            // 主机密钥信任绑定 host+port。远端完整值没有携带“刚验证”
            // 证明，因此 endpoint 变化时必须在本机失效，不能把旧信任
            // 静默迁移到另一台服务器。
            if existing.host != profile.host || existing.port != profile.port {
                profile.trusted_host_key = None;
            }
            *existing = profile;
        } else {
            next.profiles.push(profile);
        }
        next.validate_loaded()?;
        *self = next;
        Ok(())
    }

    /// 应用服务端分组墓碑。删除分组不删除服务器，也不生成本机 wall-clock 时间。
    pub(crate) fn apply_synced_group_deletion(&mut self, group_id: &str) {
        self.groups.retain(|group| group.id != group_id);
        let append_at = self.profiles_in_group(None).len() as u32;
        let moved_ids = self
            .profiles_in_group(Some(group_id))
            .into_iter()
            .map(|profile| profile.id.clone())
            .collect::<Vec<_>>();
        for (offset, profile_id) in moved_ids.into_iter().enumerate() {
            if let Some(profile) = self
                .profiles
                .iter_mut()
                .find(|profile| profile.id == profile_id)
            {
                profile.group_id = None;
                profile.sort_order = append_at + offset as u32;
            }
        }
        self.normalize_group_order();
        self.normalize_profile_order(None);
    }

    /// 应用服务端服务器墓碑；重复墓碑是幂等 no-op。
    pub(crate) fn apply_synced_profile_deletion(&mut self, profile_id: &str) {
        let group_id = self
            .profile(profile_id)
            .and_then(|profile| profile.group_id.clone());
        self.profiles.retain(|profile| profile.id != profile_id);
        self.normalize_profile_order(group_id.as_deref());
    }

    pub(crate) fn validate_loaded(&mut self) -> Result<(), InventoryError> {
        if self.groups.len() > MAX_GROUPS {
            return Err(InventoryError::TooManyGroups);
        }
        if self.profiles.len() > MAX_PROFILES {
            return Err(InventoryError::TooManyProfiles);
        }
        for group in &self.groups {
            validate_id(&group.id, "grp_")?;
        }
        for profile in &self.profiles {
            validate_id(&profile.id, "ssh_")?;
        }
        if has_duplicates(
            &self
                .groups
                .iter()
                .map(|group| group.id.clone())
                .collect::<Vec<_>>(),
        ) || has_duplicates(
            &self
                .profiles
                .iter()
                .map(|profile| profile.id.clone())
                .collect::<Vec<_>>(),
        ) {
            return Err(InventoryError::InvalidOrder);
        }
        for index in 0..self.groups.len() {
            let id = self.groups[index].id.clone();
            let name = self.groups[index].name.clone();
            self.groups[index].name = self.validate_group_name(&name, Some(&id))?;
        }
        for index in 0..self.profiles.len() {
            let profile = &self.profiles[index];
            let draft = NewSshProfile {
                name: profile.name.clone(),
                host: profile.host.clone(),
                port: profile.port,
                username: profile.username.clone(),
                auth_method: profile.auth_method,
                group_id: profile.group_id.clone(),
                initial_directory: profile.initial_directory.clone(),
                connect_timeout_secs: profile.connect_timeout_secs,
                keep_alive_secs: profile.keep_alive_secs,
                monitor_enabled: profile.monitor_enabled,
                trusted_host_key: profile.trusted_host_key.clone(),
            };
            let draft = self.validate_profile_draft(draft)?;
            let profile = &mut self.profiles[index];
            profile.name = draft.name;
            profile.host = draft.host;
            profile.username = draft.username;
            profile.initial_directory = draft.initial_directory;
            profile.trusted_host_key = draft.trusted_host_key;
        }
        self.groups.sort_by_key(|group| group.sort_order);
        self.normalize_group_order();
        let group_ids = self
            .groups
            .iter()
            .map(|group| group.id.clone())
            .collect::<Vec<_>>();
        self.normalize_profile_order(None);
        for id in group_ids {
            self.normalize_profile_order(Some(&id));
        }
        Ok(())
    }

    fn validate_group_name(
        &self,
        raw_name: &str,
        except_id: Option<&str>,
    ) -> Result<String, InventoryError> {
        let name = raw_name.trim();
        if name.is_empty() {
            return Err(InventoryError::EmptyGroupName);
        }
        if name.chars().count() > MAX_GROUP_NAME_CHARS {
            return Err(InventoryError::GroupNameTooLong);
        }
        if name.chars().any(char::is_control) {
            return Err(InventoryError::InvalidGroupName);
        }
        let folded_name = name.to_lowercase();
        if self.groups.iter().any(|group| {
            Some(group.id.as_str()) != except_id && group.name.to_lowercase() == folded_name
        }) {
            return Err(InventoryError::DuplicateGroupName);
        }
        Ok(name.to_owned())
    }

    fn validate_profile_draft(
        &self,
        mut draft: NewSshProfile,
    ) -> Result<NewSshProfile, InventoryError> {
        draft.name = draft.name.trim().to_owned();
        draft.host = draft.host.trim().to_owned();
        draft.username = draft.username.trim().to_owned();
        draft.initial_directory = draft
            .initial_directory
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty());
        if draft.name.is_empty() {
            return Err(InventoryError::EmptyProfileName);
        }
        if draft.name.chars().count() > MAX_PROFILE_NAME_CHARS
            || draft.name.chars().any(char::is_control)
        {
            return Err(InventoryError::ProfileNameTooLong);
        }
        if draft.host.is_empty() {
            return Err(InventoryError::EmptyHost);
        }
        if draft.host.chars().count() > MAX_HOST_CHARS
            || draft.host.chars().any(char::is_whitespace)
            || draft.host.chars().any(char::is_control)
        {
            return Err(InventoryError::InvalidHost);
        }
        if draft.username.is_empty() {
            return Err(InventoryError::EmptyUsername);
        }
        if draft.username.chars().count() > MAX_USERNAME_CHARS
            || draft.username.chars().any(char::is_whitespace)
            || draft.username.chars().any(char::is_control)
        {
            return Err(InventoryError::InvalidUsername);
        }
        if draft.port == 0 {
            return Err(InventoryError::InvalidPort);
        }
        if draft.initial_directory.as_deref().is_some_and(|directory| {
            directory.chars().count() > MAX_INITIAL_DIRECTORY_CHARS
                || directory.chars().any(char::is_control)
        }) {
            return Err(InventoryError::InvalidInitialDirectory);
        }
        if !(1..=MAX_CONNECT_TIMEOUT_SECS).contains(&draft.connect_timeout_secs) {
            return Err(InventoryError::InvalidConnectTimeout);
        }
        if draft
            .keep_alive_secs
            .is_some_and(|seconds| !(1..=MAX_KEEP_ALIVE_SECS).contains(&seconds))
        {
            return Err(InventoryError::InvalidKeepAlive);
        }
        if let Some(trusted) = &mut draft.trusted_host_key {
            trusted.algorithm = trusted.algorithm.trim().to_owned();
            trusted.fingerprint = trusted.fingerprint.trim().to_owned();
            if trusted.algorithm.is_empty()
                || trusted.algorithm.chars().count() > MAX_HOST_KEY_ALGORITHM_CHARS
                || trusted.algorithm.chars().any(char::is_whitespace)
                || trusted.algorithm.chars().any(char::is_control)
                || trusted.fingerprint.is_empty()
                || trusted.fingerprint.chars().count() > MAX_HOST_KEY_FINGERPRINT_CHARS
                || trusted.fingerprint.chars().any(char::is_whitespace)
                || trusted.fingerprint.chars().any(char::is_control)
            {
                return Err(InventoryError::InvalidHostKey);
            }
        }
        if draft
            .group_id
            .as_deref()
            .is_some_and(|id| self.group(id).is_none())
        {
            return Err(InventoryError::InvalidGroup);
        }
        Ok(draft)
    }

    fn group_mut(&mut self, id: &str) -> Option<&mut SshGroup> {
        self.groups.iter_mut().find(|group| group.id == id)
    }

    fn normalize_group_order(&mut self) {
        self.groups.sort_by_key(|group| group.sort_order);
        for (index, group) in self.groups.iter_mut().enumerate() {
            group.sort_order = index as u32;
        }
    }

    fn normalize_profile_order(&mut self, group_id: Option<&str>) {
        let mut positions = self
            .profiles
            .iter()
            .enumerate()
            .filter(|(_, profile)| profile.group_id.as_deref() == group_id)
            .map(|(index, profile)| (index, profile.sort_order))
            .collect::<Vec<_>>();
        positions.sort_by_key(|(_, order)| *order);
        for (sort_order, (index, _)) in positions.into_iter().enumerate() {
            self.profiles[index].sort_order = sort_order as u32;
        }
    }
}

fn has_duplicates(ids: &[String]) -> bool {
    ids.iter()
        .enumerate()
        .any(|(index, id)| ids[index + 1..].iter().any(|other| other == id))
}

fn validate_id(id: &str, prefix: &str) -> Result<(), InventoryError> {
    if id.len() != prefix.len() + 32
        || !id.starts_with(prefix)
        || !id[prefix.len()..]
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(InventoryError::InvalidId);
    }
    Ok(())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn draft(name: &str, group_id: Option<String>) -> NewSshProfile {
        NewSshProfile {
            name: name.to_owned(),
            host: format!("{name}.example.com"),
            username: "root".to_owned(),
            group_id,
            ..Default::default()
        }
    }

    #[test]
    fn 分组名称会trim且大小写不敏感唯一() {
        let mut inventory = SshInventory::default();
        let id = inventory.create_group("  Production  ").unwrap();
        assert_eq!(inventory.group(&id).unwrap().name, "Production");
        assert_eq!(
            inventory.create_group("production"),
            Err(InventoryError::DuplicateGroupName)
        );
        assert_eq!(
            inventory.create_group(" "),
            Err(InventoryError::EmptyGroupName)
        );
        assert_eq!(
            inventory.create_group(&"分".repeat(51)),
            Err(InventoryError::GroupNameTooLong)
        );
    }

    #[test]
    fn 删除分组只把服务器移到未分组() {
        let mut inventory = SshInventory::default();
        let loose_a = inventory.create_profile(draft("loose-a", None)).unwrap();
        let loose_b = inventory.create_profile(draft("loose-b", None)).unwrap();
        let group_id = inventory.create_group("生产").unwrap();
        let grouped_a = inventory
            .create_profile(draft("db-a", Some(group_id.clone())))
            .unwrap();
        let grouped_b = inventory
            .create_profile(draft("db-b", Some(group_id.clone())))
            .unwrap();
        inventory.delete_group(&group_id).unwrap();
        assert!(inventory.group(&group_id).is_none());
        assert_eq!(inventory.profile(&grouped_a).unwrap().group_id, None);
        let ids = inventory
            .profiles_in_group(None)
            .into_iter()
            .map(|profile| profile.id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            ids,
            vec![
                loose_a.as_str(),
                loose_b.as_str(),
                grouped_a.as_str(),
                grouped_b.as_str()
            ]
        );
    }

    #[test]
    fn 跨组拖放与组内排序互不污染未分组() {
        let mut inventory = SshInventory::default();
        let left = inventory.create_group("左").unwrap();
        let right = inventory.create_group("右").unwrap();
        let a = inventory
            .create_profile(draft("a", Some(left.clone())))
            .unwrap();
        let b = inventory
            .create_profile(draft("b", Some(left.clone())))
            .unwrap();
        let loose = inventory.create_profile(draft("loose", None)).unwrap();

        inventory.move_profile(&b, Some(&right), 0).unwrap();
        inventory.move_profile(&a, Some(&right), 0).unwrap();
        let right_ids = inventory
            .profiles_in_group(Some(&right))
            .into_iter()
            .map(|profile| profile.id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(right_ids, vec![a.as_str(), b.as_str()]);
        assert_eq!(
            inventory.profiles_in_group(None)[0].id.as_str(),
            loose.as_str()
        );

        inventory.move_profile(&b, None, 0).unwrap();
        let loose_ids = inventory
            .profiles_in_group(None)
            .into_iter()
            .map(|profile| profile.id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(loose_ids, vec![b.as_str(), loose.as_str()]);
    }

    #[test]
    fn 同组拖放下标按移除当前项后的列表解释() {
        let mut inventory = SshInventory::default();
        let group = inventory.create_group("组").unwrap();
        let a = inventory
            .create_profile(draft("a", Some(group.clone())))
            .unwrap();
        let b = inventory
            .create_profile(draft("b", Some(group.clone())))
            .unwrap();
        let c = inventory
            .create_profile(draft("c", Some(group.clone())))
            .unwrap();

        inventory.move_profile(&a, Some(&group), 1).unwrap();
        let ids = inventory
            .profiles_in_group(Some(&group))
            .into_iter()
            .map(|profile| profile.id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(ids, vec![b.as_str(), a.as_str(), c.as_str()]);
    }

    #[test]
    fn 服务器可更新显式重排并删除() {
        let mut inventory = SshInventory::default();
        let group_id = inventory.create_group("测试").unwrap();
        let a = inventory
            .create_profile(draft("a", Some(group_id.clone())))
            .unwrap();
        let b = inventory
            .create_profile(draft("b", Some(group_id.clone())))
            .unwrap();

        let mut updated = draft("a-renamed", Some(group_id.clone()));
        updated.host = "new.example.com".to_owned();
        inventory.update_profile(&a, updated).unwrap();
        assert_eq!(inventory.profile(&a).unwrap().name, "a-renamed");
        assert_eq!(inventory.profile(&a).unwrap().host, "new.example.com");

        inventory
            .reorder_profiles_in_group(Some(&group_id), &[b.clone(), a.clone()])
            .unwrap();
        let ordered = inventory
            .profiles_in_group(Some(&group_id))
            .into_iter()
            .map(|profile| profile.id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(ordered, vec![b.as_str(), a.as_str()]);

        inventory.delete_profile(&b).unwrap();
        assert!(inventory.profile(&b).is_none());
        assert_eq!(inventory.profile(&a).unwrap().sort_order, 0);
    }

    #[test]
    fn 远端更新endpoint时不会迁移旧主机密钥信任() {
        let mut inventory = SshInventory::default();
        let trust = crate::ssh::HostKeyTrust {
            algorithm: "ssh-ed25519".to_owned(),
            fingerprint: "SHA256:abcdefghijklmnopqrstuvwxyz0123456789ABC".to_owned(),
        };
        let mut original = draft("original", None);
        original.trusted_host_key = Some(trust.clone());
        let profile_id = inventory.create_profile(original).unwrap();
        let mut remote = inventory.profile(&profile_id).unwrap().clone();
        remote.host = "other.example.com".to_owned();
        remote.trusted_host_key = Some(trust);

        inventory.apply_synced_profile(remote).unwrap();

        let profile = inventory.profile(&profile_id).unwrap();
        assert_eq!(profile.host, "other.example.com");
        assert!(profile.trusted_host_key.is_none());
    }

    #[test]
    fn 新建和加载都会拒绝非法服务器字段() {
        let mut inventory = SshInventory::default();
        let mut invalid = draft("bad", None);
        invalid.host = "host\ninjection".to_owned();
        assert_eq!(
            inventory.create_profile(invalid),
            Err(InventoryError::InvalidHost)
        );

        let profile_id = inventory.create_profile(draft("valid", None)).unwrap();
        inventory
            .profiles
            .iter_mut()
            .find(|profile| profile.id == profile_id)
            .unwrap()
            .port = 0;
        assert_eq!(
            inventory.validate_loaded(),
            Err(InventoryError::InvalidPort)
        );
    }

    #[test]
    fn 加载拒绝非法id和超长字段() {
        let mut inventory = SshInventory::default();
        let profile_id = inventory.create_profile(draft("valid", None)).unwrap();
        let profile = inventory
            .profiles
            .iter_mut()
            .find(|profile| profile.id == profile_id)
            .unwrap();
        profile.id = "../outside".to_owned();
        assert_eq!(inventory.validate_loaded(), Err(InventoryError::InvalidId));

        let mut inventory = SshInventory::default();
        let mut invalid = draft("valid", None);
        invalid.initial_directory = Some("x".repeat(MAX_INITIAL_DIRECTORY_CHARS + 1));
        assert_eq!(
            inventory.create_profile(invalid),
            Err(InventoryError::InvalidInitialDirectory)
        );
    }
}
