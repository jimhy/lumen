//! 全量文案结构体（编译期完备性保证）。
//!
//! # 新增文案纪律
//! 每新增一条用户可见文案：
//! 1. 在 [`Strings`] 加对应字段（`pub xxx: &'static str`）；
//! 2. 在 [`super::zh_cn`]、[`super::zh_tw`]、[`super::en`] 三个文件里
//!    各填一条——只要有一个实例缺字段，**编译就会报错**，这是本方案的
//!    核心保证，不依赖运行期检查；
//! 3. 插值文案用 `{}` 单参或 `{0}` `{1}` 双参占位符，调用方用
//!    [`super::fmt1`] / [`super::fmt2`] 组装。

/// 全量 UI 文案（三语实例：[`super::zh_cn::STRINGS`] /
/// [`super::zh_tw::STRINGS`] / [`super::en::STRINGS`]）。
///
/// 缺任何字段 → 编译错误：无法在运行期出现翻译遗漏。
pub struct Strings {
    // ── 侧栏 / 窗格标题栏 ───────────────────────────────────────────
    /// "会话" 侧栏分组标签
    pub sidebar_sessions: &'static str,
    /// 右键菜单"重命名"
    pub menu_rename: &'static str,
    /// 右键菜单"关闭"
    pub menu_close: &'static str,
    // sidebar_settings_btn / sidebar_settings_tip / sidebar_new_session_btn
    // 已于 R8 删除（底部按钮区删除，入口改为头像菜单 + 侧栏标题栏小「＋」）。
    /// 窗格 ✕ tooltip
    pub pane_close_tip: &'static str,
    /// 还原窗格 tooltip（最大化态）
    pub pane_restore_tip: &'static str,
    /// 最大化窗格 tooltip（普通态）
    pub pane_maximize_tip: &'static str,
    /// shell 忙 toast "Shell 正忙，未执行 cd"
    pub shell_busy_cd: &'static str,

    // ── 顶栏 ─────────────────────────────────────────────────────────
    /// 窗控：最小化按钮 tooltip
    pub wc_minimize: &'static str,
    /// 窗控：最大化按钮 tooltip（普通态）
    pub wc_maximize: &'static str,
    /// 窗控：还原按钮 tooltip（最大化态）
    pub wc_restore: &'static str,
    /// 窗控：关闭按钮 tooltip
    pub wc_close: &'static str,
    /// 新增窗格 tooltip "新增窗格 (Ctrl+Shift+D)"
    pub topbar_new_pane_tip: &'static str,
    /// 新增窗格禁用 tooltip，单参 `{}`：MAX_PANES 数字
    pub topbar_max_panes_fmt: &'static str,
    /// 头像 tooltip（未登录态）"未登录"
    pub topbar_not_logged_in: &'static str,
    /// 头像 tooltip（登录态过期）"登录态已过期，请重新登录"
    pub topbar_session_expired: &'static str,
    /// 头像菜单红字项（登录态过期，点此重登）"登录过期，点此重新登录"
    pub menu_session_expired: &'static str,
    /// 头像菜单 Settings
    pub menu_settings: &'static str,
    /// 头像菜单 Keyboard shortcuts
    pub menu_keyboard_shortcuts: &'static str,
    /// 头像菜单 Documentation
    pub menu_documentation: &'static str,
    /// 头像菜单：检查更新（无可用更新时）
    pub menu_check_update: &'static str,
    /// 头像菜单：更新到 vX.Y.Z（有就绪更新时，fmt1：版本号）
    pub menu_update_to_fmt: &'static str,
    /// 头像菜单：更新日志（打开 GitHub Releases）
    pub menu_whats_new: &'static str,
    /// 头像菜单：反馈（打开 GitHub Issues）
    pub menu_feedback: &'static str,
    /// 头像菜单 Log out
    pub menu_log_out: &'static str,
    /// 头像菜单 Log in
    pub menu_log_in: &'static str,

    // ── 设置页 ───────────────────────────────────────────────────────
    /// 设置页顶栏标题 "Settings"
    pub settings_title: &'static str,
    /// 设置页关闭按钮 tooltip
    pub settings_close: &'static str,
    /// 导航 "Account"
    pub nav_account: &'static str,
    /// 导航 "Appearance"
    pub nav_appearance: &'static str,
    /// 导航 "Keyboard shortcuts"
    pub nav_keyboard_shortcuts: &'static str,
    /// 导航 "Network"（网络代理）
    pub nav_network: &'static str,
    /// 导航 "Security"
    pub nav_security: &'static str,
    /// 导航 "About"
    pub nav_about: &'static str,
    // Account 页
    /// 未登录文字 "未登录"
    pub account_not_logged_in: &'static str,
    /// 未登录副文字
    pub account_not_logged_in_sub: &'static str,
    /// Log out 按钮
    pub account_log_out: &'static str,
    /// Log in 按钮
    pub account_log_in: &'static str,
    // Appearance 页
    /// Appearance heading
    pub appearance_heading: &'static str,
    /// Themes 组标题
    pub appearance_themes: &'static str,
    /// "Sync with OS" 开关标签
    pub appearance_sync_with_os: &'static str,
    /// Sync 副文字
    pub appearance_sync_sub: &'static str,
    /// Sync 开启时的双槽说明，双参 `{0}`=深色主题名 `{1}`=浅色主题名
    pub appearance_sync_slots_fmt: &'static str,
    /// Current theme 标签
    pub appearance_current_theme: &'static str,
    /// Text 组标题
    pub appearance_text: &'static str,
    /// 终端字体标签
    pub appearance_font_family: &'static str,
    /// 字体下拉"自定义…"
    pub appearance_font_custom: &'static str,
    /// 字体下拉"自动（系统等宽）"
    pub appearance_font_auto: &'static str,
    /// 字体输入框 hint
    pub appearance_font_hint: &'static str,
    /// "应用" 按钮
    pub appearance_font_apply: &'static str,
    /// 终端字号标签
    pub appearance_font_size: &'static str,
    /// 背景图片组标题
    pub appearance_bg_title: &'static str,
    /// 启用背景图片开关标签
    pub appearance_bg_enable: &'static str,
    /// "选择图片…" 按钮
    pub appearance_bg_pick: &'static str,
    /// rfd 对话框标题 "选择背景图片"
    pub appearance_bg_dialog_title: &'static str,
    /// rfd 过滤器名 "图片文件"
    pub appearance_bg_filter_name: &'static str,
    /// "清除" 按钮
    pub appearance_bg_clear: &'static str,
    /// "未选择图片" 占位
    pub appearance_bg_none: &'static str,
    /// 不透明度标签
    pub appearance_bg_opacity: &'static str,
    /// 暗化标签
    pub appearance_bg_dim: &'static str,
    /// 暗化说明
    pub appearance_bg_dim_sub: &'static str,
    /// 主题卡徽标"浅色"
    pub appearance_theme_badge_light: &'static str,
    /// 主题卡徽标"深色"
    pub appearance_theme_badge_dark: &'static str,
    // Keyboard shortcuts 页
    /// Keyboard shortcuts heading
    pub shortcuts_heading: &'static str,
    pub shortcuts_hint: &'static str,
    pub shortcuts_capture: &'static str,
    pub shortcuts_reset: &'static str,
    pub shortcuts_reset_all: &'static str,
    pub shortcuts_conflict_fmt: &'static str,
    pub shortcuts_invalid: &'static str,
    pub shortcuts_lock_managed: &'static str,
    // 快捷键动作名称
    pub shortcut_new_session: &'static str,
    pub shortcut_close_session: &'static str,
    pub shortcut_next_session: &'static str,
    pub shortcut_previous_session: &'static str,
    pub shortcut_new_pane: &'static str,
    pub shortcut_close_pane: &'static str,
    pub shortcut_toggle_maximize_pane: &'static str,
    pub shortcut_filetree_toggle: &'static str,
    pub shortcut_settings_toggle: &'static str,
    pub shortcut_toggle_classic_mode: &'static str,
    pub shortcut_previous_block: &'static str,
    pub shortcut_next_block: &'static str,
    pub shortcut_history_search: &'static str,
    pub shortcut_copy_or_interrupt: &'static str,
    pub shortcut_paste: &'static str,
    pub shortcut_alternate_paste: &'static str,
    pub shortcut_scroll_up: &'static str,
    pub shortcut_scroll_down: &'static str,
    pub shortcut_close_settings: &'static str,
    // Security 页
    /// Security heading
    pub security_heading: &'static str,
    /// 应用锁分组标题
    pub security_app_lock: &'static str,
    /// 应用锁默认关闭说明
    pub security_lock_disabled_hint: &'static str,
    /// 应用锁已启用说明
    pub security_lock_enabled_hint: &'static str,
    /// 启用应用锁
    pub security_enable: &'static str,
    /// 关闭应用锁
    pub security_disable: &'static str,
    /// 修改应用锁密码
    pub security_change_password: &'static str,
    /// 立即锁定
    pub security_lock_now: &'static str,
    /// 当前密码
    pub security_current_password: &'static str,
    /// 新密码
    pub security_new_password: &'static str,
    /// 确认密码
    pub security_confirm_password: &'static str,
    /// 密码长度提示（8–128 字符）
    pub security_password_hint: &'static str,
    /// 密码过短
    pub security_password_too_short: &'static str,
    /// 密码过长
    pub security_password_too_long: &'static str,
    /// 两次密码不一致
    pub security_password_mismatch: &'static str,
    /// 当前密码错误
    pub security_current_password_wrong: &'static str,
    /// 泛化操作失败提示
    pub security_operation_failed: &'static str,
    /// 上锁快捷键
    pub security_shortcut: &'static str,
    /// 自动锁定
    pub security_auto_lock: &'static str,
    /// 自动锁定关闭
    pub security_auto_lock_off: &'static str,
    /// 自动锁定分钟格式（{}=分钟数）
    pub security_auto_lock_minutes_fmt: &'static str,
    /// 启动时锁定
    pub security_lock_on_start: &'static str,
    /// 系统恢复时锁定
    pub security_lock_on_resume: &'static str,
    /// 安全设置对话框取消
    pub security_cancel: &'static str,
    /// 安全设置对话框保存
    pub security_save: &'static str,
    // About 页
    /// About heading
    pub about_heading: &'static str,
    /// 版本标签，单参 `{}`：版本字符串
    pub about_version_fmt: &'static str,

    // ── 应用锁屏 ─────────────────────────────────────────────────────
    /// 锁屏标题
    pub lock_screen_title: &'static str,
    /// 锁屏密码输入提示
    pub lock_screen_password_hint: &'static str,
    /// 显示锁屏密码
    pub lock_screen_show_password: &'static str,
    /// 隐藏锁屏密码
    pub lock_screen_hide_password: &'static str,
    /// 解锁按钮
    pub lock_screen_unlock: &'static str,
    /// 密码验证中
    pub lock_screen_verifying: &'static str,
    /// 锁屏密码错误
    pub lock_screen_wrong_password: &'static str,
    /// 锁屏重试倒计时（{}=秒数）
    pub lock_screen_retry_fmt: &'static str,
    /// Caps Lock 提示
    pub lock_screen_caps_lock: &'static str,
    /// 已授权远程控制仍在进行（不得包含设备或会话信息）
    pub lock_screen_remote_active: &'static str,
    /// 应用锁存储损坏/不可读的泛化错误
    pub lock_screen_storage_error: &'static str,

    // ── 语言设置组（设置页 Appearance 内）───────────────────────────
    /// "语言 / Language" 组标题
    pub appearance_language: &'static str,

    // ── 登录页 ───────────────────────────────────────────────────────
    /// 登录副标题
    pub login_subtitle: &'static str,
    /// 邮箱 hint
    pub login_email_hint: &'static str,
    /// 密码 hint
    pub login_password_hint: &'static str,
    /// 登录按钮
    pub login_btn: &'static str,
    /// 注册按钮文案（注册模式）
    pub login_register_btn: &'static str,
    /// 确认密码输入框 hint（注册模式）
    pub login_password_confirm_hint: &'static str,
    /// 服务器地址输入框 hint（M5.2 局域网两机互联）
    pub server_url_placeholder: &'static str,
    /// 服务器设置分组标题
    pub server_section: &'static str,
    /// 服务器地址帮助说明
    pub server_hint: &'static str,
    /// 切到注册的链接（登录模式底部）
    pub login_to_register: &'static str,
    /// 切到登录的链接（注册模式底部）
    pub login_to_login: &'static str,
    /// 两次密码不一致（注册本地校验）
    pub login_err_password_mismatch: &'static str,
    /// 账号不存在（登录，提示去注册）
    pub login_err_user_not_found: &'static str,
    /// 邮箱已注册（注册，提示去登录）
    pub login_err_email_taken: &'static str,
    /// 邮箱或密码错误（登录）
    pub login_err_bad_credentials: &'static str,

    // ── 文件树 UI ────────────────────────────────────────────────────
    /// 刷新 tooltip（旧·工具条全局刷新按钮；P15 对齐远程树后按钮已移除，
    /// 目录级刷新走行内图标、tooltip 复用 `remote_refresh_dir_tip`）
    // ALLOW: P15 起无代码读取此字段；保留字段是为了维持三语文件的编译期
    // 完备性检查（删字段会导致 zh_cn/zh_tw/en 三语实例编译报错），与同
    // 结构体 footer_running_text 同款处理。
    #[allow(dead_code)]
    pub filetree_refresh_tip: &'static str,
    /// 搜索按钮 tooltip
    pub filetree_search_tip: &'static str,
    /// 树根无 cwd 时的占位标题 "文件"
    pub filetree_root_placeholder: &'static str,
    /// 搜索输入框 hint
    pub filetree_search_hint: &'static str,
    /// shell 忙碌轻提示（树内）
    pub filetree_shell_busy: &'static str,
    /// 等待 shell 上报路径占位
    pub filetree_waiting_cwd: &'static str,
    /// 搜索中占位
    pub filetree_searching: &'static str,
    /// 无匹配项占位
    pub filetree_no_results: &'static str,
    /// 结果截断占位
    pub filetree_truncated: &'static str,
    /// 搜索结果截断 toast
    pub filetree_search_truncated_toast: &'static str,
    /// 溢出行，单参 `{}`：未显示条目数
    pub filetree_overflow_fmt: &'static str,
    /// "无法读取" 占位
    pub filetree_unreadable: &'static str,
    /// "加载中…" 占位
    pub filetree_loading: &'static str,
    /// 本地与远程树工具条共用的「显示隐藏项」勾选框
    pub remote_show_hidden: &'static str,
    /// part3c-2 远程树目录行悬停刷新图标 tooltip（P15 起本地树行内刷新同款复用）
    pub remote_refresh_dir_tip: &'static str,
    /// part3c-2 远程视图未控制任何设备时的占位（远程 tab 未连接）
    pub remote_not_connected: &'static str,
    /// part3c-2 #5：开始从被控端获取文件 toast
    pub remote_fetch_started: &'static str,
    /// part3c-2 #5：获取文件失败 toast
    pub remote_fetch_failed: &'static str,
    /// part3c-2 #5：文件过大无法获取 toast
    pub remote_fetch_too_large: &'static str,
    /// part3c-2 #7：开始下载 toast
    pub remote_download_started: &'static str,
    /// part3c-2 #7：下载完成汇总 toast，三参 `{0}`完成 `{1}`跳过 `{2}`出错
    pub remote_download_done_fmt: &'static str,
    /// part3c-2 片5：开始上传 toast
    pub remote_upload_started: &'static str,
    /// part3c-2 片5：上传完成汇总 toast，三参 `{0}`完成 `{1}`跳过 `{2}`出错
    pub remote_upload_done_fmt: &'static str,
    /// part3c-2 #7：复制项后右键「粘贴到此目录」菜单
    pub remote_menu_paste: &'static str,
    /// part3c-2 #7：远程/本地树右键「复制」菜单
    pub remote_menu_copy: &'static str,
    /// 远程/SSH 文件右键：在 Lumen 内置文本编辑器中编辑
    pub filetree_menu_edit: &'static str,
    /// SSH 删除没有回收站能力，明确提示永久删除
    pub filetree_menu_delete_permanent: &'static str,
    /// part3c-2 #7：覆盖弹窗标题 / 提示（单参 `{}` = 冲突项数）
    pub remote_overwrite_prompt_fmt: &'static str,
    /// part3c-2 #7：覆盖弹窗「覆盖全部」按钮
    pub remote_overwrite_overwrite: &'static str,
    /// part3c-2 #7：覆盖弹窗「跳过已存在」按钮
    pub remote_overwrite_skip: &'static str,
    /// part3c-2 #7：复制成功 toast（单参 `{}` = 项数）
    pub remote_copied_fmt: &'static str,
    /// 片8：正在递归枚举远程目录（复制目录后、descriptor 就绪前的提示）。
    pub remote_clip_dir_preparing: &'static str,
    /// 片8：远程目录枚举完成、可粘贴（`{}` = 子树项数）。
    pub remote_clip_dir_ready_fmt: &'static str,
    /// 片8：远程目录过大、仅复制前 N 项（`{}` = 已复制项数）。
    pub remote_clip_dir_truncated_fmt: &'static str,
    /// 片8：远程目录枚举失败（权限 / 路径不存在 / 空目录）。
    pub remote_clip_dir_failed: &'static str,
    /// part3d Phase 2：远程新建会话超上限（`REMOTE_MAX_SESSIONS`）。
    pub remote_session_limit: &'static str,
    /// part3d Phase 2：拒绝关闭被控端最后一个会话（否则被控端退出）。
    pub remote_close_last: &'static str,
    /// part3d Phase 2：远程会话增删操作失败的通用兜底（如目标不存在）。
    pub remote_op_failed: &'static str,
    /// 远程菜单「进入文件夹」：当前没有正在镜像的远程终端，cd 无处注入时的提示。
    pub remote_cd_no_terminal: &'static str,
    /// 本机复制粘贴（local→local）开始的 toast。
    pub local_copy_started: &'static str,
    /// 本机复制粘贴完成的 toast（`{0}` 完成 / `{1}` 跳过 / `{2}` 出错；走 fmt3，须用索引占位）。
    pub local_copy_done_fmt: &'static str,
    /// 已有本机复制在途时再次粘贴的提示 toast。
    pub local_copy_busy: &'static str,
    /// 复制本地文件写入系统剪贴板失败的提示 toast。
    pub local_copy_clipboard_failed: &'static str,
    /// 系统声明存在文件但剪贴板正忙，暂时无法读取。
    pub file_clipboard_read_failed: &'static str,
    /// SSH 文件复制到系统文件剪贴板前的准备提示（`{}` = 文件名）。
    pub ssh_clipboard_preparing_fmt: &'static str,
    /// SSH 暂存完成、可从资源管理器粘贴。
    pub ssh_clipboard_ready: &'static str,
    /// SSH 文件剪贴板准备失败（`{}` = 简短错误）。
    pub ssh_clipboard_prepare_failed_fmt: &'static str,
    /// 准备期间用户复制了其他内容，取消覆盖系统剪贴板。
    pub ssh_clipboard_changed: &'static str,
    /// 暂存下载完成但 CF_HDROP 写入失败。
    pub ssh_clipboard_write_failed: &'static str,
    // 新建对话框
    /// "新建文件夹" 对话框标题
    pub filetree_create_dir_title: &'static str,
    /// "新建文件" 对话框标题
    pub filetree_create_file_title: &'static str,
    /// 位于路径行，单参 `{}`：目录显示名
    pub filetree_create_location_fmt: &'static str,
    /// 名称输入框 hint
    pub filetree_create_name_hint: &'static str,
    /// "创建" 按钮
    pub filetree_create_btn: &'static str,
    /// "取消" 按钮
    pub filetree_cancel_btn: &'static str,
    /// 重命名对话框标题，单参 `{}`：原名
    pub filetree_rename_title_fmt: &'static str,
    // 删除确认对话框
    /// "删除" 对话框标题
    pub filetree_delete_title: &'static str,
    /// 类型词"文件夹（含其中全部内容）"
    pub filetree_delete_what_dir: &'static str,
    /// 类型词"文件"
    pub filetree_delete_what_file: &'static str,
    /// 删除确认文案，双参 `{0}`=类型词 `{1}`=名称
    pub filetree_delete_confirm_fmt: &'static str,
    /// "移入回收站" 确认按钮
    pub filetree_delete_trash_btn: &'static str,
    // 右键菜单
    /// "进入文件夹"
    pub filetree_menu_enter_dir: &'static str,
    /// "新建文件"
    pub filetree_menu_new_file: &'static str,
    /// "新建文件夹"
    pub filetree_menu_new_dir: &'static str,
    /// "在文件管理器中打开"
    pub filetree_menu_reveal: &'static str,
    /// "复制绝对路径"
    pub filetree_menu_copy_abs: &'static str,
    /// "复制相对路径"
    pub filetree_menu_copy_rel: &'static str,
    /// "删除（移入回收站）"
    pub filetree_menu_delete: &'static str,

    // ── 内置远端文本编辑器 ─────────────────────────────────────────
    pub text_editor_hide: &'static str,
    pub text_editor_restore: &'static str,
    pub text_editor_find_hint: &'static str,
    pub text_editor_completion_title: &'static str,
    pub text_editor_completion_keys: &'static str,
    pub text_editor_completion_hint: &'static str,
    pub text_editor_completion_snippet: &'static str,
    pub text_editor_completion_keyword: &'static str,
    pub text_editor_completion_builtin: &'static str,
    pub text_editor_completion_document: &'static str,
    /// `{0}` 行数，`{1}` UTF-8 缓冲字节数。
    pub text_editor_stats_fmt: &'static str,
    pub text_editor_remote_changed_title: &'static str,
    pub text_editor_remote_changed_body: &'static str,
    pub text_editor_keep_editing: &'static str,
    pub text_editor_reload: &'static str,
    pub text_editor_overwrite: &'static str,
    pub text_editor_unsaved_title: &'static str,
    pub text_editor_unsaved_body: &'static str,
    pub text_editor_dont_save: &'static str,
    pub text_editor_discard: &'static str,
    /// 单参 `{}`：MiB 上限。
    pub text_editor_save_too_large_fmt: &'static str,
    /// 单参 `{}`：MiB 上限。
    pub text_editor_open_too_large_fmt: &'static str,
    pub text_editor_binary_error: &'static str,
    pub text_editor_utf8_only_error: &'static str,
    pub text_editor_mixed_eol_error: &'static str,
    pub text_editor_source_invalidated: &'static str,
    pub text_editor_closed_account_change: &'static str,
    /// 查找栏替换输入框占位文本与「替换当前」按钮
    pub text_editor_replace: &'static str,
    /// 查找栏「全部替换」按钮
    pub text_editor_replace_all: &'static str,
    /// 查找栏大小写敏感开关悬停提示
    pub text_editor_case_sensitive: &'static str,
    /// 查找栏上一个/下一个匹配按钮悬停提示
    pub text_editor_prev_match: &'static str,
    pub text_editor_next_match: &'static str,
    /// Ctrl+G 跳转到行输入框占位文本
    pub text_editor_goto_hint: &'static str,
    /// 状态栏软换行开关（Alt+Z）
    pub text_editor_wrap: &'static str,

    // ── main.rs toast ────────────────────────────────────────────────
    /// 背景图加载失败 toast，单参 `{}`：错误文本
    pub toast_bg_load_failed_fmt: &'static str,
    /// 每个会话最多 N 个窗格 toast，单参 `{}`：MAX_PANES
    pub toast_max_panes_fmt: &'static str,
    /// 新建窗格失败 toast，单参 `{}`：错误文本
    pub toast_new_pane_failed_fmt: &'static str,
    /// 旧 cwd 失效 toast，单参 `{}`：失效会话数
    pub toast_stale_cwd_fmt: &'static str,
    /// 字体回退提示，双参 `{0}`=请求字体名 `{1}`=实际字体名
    pub toast_font_fallback_fmt: &'static str,
    /// 设置保存失败 toast，单参 `{}`：错误文本
    pub toast_settings_save_failed_fmt: &'static str,
    /// 登录成功 toast，单参 `{}`：展示名
    pub toast_logged_in_fmt: &'static str,
    /// 复制成功 toast，单参 `{}`：复制内容
    pub toast_copied_fmt: &'static str,
    /// 复制失败 toast
    pub toast_copy_failed: &'static str,
    /// 窗格兜底名，单参 `{}`：(index+1)
    pub pane_default_name_fmt: &'static str,
    /// 会话兜底名，单参 `{}`：(id+1)
    pub session_default_name_fmt: &'static str,

    // ── filetree 后台操作 toast（OpReply 结果枚举化后的文案）────────
    /// "已创建：{name}"，单参 `{}`：名称
    pub filetree_created_fmt: &'static str,
    /// "创建失败：「{name}」已存在"，单参 `{}`：名称
    pub filetree_create_exists_fmt: &'static str,
    /// "创建失败：{e}"，单参 `{}`：错误文本
    pub filetree_create_failed_fmt: &'static str,
    /// "已移入回收站：{name}"，单参 `{}`：名称
    pub filetree_trashed_fmt: &'static str,
    /// "删除失败：{e}"，单参 `{}`：错误文本
    pub filetree_delete_failed_fmt: &'static str,
    /// "打开文件管理器失败：{e}"，单参 `{}`：错误文本
    pub filetree_reveal_failed_fmt: &'static str,

    // ── M4.1 批B：经典直通模式切换 toast ────────────────────────────
    /// 切换为经典直通模式的 toast（Ctrl+Shift+E 开启）
    pub toast_fallback_enabled: &'static str,
    /// 关闭经典直通模式的 toast（Ctrl+Shift+E 关闭）
    pub toast_fallback_disabled: &'static str,

    // ── M4.1 批C：footer 状态条文案（已停用，保留供三语完备性）──────────
    // 海风哥反馈后 Running 态 footer 改为隐藏（见 composer.rs），本文案不再被
    // 任何代码读取；字段仍保留在 struct 内——删它会让 zh_cn/zh_tw/en 三语实例
    // 编译报错（Strings 完备性约束），故无条件 allow(dead_code)。
    /// Running 态状态条主文案（旧·已停用：Running footer 现为隐藏，不再渲染）。
    #[allow(dead_code)]
    pub footer_running_text: &'static str,

    // ── M4.1 批D1：Compose 态键位占位提示 ──────────────────────────
    /// Compose 态 Tab 键占位提示 toast（M3.4 补全未实现；M4.4 批2 后降级路径仅在
    /// `not(feature = "input-editor")` 分支使用，但需保留字段以维持三语文件编译完备性）
    // ALLOW: 字段在 input-editor feature 启用时仅被 cfg(not(input-editor)) 分支使用，
    // 看似 dead_code 实为多语言 Strings 结构体的完备性约束——删字段会导致三语实例
    // 编译报错；故此处允许 dead_code，与同结构体的 toast_compose_history_hint 同款处理。
    #[allow(dead_code)]
    pub toast_compose_tab_hint: &'static str,
    /// Compose 态 Ctrl+R 占位提示 toast（M4.3 面板已实现，此字段保留供降级/非 input-editor 模式）
    // ALLOW: M4.3 后 ComposeHistorySearch 直接打开面板，toast 路径不再走此字段；
    // 保留字段是为了维持三语文件的编译期完备性检查（删字段会导致三语实例编译报错）。
    #[allow(dead_code)]
    pub toast_compose_history_hint: &'static str,

    // ── 侧栏标题栏（R8）─────────────────────────────────────────────────────
    /// 侧栏标题栏「＋」按钮 tooltip（新建会话，含快捷键）
    pub sidebar_new_session_tip: &'static str,

    // ── 顶栏三视图切换按钮（问题7）────────────────────────────────────
    /// 显示/隐藏会话栏 tooltip（展开态）
    pub topbar_sidebar_show_tip: &'static str,
    /// 显示/隐藏会话栏 tooltip（隐藏态）
    pub topbar_sidebar_hide_tip: &'static str,
    /// 显示远程设备栏 tooltip
    pub toolbar_remote_list_show_tip: &'static str,
    /// 隐藏远程设备栏 tooltip
    pub toolbar_remote_list_hide_tip: &'static str,
    /// 显示 SSH 服务器栏 tooltip
    pub toolbar_ssh_server_list_show_tip: &'static str,
    /// 隐藏 SSH 服务器栏 tooltip
    pub toolbar_ssh_server_list_hide_tip: &'static str,
    /// 显示/隐藏文件树 tooltip（展开态）
    pub topbar_filetree_show_tip: &'static str,
    /// 显示/隐藏文件树 tooltip（隐藏态）
    pub topbar_filetree_hide_tip: &'static str,
    /// 还原窗格大小 tooltip（启用态，对应原「▦」功能）
    pub topbar_reset_layout_tip: &'static str,
    /// 顶栏「本地」tab（M5.2）
    pub topbar_tab_local: &'static str,
    /// 顶栏「远程」tab（M5.2）
    pub topbar_tab_remote: &'static str,
    /// 顶栏「SSH」tab
    pub topbar_tab_ssh: &'static str,
    /// SSH 模式尚未选择服务器时的占位提示
    pub ssh_select_server: &'static str,
    /// SSH 会话栏尚无已打开会话
    pub ssh_no_sessions: &'static str,
    /// SSH 会话栏新增一个独立 Shell
    pub ssh_new_session: &'static str,
    /// SSH 服务器列表与配置表单
    pub ssh_title: &'static str,
    pub ssh_search_hint: &'static str,
    pub ssh_new_profile: &'static str,
    pub ssh_new_group: &'static str,
    pub ssh_ungrouped: &'static str,
    pub ssh_empty_group: &'static str,
    pub ssh_no_search_results: &'static str,
    pub ssh_edit: &'static str,
    pub ssh_delete: &'static str,
    pub ssh_connect: &'static str,
    pub ssh_rename_group: &'static str,
    pub ssh_delete_group: &'static str,
    pub ssh_create_group_title: &'static str,
    pub ssh_rename_group_title: &'static str,
    pub ssh_delete_group_title: &'static str,
    pub ssh_delete_group_message: &'static str,
    pub ssh_delete_profile_title: &'static str,
    pub ssh_delete_profile_message: &'static str,
    pub ssh_group_name: &'static str,
    pub ssh_create_profile_title: &'static str,
    pub ssh_edit_profile_title: &'static str,
    pub ssh_profile_name: &'static str,
    pub ssh_host: &'static str,
    pub ssh_port: &'static str,
    pub ssh_username: &'static str,
    pub ssh_auth_method: &'static str,
    pub ssh_auth_password: &'static str,
    pub ssh_auth_private_key: &'static str,
    pub ssh_auth_agent: &'static str,
    pub ssh_password_required_hint: &'static str,
    pub ssh_password_saved_hint: &'static str,
    pub ssh_private_key_required_hint: &'static str,
    pub ssh_private_key_saved_hint: &'static str,
    pub ssh_private_key_invalid_hint: &'static str,
    pub ssh_group: &'static str,
    pub ssh_initial_directory: &'static str,
    pub ssh_connect_timeout: &'static str,
    pub ssh_keep_alive: &'static str,
    pub ssh_keep_alive_disabled: &'static str,
    pub ssh_monitor_enabled: &'static str,
    pub ssh_seconds: &'static str,
    pub ssh_save: &'static str,
    pub ssh_create: &'static str,
    pub ssh_test_connection: &'static str,
    pub ssh_test_connecting: &'static str,
    pub ssh_test_success: &'static str,
    pub ssh_test_failed: &'static str,
    pub ssh_test_host_key_unknown: &'static str,
    pub ssh_test_host_key_changed: &'static str,
    pub ssh_test_trust_and_retry: &'static str,
    pub ssh_cancel: &'static str,
    pub ssh_confirm_delete: &'static str,
    /// SSH 连接、凭据、主机密钥与监控界面
    pub ssh_disconnect: &'static str,
    pub ssh_status_credentials: &'static str,
    pub ssh_status_connecting: &'static str,
    pub ssh_status_host_key: &'static str,
    pub ssh_status_connected: &'static str,
    /// 凭据提交已过期（弹窗期间配置被同步/编辑变更）
    pub ssh_cred_toast_stale: &'static str,
    /// 密码为空
    pub ssh_cred_toast_empty_password: &'static str,
    /// profile id 不合法，无法构造凭据引用
    pub ssh_cred_toast_invalid_id: &'static str,
    /// 写入系统凭据管理器失败（带 Win32 错误码）
    pub ssh_cred_toast_write_failed_fmt: &'static str,
    /// 凭据已写入但保存配置绑定失败
    pub ssh_cred_toast_commit_failed: &'static str,
    pub ssh_status_disconnecting: &'static str,
    pub ssh_status_disconnected: &'static str,
    pub ssh_status_error: &'static str,
    pub ssh_status_host_key_changed: &'static str,
    pub ssh_password_title: &'static str,
    pub ssh_private_key_title: &'static str,
    pub ssh_password_prompt: &'static str,
    pub ssh_private_key_file: &'static str,
    pub ssh_choose_private_key: &'static str,
    pub ssh_key_passphrase: &'static str,
    pub ssh_credentials_memory_only: &'static str,
    pub ssh_host_key_unknown_title: &'static str,
    pub ssh_host_key_unknown_message: &'static str,
    pub ssh_host_key_algorithm: &'static str,
    pub ssh_host_key_fingerprint: &'static str,
    pub ssh_host_key_accept: &'static str,
    pub ssh_host_key_changed_title: &'static str,
    pub ssh_host_key_changed_message: &'static str,
    pub ssh_host_key_expected: &'static str,
    pub ssh_host_key_presented: &'static str,
    pub ssh_monitor_title: &'static str,
    pub ssh_monitor_cpu: &'static str,
    pub ssh_monitor_memory: &'static str,
    pub ssh_monitor_load: &'static str,
    pub ssh_monitor_disk: &'static str,
    pub ssh_monitor_network: &'static str,
    pub ssh_monitor_uptime: &'static str,
    pub ssh_monitor_waiting: &'static str,
    pub ssh_monitor_system: &'static str,
    pub ssh_monitor_timezone: &'static str,
    pub ssh_monitor_kernel: &'static str,
    pub ssh_monitor_used: &'static str,
    pub ssh_monitor_cached: &'static str,
    pub ssh_monitor_available: &'static str,
    pub ssh_monitor_total: &'static str,
    pub ssh_monitor_upload: &'static str,
    pub ssh_monitor_download: &'static str,
    pub ssh_monitor_speed: &'static str,
    pub ssh_monitor_traffic: &'static str,
    pub ssh_monitor_filesystem: &'static str,
    pub ssh_monitor_read: &'static str,
    pub ssh_monitor_write: &'static str,
    pub ssh_monitor_processes: &'static str,
    pub ssh_monitor_command: &'static str,
    pub ssh_monitor_no_processes: &'static str,
    /// 进程卡统一搜索框占位提示（进程名，或 :端口）
    pub ssh_monitor_search_hint: &'static str,
    /// 搜索进行中
    pub ssh_monitor_searching: &'static str,
    /// 远端查询失败
    pub ssh_monitor_search_failed: &'static str,
    /// 名称搜索无匹配
    pub ssh_monitor_no_match: &'static str,
    /// 端口无监听进程
    pub ssh_monitor_port_none: &'static str,
    /// 无权限看到进程归属
    pub ssh_monitor_port_unknown: &'static str,
    /// 终止进程按钮
    pub ssh_monitor_kill: &'static str,
    /// 强制终止（SIGKILL）按钮
    pub ssh_monitor_kill_force: &'static str,
    /// 取消终止确认
    pub ssh_monitor_kill_cancel: &'static str,
    /// 终止确认语（后接 PID）
    pub ssh_monitor_kill_confirm: &'static str,
    /// 已发送终止信号（后接 PID）
    pub ssh_monitor_kill_sent: &'static str,
    /// 权限不足（后接 PID）
    pub ssh_monitor_kill_denied: &'static str,
    /// 进程已不存在（后接 PID）
    pub ssh_monitor_kill_missing: &'static str,
    /// 终止命令执行失败（后接 PID）
    pub ssh_monitor_kill_failed: &'static str,
    /// SSH 状态栏：收起监控面板按钮
    pub ssh_statusbar_monitor_hide: &'static str,
    /// SSH 状态栏：展开监控面板按钮
    pub ssh_statusbar_monitor_show: &'static str,
    /// 进程卡「详情」按钮（打开进程详情弹窗）
    pub ssh_monitor_detail: &'static str,
    /// 详情弹窗「端口号」列标题
    pub ssh_process_ports: &'static str,
    /// 进程详情弹窗标题
    pub ssh_process_window_title: &'static str,
    /// 详情弹窗刷新按钮
    pub ssh_process_refresh: &'static str,
    /// 详情弹窗未加载时的占位提示
    pub ssh_process_empty_hint: &'static str,
    /// 详情弹窗进程计数（fmt1 占位）
    pub ssh_process_count_fmt: &'static str,
    /// 详情弹窗 CPU/MEM 表头排序切换提示
    pub ssh_process_sort_tip: &'static str,
    /// 工具栏：隐藏服务器监控面板提示
    pub toolbar_ssh_monitor_hide_tip: &'static str,
    /// 工具栏：显示服务器监控面板提示
    pub toolbar_ssh_monitor_show_tip: &'static str,
    /// 远程设备列表标题（M5.2）
    pub remote_list_title: &'static str,
    /// 设备在线
    pub remote_online: &'static str,
    /// 设备离线
    pub remote_offline: &'static str,
    /// 本机标记
    pub remote_this_device: &'static str,
    /// 离线不可连接
    pub remote_unavailable: &'static str,
    /// 暂无设备
    pub remote_empty: &'static str,
    /// 右键删除设备
    pub remote_menu_delete: &'static str,
    /// 右键/双击连接（控制）设备（M5.3）
    pub remote_menu_connect: &'static str,
    /// 配对弹窗标题
    pub remote_pairing_title: &'static str,
    /// 配对弹窗提示（{}=被控端设备名）
    pub remote_pairing_prompt_fmt: &'static str,
    /// 配对码输入框 hint
    pub remote_pairing_hint: &'static str,
    /// 配对弹窗连接按钮
    pub remote_pairing_connect: &'static str,
    /// 配对弹窗取消按钮
    pub remote_pairing_cancel: &'static str,
    /// 配对码错误（{}=剩余尝试次数）
    pub remote_pairing_invalid_fmt: &'static str,
    /// 被控来件横幅（{}=控制端设备名）
    pub remote_incoming_fmt: &'static str,
    /// 配对码标签
    pub remote_incoming_code: &'static str,
    /// M7 片 6：配对二维码下方的一行说明（扫码或念数字，两条路都通）。
    pub remote_incoming_scan_hint: &'static str,
    /// 拒绝控制按钮
    pub remote_decline: &'static str,
    /// 正在被控横幅（{}=控制端设备名）
    pub remote_being_controlled_fmt: &'static str,
    /// 正在控制横幅（{}=被控端设备名）
    pub remote_controlling_fmt: &'static str,
    /// 断开会话按钮
    pub remote_disconnect: &'static str,
    /// toast：会话已建立（控制端，{}=对端名）
    pub remote_toast_controlling_fmt: &'static str,
    /// toast：会话已建立（被控端，{}=对端名）
    pub remote_toast_controlled_fmt: &'static str,
    /// toast：会话已结束
    pub remote_toast_session_ended: &'static str,
    /// toast：配对码已复制（敏感信息，不插入配对码正文）
    pub remote_toast_pairing_code_copied: &'static str,
    /// toast：M6 P2P 已切换到直连（绕开中继）
    pub remote_toast_p2p_direct: &'static str,
    /// toast：M6 P2P 已回退到中继（直连断开）
    pub remote_toast_p2p_relay: &'static str,
    /// toast：断线宽限重挂中（连接中断，会话保留，自动重连）
    pub remote_toast_reconnecting: &'static str,
    /// toast：断线宽限重挂成功（会话已自动恢复）
    pub remote_toast_restored: &'static str,
    /// toast：M7 对端 Lumen 不支持 LLM 远程控制面（`LlmFrame::Hello` 5 秒无 `HelloAck`）
    ///
    /// **文案必须点明「终端功能不受影响」**：LLM 是纯增量能力（`MIN_SUPPORTED_VERSION`
    /// 因此保持 3），只说「版本过低」会让用户以为整条远程链路坏了、去做无谓的排查。
    pub remote_toast_llm_peer_too_old: &'static str,
    /// 状态栏链路指示：P2P 直连（短标签）
    pub statusbar_link_direct: &'static str,
    /// 状态栏链路指示：中继转发（短标签）
    pub statusbar_link_relay: &'static str,
    /// 状态栏链路指示：断线重挂中（短标签，黄）
    pub statusbar_link_reconnecting: &'static str,
    /// 状态栏服务器连接指示：已连接（绿）
    pub statusbar_server_connected: &'static str,
    /// 状态栏服务器连接指示：未连接（黄，已配置地址但尚未连上/未登录）
    pub statusbar_server_disconnected: &'static str,
    /// 状态栏服务器连接指示：连接错误（红，网络层连不上服务器）
    pub statusbar_server_error: &'static str,
    /// toast：请求被拒——目标离线
    pub remote_denied_offline: &'static str,
    /// toast：请求被拒——对方忙（已被控/配对中）
    pub remote_denied_busy: &'static str,
    /// toast：请求被拒——对方拒绝
    pub remote_denied_rejected: &'static str,
    /// toast：请求被拒——其他（跨账户/自控/本端忙/超时等）
    pub remote_denied_generic: &'static str,
    /// toast：配对失败（次数用尽）
    pub remote_pairing_failed: &'static str,
    /// toast：配对/会话超时
    pub remote_toast_pairing_expired: &'static str,
    /// toast：控制请求被取消（控制端撤销）
    pub remote_toast_pairing_cancelled: &'static str,
    /// toast：会话结束——对端断线
    pub remote_toast_peer_offline: &'static str,
    /// toast：会话结束——本设备在别处登录被取代
    pub remote_toast_replaced: &'static str,
    /// 还原窗格大小禁用 tooltip（单窗格时）
    pub topbar_reset_layout_disabled_tip: &'static str,

    // ── profile 校验错误（UI 侧翻译）────────────────────────────────
    /// 邮箱格式不正确
    pub login_err_invalid_email: &'static str,
    /// 请输入密码
    pub login_err_empty_password: &'static str,

    // ── M4.1 批E：底部状态栏（M3.8 海风哥反馈 #3/#6）────────────────
    /// 状态栏：Compose 态模式指示文字（含图标前缀）
    #[cfg_attr(not(feature = "input-editor"), allow(dead_code))]
    pub statusbar_mode_compose: &'static str,
    /// 状态栏：Running 态模式指示文字
    #[cfg_attr(not(feature = "input-editor"), allow(dead_code))]
    pub statusbar_mode_running: &'static str,
    /// 状态栏：AltScreen 态模式指示文字
    #[cfg_attr(not(feature = "input-editor"), allow(dead_code))]
    pub statusbar_mode_altscreen: &'static str,
    /// 状态栏：Fallback 态模式指示文字（警示色）
    #[cfg_attr(not(feature = "input-editor"), allow(dead_code))]
    pub statusbar_mode_fallback: &'static str,
    /// 状态栏：经典模式切换按钮关态文字
    #[cfg_attr(not(feature = "input-editor"), allow(dead_code))]
    pub statusbar_classic_off: &'static str,
    /// 状态栏：经典模式切换按钮开态文字（已开启时显示）
    #[cfg_attr(not(feature = "input-editor"), allow(dead_code))]
    pub statusbar_classic_on: &'static str,
    /// 状态栏：经典模式切换按钮 hover tooltip
    #[cfg_attr(not(feature = "input-editor"), allow(dead_code))]
    pub statusbar_classic_tip: &'static str,
    /// LLM HUD：关闭按钮 tooltip
    pub hud_close_tip: &'static str,
    /// LLM HUD：收起按钮 tooltip
    pub hud_open_tip: &'static str,
    /// LLM HUD：左上角调整大小 tooltip
    pub hud_resize_tip: &'static str,
    /// LLM HUD：项目字段
    pub hud_project: &'static str,
    /// LLM HUD：上下文字段
    pub hud_context: &'static str,
    /// LLM HUD：CLI 未在终端公开该字段时的占位
    pub hud_waiting_cli: &'static str,
    /// LLM HUD：已用上下文百分比，单参 `{}`
    pub hud_context_used_fmt: &'static str,
    /// LLM HUD：剩余上下文百分比，单参 `{}`
    pub hud_context_remaining_fmt: &'static str,
    /// LLM HUD：运行状态
    pub hud_status_working: &'static str,
    /// LLM HUD：等待输入状态
    pub hud_status_ready: &'static str,
    /// LLM HUD：会话时长
    pub hud_session: &'static str,
    /// LLM HUD：token 统计
    pub hud_tokens: &'static str,
    /// LLM HUD：订阅/API 用量窗口
    pub hud_usage: &'static str,
    /// LLM HUD：工具活动
    pub hud_tools: &'static str,
    /// LLM HUD：子代理活动
    pub hud_agents: &'static str,
    /// LLM HUD：任务进度
    pub hud_tasks: &'static str,
    /// LLM HUD：配置计数
    pub hud_config: &'static str,
    /// LLM HUD：上下文压缩次数
    pub hud_compactions: &'static str,
    /// LLM HUD：后台数据加载中
    pub hud_loading: &'static str,
    /// Compose 态输入框占位提示文字（缓冲为空时 footer 显示）
    #[cfg_attr(not(feature = "input-editor"), allow(dead_code))]
    pub composer_placeholder: &'static str,

    // ── 输入框右键菜单（第十一轮）────────────────────────────────────
    /// 右键菜单：复制
    #[cfg_attr(not(feature = "input-editor"), allow(dead_code))]
    pub ctx_menu_copy: &'static str,
    /// 右键菜单：剪切
    #[cfg_attr(not(feature = "input-editor"), allow(dead_code))]
    pub ctx_menu_cut: &'static str,
    /// 右键菜单：粘贴
    #[cfg_attr(not(feature = "input-editor"), allow(dead_code))]
    pub ctx_menu_paste: &'static str,
    /// 右键菜单：全选
    #[cfg_attr(not(feature = "input-editor"), allow(dead_code))]
    pub ctx_menu_select_all: &'static str,

    // ── M4.3 历史搜索面板 ────────────────────────────────────────────
    /// 历史搜索输入框占位提示
    #[cfg_attr(not(feature = "input-editor"), allow(dead_code))]
    pub history_search_placeholder: &'static str,
    /// 历史搜索无匹配项时的空态提示
    #[cfg_attr(not(feature = "input-editor"), allow(dead_code))]
    pub history_search_empty: &'static str,
    /// 历史搜索面板底部操作提示（↑↓ 选择 · Enter 填入 · Esc 关闭）
    #[cfg_attr(not(feature = "input-editor"), allow(dead_code))]
    pub history_search_hint: &'static str,

    // ── filetree 名字校验错误（UI 侧翻译）──────────────────────────
    /// 名称不能为空
    pub validate_name_empty: &'static str,
    /// 名称不合法（"." / ".."）
    pub validate_name_illegal: &'static str,
    /// 名称不能包含控制字符
    pub validate_name_control_chars: &'static str,
    /// 名称不能包含非法字符
    pub validate_name_bad_chars: &'static str,
    /// 名称不能以点或空格结尾
    pub validate_name_trailing: &'static str,

    // ── F3 热更（自动更新）─────────────────────────────────────────
    /// 发现新版本 toast（fmt1：版本号）
    pub update_toast_available_fmt: &'static str,
    /// 更新提示弹窗标题
    pub update_modal_title: &'static str,
    /// 弹窗版本行（fmt1：版本号）
    pub update_modal_version_fmt: &'static str,
    /// 弹窗「更新内容」小标题
    pub update_modal_notes_label: &'static str,
    /// 弹窗「安装包已下载就绪」提示行（Warp 式静默预下载完成）
    pub update_modal_ready_hint: &'static str,
    /// 弹窗提示行（非 Windows：自动安装是 Windows 专属，引导手动下载）
    pub update_modal_manual_hint: &'static str,
    /// 立即更新按钮
    pub update_btn_install: &'static str,
    /// 前往下载按钮（非 Windows 更新弹窗主 CTA）
    pub update_btn_download: &'static str,
    /// 稍后按钮
    pub update_btn_later: &'static str,
    /// 跳过此版本按钮
    pub update_btn_skip: &'static str,
    /// 设置页「更新」分区标题
    pub update_settings_section: &'static str,
    /// 设置页：启动时自动检查更新
    pub update_settings_auto_check: &'static str,
    /// 设置页：检查更新按钮
    pub update_btn_check_now: &'static str,
    /// 设置页「网络」分区标题
    pub proxy_section: &'static str,
    /// 设置页：启用代理开关标签
    pub proxy_enable: &'static str,
    /// 设置页：代理地址输入框标签
    pub proxy_url_label: &'static str,
    /// 设置页：代理地址输入框占位提示
    pub proxy_url_placeholder: &'static str,
    /// 设置页：代理格式说明
    pub proxy_hint: &'static str,
    /// 正在检查更新 toast
    pub update_toast_checking: &'static str,
    /// 已是最新版本 toast
    pub update_toast_up_to_date: &'static str,
    /// 检查更新失败 toast
    pub update_toast_check_failed: &'static str,
    /// 正在下载更新 toast
    pub update_toast_downloading: &'static str,
    /// 下载失败 toast（fmt1：错误信息）
    pub update_toast_download_failed_fmt: &'static str,
    /// 正在启动安装程序 toast
    pub update_toast_installing: &'static str,

    // ── F10 链接 hover 提示浮层 ────────────────────────────────────
    /// 悬停文件路径链接：提示「打开文件（Ctrl+单击）」
    pub link_open_file_hint: &'static str,
    /// 悬停 URL 链接：提示「打开链接（Ctrl+单击）」
    pub link_open_url_hint: &'static str,
}
