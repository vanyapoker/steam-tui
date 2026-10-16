//! Local cache formats 0x07564427..29, as documented by SteamAppInfo:
//! https://github.com/xPaw/SteamAppInfo/blob/master/SteamAppInfoParser/AppInfo.cs
use super::{Result, failure, read_bounded};
use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct Node {
    pub value: Option<String>,
    pub children: BTreeMap<String, Node>,
}
impl Node {
    pub fn child(&self, key: &str) -> Option<&Node> {
        self.children.get(key).or_else(|| {
            self.children
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(key))
                .map(|(_, v)| v)
        })
    }
    pub fn text(&self, key: &str) -> Option<&str> {
        self.child(key)?.value.as_deref()
    }
    pub fn at(&self, path: &[&str]) -> Option<&Node> {
        path.iter().try_fold(self, |node, key| node.child(key))
    }
}

pub(crate) fn read_vdf(path: &Path) -> Result<Node> {
    let bytes = read_bounded(path, 16 * 1024 * 1024)?;
    text(std::str::from_utf8(&bytes).map_err(|_| failure("invalid UTF-8 in text VDF"))?)
}

/// Steam KeyValues1 token pairing is independent of line breaks. The registry
/// reader's regex lexer recursively processes escaped strings; actual Steam
/// localconfig JSON values can exhaust its stack. Keep both lexing and parsing
/// iterative here, with depth/node/file bounds independent of string length.
pub(crate) fn text(text: &str) -> Result<Node> {
    if text.len() > 16 * 1024 * 1024 {
        return Err(failure("text VDF exceeds the 16 MiB limit"));
    }
    if text.contains('\0') {
        return Err(failure("NUL in text VDF"));
    }
    let mut stack = vec![(String::new(), Node::default())];
    let mut reader = TextReader {
        source: text.strip_prefix('\u{feff}').unwrap_or(text),
        offset: 0,
    };
    let mut key = None;
    let mut nodes = 0;
    while let Some(token) = reader.token()? {
        match token {
            TextToken::Scalar(value) => {
                if let Some(key) = key.take() {
                    insert(
                        &mut stack
                            .last_mut()
                            .ok_or_else(|| failure("missing VDF parent"))?
                            .1,
                        key,
                        Node {
                            value: Some(value.into_owned()),
                            children: BTreeMap::new(),
                        },
                    )?;
                } else {
                    nodes += 1;
                    if nodes > 500_000 {
                        return Err(failure("VDF exceeds node limit"));
                    }
                    key = Some(value.into_owned());
                }
            }
            TextToken::Open => {
                let key = key.take().ok_or_else(|| failure("VDF group has no key"))?;
                if stack.len() > 64 {
                    return Err(failure("VDF exceeds nesting limit"));
                }
                stack.push((key, Node::default()));
            }
            TextToken::Close => {
                if key.is_some() {
                    return Err(failure("VDF key has no value"));
                }
                if stack.len() == 1 {
                    return Err(failure("unexpected VDF closing brace"));
                }
                let (key, node) = stack.pop().ok_or_else(|| failure("missing VDF group"))?;
                insert(
                    &mut stack
                        .last_mut()
                        .ok_or_else(|| failure("missing VDF parent"))?
                        .1,
                    key,
                    node,
                )?;
            }
        }
    }
    if key.is_some() {
        return Err(failure("VDF key has no value"));
    }
    if stack.len() != 1 {
        return Err(failure("unclosed VDF group"));
    }
    Ok(stack.pop().ok_or_else(|| failure("missing VDF root"))?.1)
}

enum TextToken<'a> {
    Scalar(Cow<'a, str>),
    Open,
    Close,
}

struct TextReader<'a> {
    source: &'a str,
    offset: usize,
}

impl<'a> TextReader<'a> {
    fn token(&mut self) -> Result<Option<TextToken<'a>>> {
        let bytes = self.source.as_bytes();
        loop {
            while bytes.get(self.offset).is_some_and(u8::is_ascii_whitespace) {
                self.offset += 1;
            }
            if bytes.get(self.offset..self.offset + 2) == Some(b"//") {
                while bytes.get(self.offset).is_some_and(|byte| *byte != b'\n') {
                    self.offset += 1;
                }
            } else if bytes.get(self.offset..self.offset + 2) == Some(b"/*") {
                self.offset += 2;
                loop {
                    if bytes.get(self.offset..self.offset + 2) == Some(b"*/") {
                        self.offset += 2;
                        break;
                    }
                    if self.offset >= bytes.len() {
                        return Err(failure("unclosed VDF block comment"));
                    }
                    self.offset += 1;
                }
            } else {
                break;
            }
        }
        let Some(byte) = bytes.get(self.offset) else {
            return Ok(None);
        };
        match byte {
            b'{' => {
                self.offset += 1;
                Ok(Some(TextToken::Open))
            }
            b'}' => {
                self.offset += 1;
                Ok(Some(TextToken::Close))
            }
            b'"' => self.quoted().map(|value| Some(TextToken::Scalar(value))),
            b'#' | b'[' => Err(failure("unsupported VDF directive/conditional")),
            _ => {
                let start = self.offset;
                while bytes.get(self.offset).is_some_and(|byte| {
                    !byte.is_ascii_whitespace() && !matches!(byte, b'{' | b'}' | b'"')
                }) {
                    self.offset += 1;
                }
                Ok(Some(TextToken::Scalar(Cow::Borrowed(
                    &self.source[start..self.offset],
                ))))
            }
        }
    }

    fn quoted(&mut self) -> Result<Cow<'a, str>> {
        let bytes = self.source.as_bytes();
        self.offset += 1;
        let start = self.offset;
        let mut segment = start;
        let mut decoded: Option<String> = None;
        while let Some(byte) = bytes.get(self.offset) {
            match byte {
                b'"' => {
                    let end = self.offset;
                    self.offset += 1;
                    return Ok(match decoded {
                        Some(mut decoded) => {
                            decoded.push_str(&self.source[segment..end]);
                            Cow::Owned(decoded)
                        }
                        None => Cow::Borrowed(&self.source[start..end]),
                    });
                }
                b'\\' => {
                    let escaped = bytes
                        .get(self.offset + 1)
                        .ok_or_else(|| failure("unfinished VDF string escape"))?;
                    let replacement = match escaped {
                        b'\\' => Some('\\'),
                        b'"' => Some('"'),
                        b'n' => Some('\n'),
                        b'r' => Some('\r'),
                        b't' => Some('\t'),
                        // Unknown escapes remain literal (important for paths).
                        _ => None,
                    };
                    if let Some(replacement) = replacement {
                        let decoded = decoded.get_or_insert_with(String::new);
                        decoded.push_str(&self.source[segment..self.offset]);
                        decoded.push(replacement);
                        segment = self.offset + 2;
                    }
                    self.offset += 2;
                }
                _ => self.offset += 1,
            }
        }
        Err(failure("unclosed VDF quoted string"))
    }
}
fn insert(parent: &mut Node, mut key: String, value: Node) -> Result<()> {
    key.make_ascii_lowercase();
    match parent.children.entry(key) {
        std::collections::btree_map::Entry::Vacant(entry) => {
            entry.insert(value);
            Ok(())
        }
        std::collections::btree_map::Entry::Occupied(entry) => {
            Err(failure(format!("duplicate VDF key {}", entry.key())))
        }
    }
}

pub(crate) fn app_metadata(root: &Path, wanted: &BTreeSet<u32>) -> Result<BTreeMap<u32, Node>> {
    let bytes = read_bounded(&root.join("appcache/appinfo.vdf"), 256 * 1024 * 1024)?;
    cache(&bytes, wanted)
}

pub(super) fn cache(bytes: &[u8], wanted: &BTreeSet<u32>) -> Result<BTreeMap<u32, Node>> {
    let mut reader = Binary {
        bytes,
        pos: 0,
        strings: None,
        nodes: 0,
    };
    let version = reader.u32()?;
    if !matches!(version, 0x07564427..=0x07564429) {
        return Err(failure(format!(
            "unsupported appinfo cache format {version:#x}"
        )));
    }
    reader.u32()?; // universe
    let mut table_start = bytes.len();
    let mut strings = Vec::new();
    if version >= 0x07564429 {
        table_start = usize::try_from(reader.u64()?)?;
        if table_start < reader.pos || table_start >= bytes.len() {
            return Err(failure("invalid appinfo string table offset"));
        }
        let mut table = Binary {
            bytes: &bytes[table_start..],
            pos: 0,
            strings: None,
            nodes: 0,
        };
        let count = table.u32()?;
        if count > 1_000_000 {
            return Err(failure("appinfo string table exceeds limit"));
        }
        for _ in 0..count {
            strings.push(table.string()?);
        }
        reader.strings = Some(&strings);
    }
    let mut result = BTreeMap::new();
    loop {
        let id = reader.u32()?;
        if id == 0 {
            break;
        }
        let size = usize::try_from(reader.u32()?)?;
        let end = reader
            .pos
            .checked_add(size)
            .filter(|end| *end <= table_start)
            .ok_or_else(|| failure("truncated appinfo entry"))?;
        let header_size = if version >= 0x07564428 { 60 } else { 40 };
        if size < header_size {
            return Err(failure("invalid appinfo entry size"));
        }
        reader.take(header_size)?;
        if wanted.contains(&id) {
            if size > 16 * 1024 * 1024 {
                return Err(failure("appinfo entry exceeds limit"));
            }
            let mut entry = Binary {
                bytes: &bytes[reader.pos..end],
                pos: 0,
                strings: reader.strings,
                nodes: 0,
            };
            let mut doc = entry.object(0)?;
            if entry.pos != entry.bytes.len() {
                return Err(failure("trailing binary app metadata"));
            }
            let app = doc
                .children
                .remove("appinfo")
                .ok_or_else(|| failure("missing appinfo root"))?;
            if app
                .text("appid")
                .and_then(|s| s.parse::<u32>().ok())
                .is_some_and(|found| found != id)
            {
                return Err(failure("appinfo app ID mismatch"));
            }
            if result.insert(id, app).is_some() {
                return Err(failure("duplicate appinfo app ID"));
            }
        }
        reader.pos = end;
    }
    if reader.pos > table_start {
        return Err(failure("appinfo entry overlaps string table"));
    }
    Ok(result)
}

struct Binary<'a> {
    bytes: &'a [u8],
    pos: usize,
    strings: Option<&'a Vec<&'a str>>,
    nodes: usize,
}
impl<'a> Binary<'a> {
    fn take(&mut self, count: usize) -> Result<&'a [u8]> {
        let end = self
            .pos
            .checked_add(count)
            .filter(|end| *end <= self.bytes.len())
            .ok_or_else(|| failure("truncated binary VDF"))?;
        let bytes = &self.bytes[self.pos..end];
        self.pos = end;
        Ok(bytes)
    }
    fn byte(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into()?))
    }
    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into()?))
    }
    fn string(&mut self) -> Result<&'a str> {
        let len = self.bytes[self.pos..]
            .iter()
            .position(|b| *b == 0)
            .ok_or_else(|| failure("unterminated binary VDF string"))?;
        let bytes = self.take(len + 1)?;
        std::str::from_utf8(&bytes[..len]).map_err(|_| failure("invalid UTF-8 in binary VDF"))
    }
    fn key(&mut self) -> Result<String> {
        if let Some(strings) = self.strings {
            let index = self.u32()? as usize;
            Ok(strings
                .get(index)
                .ok_or_else(|| failure("invalid appinfo string index"))?
                .to_string())
        } else {
            Ok(self.string()?.to_owned())
        }
    }
    fn object(&mut self, depth: usize) -> Result<Node> {
        if depth > 64 {
            return Err(failure("binary VDF nesting limit"));
        }
        let mut node = Node::default();
        loop {
            let tag = self.byte()?;
            if matches!(tag, 8 | 11) {
                return Ok(node);
            }
            self.nodes += 1;
            if self.nodes > 500_000 {
                return Err(failure("binary VDF node limit"));
            }
            let key = self.key()?;
            let child = if tag == 0 {
                self.object(depth + 1)?
            } else {
                let value = match tag {
                    1 => self.string()?.to_owned(),
                    2 => (self.u32()? as i32).to_string(),
                    3 => f32::from_bits(self.u32()?).to_string(),
                    4 | 6 => self.u32()?.to_string(),
                    7 => self.u64()?.to_string(),
                    10 => (self.u64()? as i64).to_string(),
                    5 => {
                        let mut wide = Vec::new();
                        loop {
                            let word = u16::from_le_bytes(self.take(2)?.try_into()?);
                            if word == 0 {
                                break;
                            }
                            wide.push(word);
                        }
                        String::from_utf16(&wide)
                            .map_err(|_| failure("invalid wide binary VDF string"))?
                    }
                    _ => return Err(failure(format!("unsupported binary VDF tag {tag}"))),
                };
                Node {
                    value: Some(value),
                    children: BTreeMap::new(),
                }
            };
            insert(&mut node, key, child)?;
        }
    }
}
