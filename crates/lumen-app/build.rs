//! build.rs — lumen-app 构建脚本
//!
//! Windows 目标：用 winresource 把 lumen.ico 嵌入 PE 资源区段，
//! 使文件管理器缩略图、快捷方式、任务栏非运行态均显示应用图标。
//! 非 Windows 目标：本脚本为空操作，不引入任何额外依赖。

fn main() {
    #[cfg(target_os = "windows")]
    embed_icon();
}

#[cfg(target_os = "windows")]
fn embed_icon() {
    // CARGO_MANIFEST_DIR 指向 crates/lumen-app/（build.rs 的 crate 根）。
    // icons/ 目录位于工作区根，相对路径为 ../../icons/lumen.ico。
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let icon_path = std::path::Path::new(&manifest_dir)
        .join("..") // crates/
        .join("..") // workspace root
        .join("icons")
        .join("lumen.ico");

    // 告知 Cargo：图标文件变化时重跑本脚本。
    println!("cargo:rerun-if-changed={}", icon_path.display());

    let mut res = winresource::WindowsResource::new();
    res.set_icon(icon_path.to_str().expect("icon path is valid UTF-8"));
    if let Err(e) = res.compile() {
        // 图标嵌入失败仅打印警告，不阻断构建。
        // 常见原因：CI 环境缺少 Windows SDK rc.exe；本机开发一般不会触发。
        eprintln!("cargo:warning=嵌入 exe 图标失败（不影响功能）：{e}");
    }
}
