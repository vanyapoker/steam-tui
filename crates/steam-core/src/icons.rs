//! Public app icons from cached product metadata, local assets, or Valve's CDN.
use crate::installed::{Node, read_bounded};
use image::{DynamicImage, ImageFormat, ImageReader, Limits};
use std::{
    io::Cursor,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

const MAX_BYTES: usize = 512 * 1024;
static NEXT_FILE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone)]
pub struct Icon {
    pub key: String,
    pub image: Arc<DynamicImage>,
}
struct Reference {
    app_id: u32,
    hash: String,
}

fn reference(app_id: u32, doc: &Node) -> Option<Reference> {
    let hash = doc.at(&["common", "icon"])?.value.as_deref()?;
    (app_id != 0
        && hash.len() == 40
        && hash.bytes().all(|b| b.is_ascii_hexdigit())
        && hash.bytes().any(|b| b != b'0'))
    .then(|| Reference {
        app_id,
        hash: hash.to_ascii_lowercase(),
    })
}
impl Reference {
    fn key(&self) -> String {
        format!("{}-{}", self.app_id, self.hash)
    }
    fn url(&self) -> String {
        format!(
            "https://cdn.fastly.steamstatic.com/steamcommunity/public/images/apps/{}/{}.jpg",
            self.app_id, self.hash
        )
    }
}
fn directory() -> Option<PathBuf> {
    let root = std::env::var_os("XDG_CACHE_HOME")
        .filter(|p| !p.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))?;
    root.is_absolute().then(|| root.join("steam-tui/icons"))
}
fn decode(bytes: Vec<u8>) -> Result<Arc<DynamicImage>, String> {
    let mut reader = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|_| "Invalid game icon")?;
    let mut limits = Limits::default();
    limits.max_image_width = Some(512);
    limits.max_image_height = Some(512);
    limits.max_alloc = Some(2 * 1024 * 1024);
    reader.limits(limits);
    let image = reader.decode().map_err(|_| "Unsupported game icon")?;
    let image = if image.width() > 64 || image.height() > 64 {
        image.thumbnail(64, 64)
    } else {
        image
    };
    Ok(Arc::new(image))
}
fn persist(dir: &Path, key: &str, image: &DynamicImage) {
    let result = (|| -> Result<(), Box<dyn std::error::Error>> {
        std::fs::create_dir_all(dir)?;
        let temporary = dir.join(format!(
            ".{key}.{}.{}.tmp",
            std::process::id(),
            NEXT_FILE.fetch_add(1, Ordering::Relaxed)
        ));
        let mut encoded = Cursor::new(Vec::new());
        image.write_to(&mut encoded, ImageFormat::Png)?;
        use std::{io::Write, os::unix::fs::OpenOptionsExt};
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)?;
        let write = (|| -> std::io::Result<()> {
            file.write_all(&encoded.into_inner())?;
            file.sync_all()?;
            std::fs::rename(&temporary, dir.join(format!("{key}.png")))
        })();
        if write.is_err() {
            let _ = std::fs::remove_file(temporary);
        }
        write?;
        Ok(())
    })();
    // An unwritable cache must not hide an otherwise valid public icon.
    let _ = result;
}
fn from_file(path: &Path) -> Option<Arc<DynamicImage>> {
    decode(read_bounded(path, MAX_BYTES).ok()?).ok()
}

enum Cached {
    Ready(Icon),
    Remote(Reference),
    Missing,
}
fn cached(app_id: u32) -> Cached {
    let Some(doc) = crate::library::cached_app(app_id).ok().flatten() else {
        return Cached::Missing;
    };
    let Some(reference) = reference(app_id, &doc) else {
        return Cached::Missing;
    };
    let key = reference.key();
    let dir = directory();
    if let Some(image) = dir
        .as_ref()
        .and_then(|dir| from_file(&dir.join(format!("{key}.png"))))
    {
        return Cached::Ready(Icon { key, image });
    }
    // Reusing installed assets is optional; downloading and our cache work on
    // machines which have never installed Valve's client.
    if let Ok(root) = crate::installed::discover_root(None) {
        let path = root
            .join("appcache/librarycache")
            .join(app_id.to_string())
            .join(format!("{}.jpg", reference.hash));
        if let Some(image) = from_file(&path) {
            if let Some(dir) = dir {
                persist(&dir, &key, &image);
            }
            return Cached::Ready(Icon { key, image });
        }
    }
    Cached::Remote(reference)
}

/// Call only for visible games. All disk reads and image work stay off the UI
/// thread. Explicit offline mode never initiates a CDN request.
pub async fn load(app_id: u32, network: bool) -> Result<Option<Icon>, String> {
    let reference = match tokio::task::spawn_blocking(move || cached(app_id))
        .await
        .map_err(|_| "Game icon cache worker failed")?
    {
        Cached::Ready(icon) => return Ok(Some(icon)),
        Cached::Missing => return Ok(None),
        Cached::Remote(reference) if network => reference,
        Cached::Remote(_) => return Ok(None),
    };
    let client = reqwest::Client::builder()
        .https_only(true)
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(4))
        .timeout(Duration::from_secs(10))
        .user_agent("steam-tui/icons")
        .build()
        .map_err(|_| "Could not initialize icon downloads")?;
    let mut response = client
        .get(reference.url())
        .send()
        .await
        .map_err(|_| "Icon download failed")?;
    if !response.status().is_success() {
        return Err(format!(
            "Icon unavailable (HTTP {})",
            response.status().as_u16()
        ));
    }
    if response
        .content_length()
        .is_some_and(|len| len > MAX_BYTES as u64)
    {
        return Err("Game icon exceeds size limit".into());
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| "Icon download interrupted")?
    {
        if bytes.len() + chunk.len() > MAX_BYTES {
            return Err("Game icon exceeds size limit".into());
        }
        bytes.extend_from_slice(&chunk);
    }
    tokio::task::spawn_blocking(move || {
        let image = decode(bytes)?;
        let key = reference.key();
        if let Some(dir) = directory() {
            persist(&dir, &key, &image);
        }
        Ok(Some(Icon { key, image }))
    })
    .await
    .map_err(|_| "Icon decoding worker failed".to_owned())?
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{Rgba, RgbaImage};

    fn document(hash: &str) -> Node {
        Node {
            value: None,
            children: std::collections::BTreeMap::from([(
                "common".into(),
                Node {
                    value: None,
                    children: std::collections::BTreeMap::from([(
                        "icon".into(),
                        Node {
                            value: Some(hash.into()),
                            children: Default::default(),
                        },
                    )]),
                },
            )]),
        }
    }
    fn fixture(width: u32, height: u32) -> Vec<u8> {
        let image = DynamicImage::ImageRgba8(RgbaImage::from_pixel(
            width,
            height,
            Rgba([20, 50, 180, 255]),
        ));
        let mut bytes = Cursor::new(Vec::new());
        image.write_to(&mut bytes, ImageFormat::Png).unwrap();
        bytes.into_inner()
    }
    #[test]
    fn public_icon_reference_uses_only_valid_content_hashes() {
        let hash = "18EF632A152A69A8FB4F62C074CD50AA15D3BF33";
        let reference = reference(361420, &document(hash)).unwrap();
        assert_eq!(
            reference.key(),
            "361420-18ef632a152a69a8fb4f62c074cd50aa15d3bf33"
        );
        assert!(
            reference
                .url()
                .ends_with("/361420/18ef632a152a69a8fb4f62c074cd50aa15d3bf33.jpg")
        );
        for hash in [
            "../../outside",
            "https://evil.test/icon.jpg",
            "0000000000000000000000000000000000000000",
            "zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz",
            "abc",
        ] {
            assert!(super::reference(1, &document(hash)).is_none());
        }
        assert!(super::reference(0, &document(hash)).is_none());
    }
    #[test]
    fn icon_decoding_is_bounded_and_cache_roundtrips_without_network() {
        assert!(decode(b"not an image".to_vec()).is_err());
        assert!(decode(fixture(513, 1)).is_err());
        let icon = decode(fixture(184, 184)).unwrap();
        assert_eq!((icon.width(), icon.height()), (64, 64));
        let dir = std::env::temp_dir().join(format!(
            "steam-tui-icon-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        persist(&dir, "1-test", &icon);
        let loaded = from_file(&dir.join("1-test.png")).unwrap();
        assert_eq!(loaded.to_rgba8(), icon.to_rgba8());
        std::fs::write(dir.join("1-test.png"), b"damaged").unwrap();
        assert!(from_file(&dir.join("1-test.png")).is_none());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
