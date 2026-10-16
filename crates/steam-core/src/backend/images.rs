//! Images for the game's ISteamUtils image handles (achievement icons, avatars):
//! fetched over HTTPS from Valve's image CDN only, size-bounded and decoded to
//! RGBA. No credentials are sent.
use std::time::Duration;

use steam_ipc::{ImageOp, Response, hex_encode};

const HOSTS: [&str; 5] = [
    "cdn.fastly.steamstatic.com",
    "shared.fastly.steamstatic.com",
    "avatars.fastly.steamstatic.com",
    "avatars.steamstatic.com",
    "cdn.cloudflare.steamstatic.com",
];
/// Compressed bytes accepted from the CDN.
const MAX_DOWNLOAD: usize = 2 << 20;
/// Pixels are bounded before decoding; larger images are scaled down after.
const MAX_PIXELS: u64 = 16 << 20;
/// Largest edge handed to the game: 512x512x4 bytes is 2 MiB of hex in a reply.
const MAX_EDGE: u32 = 512;
const DEADLINE: Duration = Duration::from_secs(6);

/// The parsed URL when it names an allow-listed https image host.
pub fn allowed(url: &str) -> Option<reqwest::Url> {
    let url = reqwest::Url::parse(url).ok()?;
    let host = url.host_str()?;
    (url.scheme() == "https"
        && url.username().is_empty()
        && url.password().is_none()
        && url.port().is_none()
        && HOSTS.contains(&host))
    .then_some(url)
}

/// Decodes a JPEG or PNG into RGBA, scaling down to `MAX_EDGE`.
pub fn decode(bytes: &[u8]) -> Result<(u32, u32, Vec<u8>), &'static str> {
    let format = image::guess_format(bytes).map_err(|_| "unrecognized image format")?;
    if !matches!(format, image::ImageFormat::Jpeg | image::ImageFormat::Png) {
        return Err("unsupported image format");
    }
    let mut reader = image::ImageReader::with_format(std::io::Cursor::new(bytes), format);
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(MAX_PIXELS * 4);
    reader.limits(limits);
    let decoded = reader.decode().map_err(|_| "image could not be decoded")?;
    let (width, height) = (decoded.width(), decoded.height());
    if u64::from(width) * u64::from(height) > MAX_PIXELS {
        return Err("image is too large");
    }
    let scaled = if width > MAX_EDGE || height > MAX_EDGE {
        decoded.thumbnail(MAX_EDGE, MAX_EDGE)
    } else {
        decoded
    };
    let rgba = scaled.into_rgba8();
    Ok((rgba.width(), rgba.height(), rgba.into_raw()))
}

async fn download(url: reqwest::Url) -> Result<Vec<u8>, String> {
    let client = reqwest::Client::builder()
        .https_only(true)
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(4))
        .timeout(DEADLINE)
        .user_agent("steam-tui/images")
        .build()
        .map_err(|_| "could not initialize HTTPS client".to_owned())?;
    let mut response = client
        .get(url)
        .send()
        .await
        .map_err(|_| "image request failed".to_owned())?;
    if !response.status().is_success() {
        return Err(format!("image server answered {}", response.status()));
    }
    if response
        .content_length()
        .is_some_and(|n| n > MAX_DOWNLOAD as u64)
    {
        return Err("image is too large".into());
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| "image download failed".to_owned())?
    {
        if bytes.len() + chunk.len() > MAX_DOWNLOAD {
            return Err("image is too large".into());
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

pub async fn handle(op: ImageOp) -> Response {
    let ImageOp::Fetch { url } = op;
    let Some(url) = allowed(&url) else {
        return Response::Error {
            message: "Image host is not allowed".into(),
        };
    };
    let bytes = match download(url).await {
        Ok(bytes) => bytes,
        Err(message) => return Response::Error { message },
    };
    let decoded = tokio::task::spawn_blocking(move || decode(&bytes)).await;
    match decoded {
        Ok(Ok((width, height, rgba))) => Response::Image {
            width,
            height,
            rgba: hex_encode(&rgba),
        },
        Ok(Err(message)) => Response::Error {
            message: message.into(),
        },
        Err(_) => Response::Error {
            message: "image decoding worker failed".into(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(width: u32, height: u32) -> Vec<u8> {
        let image = image::RgbaImage::from_pixel(width, height, image::Rgba([10, 20, 30, 255]));
        let mut out = Vec::new();
        image::DynamicImage::ImageRgba8(image)
            .write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)
            .unwrap();
        out
    }

    #[test]
    fn only_https_on_valve_image_hosts_is_allowed() {
        assert!(
            allowed("https://cdn.fastly.steamstatic.com/steamcommunity/public/images/apps/1/a.jpg")
                .is_some()
        );
        for bad in [
            "http://cdn.fastly.steamstatic.com/a.jpg",
            "https://cdn.fastly.steamstatic.com:8443/a.jpg",
            "https://user@cdn.fastly.steamstatic.com/a.jpg",
            "https://evil.example/a.jpg",
            "https://cdn.fastly.steamstatic.com.evil.example/a.jpg",
            "file:///etc/passwd",
            "not a url",
        ] {
            assert!(allowed(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn decodes_png_to_rgba() {
        let (w, h, rgba) = decode(&png(3, 2)).unwrap();
        assert_eq!((w, h, rgba.len()), (3, 2, 24));
        assert_eq!(&rgba[..4], &[10, 20, 30, 255]);
    }

    #[test]
    fn large_images_are_scaled_down() {
        let (w, h, rgba) = decode(&png(1024, 512)).unwrap();
        assert_eq!((w, h), (512, 256));
        assert_eq!(rgba.len(), 512 * 256 * 4);
    }

    #[test]
    fn rejects_non_images_and_other_formats() {
        assert!(decode(b"not an image at all").is_err());
        assert!(decode(b"GIF89a\x01\x00\x01\x00\x00\x00\x00;").is_err());
        assert!(decode(&[]).is_err());
    }
}
