//! Image attachments. A local file becomes an inline base64 data URL; an
//! `http(s)` URL is passed through for the provider to fetch.

use crate::Image;

/// DeepSeek rejects an inline image above 32 MiB.
pub const MAX_IMAGE_BYTES: usize = 32 * 1024 * 1024;

pub fn attach(source: &str) -> Result<Image, String> {
    if source.starts_with("http://") || source.starts_with("https://") {
        return Ok(Image {
            path: String::new(),
            url: source.to_string(),
        });
    }
    let data = std::fs::read(source).map_err(|error| format!("read image {source}: {error}"))?;
    if data.len() > MAX_IMAGE_BYTES {
        return Err(format!(
            "image {source} is {} MiB; the limit is {} MiB",
            data.len() / (1024 * 1024),
            MAX_IMAGE_BYTES / (1024 * 1024)
        ));
    }
    let url = format!("data:{};base64,{}", mime(&data, source), base64(&data));
    Ok(Image {
        path: source.to_string(),
        url,
    })
}

/// Attach `path` when its content is a supported image, so a tool can hand an
/// image back to the model instead of dumping bytes into the context.
pub fn attach_if_image(path: &str) -> Option<Image> {
    use std::io::Read;
    let mut file = std::fs::File::open(path).ok()?;
    let mut head = [0u8; 16];
    let read = file.read(&mut head).ok()?;
    sniff(&head[..read])?;
    attach(path).ok()
}

/// Short label for a composer or transcript chip.
pub fn label(image: &Image) -> String {
    let source = if image.path.is_empty() {
        &image.url
    } else {
        &image.path
    };
    if source.chars().count() <= 60 {
        return source.clone();
    }
    let head: String = source.chars().take(59).collect();
    format!("{head}…")
}

/// True when the text names an existing image file, so a path dropped into the
/// terminal can attach instead of being typed.
pub fn is_image_path(text: &str) -> bool {
    if text.is_empty() || text.contains('\n') {
        return false;
    }
    let lower = text.to_ascii_lowercase();
    if !matches!(
        lower.rsplit('.').next(),
        Some("png" | "jpg" | "jpeg" | "gif" | "webp")
    ) {
        return false;
    }
    std::path::Path::new(text).is_file()
}

/// Sniff the format from content, as both DeepSeek and OpenAI do, and fall
/// back to the file extension.
fn mime(data: &[u8], path: &str) -> &'static str {
    sniff(data).unwrap_or_else(|| by_extension(path))
}

fn sniff(data: &[u8]) -> Option<&'static str> {
    if data.starts_with(b"RIFF") && data.get(8..12) == Some(b"WEBP") {
        return Some("image/webp");
    }
    if data.starts_with(&[0x89, b'P', b'N', b'G']) {
        return Some("image/png");
    }
    if data.starts_with(&[0xff, 0xd8, 0xff]) {
        return Some("image/jpeg");
    }
    if data.starts_with(b"GIF8") {
        return Some("image/gif");
    }
    None
}

fn by_extension(path: &str) -> &'static str {
    match path
        .rsplit('.')
        .next()
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        _ => "image/png",
    }
}

pub fn base64(data: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b1 = chunk[0];
        let b2 = *chunk.get(1).unwrap_or(&0);
        let b3 = *chunk.get(2).unwrap_or(&0);
        out.push(TABLE[(b1 >> 2) as usize] as char);
        out.push(TABLE[(((b1 & 0x03) << 4) | (b2 >> 4)) as usize] as char);
        out.push(if chunk.len() > 1 {
            TABLE[(((b2 & 0x0f) << 2) | (b3 >> 6)) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[(b3 & 0x3f) as usize] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_encodes_padding() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foob"), "Zm9vYg==");
        assert_eq!(base64(b"hello world"), "aGVsbG8gd29ybGQ=");
    }

    #[test]
    fn mime_sniffs_content_then_extension() {
        assert_eq!(mime(&[0x89, b'P', b'N', b'G'], "x"), "image/png");
        assert_eq!(mime(&[0xff, 0xd8, 0xff], "x"), "image/jpeg");
        assert_eq!(mime(b"GIF89a", "x"), "image/gif");
        assert_eq!(mime(b"RIFF\0\0\0\0WEBP", "x"), "image/webp");
        assert_eq!(mime(b"??", "photo.JPG"), "image/jpeg");
    }

    #[test]
    fn sniff_only_matches_real_images() {
        assert_eq!(sniff(&[0x89, b'P', b'N', b'G']), Some("image/png"));
        assert_eq!(sniff(b"plain text"), None);
        assert_eq!(sniff(b""), None);
    }

    #[test]
    fn url_sources_pass_through() {
        let image = attach("https://example.com/a.png").unwrap();
        assert!(image.path.is_empty());
        assert_eq!(image.url, "https://example.com/a.png");
        assert_eq!(label(&image), "https://example.com/a.png");
    }
}
