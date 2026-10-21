use super::chat_images::Images;
use ratatui::{buffer::Buffer, layout::Rect};
use std::collections::{BTreeMap, VecDeque};
use steam_core::icons::Icon;

pub(super) struct Request {
    pub app_id: u32,
    pub generation: u64,
    pub network: bool,
}
pub(super) struct Loaded {
    pub app_id: u32,
    pub generation: u64,
    pub result: Result<Option<Icon>, String>,
}
struct Entry {
    icon: Option<Icon>,
    used: u64,
    retry_at: Option<std::time::Instant>,
}

pub(super) struct LibraryIcons {
    pub images: Images,
    pub pending: VecDeque<Request>,
    pub active: usize,
    entries: BTreeMap<u32, Entry>,
    generation: u64,
    clock: u64,
    network: bool,
}
impl LibraryIcons {
    pub fn new(network: bool) -> Self {
        Self {
            images: Images::with_capacity(256),
            pending: VecDeque::new(),
            active: 0,
            entries: BTreeMap::new(),
            generation: 0,
            clock: 0,
            network,
        }
    }
    pub fn refresh(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.entries.clear();
        self.pending.clear();
    }
    pub fn visible(&mut self, ids: &std::collections::BTreeSet<u32>) {
        let canceled: Vec<_> = self
            .pending
            .iter()
            .filter(|request| !ids.contains(&request.app_id))
            .map(|request| request.app_id)
            .collect();
        self.pending.retain(|request| ids.contains(&request.app_id));
        for id in canceled {
            self.entries.remove(&id);
        }
    }
    pub fn prepare(&mut self, app_id: u32) {
        self.clock = self.clock.wrapping_add(1);
        if self.entries.get(&app_id).is_some_and(|entry| {
            entry
                .retry_at
                .is_some_and(|at| std::time::Instant::now() >= at)
        }) {
            self.entries.remove(&app_id);
        }
        if !self.entries.contains_key(&app_id) {
            if self.entries.len() >= 512 {
                let victim = self
                    .entries
                    .iter()
                    .min_by_key(|(_, entry)| entry.used)
                    .map(|(&id, _)| id);
                if let Some(victim) = victim {
                    self.entries.remove(&victim);
                }
            }
            if self.pending.len() >= 128 {
                return;
            }
            self.entries.insert(
                app_id,
                Entry {
                    icon: None,
                    used: self.clock,
                    retry_at: None,
                },
            );
            self.pending.push_back(Request {
                app_id,
                generation: self.generation,
                network: self.network,
            });
        }
        if let Some(entry) = self.entries.get_mut(&app_id) {
            entry.used = self.clock;
            if let Some(icon) = &entry.icon {
                self.images
                    .prepare_local(&icon.key, icon.image.clone(), (2, 1));
            }
        }
    }
    pub fn loaded(&mut self, loaded: Loaded) {
        self.active = self.active.saturating_sub(1);
        if loaded.generation != self.generation {
            return;
        }
        if let Some(entry) = self.entries.get_mut(&loaded.app_id) {
            match loaded.result {
                Ok(icon) => {
                    entry.icon = icon;
                    entry.retry_at = None;
                }
                Err(_) => {
                    entry.retry_at =
                        Some(std::time::Instant::now() + std::time::Duration::from_secs(30))
                }
            }
        }
    }
    pub fn render(&self, app_id: u32, area: Rect, native: bool, buffer: &mut Buffer) {
        let Some(icon) = self
            .entries
            .get(&app_id)
            .and_then(|entry| entry.icon.as_ref())
        else {
            return;
        };
        if native {
            self.images.render(&icon.key, area, 0, 1, buffer);
        } else {
            self.images.render_fallback(&icon.key, area, buffer);
        }
    }
}

pub(super) async fn load(request: Request) -> Loaded {
    Loaded {
        app_id: request.app_id,
        generation: request.generation,
        result: steam_core::icons::load(request.app_id, request.network).await,
    }
}

#[cfg(test)]
mod tests {
    use super::super::{chat_images, model::Model, render};
    use super::*;
    use crossterm::event::{
        KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    };
    use image::{DynamicImage, Rgba, RgbaImage};
    use ratatui::{Terminal, backend::TestBackend};
    use std::sync::Arc;
    use steam_core::catalog::{CatalogItem, CatalogSnapshot, UpdateStatus};

    fn icon(id: u32) -> Icon {
        Icon {
            key: format!("test-icon-{id}"),
            image: Arc::new(DynamicImage::ImageRgba8(RgbaImage::from_pixel(
                32,
                32,
                Rgba([37, id as u8, 173, 255]),
            ))),
        }
    }
    fn game(app_id: u32) -> CatalogItem {
        CatalogItem {
            app_id,
            name: format!("Game {app_id:02}"),
            installed: None,
            plan: None,
            update: UpdateStatus::Unknown,
            last_played: None,
            playtime_minutes: None,
            size_bytes: None,
            unavailable_reason: None,
        }
    }
    #[tokio::test]
    async fn icons_follow_visible_game_rows_including_pins_filter_and_mouse_selection() {
        let mut model = Model::new(true, None);
        model.set_catalog(CatalogSnapshot {
            items: (1..40).map(game).collect(),
            warnings: vec![],
        });
        model.key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE));
        let mut terminal = Terminal::new(TestBackend::new(150, 40)).unwrap();
        terminal
            .draw(|frame| render::draw(frame, &mut model))
            .unwrap();
        assert!(!model.library_icons.pending.is_empty());
        assert!(
            model.library_icons.pending.len() < 39,
            "only visible rows request assets"
        );
        while let Some(request) = model.library_icons.pending.pop_front() {
            assert!(!request.network, "offline mode must use only local assets");
            model.library_icons.active += 1;
            model.library_icons.loaded(Loaded {
                app_id: request.app_id,
                generation: request.generation,
                result: Ok(Some(icon(request.app_id))),
            });
        }
        terminal
            .draw(|frame| render::draw(frame, &mut model))
            .unwrap();
        while let Some(request) = model.library_icons.images.pending.pop_front() {
            model.library_icons.images.active += 1;
            model
                .library_icons
                .images
                .loaded(chat_images::load(request).await);
        }
        terminal
            .draw(|frame| render::draw(frame, &mut model))
            .unwrap();
        for (rect, index) in &model.library_hits {
            let color =
                ratatui::style::Color::Rgb(37, model.games[model.order[*index]].app_id as u8, 173);
            assert!(
                (rect.x..rect.right())
                    .any(|x| terminal.backend().buffer()[(x, rect.y)].bg == color),
                "icon must identify the game on the same row"
            );
        }
        let (rect, index) = model.library_hits[1];
        let app_id = model.games[model.order[index]].app_id;
        model.mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: rect.x + 4,
            row: rect.y,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(model.selected_game().unwrap().app_id, app_id);
        model.key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE));
        model.key(KeyEvent::new(KeyCode::Char('3'), KeyModifiers::NONE));
        terminal
            .draw(|frame| render::draw(frame, &mut model))
            .unwrap();
        assert!(
            model
                .order
                .iter()
                .all(|&i| model.games[i].name.contains('3'))
        );
        for (rect, index) in &model.library_hits {
            let app_id = model.games[model.order[*index]].app_id;
            if model
                .library_icons
                .entries
                .get(&app_id)
                .is_some_and(|entry| entry.icon.is_some())
            {
                let color = ratatui::style::Color::Rgb(37, app_id as u8, 173);
                assert!(
                    (rect.x..rect.right())
                        .any(|x| terminal.backend().buffer()[(x, rect.y)].bg == color)
                );
            }
        }
    }
    #[test]
    fn refresh_discards_stale_results_and_scrolling_cancels_queued_assets() {
        let mut icons = LibraryIcons::new(false);
        icons.prepare(1);
        icons.prepare(2);
        icons.visible(&[2].into_iter().collect());
        assert_eq!(icons.pending.len(), 1);
        assert_eq!(icons.pending.front().unwrap().app_id, 2);
        let request = icons.pending.pop_front().unwrap();
        icons.active = 1;
        icons.refresh();
        icons.loaded(Loaded {
            app_id: 2,
            generation: request.generation,
            result: Ok(Some(icon(2))),
        });
        assert_eq!(icons.active, 0);
        assert!(icons.entries.is_empty());
        icons.prepare(2);
        assert_eq!(icons.pending.len(), 1);
        assert_ne!(
            icons.pending.front().unwrap().generation,
            request.generation
        );
    }
    #[tokio::test]
    async fn covered_icons_use_text_without_consuming_native_graphics_transmission() {
        let mut icons = LibraryIcons::new(false);
        icons
            .images
            .picker
            .set_protocol_type(ratatui_image::picker::ProtocolType::Kitty);
        icons.prepare(1);
        let request = icons.pending.pop_front().unwrap();
        icons.loaded(Loaded {
            app_id: 1,
            generation: request.generation,
            result: Ok(Some(icon(1))),
        });
        icons.prepare(1);
        let request = icons.images.pending.pop_front().unwrap();
        icons.images.loaded(chat_images::load(request).await);
        let area = Rect::new(0, 0, 2, 1);
        let mut covered = Buffer::empty(area);
        icons.render(1, area, false, &mut covered);
        assert!(
            !covered
                .content
                .iter()
                .any(|cell| cell.symbol().contains('\u{1b}'))
        );
        let mut visible = Buffer::empty(area);
        icons.render(1, area, true, &mut visible);
        assert!(
            visible
                .content
                .iter()
                .any(|cell| cell.symbol().contains("\u{1b}_G")),
            "native transmission must still occur after closing a covering overlay"
        );
    }
}
