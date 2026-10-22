//! Bounded, asynchronous previews of Steam-hosted chat images.
use image::{DynamicImage, ImageReader, Limits};
use ratatui::{buffer::Buffer, layout::Rect, widgets::Widget};
use ratatui_image::{
    Image, Resize,
    picker::{Picker, ProtocolType},
    protocol::Protocol,
};
use std::{
    collections::{BTreeMap, VecDeque},
    io::Cursor,
    sync::Arc,
    time::Duration,
};

const CACHE_SIZE: usize = 12;
const MAX_DOWNLOAD: usize = 12 * 1024 * 1024;

pub(super) fn allowed(url: &reqwest::Url) -> bool {
    url.scheme() == "https"
        && url.username().is_empty()
        && url.password().is_none()
        && url.port_or_known_default() == Some(443)
        && matches!(
            url.host_str(),
            Some(
                "images.steamusercontent.com"
                    | "steamuserimages-a.akamaihd.net"
                    | "cdn.akamai.steamstatic.com"
                    | "shared.fastly.steamstatic.com"
                    | "clan.fastly.steamstatic.com"
                    | "clan.akamai.steamstatic.com"
                    | "cdn.fastly.steamstatic.com"
            )
        )
}

pub(super) fn links(text: &str) -> Vec<String> {
    let mut links = Vec::new();
    for (_, suffix) in text.match_indices("https://").map(|(i, _)| (i, &text[i..])) {
        let end = suffix
            .find(|c: char| c.is_whitespace() || matches!(c, '<' | '>' | '[' | ']' | '\'' | '"'))
            .unwrap_or(suffix.len());
        let raw = suffix[..end].trim_end_matches([')', ',', '.', ';']);
        if let Ok(mut url) = reqwest::Url::parse(raw) {
            url.set_fragment(None);
            if allowed(&url) && !links.contains(&url.to_string()) {
                links.push(url.to_string());
            }
        }
        if links.len() == 4 {
            break;
        }
    }
    links
}

pub(super) struct Encoded {
    source: Arc<DynamicImage>,
    native: Protocol,
    fallback: Protocol,
}

pub(super) struct Request {
    url: String,
    size: (u16, u16),
    picker: Picker,
    source: Option<Arc<DynamicImage>>,
}

pub(super) struct Loaded {
    url: String,
    size: (u16, u16),
    result: Result<Encoded, String>,
}

struct Entry {
    size: (u16, u16),
    image: Option<Encoded>,
    busy: bool,
    error: Option<String>,
    used: u64,
}

pub(super) struct Images {
    pub picker: Picker,
    entries: BTreeMap<String, Entry>,
    pub pending: VecDeque<Request>,
    pub active: usize,
    clock: u64,
    capacity: usize,
}

impl Default for Images {
    fn default() -> Self {
        Self {
            picker: Picker::halfblocks(),
            entries: BTreeMap::new(),
            pending: VecDeque::new(),
            active: 0,
            clock: 0,
            capacity: CACHE_SIZE,
        }
    }
}

impl Images {
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            ..Default::default()
        }
    }

    /// Keep unloaded image slots stable, but collapse failures to a single status row.
    pub fn preview_rows(&self, url: &str, maximum: u16) -> u16 {
        if self
            .entries
            .get(url)
            .is_some_and(|entry| entry.error.is_some())
        {
            1
        } else {
            maximum
        }
    }

    pub fn render_fallback(&self, key: &str, area: Rect, buffer: &mut Buffer) -> bool {
        let Some(entry) = self.entries.get(key) else {
            return false;
        };
        let Some(image) = &entry.image else {
            return false;
        };
        if entry.busy || entry.error.is_some() {
            return false;
        }
        Image::new(&image.fallback).render(area, buffer);
        true
    }

    /// Returns the reserved number of rows. Network and encoding work is queued,
    /// never performed from the renderer.
    pub fn prepare(&mut self, url: &str, size: (u16, u16)) -> u16 {
        self.clock = self.clock.wrapping_add(1);
        if !self.entries.contains_key(url) {
            if self.entries.len() >= self.capacity {
                let victim = self
                    .entries
                    .iter()
                    .filter(|(_, entry)| !entry.busy)
                    .min_by_key(|(_, entry)| entry.used)
                    .map(|(url, _)| url.clone());
                if let Some(victim) = victim {
                    self.entries.remove(&victim);
                } else {
                    return 1;
                }
            }
            self.entries.insert(
                url.into(),
                Entry {
                    size,
                    image: None,
                    busy: false,
                    error: None,
                    used: self.clock,
                },
            );
        }
        let entry = self.entries.get_mut(url).expect("inserted image entry");
        entry.used = self.clock;
        if !entry.busy && entry.error.is_none() && (entry.image.is_none() || entry.size != size) {
            entry.busy = true;
            entry.size = size;
            self.pending.push_back(Request {
                url: url.into(),
                size,
                picker: self.picker.clone(),
                source: entry.image.as_ref().map(|image| image.source.clone()),
            });
        }
        if entry.error.is_some() { 1 } else { size.1 }
    }

    pub fn prepare_local(&mut self, key: &str, source: Arc<DynamicImage>, size: (u16, u16)) {
        self.prepare(key, size);
        if let Some(request) = self.pending.iter_mut().find(|request| request.url == key) {
            request.source = Some(source);
        }
    }

    pub fn loaded(&mut self, loaded: Loaded) {
        self.active = self.active.saturating_sub(1);
        if let Some(entry) = self.entries.get_mut(&loaded.url) {
            if entry.size != loaded.size {
                return;
            }
            entry.busy = false;
            match loaded.result {
                Ok(image) => {
                    entry.image = Some(image);
                    entry.error = None;
                }
                Err(error) => entry.error = Some(error),
            }
        }
    }

    pub fn status(&self, url: &str) -> &str {
        match self.entries.get(url) {
            Some(entry) => entry.error.as_deref().unwrap_or("Loading image…"),
            None => "Image preview cache busy",
        }
    }

    /// Partially visible previews use text cells so no graphics escape the chat
    /// viewport. A fully visible preview uses the negotiated native protocol.
    pub fn render(
        &self,
        url: &str,
        area: Rect,
        skip: u16,
        full_height: u16,
        buffer: &mut Buffer,
    ) -> bool {
        let Some(entry) = self.entries.get(url) else {
            return false;
        };
        let Some(image) = &entry.image else {
            return false;
        };
        if entry.busy || entry.error.is_some() {
            return false;
        }
        if skip == 0 && area.height == full_height {
            Image::new(&image.native).render(area, buffer);
        } else {
            let full = Rect::new(0, 0, area.width, full_height);
            let mut temporary = Buffer::empty(full);
            Image::new(&image.fallback).render(full, &mut temporary);
            for y in 0..area.height {
                for x in 0..area.width {
                    if y + skip < full_height {
                        buffer[(area.x + x, area.y + y)] = temporary[(x, y + skip)].clone();
                    }
                }
            }
        }
        true
    }
}

pub(super) async fn load(request: Request) -> Loaded {
    let result = match request.source {
        Some(source) => encode(request.picker, source, request.size).await,
        None => match download(&request.url).await {
            Ok(bytes) => {
                let decoded = tokio::task::spawn_blocking(move || decode(bytes)).await;
                match decoded {
                    Ok(Ok(source)) => encode(request.picker, Arc::new(source), request.size).await,
                    _ => Err("Image format or dimensions unsupported".into()),
                }
            }
            Err(error) => Err(error),
        },
    };
    Loaded {
        url: request.url,
        size: request.size,
        result,
    }
}

async fn download(url: &str) -> Result<Vec<u8>, String> {
    let url = reqwest::Url::parse(url).map_err(|_| "Invalid image URL")?;
    if !allowed(&url) {
        return Err("Image host unsupported".into());
    }
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .connect_timeout(Duration::from_secs(5))
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            if attempt.previous().len() < 3 && allowed(attempt.url()) {
                attempt.follow()
            } else {
                attempt.stop()
            }
        }))
        .build()
        .map_err(|_| "Image download unavailable")?;
    let mut response = client
        .get(url)
        .send()
        .await
        .map_err(|_| "Image download failed")?;
    if !response.status().is_success() {
        return Err(format!(
            "Image unavailable (HTTP {})",
            response.status().as_u16()
        ));
    }
    if response.status().is_redirection() {
        return Err("Image redirect unsupported".into());
    }
    if response
        .content_length()
        .is_some_and(|size| size > MAX_DOWNLOAD as u64)
    {
        return Err("Image exceeds 12 MiB".into());
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| "Image download interrupted")?
    {
        if bytes.len().saturating_add(chunk.len()) > MAX_DOWNLOAD {
            return Err("Image exceeds 12 MiB".into());
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

fn decode(bytes: Vec<u8>) -> Result<DynamicImage, image::ImageError> {
    let mut reader = ImageReader::new(Cursor::new(bytes)).with_guessed_format()?;
    let mut limits = Limits::default();
    limits.max_image_width = Some(4096);
    limits.max_image_height = Some(4096);
    limits.max_alloc = Some(64 * 1024 * 1024);
    reader.limits(limits);
    let image = reader.decode()?;
    Ok(if image.width() > 1024 || image.height() > 1024 {
        image.thumbnail(1024, 1024)
    } else {
        image
    })
}

async fn encode(
    picker: Picker,
    source: Arc<DynamicImage>,
    size: (u16, u16),
) -> Result<Encoded, String> {
    tokio::task::spawn_blocking(move || {
        let source = if source.width() > 1024 || source.height() > 1024 {
            Arc::new(source.thumbnail(1024, 1024))
        } else {
            source
        };
        let area = Rect::new(0, 0, size.0, size.1);
        let native = picker
            .new_protocol((*source).clone(), area, Resize::Fit(None))
            .map_err(|_| "Image encoding failed")?;
        let mut fallback_picker = picker.clone();
        fallback_picker.set_protocol_type(ProtocolType::Halfblocks);
        let fallback = fallback_picker
            .new_protocol((*source).clone(), area, Resize::Fit(None))
            .map_err(|_| "Image encoding failed")?;
        Ok(Encoded {
            source,
            native,
            fallback,
        })
    })
    .await
    .map_err(|_| "Image encoding interrupted".to_owned())?
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ImageFormat, Rgba, RgbaImage};

    fn fixture(width: u32, height: u32) -> DynamicImage {
        DynamicImage::ImageRgba8(RgbaImage::from_pixel(
            width,
            height,
            Rgba([210, 40, 90, 255]),
        ))
    }

    #[test]
    fn steam_links_are_deduplicated_and_other_hosts_are_not_downloaded() {
        let url = "https://images.steamusercontent.com/ugc/123/ABC/";
        assert_eq!(
            links(&format!("[img]{url}[/img] ({url}), {url}#fragment")),
            [url]
        );
        for url in [
            "https://images.steamusercontent.com.evil.test/a.png",
            "https://evil.test/a.png",
            "https://user:pass@images.steamusercontent.com/a",
            "https://images.steamusercontent.com:444/a",
            "https://127.0.0.1/a",
            "http://images.steamusercontent.com/a",
        ] {
            assert!(links(url).is_empty(), "{url}");
        }
    }

    #[test]
    fn decoding_rejects_non_images_and_excessive_dimensions() {
        assert!(decode(b"<html>no image</html>".to_vec()).is_err());
        let mut encoded = Cursor::new(Vec::new());
        fixture(5000, 1)
            .write_to(&mut encoded, ImageFormat::Png)
            .unwrap();
        assert!(decode(encoded.into_inner()).is_err());
        let mut encoded = Cursor::new(Vec::new());
        fixture(100, 50)
            .write_to(&mut encoded, ImageFormat::Png)
            .unwrap();
        let image = decode(encoded.into_inner()).unwrap();
        assert_eq!((image.width(), image.height()), (100, 50));
    }

    #[test]
    fn cache_and_work_queue_are_bounded_and_visible_requests_are_deduplicated() {
        let mut images = Images::default();
        for i in 0..100 {
            images.prepare(&format!("https://images.steamusercontent.com/{i}"), (20, 8));
        }
        assert_eq!(images.entries.len(), CACHE_SIZE);
        assert_eq!(images.pending.len(), CACHE_SIZE);
        images.prepare("https://images.steamusercontent.com/0", (20, 8));
        assert_eq!(images.pending.len(), CACHE_SIZE);
    }

    #[tokio::test]
    async fn failed_news_image_slots_collapse_and_cdn_hosts_are_checked() {
        let mut images = Images::default();
        let url = "https://example.invalid/not-a-steam-image";
        images.prepare(url, (80, 12));
        assert_eq!(images.preview_rows(url, 12), 12);
        let request = images.pending.pop_front().unwrap();
        images.active += 1;
        images.loaded(load(request).await);
        assert_eq!(images.preview_rows(url, 12), 1);
        assert_eq!(images.status(url), "Image host unsupported");
        for url in [
            "https://clan.fastly.steamstatic.com/images/a.jpg",
            "https://clan.akamai.steamstatic.com/images/a.gif",
            "https://cdn.fastly.steamstatic.com/a.jpg",
        ] {
            assert!(allowed(&reqwest::Url::parse(url).unwrap()));
        }
    }

    #[tokio::test]
    async fn image_encoding_and_partial_render_never_touch_cells_outside_the_viewport() {
        let url = "https://images.steamusercontent.com/test";
        let mut images = Images::default();
        images.prepare(url, (20, 8));
        let mut request = images.pending.pop_front().unwrap();
        request.source = Some(Arc::new(fixture(120, 80)));
        images.active = 1;
        images.loaded(load(request).await);
        assert_eq!(images.active, 0);
        assert!(images.entries[url].image.is_some());
        let area = Rect::new(5, 3, 20, 3);
        let mut buffer = Buffer::empty(Rect::new(0, 0, 40, 20));
        for cell in &mut buffer.content {
            cell.set_symbol("x");
        }
        assert!(images.render(url, area, 2, 8, &mut buffer));
        let mut pixels = 0;
        for y in 0..20 {
            for x in 0..40 {
                if !area.contains((x, y).into()) {
                    assert_eq!(buffer[(x, y)].symbol(), "x");
                } else if buffer[(x, y)].fg != ratatui::style::Color::Reset {
                    pixels += 1;
                }
            }
        }
        assert!(pixels > 0);
        images.prepare(url, (10, 4));
        assert_eq!(images.pending.len(), 1);
        let request = images.pending.pop_front().unwrap();
        assert!(
            request.source.is_some(),
            "resize uses decoded cache without another download"
        );
    }

    #[tokio::test]
    async fn native_kitty_protocol_encodes_without_opening_a_window_or_writing_to_the_terminal() {
        let mut picker = Picker::halfblocks();
        picker.set_protocol_type(ProtocolType::Kitty);
        let image = encode(picker, Arc::new(fixture(64, 64)), (20, 8))
            .await
            .unwrap();
        assert!(matches!(image.native, Protocol::Kitty(_)));
        assert!(matches!(image.fallback, Protocol::Halfblocks(_)));
    }

    #[tokio::test]
    async fn failed_load_keeps_a_clear_placeholder_and_does_not_loop() {
        let url = "https://127.0.0.1/image.png";
        let mut images = Images::default();
        images.prepare(url, (20, 8));
        let request = images.pending.pop_front().unwrap();
        images.active = 1;
        images.loaded(load(request).await);
        assert!(images.status(url).contains("unsupported"));
        images.prepare(url, (20, 8));
        assert!(images.pending.is_empty());
        assert_eq!(images.active, 0);
    }
    #[tokio::test]
    async fn chat_layout_fetches_only_visible_previews_and_redraws_decoded_images() {
        use super::super::{model::Model, render};
        use ratatui::{Terminal, backend::TestBackend};
        use steam_core::{
            chat::{Event, Message},
            friends::{Friend, Presence},
        };
        let mut model = Model::new(false, None);
        model.set_friends(vec![Friend {
            steam_id: 10,
            name: "Alice".into(),
            state: Presence::Online,
            game: None,
        }]);
        let messages = (0..40)
            .map(|i| Message {
                timestamp: 100 + i,
                ordinal: 0,
                outgoing: false,
                text: format!("https://images.steamusercontent.com/ugc/{i}/image/"),
            })
            .collect();
        model.chat_event(Event::History {
            friend: 10,
            messages,
            more: false,
            older: false,
        });
        model.open_chat();
        let mut terminal = Terminal::new(TestBackend::new(100, 40)).unwrap();
        terminal
            .draw(|frame| render::draw(frame, &mut model))
            .unwrap();
        assert!(!model.chat.images.pending.is_empty());
        assert!(
            model.chat.images.pending.len() <= 3,
            "off-screen history must not enqueue image downloads"
        );
        while let Some(mut request) = model.chat.images.pending.pop_front() {
            request.source = Some(Arc::new(fixture(120, 80)));
            model.chat.images.active += 1;
            model.chat.images.loaded(load(request).await);
        }
        terminal
            .draw(|frame| render::draw(frame, &mut model))
            .unwrap();
        assert!(
            terminal
                .backend()
                .buffer()
                .content
                .iter()
                .any(|cell| cell.bg == ratatui::style::Color::Rgb(210, 40, 90))
        );
        model.chat.offset = 5;
        terminal
            .draw(|frame| render::draw(frame, &mut model))
            .unwrap();
        model.chat.active = None;
        terminal
            .draw(|frame| render::draw(frame, &mut model))
            .unwrap();
        assert!(
            !terminal
                .backend()
                .buffer()
                .content
                .iter()
                .any(|cell| cell.bg == ratatui::style::Color::Rgb(210, 40, 90))
        );
    }
    #[tokio::test]
    async fn local_clipboard_attachment_previews_never_attempt_a_download() {
        use super::super::{clipboard::Content, model::Model, render};
        use ratatui::{Terminal, backend::TestBackend};
        use steam_core::{
            chat::upload::Attachment,
            friends::{Friend, Presence},
        };
        let mut model = Model::new(false, None);
        model.set_friends(vec![Friend {
            steam_id: 10,
            name: "Alice".into(),
            state: Presence::Online,
            game: None,
        }]);
        model.open_chat();
        let mut bytes = Cursor::new(Vec::new());
        fixture(120, 80)
            .write_to(&mut bytes, ImageFormat::Png)
            .unwrap();
        model.clipboard_result(
            10,
            Ok(Content::Image(
                Attachment::decode(bytes.into_inner()).unwrap(),
            )),
        );
        let mut terminal = Terminal::new(TestBackend::new(100, 40)).unwrap();
        terminal
            .draw(|frame| render::draw(frame, &mut model))
            .unwrap();
        assert_eq!(model.chat.images.pending.len(), 1);
        let request = model.chat.images.pending.pop_front().unwrap();
        assert!(request.source.is_some());
        assert!(request.url.starts_with("clipboard:"));
        model.chat.images.active += 1;
        model.chat.images.loaded(load(request).await);
        terminal
            .draw(|frame| render::draw(frame, &mut model))
            .unwrap();
        assert!(
            terminal
                .backend()
                .buffer()
                .content
                .iter()
                .any(|cell| cell.bg == ratatui::style::Color::Rgb(210, 40, 90))
        );
        for (width, height) in [(45, 16), (12, 5), (1, 1)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal
                .draw(|frame| render::draw(frame, &mut model))
                .unwrap();
        }
    }
}
