//! LLM CLI 草稿图片附件。
//!
//! 图片落在当前工作目录下的 `.lumen/attachments`，让 CLI 与模型工具
//! 可以按绝对路径读取；编辑器正文只保存稳定的 `[#N]` 引用。

use std::path::{Path, PathBuf};

use anyhow::{ensure, Context, Result};
use image::ImageFormat;

const THUMBNAIL_EDGE: u32 = 72;

#[derive(Debug)]
pub struct ClipboardImage {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

#[derive(Debug)]
pub struct ImageAttachment {
    pub id: u64,
    pub label: String,
    pub path: PathBuf,
    pub width: u32,
    pub height: u32,
    pub thumbnail_width: u32,
    pub thumbnail_height: u32,
    pub thumbnail_rgba: Vec<u8>,
}

/// 读取系统剪贴板中的图片。
///
/// 截图工具在 Windows 上常同时发布 DIB 与 `CF_HDROP` 临时图片文件。
/// arboard 能直接读取标准位图；若具体截图工具的 DIB 变体不兼容，则
/// 回退读取文件剪贴板中的第一张可解码图片。
pub fn read_clipboard_image(
    clipboard: &mut Option<arboard::Clipboard>,
) -> Result<Option<ClipboardImage>> {
    let direct_error = match clipboard.as_mut() {
        Some(clipboard) => match clipboard.get_image() {
            Ok(image) => {
                let width = u32::try_from(image.width).context("剪贴板图片宽度过大")?;
                let height = u32::try_from(image.height).context("剪贴板图片高度过大")?;
                return Ok(Some(ClipboardImage {
                    width,
                    height,
                    rgba: image.bytes.into_owned(),
                }));
            }
            Err(error) => Some(error),
        },
        None => None,
    };

    if let Some(image) = read_first_image_path(crate::clipboard_files::paste_files()) {
        return Ok(Some(image));
    }

    if let Some(error) = direct_error {
        log::debug!("剪贴板没有可直接读取的位图：{error}");
    }
    Ok(None)
}

fn read_first_image_path(paths: impl IntoIterator<Item = PathBuf>) -> Option<ClipboardImage> {
    for path in paths {
        let decoded = match image::open(&path) {
            Ok(image) => image.into_rgba8(),
            Err(error) => {
                log::debug!("文件剪贴板项目不是可读图片 {}: {error}", path.display());
                continue;
            }
        };
        return Some(ClipboardImage {
            width: decoded.width(),
            height: decoded.height(),
            rgba: decoded.into_raw(),
        });
    }
    None
}

#[derive(Debug, Default)]
pub struct AttachmentDraft {
    images: Vec<ImageAttachment>,
    next_label: u32,
    generation: u64,
    /// 本会话创建的全部附件；提交后仍保留到会话关闭，保证 CLI 后续可读。
    owned_paths: Vec<PathBuf>,
}

impl AttachmentDraft {
    pub fn images(&self) -> &[ImageAttachment] {
        &self.images
    }

    pub fn len(&self) -> usize {
        self.images.len()
    }

    pub fn is_empty(&self) -> bool {
        self.images.is_empty()
    }

    /// 保存 RGBA8 图片并返回应插入编辑器光标处的 `[#N]` 标记。
    pub fn add_rgba(
        &mut self,
        workspace: &Path,
        session_id: u64,
        width: u32,
        height: u32,
        rgba: &[u8],
    ) -> Result<String> {
        ensure!(width > 0 && height > 0, "剪贴板图片尺寸无效");
        ensure!(
            rgba.len() == width as usize * height as usize * 4,
            "剪贴板图片像素长度与尺寸不匹配"
        );

        self.next_label = self.next_label.saturating_add(1).max(1);
        let label = format!("[#{}]", self.next_label);
        let dir = workspace
            .join(".lumen")
            .join("attachments")
            .join(format!("session-{session_id}"))
            .join(format!("draft-{}", self.generation));
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("创建图片附件目录失败：{}", dir.display()))?;
        let path = dir.join(format!("{label}.png"));
        image::save_buffer_with_format(
            &path,
            rgba,
            width,
            height,
            image::ColorType::Rgba8,
            ImageFormat::Png,
        )
        .with_context(|| format!("保存图片附件失败：{}", path.display()))?;

        let source =
            image::RgbaImage::from_raw(width, height, rgba.to_vec()).context("构造图片附件失败")?;
        let thumb = image::imageops::thumbnail(&source, THUMBNAIL_EDGE, THUMBNAIL_EDGE);
        let id = (self.generation << 32) | u64::from(self.next_label);
        self.owned_paths.push(path.clone());
        self.images.push(ImageAttachment {
            id,
            label: label.clone(),
            path,
            width,
            height,
            thumbnail_width: thumb.width(),
            thumbnail_height: thumb.height(),
            thumbnail_rgba: thumb.into_raw(),
        });
        Ok(label)
    }

    /// 把用户原文与附件映射编译成发给 CLI 的提示。映射中的 label 与
    /// 正文/缩略图完全一致，路径为 CLI 可直接读取的绝对路径。
    pub fn compile_prompt(&self, text: &str) -> String {
        if self.images.is_empty() {
            return text.to_owned();
        }
        let mut out = String::with_capacity(text.len() + self.images.len() * 96);
        out.push_str(text);
        out.push_str("\n\n图片附件映射（请按正文中的编号读取对应图片）：\n");
        for image in &self.images {
            out.push_str(&image.label);
            out.push_str(" = \"");
            out.push_str(&image.path.display().to_string());
            out.push_str("\"\n");
        }
        out
    }

    /// 提交后开启新草稿，避免下一次粘贴覆盖仍在被 CLI 读取的图片。
    pub fn advance_draft(&mut self) {
        self.images.clear();
        self.next_label = 0;
        self.generation = self.generation.saturating_add(1);
    }

    pub fn remove(&mut self, id: u64) -> Option<ImageAttachment> {
        let index = self.images.iter().position(|image| image.id == id)?;
        let image = self.images.remove(index);
        self.owned_paths.retain(|path| path != &image.path);
        Some(image)
    }

    /// 取消当前草稿：返回需要立即删除的文件，并开启新一代草稿。
    pub fn discard_draft(&mut self) -> Vec<PathBuf> {
        let paths: Vec<PathBuf> = self.images.iter().map(|image| image.path.clone()).collect();
        self.images.clear();
        self.owned_paths
            .retain(|owned| !paths.iter().any(|path| path == owned));
        self.next_label = 0;
        self.generation = self.generation.saturating_add(1);
        paths
    }
}

impl Drop for AttachmentDraft {
    fn drop(&mut self) {
        for path in &self.owned_paths {
            if let Err(error) = std::fs::remove_file(path) {
                if error.kind() != std::io::ErrorKind::NotFound {
                    log::warn!("清理 LLM 图片附件失败 {}: {error}", path.display());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_mapping_preserves_labels() {
        let mut draft = AttachmentDraft::default();
        draft.images.push(ImageAttachment {
            id: 1,
            label: "[#1]".into(),
            path: PathBuf::from(r"C:\work\.lumen\attachments\[#1].png"),
            width: 1,
            height: 1,
            thumbnail_width: 1,
            thumbnail_height: 1,
            thumbnail_rgba: vec![0; 4],
        });
        let prompt = draft.compile_prompt("比较 [#1] 和上文");
        assert!(prompt.contains("比较 [#1] 和上文"));
        assert!(prompt.contains(r#"[#1] = "C:\work\.lumen\attachments\[#1].png""#));
    }

    #[test]
    fn new_draft_resets_visible_labels_but_not_identity() {
        let mut draft = AttachmentDraft {
            images: Vec::new(),
            next_label: 3,
            generation: 9,
            owned_paths: Vec::new(),
        };
        draft.advance_draft();
        assert_eq!(draft.next_label, 0);
        assert_eq!(draft.generation, 10);
    }

    #[test]
    fn saves_png_with_the_same_name_as_the_editor_marker() {
        let root =
            std::env::temp_dir().join(format!("lumen-llm-attachment-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("创建测试目录");
        let mut draft = AttachmentDraft::default();
        let label = draft
            .add_rgba(&root, 42, 2, 1, &[255, 0, 0, 255, 0, 255, 0, 255])
            .expect("保存图片");
        assert_eq!(label, "[#1]");
        let image = &draft.images()[0];
        assert_eq!(
            image.path.file_name().and_then(|name| name.to_str()),
            Some("[#1].png")
        );
        assert_eq!(
            image::image_dimensions(&image.path).expect("读取 PNG 尺寸"),
            (2, 1)
        );
        std::fs::remove_dir_all(&root).expect("清理测试目录");
    }

    #[test]
    fn file_clipboard_fallback_decodes_an_image_path() {
        let root = std::env::temp_dir().join(format!(
            "lumen-clipboard-image-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("创建测试目录");
        let path = root.join("clipboard.png");
        image::save_buffer_with_format(
            &path,
            &[255, 0, 0, 255, 0, 255, 0, 255],
            2,
            1,
            image::ColorType::Rgba8,
            ImageFormat::Png,
        )
        .expect("写测试图片");

        let image = read_first_image_path([path]).expect("应读取文件剪贴板图片");
        assert_eq!((image.width, image.height), (2, 1));
        assert_eq!(image.rgba.len(), 8);
        std::fs::remove_dir_all(&root).expect("清理测试目录");
    }
}
