//! Steam announcement markup, reduced to styled paragraphs and image slots.
use super::{chat_images, theme};
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};

#[derive(Clone, Debug)]
pub(super) enum Block {
    Text(RichLine),
    Image(String),
}

impl Block {
    pub fn text(line: Line<'static>) -> Self {
        Self::Text(RichLine::new(line, Vec::new()))
    }
}

#[derive(Clone, Debug)]
pub(super) struct RichLine {
    pub line: Line<'static>,
    pub targets: Vec<Option<String>>,
}
impl std::ops::Deref for RichLine {
    type Target = Line<'static>;
    fn deref(&self) -> &Self::Target {
        &self.line
    }
}
impl std::fmt::Display for RichLine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.line.fmt(f)
    }
}
impl From<RichLine> for ratatui::text::Text<'static> {
    fn from(line: RichLine) -> Self {
        line.line.into()
    }
}
impl RichLine {
    /// Render link IDs through the same word wrapping as the visible paragraph.
    /// This preserves exact hit regions for styled spans, wide text and scrolling.
    pub fn link_hits(
        &self,
        area: ratatui::layout::Rect,
        skip: u16,
    ) -> Vec<(ratatui::layout::Rect, String)> {
        use ratatui::{
            buffer::Buffer,
            layout::Rect,
            widgets::{Paragraph, Widget, Wrap},
        };
        if !self.targets.iter().any(Option::is_some) {
            return Vec::new();
        }
        let bounds = Rect::new(0, 0, area.width, area.height);
        let mut buffer = Buffer::empty(bounds);
        let mut line = self.line.clone();
        line.style = Style::default();
        for (index, span) in line.spans.iter_mut().enumerate() {
            let id = if self.targets.get(index).is_some_and(Option::is_some) {
                index + 1
            } else {
                0
            };
            span.style = Style::default().fg(if id == 0 {
                Color::Reset
            } else {
                Color::Rgb((id >> 16) as u8, (id >> 8) as u8, id as u8)
            });
        }
        Paragraph::new(line)
            .wrap(Wrap { trim: false })
            .scroll((skip, 0))
            .render(bounds, &mut buffer);
        let target = |x, y| {
            if let Color::Rgb(r, g, b) = buffer[(x, y)].fg {
                let id = (usize::from(r) << 16) | (usize::from(g) << 8) | usize::from(b);
                id.checked_sub(1)
                    .and_then(|index| self.targets.get(index))
                    .and_then(Option::as_deref)
            } else {
                None
            }
        };
        let mut hits = Vec::new();
        for y in 0..area.height {
            let mut x = 0;
            while x < area.width {
                if let Some(url) = target(x, y) {
                    let start = x;
                    x += 1;
                    while x < area.width && target(x, y) == Some(url) {
                        x += 1;
                    }
                    hits.push((
                        Rect::new(area.x + start, area.y + y, x - start, 1),
                        url.to_owned(),
                    ));
                } else {
                    x += 1;
                }
            }
        }
        hits
    }

    fn new(mut line: Line<'static>, targets: Vec<Option<String>>) -> Self {
        let mut spans = Vec::new();
        let mut links = Vec::new();
        for (index, span) in std::mem::take(&mut line.spans).into_iter().enumerate() {
            if let Some(target) = targets.get(index).and_then(Clone::clone) {
                spans.push(span);
                links.push(Some(target));
                continue;
            }
            let text = span.content.as_ref();
            let mut cursor = 0;
            while let Some(at) = text[cursor..]
                .find("https://")
                .map(|at| cursor + at)
                .into_iter()
                .chain(text[cursor..].find("http://").map(|at| cursor + at))
                .min()
            {
                let end = text[at..]
                    .find(|ch: char| ch.is_whitespace() || matches!(ch, '<' | '>' | '\'' | '"'))
                    .map_or(text.len(), |end| at + end);
                let raw =
                    text[at..end].trim_end_matches(['.', ',', ';', ':', '!', '?', ')', ']', '}']);
                let end = at + raw.len();
                if at > cursor {
                    spans.push(Span::styled(text[cursor..at].to_owned(), span.style));
                    links.push(None);
                }
                spans.push(Span::styled(
                    raw.to_owned(),
                    span.style
                        .patch(theme::accent())
                        .add_modifier(Modifier::UNDERLINED),
                ));
                links.push(link_url(raw));
                cursor = end;
                if cursor == text.len() {
                    break;
                }
            }
            if cursor < text.len() {
                spans.push(Span::styled(text[cursor..].to_owned(), span.style));
                links.push(None);
            }
        }
        line.spans = spans;
        Self {
            line,
            targets: links,
        }
    }
}

pub(super) fn image_url(raw: &str) -> Option<String> {
    let raw = entities(raw).replace(
        "{STEAM_CLAN_IMAGE}",
        "https://clan.fastly.steamstatic.com/images",
    );
    let mut url = reqwest::Url::parse(raw.trim()).ok()?;
    url.set_fragment(None);
    chat_images::allowed(&url).then(|| url.to_string())
}

pub(super) fn link_url(raw: &str) -> Option<String> {
    let raw = entities(raw);
    let url = reqwest::Url::parse(raw.trim()).ok()?;
    (matches!(url.scheme(), "https" | "http")
        && url.username().is_empty()
        && url.password().is_none())
    .then(|| url.to_string())
}

fn entities(text: &str) -> String {
    let mut output = String::new();
    let mut rest = text;
    while let Some(at) = rest.find('&') {
        output.push_str(&rest[..at]);
        rest = &rest[at..];
        let Some(end) = rest
            .char_indices()
            .take_while(|(index, _)| *index <= 16)
            .find(|(_, ch)| *ch == ';')
            .map(|(index, _)| index)
        else {
            output.push('&');
            rest = &rest[1..];
            continue;
        };
        let entity = &rest[1..end];
        let decoded = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" | "#39" => Some('\''),
            "nbsp" => Some(' '),
            "ndash" => Some('–'),
            "mdash" => Some('—'),
            "hellip" => Some('…'),
            "lsquo" => Some('‘'),
            "rsquo" => Some('’'),
            "ldquo" => Some('“'),
            "rdquo" => Some('”'),
            _ => entity
                .strip_prefix("#x")
                .or_else(|| entity.strip_prefix("#X"))
                .and_then(|value| u32::from_str_radix(value, 16).ok())
                .or_else(|| {
                    entity
                        .strip_prefix('#')
                        .and_then(|value| value.parse().ok())
                })
                .and_then(char::from_u32),
        };
        if let Some(ch) = decoded {
            output.push(ch);
        } else {
            output.push_str(&rest[..=end]);
        }
        rest = &rest[end + 1..];
    }
    output.push_str(rest);
    output
        .replace("\r\n", "\n")
        .split('\n')
        .map(|line| super::super::terminal_text(&line.replace('\t', "    ")).into_owned())
        .collect::<Vec<_>>()
        .join("\n")
}

// Both BBCode's [url="..."] and HTML/Steam's href/src attributes.
fn attribute<'a>(tag: &'a str, wanted: &str) -> Option<&'a str> {
    let mut rest = tag.trim();
    let name_end = rest
        .find(|ch: char| ch.is_whitespace() || ch == '=')
        .unwrap_or(rest.len());
    rest = &rest[name_end..];
    if let Some(value) = rest.strip_prefix('=') {
        return Some(value.trim().trim_matches(['\'', '"']));
    }
    while !rest.trim_start().is_empty() {
        rest = rest.trim_start();
        let end = rest
            .find(|ch: char| ch.is_whitespace() || ch == '=')
            .unwrap_or(rest.len());
        let name = &rest[..end];
        rest = rest[end..].trim_start();
        let Some(value) = rest.strip_prefix('=') else {
            if rest.is_empty() {
                break;
            }
            continue;
        };
        rest = value.trim_start();
        let (value, tail) =
            if let Some(quote) = rest.chars().next().filter(|ch| matches!(ch, '\'' | '"')) {
                let after = &rest[1..];
                let end = after.find(quote).unwrap_or(after.len());
                (&after[..end], after.get(end + 1..).unwrap_or(""))
            } else {
                let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
                (&rest[..end], &rest[end..])
            };
        rest = tail;
        if name.eq_ignore_ascii_case(wanted) {
            return Some(value);
        }
    }
    None
}

struct Open {
    name: String,
    style: Style,
    link: Option<String>,
    label: String,
}
struct Parser {
    blocks: Vec<Block>,
    spans: Vec<Span<'static>>,
    targets: Vec<Option<String>>,
    marker: Option<String>,
    stack: Vec<Open>,
    lists: Vec<Option<usize>>,
    quote: usize,
    code: usize,
    gap: bool,
}
impl Parser {
    fn span(&mut self, span: Span<'static>) {
        self.targets
            .push(self.stack.iter().rev().find_map(|open| open.link.clone()));
        self.spans.push(span);
    }
    fn style(&self) -> Style {
        self.stack
            .iter()
            .fold(Style::default(), |style, open| style.patch(open.style))
    }
    fn begin_line(&mut self) {
        if self.spans.is_empty() {
            if self.gap
                && !self.blocks.is_empty()
                && !matches!(self.blocks.last(), Some(Block::Text(line)) if line.spans.is_empty())
            {
                self.blocks.push(Block::text(Line::default()));
            }
            self.gap = false;
            if let Some(marker) = self.marker.take() {
                self.span(Span::styled(marker, theme::dim()));
            }
            if self.quote > 0 {
                self.span(Span::styled("│ ".repeat(self.quote.min(4)), theme::dim()));
            }
        }
    }
    fn text(&mut self, raw: &str) {
        let text = entities(raw);
        for (index, line) in text.split('\n').enumerate() {
            if index > 0 {
                let empty = self.spans.is_empty();
                self.finish(false);
                if self.code > 0 && empty {
                    self.blocks.push(Block::text(Line::default()));
                }
            }
            if line.trim().is_empty() {
                if self.code > 0 && !line.is_empty() {
                    self.begin_line();
                    self.span(Span::styled(line.to_owned(), self.style()));
                } else if !line.is_empty()
                    && !self.spans.is_empty()
                    && !self
                        .spans
                        .last()
                        .is_some_and(|span| span.content.ends_with(char::is_whitespace))
                {
                    self.span(Span::styled(" ", self.style()));
                }
                if index > 0 && self.code == 0 && self.marker.is_none() {
                    self.gap = true;
                }
                continue;
            }
            self.begin_line();
            let value = if self.code > 0 {
                line.to_owned()
            } else {
                let mut value = String::new();
                let mut space = false;
                for ch in line.chars() {
                    if ch.is_whitespace() {
                        if !space {
                            value.push(' ');
                        }
                        space = true;
                    } else {
                        value.push(ch);
                        space = false;
                    }
                }
                if self.spans.is_empty()
                    || self
                        .spans
                        .last()
                        .is_some_and(|span| span.content.ends_with(char::is_whitespace))
                {
                    value.trim_start().to_owned()
                } else {
                    value
                }
            };
            if let Some(open) = self.stack.iter_mut().rev().find(|open| open.link.is_some()) {
                open.label.push_str(&value);
            }
            self.span(Span::styled(value, self.style()));
        }
    }
    fn finish(&mut self, gap: bool) {
        if let Some(last) = self.spans.last_mut() {
            last.content = last.content.trim_end().to_owned().into();
        }
        if self
            .spans
            .iter()
            .any(|span| !span.content.trim().is_empty())
        {
            self.blocks.push(Block::Text(RichLine::new(
                Line::from(std::mem::take(&mut self.spans)),
                std::mem::take(&mut self.targets),
            )));
        } else {
            self.spans.clear();
            self.targets.clear();
        }
        self.gap |= gap;
    }
    fn image(&mut self, raw: &str) {
        self.finish(true);
        if let Some(url) = image_url(raw) {
            if self.gap
                && !self.blocks.is_empty()
                && !matches!(self.blocks.last(), Some(Block::Text(line)) if line.spans.is_empty())
            {
                self.blocks.push(Block::text(Line::default()));
            }
            self.blocks.push(Block::Image(url));
            self.gap = true;
        } else if !raw.trim().is_empty() {
            self.text(&format!("Image: {}", raw.trim()));
            self.finish(true);
        }
    }
    fn close(&mut self, name: &str) {
        if let Some(index) = self.stack.iter().rposition(|open| open.name == name) {
            let closing: Vec<_> = self.stack.drain(index..).collect();
            for open in closing {
                if let Some(url) = open.link
                    && open.label.trim().is_empty()
                {
                    self.span(Span::styled(
                        url,
                        theme::accent().add_modifier(Modifier::UNDERLINED),
                    ));
                }
            }
        }
    }
    fn tag(&mut self, tag: &str) -> bool {
        let closing = tag.starts_with('/');
        let body = tag.trim_start_matches('/').trim();
        let name = body
            .split(|ch: char| ch.is_whitespace() || ch == '=' || ch == '/')
            .next()
            .unwrap_or("")
            .to_ascii_lowercase();
        match name.as_str() {
            "p" | "div" => {
                if self.marker.is_none() {
                    self.finish(self.lists.is_empty());
                }
            }
            "br" => self.finish(false),
            "hr" => {
                if !closing {
                    self.finish(true);
                    self.text("────────");
                    self.finish(true);
                }
            }
            "h1" | "h2" | "h3" | "h4" => {
                self.finish(true);
                if closing {
                    self.close(&name);
                } else {
                    self.push(&name, theme::heading(), None);
                }
            }
            "b" | "strong" | "i" | "em" | "u" | "s" | "strike" | "del" => {
                if closing {
                    self.close(&name);
                } else {
                    let modifier = match name.as_str() {
                        "b" | "strong" => Modifier::BOLD,
                        "i" | "em" => Modifier::ITALIC,
                        "u" => Modifier::UNDERLINED,
                        _ => Modifier::CROSSED_OUT,
                    };
                    self.push(&name, Style::default().add_modifier(modifier), None);
                }
            }
            "url" | "a" | "dynamiclink" => {
                if closing {
                    self.close(&name);
                } else {
                    let link = attribute(body, "href").and_then(link_url);
                    self.push(
                        &name,
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::UNDERLINED),
                        link,
                    );
                }
            }
            "list" | "ul" | "ol" => {
                self.finish(false);
                if closing {
                    self.lists.pop();
                    self.marker = None;
                    self.gap = self.lists.is_empty();
                } else if self.lists.len() < 32 {
                    self.lists.push(if name == "ol" || body.contains('=') {
                        Some(0)
                    } else {
                        None
                    });
                }
            }
            "*" | "li" => {
                self.finish(false);
                if !closing {
                    let prefix = match self.lists.last_mut() {
                        Some(Some(number)) => {
                            *number += 1;
                            format!("{}. ", *number)
                        }
                        _ => "• ".into(),
                    };
                    self.gap = false;
                    self.marker = Some(format!(
                        "{}{prefix}",
                        "  ".repeat(self.lists.len().saturating_sub(1))
                    ));
                } else {
                    self.marker = None;
                    self.gap = false;
                }
            }
            "quote" | "blockquote" => {
                self.finish(true);
                if closing {
                    self.quote = self.quote.saturating_sub(1);
                } else {
                    self.quote = (self.quote + 1).min(4);
                }
            }
            "code" | "pre" => {
                self.finish(true);
                if closing {
                    self.code = self.code.saturating_sub(1);
                    self.close(&name);
                } else {
                    self.code += 1;
                    self.push(&name, Style::default().fg(Color::Green), None);
                }
            }
            "table" | "tr" => self.finish(false),
            "center" | "left" | "right" => self.finish(true),
            "color" | "size" | "font" | "spoiler" => {}
            "td" | "th" => {
                if closing {
                    self.close(&name);
                } else {
                    if !self.spans.is_empty() {
                        self.text("  │  ");
                    }
                    self.push(
                        &name,
                        if name == "th" {
                            theme::bold()
                        } else {
                            Style::default()
                        },
                        None,
                    );
                }
            }
            "img" => {
                if !closing && let Some(src) = attribute(body, "src") {
                    self.image(src);
                }
            }
            "previewyoutube" => {
                if !closing
                    && let Some(id) = attribute(body, "")
                        .and_then(|value| value.split(';').next())
                        .filter(|id| {
                            !id.is_empty()
                                && id
                                    .bytes()
                                    .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
                        })
                {
                    self.finish(true);
                    self.text(&format!("Video: https://www.youtube.com/watch?v={id}"));
                    self.finish(true);
                }
            }
            _ => return false,
        }
        true
    }
    fn push(&mut self, name: &str, style: Style, link: Option<String>) {
        if self.stack.len() < 64 {
            self.stack.push(Open {
                name: name.into(),
                style,
                link,
                label: String::new(),
            });
        }
    }
}

pub(super) fn parse(text: &str) -> Vec<Block> {
    let mut parser = Parser {
        blocks: Vec::new(),
        spans: Vec::new(),
        targets: Vec::new(),
        marker: None,
        stack: Vec::new(),
        lists: Vec::new(),
        quote: 0,
        code: 0,
        gap: false,
    };
    let mut rest = text;
    while let Some(at) = rest.find(['[', '<']) {
        parser.text(&rest[..at]);
        rest = &rest[at..];
        let close = if rest.starts_with('[') { ']' } else { '>' };
        let Some(end) = rest.find(close) else {
            break;
        };
        let tag = rest[1..end].trim();
        if parser.code > 0
            && !tag.eq_ignore_ascii_case("/code")
            && !tag.eq_ignore_ascii_case("/pre")
        {
            parser.text(&rest[..=end]);
            rest = &rest[end + 1..];
            continue;
        }
        // BBCode may put an image's URL between its opening and closing tags.
        if tag.eq_ignore_ascii_case("img")
            && let Some(image_end) = rest[end + 1..]
                .match_indices("[/")
                .find(|(at, _)| {
                    rest[end + 1 + at..]
                        .get(..6)
                        .is_some_and(|tag| tag.eq_ignore_ascii_case("[/img]"))
                })
                .map(|(at, _)| at)
        {
            parser.image(&rest[end + 1..end + 1 + image_end]);
            rest = &rest[end + 1 + image_end + 6..];
            continue;
        }
        if !parser.tag(tag) {
            parser.text(&rest[..=end]);
        }
        rest = &rest[end + 1..];
    }
    parser.text(rest);
    parser.finish(false);
    // Finish dangling links without leaking formatting into the next article.
    while let Some(name) = parser.stack.last().map(|open| open.name.clone()) {
        parser.close(&name);
    }
    parser.finish(false);
    parser.blocks
}

#[cfg(test)]
mod tests {
    use super::*;
    fn plain(blocks: &[Block]) -> String {
        blocks
            .iter()
            .map(|block| match block {
                Block::Text(line) => line.to_string(),
                Block::Image(url) => format!("IMAGE {url}"),
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
    #[test]
    fn steam_editor_paragraphs_images_and_dynamic_links() {
        let blocks = parse(
            r#"[p][/p][p][dynamiclink href="https://youtu.be/OAr7WlfFDx4"][/dynamiclink][/p][h1]Expansion[/h1][p]Hello [b]miners[/b]![/p][p][/p][p]  [/p][p][img src="{STEAM_CLAN_IMAGE}/43726543/art.jpg"][/img][/p][p][/p][hr][/hr][h2]Next biome[/h2]"#,
        );
        let text = plain(&blocks);
        assert!(text.contains("https://youtu.be/OAr7WlfFDx4"));
        assert!(text.contains("Hello miners!"));
        assert!(text.contains("IMAGE https://clan.fastly.steamstatic.com/images/43726543/art.jpg"));
        assert!(!text.contains("[dynamiclink"));
        assert!(!text.contains("\n\n\n"));
        assert!(blocks.iter().any(|block| matches!(block, Block::Text(line) if line.spans.iter().any(|span| span.content == "miners" && span.style.add_modifier.contains(Modifier::BOLD)))));
        assert!(blocks.iter().any(|block| matches!(block, Block::Text(line) if line.to_string() == "Next biome" && line.spans[0].style.fg == Some(Color::Yellow))));
    }
    #[test]
    fn html_entities_lists_inline_spaces_and_unknown_brackets() {
        let blocks = parse(
            "<h2>Notes</h2><p><b>bold</b> <i>italic</i> &amp; &#x1F680; &nbsp; &#8212;</p><ol><li>one</li><li>two</li></ol><p>[beta] release <a href='https://example.com/patch?a=1&amp;b=2'>Read more</a></p>",
        );
        let text = plain(&blocks);
        assert!(text.contains("bold italic & 🚀 —"), "{text}");
        assert!(text.contains("1. one\n2. two"), "{text}");
        assert!(text.contains("[beta] release Read more"));
        assert!(blocks.iter().any(|block| matches!(block, Block::Text(line) if line.targets.iter().flatten().any(|url| url == "https://example.com/patch?a=1&b=2"))));
        assert_eq!(
            plain(&parse("[beta] [b]ok[/b]\nnext\u{1b}")),
            "[beta] ok\nnext"
        );
        assert_eq!(plain(&parse("a\n\n\n\nb")), "a\n\nb");
        assert_eq!(plain(&parse("before [unfinished")), "before [unfinished");
    }
    #[test]
    fn code_quotes_tables_and_nested_lists_remain_readable() {
        let text = plain(&parse(
            "[code]  [b]literal[/b]\n\n  let x = 1;[/code][quote]quoted text[/quote]<table><tr><th>Item</th><th>Result</th></tr><tr><td>Fix</td><td>Done</td></tr></table>[list][*]first[list][*]nested[/list][*]last[/list]",
        ));
        assert!(text.contains("  [b]literal[/b]\n\n  let x = 1;"), "{text}");
        assert!(text.contains("│ quoted text"));
        assert!(text.contains("Item │ Result\nFix │ Done"), "{text}");
        assert!(text.contains("  • nested"), "{text}");
        let unclosed = parse("[b]bold [url=https://example.com]link");
        assert!(unclosed.iter().any(|block| matches!(block, Block::Text(line) if line.targets.iter().flatten().any(|url| url == "https://example.com/"))));
    }

    #[test]
    fn paragraph_wrapped_list_items_keep_the_marker_and_text_together() {
        let text = plain(&parse(
            "[p][b]Overclocks:[/b][/p][list]\n[*]\n[p]Cluster Grenade nerfed: Explosive radius 0% → -20%[/p][/*]\n[*][p]Diffractor prism nerfed: Weapon Range +30% → +10%[/p][/*][*][p][/p][/*][/list]",
        ));
        assert_eq!(
            text,
            "Overclocks:\n• Cluster Grenade nerfed: Explosive radius 0% → -20%\n• Diffractor prism nerfed: Weapon Range +30% → +10%"
        );
        assert_eq!(
            plain(&parse(
                "<ul><li><p>first</p></li><li><p>second</p></li></ul>"
            )),
            "• first\n• second"
        );
    }

    #[test]
    fn links_retain_destinations_across_styled_and_wrapped_labels() {
        use ratatui::layout::Rect;
        let blocks = parse(
            "[url=https://example.com/one]Read [b]矿工 notes[/b] here[/url] and [url=https://example.com/two]Read notes[/url]. https://example.com/three.",
        );
        let Block::Text(line) = &blocks[0] else {
            panic!("expected text");
        };
        let hits = line.link_hits(Rect::new(7, 9, 16, 8), 0);
        for url in [
            "https://example.com/one",
            "https://example.com/two",
            "https://example.com/three",
        ] {
            assert!(
                hits.iter().any(|(_, target)| target == url),
                "{url}: {hits:?}"
            );
        }
        assert!(hits.iter().filter(|(_, url)| url.ends_with("/one")).count() > 1);
        assert!(hits.iter().all(|(rect, _)| rect.x >= 7
            && rect.right() <= 23
            && rect.y >= 9
            && rect.bottom() <= 17));
        let scrolled = line.link_hits(Rect::new(7, 9, 16, 2), 2);
        assert!(!scrolled.is_empty());
        assert!(
            scrolled.iter().all(|(_, url)| !url.ends_with("/one")),
            "{scrolled:?}"
        );
        assert!(link_url("steam://run/440").is_none());
        assert!(link_url("javascript:alert(1)").is_none());
    }

    #[test]
    fn normalizes_all_image_forms_and_rejects_external_or_unsafe_hosts() {
        let blocks = parse(
            "[img]https://images.steamusercontent.com/a.png[/IMG]<img alt='picture' src='https://clan.akamai.steamstatic.com/images/a.jpg' />",
        );
        assert_eq!(
            blocks
                .iter()
                .filter(|block| matches!(block, Block::Image(_)))
                .count(),
            2
        );
        for raw in [
            "http://clan.fastly.steamstatic.com/a",
            "https://clan.fastly.steamstatic.com.evil.test/a",
            "https://user@clan.fastly.steamstatic.com/a",
            "file:///tmp/a",
            "https://127.0.0.1/a",
            "https://clan.fastly.steamstatic.com:123/a",
        ] {
            assert!(image_url(raw).is_none(), "{raw}");
        }
        let text = plain(&parse(
            "[p]&#27;bad&#x202E;[/p][url=javascript:alert(1)]label[/url][img src='https://example.com/img']",
        ));
        assert!(!text.contains('\u{1b}'));
        assert!(!text.contains('\u{202e}'));
        assert!(!text.contains("javascript:"));
        assert!(text.contains("Image: https://example.com/img"));
        let youtube = plain(&parse(
            "[previewyoutube=\"S86vIZTUi_k;full\"][/previewyoutube]",
        ));
        assert_eq!(
            youtube,
            "Video: https://www.youtube.com/watch?v=S86vIZTUi_k"
        );
    }
}
