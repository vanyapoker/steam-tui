//! Steam stats schema: the binary KeyValues document `ClientGetUserStatsResponse`
//! carries (`UserGameStatsSchema_<app>.bin`). Shared by the backend (validation)
//! and the game-side library (names, types, achievement display text).
//!
//! Bounded on every axis; malformed input yields `None`, never a panic.
use std::collections::BTreeMap;

const MAX_DEPTH: usize = 24;
const MAX_NODES: usize = 200_000;
const MAX_STRING: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Children(Vec<Kv>),
    Str(String),
    Int(i64),
    Float(f32),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Kv {
    pub name: String,
    pub value: Value,
}

impl Kv {
    pub fn child(&self, name: &str) -> Option<&Kv> {
        match &self.value {
            Value::Children(children) => children
                .iter()
                .find(|kv| kv.name.eq_ignore_ascii_case(name)),
            _ => None,
        }
    }
    pub fn children(&self) -> &[Kv] {
        match &self.value {
            Value::Children(children) => children,
            _ => &[],
        }
    }
    pub fn as_str(&self) -> Option<&str> {
        match &self.value {
            Value::Str(text) => Some(text),
            _ => None,
        }
    }
    pub fn as_i64(&self) -> Option<i64> {
        match &self.value {
            Value::Int(n) => Some(*n),
            Value::Str(text) => text.trim().parse().ok(),
            Value::Float(f) => Some(*f as i64),
            Value::Children(_) => None,
        }
    }
    pub fn as_f64(&self) -> Option<f64> {
        match &self.value {
            Value::Int(n) => Some(*n as f64),
            Value::Float(f) => Some(f64::from(*f)),
            Value::Str(text) => text.trim().parse().ok(),
            Value::Children(_) => None,
        }
    }
}

struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
    nodes: usize,
}

impl Reader<'_> {
    fn byte(&mut self) -> Option<u8> {
        let b = *self.bytes.get(self.at)?;
        self.at += 1;
        Some(b)
    }
    fn take(&mut self, n: usize) -> Option<&[u8]> {
        let slice = self.bytes.get(self.at..self.at.checked_add(n)?)?;
        self.at += n;
        Some(slice)
    }
    fn cstr(&mut self) -> Option<String> {
        let rest = self.bytes.get(self.at..)?;
        let end = rest.iter().position(|b| *b == 0)?;
        if end > MAX_STRING {
            return None;
        }
        let text = String::from_utf8_lossy(&rest[..end]).into_owned();
        self.at += end + 1;
        Some(text)
    }
    fn wstr(&mut self) -> Option<String> {
        let mut units = Vec::new();
        loop {
            let pair = self.take(2)?;
            let unit = u16::from_le_bytes([pair[0], pair[1]]);
            if unit == 0 {
                break;
            }
            units.push(unit);
            if units.len() > MAX_STRING {
                return None;
            }
        }
        Some(String::from_utf16_lossy(&units))
    }
    /// Children of an open object, up to its End marker.
    fn object(&mut self, depth: usize) -> Option<Vec<Kv>> {
        if depth > MAX_DEPTH {
            return None;
        }
        let mut out = Vec::new();
        loop {
            let kind = self.byte()?;
            if kind == 8 {
                return Some(out);
            }
            self.nodes += 1;
            if self.nodes > MAX_NODES {
                return None;
            }
            let name = self.cstr()?;
            let value = match kind {
                0 => Value::Children(self.object(depth + 1)?),
                1 => Value::Str(self.cstr()?),
                2 | 4 | 6 => {
                    let b = self.take(4)?;
                    Value::Int(i64::from(i32::from_le_bytes([b[0], b[1], b[2], b[3]])))
                }
                3 => {
                    let b = self.take(4)?;
                    Value::Float(f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
                }
                5 => Value::Str(self.wstr()?),
                7 | 9 => {
                    let b = self.take(8)?;
                    let mut raw = [0; 8];
                    raw.copy_from_slice(b);
                    Value::Int(i64::from_le_bytes(raw))
                }
                _ => return None,
            };
            out.push(Kv { name, value });
        }
    }
}

/// The document's root object (`00 "<name>" ... 08`).
pub fn parse_kv(bytes: &[u8]) -> Option<Kv> {
    let mut reader = Reader {
        bytes,
        at: 0,
        nodes: 0,
    };
    if reader.byte()? != 0 {
        return None;
    }
    let name = reader.cstr()?;
    let children = reader.object(0)?;
    Some(Kv {
        name,
        value: Value::Children(children),
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatKind {
    Int,
    Float,
    AvgRate,
    /// Bitfield stat whose bits are achievements.
    Bits,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Achievement {
    pub name: String,
    pub bit: u32,
    pub display_name: String,
    pub description: String,
    pub hidden: bool,
    /// (min, max) when the achievement tracks progress.
    pub progress: Option<(u32, u32)>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Stat {
    pub id: u32,
    pub name: String,
    pub kind: StatKind,
    /// Raw 32-bit value (float stats hold the IEEE bits).
    pub default: u32,
    pub min: Option<f64>,
    pub max: Option<f64>,
    pub max_change: Option<f64>,
    pub increment_only: bool,
    pub achievements: Vec<Achievement>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Schema {
    pub stats: Vec<Stat>,
}

fn localized(kv: &Kv, language: &str) -> String {
    match &kv.value {
        Value::Children(children) => children
            .iter()
            .find(|c| c.name.eq_ignore_ascii_case(language))
            .or_else(|| {
                children
                    .iter()
                    .find(|c| c.name.eq_ignore_ascii_case("english"))
            })
            .or_else(|| children.iter().find(|c| c.as_str().is_some()))
            .and_then(|c| c.as_str())
            .unwrap_or_default()
            .to_owned(),
        Value::Str(text) => text.clone(),
        _ => String::new(),
    }
}

fn kind_of(kv: &Kv) -> Option<StatKind> {
    let text = kv.as_str().map(|s| s.trim().to_ascii_uppercase());
    match text.as_deref() {
        Some("INT") | Some("1") => Some(StatKind::Int),
        Some("FLOAT") | Some("2") => Some(StatKind::Float),
        Some("AVGRATE") | Some("3") => Some(StatKind::AvgRate),
        Some("BITS") | Some("ACHIEVEMENTS") | Some("4") => Some(StatKind::Bits),
        _ => match kv.as_i64()? {
            1 => Some(StatKind::Int),
            2 => Some(StatKind::Float),
            3 => Some(StatKind::AvgRate),
            4 => Some(StatKind::Bits),
            _ => None,
        },
    }
}

fn raw_value(kind: StatKind, kv: &Kv) -> u32 {
    match kind {
        StatKind::Float | StatKind::AvgRate => kv.as_f64().map_or(0, |f| (f as f32).to_bits()),
        _ => kv.as_i64().map_or(0, |n| n as u32),
    }
}

impl Schema {
    /// Parses the schema document; `language` picks the display text.
    pub fn parse(bytes: &[u8], language: &str) -> Option<Self> {
        let root = parse_kv(bytes)?;
        let stats_node = root
            .child("stats")
            .or_else(|| root.children().iter().find_map(|c| c.child("stats")))?;
        let mut stats = Vec::new();
        for node in stats_node.children() {
            let Ok(id) = node.name.trim().parse::<u32>() else {
                continue;
            };
            let kind = node
                .child("type_int")
                .and_then(kind_of)
                .or_else(|| node.child("type").and_then(kind_of))
                .unwrap_or(StatKind::Int);
            let mut achievements = Vec::new();
            if let Some(bits) = node.child("bits") {
                for bit in bits.children() {
                    let Ok(index) = bit.name.trim().parse::<u32>() else {
                        continue;
                    };
                    let Some(name) = bit.child("name").and_then(Kv::as_str) else {
                        continue;
                    };
                    let display = bit.child("display");
                    let text = |key: &str| {
                        display
                            .and_then(|d| d.child(key))
                            .map(|kv| localized(kv, language))
                            .unwrap_or_default()
                    };
                    let progress = bit.child("progress").and_then(|p| {
                        let min = p.child("min_val")?.as_i64()?;
                        let max = p.child("max_val")?.as_i64()?;
                        Some((min as u32, max as u32))
                    });
                    achievements.push(Achievement {
                        name: name.to_owned(),
                        bit: index,
                        display_name: text("name"),
                        description: text("desc"),
                        hidden: display
                            .and_then(|d| d.child("hidden"))
                            .and_then(Kv::as_i64)
                            .is_some_and(|h| h != 0),
                        progress,
                    });
                }
                achievements.sort_by_key(|a| a.bit);
            }
            stats.push(Stat {
                id,
                name: node
                    .child("name")
                    .and_then(Kv::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                kind,
                default: node.child("default").map_or(0, |d| raw_value(kind, d)),
                min: node.child("min").and_then(Kv::as_f64),
                max: node.child("max").and_then(Kv::as_f64),
                max_change: node.child("maxchange").and_then(Kv::as_f64),
                increment_only: node
                    .child("incrementonly")
                    .and_then(Kv::as_i64)
                    .is_some_and(|v| v != 0),
                achievements,
            });
        }
        stats.sort_by_key(|s| s.id);
        Some(Self { stats })
    }

    pub fn stat(&self, name: &str) -> Option<&Stat> {
        self.stats
            .iter()
            .find(|s| s.kind != StatKind::Bits && s.name == name)
    }

    /// Achievements in schema order: (stat id, achievement).
    pub fn achievements(&self) -> impl Iterator<Item = (u32, &Achievement)> {
        self.stats
            .iter()
            .flat_map(|s| s.achievements.iter().map(move |a| (s.id, a)))
    }

    pub fn achievement(&self, name: &str) -> Option<(u32, &Achievement)> {
        self.achievements().find(|(_, a)| a.name == name)
    }

    /// Stat id -> stat name for every non-bitfield stat (global stats use it).
    pub fn names_by_id(&self) -> BTreeMap<u32, &str> {
        self.stats
            .iter()
            .filter(|s| s.kind != StatKind::Bits)
            .map(|s| (s.id, s.name.as_str()))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sub(out: &mut Vec<u8>, name: &str) {
        out.push(0);
        out.extend(name.as_bytes());
        out.push(0);
    }
    fn text(out: &mut Vec<u8>, name: &str, value: &str) {
        out.push(1);
        out.extend(name.as_bytes());
        out.push(0);
        out.extend(value.as_bytes());
        out.push(0);
    }
    fn int(out: &mut Vec<u8>, name: &str, value: i32) {
        out.push(2);
        out.extend(name.as_bytes());
        out.push(0);
        out.extend(value.to_le_bytes());
    }

    fn sample() -> Vec<u8> {
        let mut b = Vec::new();
        sub(&mut b, "480");
        sub(&mut b, "stats");
        sub(&mut b, "1");
        text(&mut b, "type", "INT");
        text(&mut b, "name", "NumGames");
        int(&mut b, "default", 5);
        int(&mut b, "incrementonly", 1);
        b.push(8);
        sub(&mut b, "2");
        text(&mut b, "type", "FLOAT");
        text(&mut b, "name", "Speed");
        text(&mut b, "default", "1.5");
        b.push(8);
        sub(&mut b, "3");
        int(&mut b, "type", 4);
        text(&mut b, "name", "Ach1");
        sub(&mut b, "bits");
        sub(&mut b, "0");
        text(&mut b, "name", "ACH_WIN");
        sub(&mut b, "display");
        sub(&mut b, "name");
        text(&mut b, "english", "Winner");
        text(&mut b, "german", "Gewinner");
        b.push(8);
        text(&mut b, "desc", "Win once");
        int(&mut b, "hidden", 1);
        b.push(8);
        sub(&mut b, "progress");
        int(&mut b, "min_val", 0);
        int(&mut b, "max_val", 10);
        b.push(8);
        b.push(8);
        b.push(8);
        b.push(8);
        b.push(8);
        b.push(8);
        b.push(8);
        b
    }

    #[test]
    fn schema_exposes_stats_achievements_and_localized_text() {
        let schema = Schema::parse(&sample(), "german").unwrap();
        assert_eq!(schema.stats.len(), 3);
        let games = schema.stat("NumGames").unwrap();
        assert_eq!((games.id, games.kind, games.default), (1, StatKind::Int, 5));
        assert!(games.increment_only);
        let speed = schema.stat("Speed").unwrap();
        assert_eq!(speed.default, 1.5f32.to_bits());
        let (stat, ach) = schema.achievement("ACH_WIN").unwrap();
        assert_eq!(stat, 3);
        assert_eq!(ach.display_name, "Gewinner");
        assert_eq!(ach.description, "Win once");
        assert!(ach.hidden);
        assert_eq!(ach.progress, Some((0, 10)));
        // Unknown languages fall back to english.
        let fallback = Schema::parse(&sample(), "french").unwrap();
        assert_eq!(
            fallback.achievement("ACH_WIN").unwrap().1.display_name,
            "Winner"
        );
    }

    #[test]
    fn malformed_or_truncated_documents_are_rejected() {
        let good = sample();
        for cut in 0..good.len() {
            let _ = Schema::parse(&good[..cut], "english");
        }
        assert!(Schema::parse(&[], "english").is_none());
        assert!(Schema::parse(&[9, 0], "english").is_none());
        // Excessive nesting is refused rather than recursed into.
        let mut deep = Vec::new();
        for _ in 0..64 {
            sub(&mut deep, "a");
        }
        assert!(parse_kv(&deep).is_none());
    }
}
