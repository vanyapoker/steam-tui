use super::{LibraryApp, Result, failure};
use std::collections::BTreeSet;

pub(super) const MAX_PRODUCT_BYTES: usize = 8 * 1024 * 1024;
const MAX_DEPTH: usize = 64;
const MAX_NODES: usize = 250_000;

/// PICS package buffers start with a little-endian version, then binary KeyValues.
/// Read the `appids` values, not their numeric array-index keys or depot IDs.
pub(super) fn package_apps(package_id: u32, buffer: &[u8]) -> Result<Option<BTreeSet<u32>>> {
    package_ids(package_id, buffer, "appids")
}

pub(super) fn package_depots(package_id: u32, buffer: &[u8]) -> Result<Option<BTreeSet<u32>>> {
    package_ids(package_id, buffer, "depotids")
}

fn package_ids(
    package_id: u32,
    buffer: &[u8],
    field: &'static str,
) -> Result<Option<BTreeSet<u32>>> {
    if buffer.len() > MAX_PRODUCT_BYTES {
        return Err(failure("package metadata exceeds the 8 MiB limit"));
    }
    let mut reader = BinaryReader {
        bytes: buffer,
        offset: 0,
        nodes: 0,
        field,
    };
    if reader.u32()? != 1 {
        return Err(failure("unsupported PICS package metadata version"));
    }
    if reader.byte()? != 0 || reader.string()?.parse::<u32>().ok() != Some(package_id) {
        return Err(failure(
            "package metadata root does not match the requested package",
        ));
    }
    let mut app_ids = None;
    reader.table(1, Scope::Package, &mut app_ids)?;
    // A binary KeyValues document has an end marker outside the root object too.
    if !matches!(reader.byte()?, 8 | 11) || reader.offset != buffer.len() {
        return Err(failure("invalid trailing package metadata"));
    }
    Ok(app_ids)
}

#[derive(Clone, Copy)]
enum Scope {
    Package,
    AppIds,
    Other,
}

struct BinaryReader<'a> {
    bytes: &'a [u8],
    offset: usize,
    nodes: usize,
    field: &'static str,
}

impl<'a> BinaryReader<'a> {
    fn take(&mut self, count: usize) -> Result<&'a [u8]> {
        let end = self
            .offset
            .checked_add(count)
            .filter(|end| *end <= self.bytes.len())
            .ok_or_else(|| failure("truncated binary package metadata"))?;
        let bytes = &self.bytes[self.offset..end];
        self.offset = end;
        Ok(bytes)
    }

    fn byte(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into()?))
    }

    fn string(&mut self) -> Result<&'a str> {
        let len = self.bytes[self.offset..]
            .iter()
            .position(|byte| *byte == 0)
            .ok_or_else(|| failure("unterminated binary package string"))?;
        let bytes = self.take(len + 1)?;
        std::str::from_utf8(&bytes[..len])
            .map_err(|_| failure("invalid UTF-8 in binary package metadata"))
    }

    fn table(
        &mut self,
        depth: usize,
        scope: Scope,
        apps: &mut Option<BTreeSet<u32>>,
    ) -> Result<()> {
        if depth > MAX_DEPTH {
            return Err(failure("package metadata exceeds the nesting limit"));
        }
        loop {
            let tag = self.byte()?;
            if matches!(tag, 8 | 11) {
                return Ok(());
            }
            self.nodes += 1;
            if self.nodes > MAX_NODES {
                return Err(failure("package metadata exceeds the entry limit"));
            }
            let key = self.string()?;
            if tag == 0 {
                let child_scope = match scope {
                    Scope::Package if key == self.field => {
                        if apps.is_some() {
                            return Err(failure("duplicate package appids table"));
                        }
                        *apps = Some(BTreeSet::new());
                        Scope::AppIds
                    }
                    Scope::AppIds => return Err(failure("nested object in package appids table")),
                    _ => Scope::Other,
                };
                self.table(depth + 1, child_scope, apps)?;
                continue;
            }
            if matches!(scope, Scope::Package) && key == self.field {
                return Err(failure("package appids is not a table"));
            }
            let is_app = matches!(scope, Scope::AppIds);
            let app_id = match tag {
                1 => {
                    let value = self.string()?;
                    if is_app {
                        Some(
                            value
                                .parse::<u32>()
                                .map_err(|_| failure("invalid app ID in package metadata"))?,
                        )
                    } else {
                        None
                    }
                }
                2 => {
                    let value = self.u32()?;
                    is_app.then_some(value)
                }
                3 | 4 | 6 => {
                    self.take(4)?;
                    if is_app {
                        return Err(failure("non-integer app ID in package metadata"));
                    }
                    None
                }
                7 => {
                    let value = u64::from_le_bytes(self.take(8)?.try_into()?);
                    if is_app {
                        Some(
                            u32::try_from(value)
                                .map_err(|_| failure("out-of-range package app ID"))?,
                        )
                    } else {
                        None
                    }
                }
                10 => {
                    let value = i64::from_le_bytes(self.take(8)?.try_into()?);
                    if is_app {
                        Some(
                            u32::try_from(value)
                                .map_err(|_| failure("out-of-range package app ID"))?,
                        )
                    } else {
                        None
                    }
                }
                _ => return Err(failure("unsupported binary package value type")),
            };
            if let Some(app_id) = app_id {
                if app_id == 0 {
                    return Err(failure("zero app ID in package metadata"));
                }
                // Scope::AppIds is only entered after initializing the set.
                if let Some(apps) = apps.as_mut() {
                    apps.insert(app_id);
                }
            }
        }
    }
}

/// Decode the entire bounded PICS document through the shared iterative lexer.
pub(super) fn document(app_id: u32, buffer: &[u8]) -> Result<crate::installed::Node> {
    if buffer.len() > MAX_PRODUCT_BYTES {
        return Err(failure("app metadata exceeds the 8 MiB limit"));
    }
    let buffer = buffer.strip_suffix(&[0]).unwrap_or(buffer);
    let source =
        std::str::from_utf8(buffer).map_err(|_| failure("invalid UTF-8 in app metadata"))?;
    let mut root = crate::installed::text(source).map_err(|error| failure(error.to_string()))?;
    if root.children.len() != 1 {
        return Err(failure("invalid app metadata root"));
    }
    let doc = root
        .children
        .remove("appinfo")
        .ok_or_else(|| failure("invalid app metadata root"))?;
    if doc.text("appid").and_then(|s| s.parse::<u32>().ok()) != Some(app_id) {
        return Err(failure("app metadata ID does not match the requested app"));
    }
    Ok(doc)
}

pub(super) fn app(app_id: u32, buffer: &[u8]) -> Result<LibraryApp> {
    let document = document(app_id, buffer)?;
    Ok(LibraryApp {
        app_id,
        name: document
            .at(&["common"])
            .and_then(|n| n.text("name"))
            .map(str::to_owned),
        app_type: document
            .at(&["common"])
            .and_then(|n| n.text("type"))
            .map(str::to_owned),
        document: Some(document),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn package(entries: &[u8]) -> Vec<u8> {
        let mut bytes = 1u32.to_le_bytes().to_vec();
        bytes.extend_from_slice(b"\0" /* object */);
        bytes.extend_from_slice(b"42\0\0appids\0");
        bytes.extend_from_slice(entries);
        bytes.extend_from_slice(&[8, 8, 8]);
        bytes
    }

    #[test]
    fn package_app_values_not_array_indices_are_sorted_and_deduplicated() {
        let mut entries = vec![2];
        entries.extend_from_slice(b"0\0");
        entries.extend_from_slice(&570u32.to_le_bytes());
        entries.extend_from_slice(b"\x011\0" /* string */);
        entries.extend_from_slice(b"440\0\x012\0" /* string */);
        entries.extend_from_slice(b"570\0");
        assert_eq!(
            package_apps(42, &package(&entries)).unwrap(),
            Some(BTreeSet::from([440, 570]))
        );
    }

    #[test]
    fn missing_package_app_table_is_not_a_known_empty_package() {
        let mut bytes = 1u32.to_le_bytes().to_vec();
        bytes.extend_from_slice(b"\0" /* object */);
        bytes.extend_from_slice(b"42\0\0depotids\0\x020\0");
        bytes.extend_from_slice(&123u32.to_le_bytes());
        bytes.extend_from_slice(&[8, 8, 8]);
        assert_eq!(package_apps(42, &bytes).unwrap(), None);
        assert_eq!(
            package_apps(42, &package(&[])).unwrap(),
            Some(BTreeSet::new())
        );
    }

    #[test]
    fn package_rejects_truncation_mismatched_ids_and_invalid_values() {
        let bytes = package(b"\x010\0-1\0");
        assert!(package_apps(42, &bytes).is_err());
        let bytes = package(&[]);
        assert!(package_apps(43, &bytes).is_err());
        assert!(package_apps(42, &bytes[..bytes.len() - 1]).is_err());
        let mut bytes = bytes;
        bytes.extend_from_slice(b"unexpected");
        assert!(package_apps(42, &bytes).is_err());
    }

    #[test]
    fn app_uses_only_common_metadata_and_keeps_unknown_fields_optional() {
        let metadata = b"\"appinfo\"\n{\n\"appid\" \"440\"\n\"name\" \"Wrong scope\"\n\"common\"\n{\n\"name\" \"Team \\\"Fortress\\\" 2\"\n}\n}\0";
        let app = app(440, metadata).unwrap();
        assert_eq!(app.name.as_deref(), Some("Team \"Fortress\" 2"));
        assert_eq!(app.app_type, None);
    }

    #[test]
    fn app_requires_matching_identity_and_complete_structure() {
        let metadata = b"\"appinfo\"\n{\n\"appid\" \"440\"\n}\n";
        assert!(app(570, metadata).is_err());
        assert!(app(440, &metadata[..metadata.len() - 2]).is_err());
        assert!(app(440, b"\"appinfo\"\n{\n}\n").is_err());
        let metadata =
            b"\"appinfo\"\n{\n\"appid\" \"440\"\n\"common\"\n{\n\"name\" \"A\" \"B\"\n}\n}\n";
        assert!(app(440, metadata).is_err());
    }

    #[test]
    fn deeply_nested_app_metadata_is_rejected_without_recursion() {
        let mut text = String::from("\"appinfo\"\n{\n\"appid\" \"440\"\n");
        for _ in 0..MAX_DEPTH {
            text.push_str("\"nested\"\n{\n");
        }
        for _ in 0..=MAX_DEPTH {
            text.push_str("}\n");
        }
        assert!(app(440, text.as_bytes()).is_err());
    }

    #[test]
    fn package_depot_grants_do_not_become_app_membership() {
        let mut bytes = 1u32.to_le_bytes().to_vec();
        bytes.extend_from_slice(b"\0" /* root object */);
        bytes.extend_from_slice(b"42\0\0appids\0\x020\0");
        bytes.extend_from_slice(&10u32.to_le_bytes());
        bytes.extend_from_slice(b"\x08\0depotids\0\x020\0");
        bytes.extend_from_slice(&101u32.to_le_bytes());
        bytes.extend_from_slice(&[8, 8, 8]);
        assert_eq!(
            package_apps(42, &bytes).unwrap(),
            Some(BTreeSet::from([10]))
        );
        assert_eq!(
            package_depots(42, &bytes).unwrap(),
            Some(BTreeSet::from([101]))
        );
    }

    #[test]
    fn full_pics_uses_iterative_lexer_for_large_escaped_values() {
        let escaped = "\\\"".repeat(100_000);
        let source = format!(
            "\"appinfo\" {{ \"appid\" \"440\" \"config\" {{ \"payload\" \"{escaped}\" \"launch\" {{ \"0\" {{ \"executable\" \"game\" }} }} }} }}"
        );
        let doc = document(440, source.as_bytes()).unwrap();
        assert_eq!(
            doc.at(&["config", "launch", "0"])
                .unwrap()
                .text("executable"),
            Some("game")
        );
        assert_eq!(
            doc.at(&["config"]).unwrap().text("payload").unwrap().len(),
            100_000
        );
    }
}
