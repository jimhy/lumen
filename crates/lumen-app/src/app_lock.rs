//! Lumen 本机应用锁。
//!
//! 安全边界：
//! - 配置独立保存在 `app_lock.json`，绝不进入通用 `settings.json` 或云同步；
//! - 默认关闭；文件缺失等价于从未启用；
//! - 只保存 Argon2id PHC 校验值，Windows 再用当前用户 DPAPI 保护；
//! - 密码哈希/校验只在后台线程执行，明文缓冲离开作用域时清零；
//! - 锁只门控本机 UI/输入，PTY 与已授权远程控制由 main 保持运行。

use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use rand_core::OsRng;
use serde::{Deserialize, Serialize};
use zeroize::{Zeroize, Zeroizing};

const FILE_VERSION: u32 = 1;
const CONFIG_FILE: &str = "app_lock.json";
const MIN_PASSWORD_CHARS: usize = 8;
const MAX_PASSWORD_CHARS: usize = 128;
const MAX_RETRY: Duration = Duration::from_secs(15 * 60);

/// 自动锁定可选分钟数。0 表示关闭。
pub const IDLE_TIMEOUT_MINUTES: [u32; 8] = [0, 1, 5, 10, 15, 30, 60, 120];

/// 应用内手动锁定快捷键。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum LockShortcut {
    #[default]
    CtrlShiftL,
    CtrlAltL,
    CtrlShiftK,
    CtrlAltK,
}

impl LockShortcut {
    pub const ALL: [Self; 4] = [
        Self::CtrlShiftL,
        Self::CtrlAltL,
        Self::CtrlShiftK,
        Self::CtrlAltK,
    ];

    pub const fn label(self) -> &'static str {
        match self {
            Self::CtrlShiftL => "Ctrl+Shift+L",
            Self::CtrlAltL => "Ctrl+Alt+L",
            Self::CtrlShiftK => "Ctrl+Shift+K",
            Self::CtrlAltK => "Ctrl+Alt+K",
        }
    }

    /// 物理键 + 修饰键是否精确匹配当前组合。
    pub fn matches(
        self,
        modifiers: winit::keyboard::ModifiersState,
        key: winit::keyboard::PhysicalKey,
    ) -> bool {
        use winit::keyboard::{KeyCode, PhysicalKey};
        let (shift, alt, code) = match self {
            Self::CtrlShiftL => (true, false, KeyCode::KeyL),
            Self::CtrlAltL => (false, true, KeyCode::KeyL),
            Self::CtrlShiftK => (true, false, KeyCode::KeyK),
            Self::CtrlAltK => (false, true, KeyCode::KeyK),
        };
        modifiers.control_key()
            && modifiers.shift_key() == shift
            && modifiers.alt_key() == alt
            && !modifiers.super_key()
            && key == PhysicalKey::Code(code)
    }
}

/// 设置页一次性写入的非密码偏好。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Preferences {
    pub shortcut: LockShortcut,
    pub idle_timeout_minutes: u32,
    pub lock_on_start: bool,
    pub lock_on_resume: bool,
}

/// 磁盘格式。`protected_verifier` 在 Windows 是 `dpapi:<base64>`，
/// 其他平台是 `phc:<base64>`；两者都不是明文密码。
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppLockFile {
    version: u32,
    enabled: bool,
    locked: bool,
    protected_verifier: Option<String>,
    shortcut: LockShortcut,
    idle_timeout_minutes: u32,
    lock_on_start: bool,
    lock_on_resume: bool,
    failed_attempts: u32,
    retry_remaining_ms: u64,
    /// 文件存在但无法读取/解码时置位；仅运行时使用，不写盘。
    #[serde(skip)]
    storage_error: bool,
}

impl Default for AppLockFile {
    fn default() -> Self {
        Self {
            version: FILE_VERSION,
            enabled: false,
            locked: false,
            protected_verifier: None,
            shortcut: LockShortcut::default(),
            idle_timeout_minutes: 0,
            lock_on_start: false,
            lock_on_resume: true,
            failed_attempts: 0,
            retry_remaining_ms: 0,
            storage_error: false,
        }
    }
}

impl AppLockFile {
    fn fail_closed() -> Self {
        Self {
            enabled: true,
            locked: true,
            storage_error: true,
            ..Self::default()
        }
    }

    fn sanitize(mut self) -> Self {
        self.version = FILE_VERSION;
        if !IDLE_TIMEOUT_MINUTES.contains(&self.idle_timeout_minutes) {
            self.idle_timeout_minutes = 0;
        }
        self.retry_remaining_ms = self.retry_remaining_ms.min(MAX_RETRY.as_millis() as u64);
        if !self.enabled {
            // disabled 文件只可能来自旧版/人工编辑；与“文件缺失=关闭”
            // 统一成干净默认态，残留 marker 不得误锁。
            return Self::default();
        }
        if self.protected_verifier.as_deref().is_none_or(str::is_empty) {
            self.locked = true;
            self.storage_error = true;
        }
        if self.failed_attempts < 5 {
            self.retry_remaining_ms = 0;
        }
        self
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    #[cfg(test)]
    fn locked_marker(&self) -> bool {
        self.locked
    }

    pub fn storage_error(&self) -> bool {
        self.storage_error
    }

    pub fn shortcut(&self) -> LockShortcut {
        self.shortcut
    }

    pub fn idle_timeout_minutes(&self) -> u32 {
        self.idle_timeout_minutes
    }

    pub fn lock_on_resume(&self) -> bool {
        self.lock_on_resume
    }

    pub fn preferences(&self) -> Preferences {
        Preferences {
            shortcut: self.shortcut,
            idle_timeout_minutes: self.idle_timeout_minutes,
            lock_on_start: self.lock_on_start,
            lock_on_resume: self.lock_on_resume,
        }
    }

    fn path() -> Option<PathBuf> {
        crate::paths::data_file(CONFIG_FILE)
    }

    fn load() -> Self {
        match Self::path() {
            Some(path) => Self::load_from(&path),
            None => {
                log::warn!("应用锁数据目录不可用；本次按未启用处理");
                Self::default()
            }
        }
    }

    fn load_from(path: &Path) -> Self {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == ErrorKind::NotFound => return Self::default(),
            Err(e) => {
                log::error!("应用锁文件存在但读取失败，进入安全锁定态: {e}");
                return Self::fail_closed();
            }
        };
        match serde_json::from_str::<Self>(text.trim_start_matches('\u{feff}')) {
            Ok(file) => file.sanitize(),
            Err(e) => {
                log::error!("应用锁文件损坏，进入安全锁定态: {e}");
                Self::fail_closed()
            }
        }
    }

    fn save_to(&self, path: &Path) -> Result<(), String> {
        let Some(dir) = path.parent() else {
            return Err("应用锁路径无父目录".to_owned());
        };
        std::fs::create_dir_all(dir).map_err(|e| format!("创建应用锁目录失败: {e}"))?;
        let json =
            serde_json::to_string_pretty(self).map_err(|e| format!("序列化应用锁失败: {e}"))?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, json).map_err(|e| format!("写应用锁临时文件失败: {e}"))?;
        std::fs::rename(&tmp, path).map_err(|e| format!("替换应用锁文件失败: {e}"))
    }

    fn save(&self) -> Result<(), String> {
        let path = Self::path().ok_or_else(|| "应用锁数据目录不可用".to_owned())?;
        self.save_to(&path)
    }
}

/// 主线程持有的应用锁运行态。
pub struct AppLock {
    file: AppLockFile,
    locked: bool,
    retry_until: Option<Instant>,
    crypto_busy: bool,
}

impl AppLock {
    pub fn load() -> Self {
        let file = AppLockFile::load();
        let now = Instant::now();
        let locked = file.enabled && (file.locked || file.lock_on_start || file.storage_error);
        let retry_until = (file.retry_remaining_ms > 0)
            .then(|| now + Duration::from_millis(file.retry_remaining_ms));
        let mut this = Self {
            file,
            locked,
            retry_until,
            crypto_busy: false,
        };
        // lock_on_start 也必须先落 marker，异常终止后仍保持锁定。
        if this.locked && !this.file.locked && !this.file.storage_error {
            if let Err(e) = this.persist_locked(true) {
                log::error!("启动锁定标记写盘失败，保持本次安全锁定: {e}");
                this.file.storage_error = true;
            }
        }
        this
    }

    #[cfg(test)]
    fn from_file(file: AppLockFile, now: Instant) -> Self {
        let file = file.sanitize();
        let locked = file.enabled && (file.locked || file.lock_on_start || file.storage_error);
        let retry_until = (file.retry_remaining_ms > 0)
            .then(|| now + Duration::from_millis(file.retry_remaining_ms));
        Self {
            file,
            locked,
            retry_until,
            crypto_busy: false,
        }
    }

    pub fn config(&self) -> &AppLockFile {
        &self.file
    }

    pub fn is_enabled(&self) -> bool {
        self.file.enabled
    }

    pub fn is_locked(&self) -> bool {
        self.locked
    }

    pub fn crypto_busy(&self) -> bool {
        self.crypto_busy
    }

    pub fn set_crypto_busy(&mut self, busy: bool) {
        self.crypto_busy = busy;
    }

    /// 校验值无法解密或密码线程无法启动时进入本次运行的失败关闭态。
    /// 该标志不覆盖磁盘配置，避免把瞬时运行错误永久写回；重启后会重新
    /// 尝试读取/解密，但本次运行绝不绕过锁屏。
    pub fn mark_storage_error(&mut self) {
        self.file.storage_error = true;
        self.locked = true;
        self.crypto_busy = false;
    }

    pub fn protected_verifier(&self) -> Option<String> {
        self.file.protected_verifier.clone()
    }

    /// 哈希并保护成功后启用。先成功写盘，再公开 enabled=true。
    pub fn finish_enable(&mut self, protected_verifier: String) -> Result<(), String> {
        let mut next = AppLockFile {
            enabled: true,
            protected_verifier: Some(protected_verifier),
            // 首次开启建议 10 分钟；用户仍可立即改成关闭或其他档位。
            idle_timeout_minutes: 10,
            ..AppLockFile::default()
        };
        next.save()?;
        next.storage_error = false;
        self.file = next;
        self.locked = false;
        self.retry_until = None;
        Ok(())
    }

    /// 当前密码验证成功后删除本机锁文件并回到严格默认关闭态。
    pub fn finish_disable(&mut self) -> Result<(), String> {
        if let Some(path) = AppLockFile::path() {
            match std::fs::remove_file(&path) {
                Ok(()) => {}
                Err(e) if e.kind() == ErrorKind::NotFound => {}
                Err(e) => return Err(format!("删除应用锁文件失败: {e}")),
            }
        }
        self.file = AppLockFile::default();
        self.locked = false;
        self.retry_until = None;
        Ok(())
    }

    /// 当前密码验证成功后替换受保护校验值。
    pub fn finish_change_password(&mut self, protected_verifier: String) -> Result<(), String> {
        let mut next = self.file.clone();
        next.protected_verifier = Some(protected_verifier);
        next.failed_attempts = 0;
        next.retry_remaining_ms = 0;
        next.storage_error = false;
        next.save()?;
        self.file = next;
        self.retry_until = None;
        Ok(())
    }

    pub fn apply_preferences(&mut self, prefs: Preferences) -> Result<(), String> {
        if !self.file.enabled {
            return Ok(());
        }
        let mut next = self.file.clone();
        next.shortcut = prefs.shortcut;
        next.idle_timeout_minutes = if IDLE_TIMEOUT_MINUTES.contains(&prefs.idle_timeout_minutes) {
            prefs.idle_timeout_minutes
        } else {
            0
        };
        next.lock_on_start = prefs.lock_on_start;
        next.lock_on_resume = prefs.lock_on_resume;
        next.save()?;
        self.file = next;
        Ok(())
    }

    /// 进入锁定态。marker 写盘成功后才对本地 UI 公开锁定，保证重启
    /// 不能绕过；已经锁定时幂等。
    pub fn lock(&mut self) -> Result<bool, String> {
        if !self.file.enabled || self.locked {
            return Ok(false);
        }
        self.persist_locked(true)?;
        self.locked = true;
        Ok(true)
    }

    fn persist_locked(&mut self, locked: bool) -> Result<(), String> {
        let mut next = self.file.clone();
        next.locked = locked;
        next.save()?;
        self.file = next;
        Ok(())
    }

    /// 正确密码结果：先落盘清 marker/退避，再公开业务 UI。
    pub fn unlock_success(&mut self) -> Result<(), String> {
        let mut next = self.file.clone();
        next.locked = false;
        next.failed_attempts = 0;
        next.retry_remaining_ms = 0;
        next.storage_error = false;
        next.save()?;
        self.file = next;
        self.retry_until = None;
        self.locked = false;
        Ok(())
    }

    /// 错误密码：递增并持久化失败次数和完整剩余等待。
    pub fn unlock_failure(&mut self, now: Instant) -> Result<(), String> {
        self.file.failed_attempts = self.file.failed_attempts.saturating_add(1);
        let delay = backoff_for_attempt(self.file.failed_attempts);
        self.file.retry_remaining_ms = u64::try_from(delay.as_millis()).unwrap_or(u64::MAX);
        self.retry_until = (!delay.is_zero()).then(|| now + delay);
        self.file.save()
    }

    /// 等待到点后清除“剩余时间”，保留失败次数供下一次失败递增。
    pub fn expire_retry_if_due(&mut self, now: Instant) {
        if self.retry_until.is_some_and(|until| now >= until) {
            self.retry_until = None;
            self.file.retry_remaining_ms = 0;
            if let Err(e) = self.file.save() {
                log::warn!("清除应用锁等待状态写盘失败: {e}");
            }
        }
    }

    pub fn retry_remaining(&self, now: Instant) -> Duration {
        self.retry_until
            .map_or(Duration::ZERO, |until| until.saturating_duration_since(now))
    }
}

/// 密码合法性错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PasswordPolicyError {
    TooShort,
    TooLong,
}

pub fn validate_password(password: &str) -> Result<(), PasswordPolicyError> {
    let count = password.chars().count();
    if count < MIN_PASSWORD_CHARS {
        Err(PasswordPolicyError::TooShort)
    } else if count > MAX_PASSWORD_CHARS {
        Err(PasswordPolicyError::TooLong)
    } else {
        Ok(())
    }
}

/// 后台密码作业。不要给此类型派生 Debug/Clone，防止误打印明文。
pub enum CryptoRequest {
    Enable {
        password: Zeroizing<String>,
    },
    Disable {
        password: Zeroizing<String>,
        protected_verifier: String,
    },
    ChangePassword {
        current_password: Zeroizing<String>,
        new_password: Zeroizing<String>,
        protected_verifier: String,
    },
    Unlock {
        password: Zeroizing<String>,
        protected_verifier: String,
    },
}

impl CryptoRequest {
    pub fn enable(password: String) -> Self {
        Self::Enable {
            password: Zeroizing::new(password),
        }
    }

    pub fn disable(password: String, protected_verifier: String) -> Self {
        Self::Disable {
            password: Zeroizing::new(password),
            protected_verifier,
        }
    }

    pub fn change(
        current_password: String,
        new_password: String,
        protected_verifier: String,
    ) -> Self {
        Self::ChangePassword {
            current_password: Zeroizing::new(current_password),
            new_password: Zeroizing::new(new_password),
            protected_verifier,
        }
    }

    pub fn unlock(password: String, protected_verifier: String) -> Self {
        Self::Unlock {
            password: Zeroizing::new(password),
            protected_verifier,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CryptoError {
    HashFailed,
    ProtectFailed,
    VerifierUnavailable,
}

pub enum CryptoResponse {
    Enable(Result<String, CryptoError>),
    Disable(Result<bool, CryptoError>),
    ChangePassword(Result<Option<String>, CryptoError>),
    Unlock(Result<bool, CryptoError>),
}

/// 在后台线程执行单个密码作业，完成后用现有 PtyWake 唤醒主循环。
pub fn spawn_crypto(
    request: CryptoRequest,
    tx: crossbeam_channel::Sender<CryptoResponse>,
    proxy: winit::event_loop::EventLoopProxy<crate::PtyWake>,
) -> bool {
    match std::thread::Builder::new()
        .name("lumen-app-lock".to_owned())
        .spawn(move || {
            let response = run_crypto(request);
            let _ = tx.send(response);
            let _ = proxy.send_event(crate::PtyWake);
        }) {
        Ok(_) => true,
        Err(e) => {
            log::error!("启动应用锁密码线程失败: {e}");
            false
        }
    }
}

fn run_crypto(request: CryptoRequest) -> CryptoResponse {
    match request {
        CryptoRequest::Enable { password } => {
            CryptoResponse::Enable(hash_and_protect(password.as_str()))
        }
        CryptoRequest::Disable {
            password,
            protected_verifier,
        } => CryptoResponse::Disable(verify_protected(password.as_str(), &protected_verifier)),
        CryptoRequest::ChangePassword {
            current_password,
            new_password,
            protected_verifier,
        } => match verify_protected(current_password.as_str(), &protected_verifier) {
            Ok(true) => {
                CryptoResponse::ChangePassword(hash_and_protect(new_password.as_str()).map(Some))
            }
            Ok(false) => CryptoResponse::ChangePassword(Ok(None)),
            Err(e) => CryptoResponse::ChangePassword(Err(e)),
        },
        CryptoRequest::Unlock {
            password,
            protected_verifier,
        } => CryptoResponse::Unlock(verify_protected(password.as_str(), &protected_verifier)),
    }
}

fn hash_and_protect(password: &str) -> Result<String, CryptoError> {
    let salt = SaltString::generate(&mut OsRng);
    let mut verifier = Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map_err(|_| CryptoError::HashFailed)?
        .to_string();
    let protected = protect_verifier(verifier.as_bytes());
    verifier.zeroize();
    protected
}

fn verify_protected(password: &str, protected: &str) -> Result<bool, CryptoError> {
    let mut verifier = unprotect_verifier(protected)?;
    let parsed = PasswordHash::new(&verifier).map_err(|_| CryptoError::VerifierUnavailable);
    let result = parsed.map(|hash| {
        Argon2::default()
            .verify_password(password.as_bytes(), &hash)
            .is_ok()
    });
    verifier.zeroize();
    result
}

#[cfg(target_os = "windows")]
fn protect_verifier(verifier: &[u8]) -> Result<String, CryptoError> {
    use base64::Engine as _;
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Cryptography::{
        CryptProtectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
    };

    const ENTROPY: &[u8] = b"Lumen.AppLock.Verifier.v1";
    let data_len = u32::try_from(verifier.len()).map_err(|_| CryptoError::ProtectFailed)?;
    let entropy_len = u32::try_from(ENTROPY.len()).map_err(|_| CryptoError::ProtectFailed)?;
    let input = CRYPT_INTEGER_BLOB {
        cbData: data_len,
        pbData: verifier.as_ptr().cast_mut(),
    };
    let entropy = CRYPT_INTEGER_BLOB {
        cbData: entropy_len,
        pbData: ENTROPY.as_ptr().cast_mut(),
    };
    let mut output = CRYPT_INTEGER_BLOB::default();
    // SAFETY: input/entropy 指向本调用期间有效的字节切片；output 由
    // CryptProtectData 分配，成功后按文档用 LocalFree 释放。
    let ok = unsafe {
        CryptProtectData(
            &input,
            std::ptr::null(),
            &entropy,
            std::ptr::null(),
            std::ptr::null(),
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output,
        )
    };
    if ok == 0 || output.pbData.is_null() {
        return Err(CryptoError::ProtectFailed);
    }
    // SAFETY: API 成功时 output 指向 cbData 字节的有效 LocalAlloc 缓冲。
    let bytes = unsafe { std::slice::from_raw_parts(output.pbData, output.cbData as usize) };
    let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
    // SAFETY: output.pbData 的所有权由 API 交给调用方，须 LocalFree。
    unsafe {
        LocalFree(output.pbData.cast());
    }
    Ok(format!("dpapi:{encoded}"))
}

#[cfg(target_os = "windows")]
fn unprotect_verifier(protected: &str) -> Result<String, CryptoError> {
    use base64::Engine as _;
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Cryptography::{
        CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
    };

    const ENTROPY: &[u8] = b"Lumen.AppLock.Verifier.v1";
    let encoded = protected
        .strip_prefix("dpapi:")
        .ok_or(CryptoError::VerifierUnavailable)?;
    let mut encrypted = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|_| CryptoError::VerifierUnavailable)?;
    let data_len = u32::try_from(encrypted.len()).map_err(|_| CryptoError::VerifierUnavailable)?;
    let entropy_len = u32::try_from(ENTROPY.len()).map_err(|_| CryptoError::VerifierUnavailable)?;
    let input = CRYPT_INTEGER_BLOB {
        cbData: data_len,
        pbData: encrypted.as_mut_ptr(),
    };
    let entropy = CRYPT_INTEGER_BLOB {
        cbData: entropy_len,
        pbData: ENTROPY.as_ptr().cast_mut(),
    };
    let mut output = CRYPT_INTEGER_BLOB::default();
    let mut description = std::ptr::null_mut();
    // SAFETY: input/entropy 指向本调用期间有效缓冲；output/description
    // 由 API 填写并在成功后用 LocalFree 释放。
    let ok = unsafe {
        CryptUnprotectData(
            &input,
            &mut description,
            &entropy,
            std::ptr::null(),
            std::ptr::null(),
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output,
        )
    };
    encrypted.zeroize();
    if ok == 0 || output.pbData.is_null() {
        if !description.is_null() {
            // SAFETY: 非空 description 是 API 分配的 LocalAlloc 缓冲。
            unsafe {
                LocalFree(description.cast());
            }
        }
        return Err(CryptoError::VerifierUnavailable);
    }
    // SAFETY: API 成功时 output 指向 cbData 字节。
    let bytes = unsafe { std::slice::from_raw_parts(output.pbData, output.cbData as usize) };
    let result = String::from_utf8(bytes.to_vec()).map_err(|_| CryptoError::VerifierUnavailable);
    // SAFETY: 两块非空输出均由 CryptUnprotectData 分配。
    unsafe {
        LocalFree(output.pbData.cast());
        if !description.is_null() {
            LocalFree(description.cast());
        }
    }
    result
}

#[cfg(not(target_os = "windows"))]
fn protect_verifier(verifier: &[u8]) -> Result<String, CryptoError> {
    use base64::Engine as _;
    Ok(format!(
        "phc:{}",
        base64::engine::general_purpose::STANDARD.encode(verifier)
    ))
}

#[cfg(not(target_os = "windows"))]
fn unprotect_verifier(protected: &str) -> Result<String, CryptoError> {
    use base64::Engine as _;
    let encoded = protected
        .strip_prefix("phc:")
        .ok_or(CryptoError::VerifierUnavailable)?;
    let mut bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|_| CryptoError::VerifierUnavailable)?;
    let result = String::from_utf8(bytes.clone()).map_err(|_| CryptoError::VerifierUnavailable);
    bytes.zeroize();
    result
}

/// 第 1–4 次无等待；第 5 次 30 秒，此后倍增，最高 15 分钟。
pub fn backoff_for_attempt(attempt: u32) -> Duration {
    if attempt < 5 {
        return Duration::ZERO;
    }
    let shift = attempt.saturating_sub(5).min(5);
    Duration::from_secs((30_u64 << shift).min(MAX_RETRY.as_secs()))
}

/// Windows 当前登录会话的系统级空闲时长。
#[cfg(target_os = "windows")]
pub fn system_idle_duration() -> Option<Duration> {
    use windows_sys::Win32::System::SystemInformation::GetTickCount;
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};

    let mut info = LASTINPUTINFO {
        cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32,
        dwTime: 0,
    };
    // SAFETY: info 是有效可写结构，cbSize 按 API 要求填写。
    if unsafe { GetLastInputInfo(&mut info) } == 0 {
        return None;
    }
    // GetLastInputInfo 与 GetTickCount 都是 u32 tick；wrapping_sub 正确
    // 处理约 49.7 天回绕。
    let elapsed_ms = unsafe { GetTickCount() }.wrapping_sub(info.dwTime);
    Some(Duration::from_millis(u64::from(elapsed_ms)))
}

#[cfg(not(target_os = "windows"))]
pub fn system_idle_duration() -> Option<Duration> {
    None
}

#[cfg(target_os = "windows")]
pub fn caps_lock_on() -> bool {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetKeyState, VK_CAPITAL};
    // SAFETY: GetKeyState 读取当前线程键盘状态，无指针参数。
    unsafe { GetKeyState(i32::from(VK_CAPITAL)) & 1 != 0 }
}

#[cfg(not(target_os = "windows"))]
pub fn caps_lock_on() -> bool {
    false
}

/// 锁定时阻止 DWM 缩略图、屏幕捕获和录屏继续暴露锁前交换链内容。
/// Lumen 远程控制使用内部终端镜像协议，不依赖系统截图，因此不受影响。
#[cfg(target_os = "windows")]
pub fn set_window_capture_protection(window: &winit::window::Window, locked: bool) {
    use windows_sys::Win32::Foundation::HWND;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        SetWindowDisplayAffinity, WDA_EXCLUDEFROMCAPTURE, WDA_MONITOR, WDA_NONE,
    };
    use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};

    let Ok(handle) = window.window_handle() else {
        log::warn!("应用锁无法获取窗口句柄，未能切换捕获保护");
        return;
    };
    let RawWindowHandle::Win32(win32) = handle.as_raw() else {
        return;
    };
    let hwnd = win32.hwnd.get() as HWND;
    let affinity = if locked {
        WDA_EXCLUDEFROMCAPTURE
    } else {
        WDA_NONE
    };
    // SAFETY: hwnd 属于当前进程的顶层 winit 窗口，调用期间有效。
    let mut ok = unsafe { SetWindowDisplayAffinity(hwnd, affinity) } != 0;
    if locked && !ok {
        // Windows 10 2004 之前不识别 EXCLUDEFROMCAPTURE；退回 WDA_MONITOR，
        // 至少让 DWM 捕获显示空白而不是锁前业务帧。
        ok = unsafe { SetWindowDisplayAffinity(hwnd, WDA_MONITOR) } != 0;
    }
    if !ok {
        log::warn!("应用锁切换窗口捕获保护失败（locked={locked}）");
    }
}

#[cfg(not(target_os = "windows"))]
pub fn set_window_capture_protection(_window: &winit::window::Window, _locked: bool) {}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "lumen_app_lock_{}_{}.json",
            std::process::id(),
            name
        ))
    }

    #[test]
    fn 默认严格关闭且不锁() {
        let file = AppLockFile::default();
        let state = AppLock::from_file(file, Instant::now());
        assert!(!state.is_enabled());
        assert!(!state.is_locked());
        assert_eq!(state.config().shortcut(), LockShortcut::CtrlShiftL);
        assert_eq!(state.config().idle_timeout_minutes(), 0);
    }

    #[test]
    fn 文件缺失默认关闭_损坏文件安全锁定() {
        let missing = temp_path("missing");
        let _ = std::fs::remove_file(&missing);
        assert!(!AppLockFile::load_from(&missing).enabled());

        let broken = temp_path("broken");
        std::fs::write(&broken, "{broken").unwrap();
        let loaded = AppLockFile::load_from(&broken);
        assert!(loaded.enabled());
        assert!(loaded.locked_marker());
        assert!(loaded.storage_error());
        let _ = std::fs::remove_file(&broken);
    }

    #[test]
    fn 密码长度按unicode字符而非字节() {
        assert_eq!(
            validate_password("短密码123"),
            Err(PasswordPolicyError::TooShort)
        );
        assert!(validate_password("中文密码 123!").is_ok());
        assert!(validate_password(&"界".repeat(128)).is_ok());
        assert_eq!(
            validate_password(&"界".repeat(129)),
            Err(PasswordPolicyError::TooLong)
        );
    }

    #[test]
    fn 失败退避档位正确且封顶() {
        let secs: Vec<u64> = (1..=11).map(|n| backoff_for_attempt(n).as_secs()).collect();
        assert_eq!(secs, [0, 0, 0, 0, 30, 60, 120, 240, 480, 900, 900]);
    }

    #[test]
    fn 重载后仍执行完整剩余等待() {
        let now = Instant::now();
        let file = AppLockFile {
            enabled: true,
            locked: true,
            protected_verifier: Some("test".to_owned()),
            failed_attempts: 5,
            retry_remaining_ms: 30_000,
            ..AppLockFile::default()
        };
        let state = AppLock::from_file(file, now);
        assert!(state.retry_remaining(now) >= Duration::from_secs(30));
    }

    #[test]
    fn 配置往返不含明文密码字段() {
        let p = temp_path("roundtrip");
        let file = AppLockFile {
            enabled: true,
            protected_verifier: Some("dpapi:opaque".to_owned()),
            idle_timeout_minutes: 10,
            ..AppLockFile::default()
        };
        file.save_to(&p).unwrap();
        let text = std::fs::read_to_string(&p).unwrap();
        assert!(!text.contains("\"password\""));
        assert!(!text.contains("password_hash"));
        let loaded = AppLockFile::load_from(&p);
        assert!(loaded.enabled());
        assert_eq!(loaded.idle_timeout_minutes(), 10);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn 配置可原子覆盖并读回最新锁标记() {
        let p = temp_path("replace");
        let mut file = AppLockFile {
            enabled: true,
            protected_verifier: Some("dpapi:opaque".to_owned()),
            ..AppLockFile::default()
        };
        file.save_to(&p).unwrap();
        file.locked = true;
        file.save_to(&p).unwrap();
        assert!(AppLockFile::load_from(&p).locked_marker());
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn 锁定快捷键要求精确修饰键() {
        use winit::keyboard::{KeyCode, ModifiersState, PhysicalKey};

        let ctrl_shift = ModifiersState::CONTROL | ModifiersState::SHIFT;
        assert!(LockShortcut::CtrlShiftL.matches(ctrl_shift, PhysicalKey::Code(KeyCode::KeyL)));
        assert!(!LockShortcut::CtrlShiftL.matches(
            ctrl_shift | ModifiersState::ALT,
            PhysicalKey::Code(KeyCode::KeyL)
        ));
        assert!(!LockShortcut::CtrlShiftL.matches(ctrl_shift, PhysicalKey::Code(KeyCode::KeyK)));
    }

    #[test]
    fn argon2校验正确错误且随机盐不同() {
        let one = hash_and_protect("正确的应用锁密码").unwrap();
        let two = hash_and_protect("正确的应用锁密码").unwrap();
        assert_ne!(one, two);
        assert!(verify_protected("正确的应用锁密码", &one).unwrap());
        assert!(!verify_protected("错误的应用锁密码", &one).unwrap());
    }
}
