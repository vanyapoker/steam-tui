use super::*;
use base64::{Engine, engine::general_purpose::STANDARD};
use bytes::BytesMut;
use protobuf::Message;
use sha1::{Digest, Sha1};
use std::collections::BTreeMap;
use std::io::{Cursor, Read};
use steam_vent_proto_steam::content_manifest::{
    ContentManifestMetadata, ContentManifestPayload, ContentManifestSignature,
};

const PAYLOAD: u32 = 0x71f617d0;
const METADATA: u32 = 0x1f4812be;
const SIGNATURE: u32 = 0x1b81b817;
const END: u32 = 0x32c415ab;
const BINARY: u32 = 0x16349781;
const MAX_FILES: usize = 1_000_000;
const MAX_CHUNKS: usize = 4_000_000;

fn read_bounded(mut source: impl Read, limit: usize) -> Result<Vec<u8>> {
    let mut bytes = vec![0; limit];
    source.read_exact(&mut bytes).map_err(|_| {
        failure("decompressed content is shorter than its declared size or corrupt")
    })?;
    let mut extra = [0];
    if source
        .read(&mut extra)
        .map_err(|_| failure("compressed content is truncated or corrupt"))?
        != 0
    {
        return Err(failure("decompressed content exceeds its size limit"));
    }
    Ok(bytes)
}
fn unzip(bytes: &[u8], limit: usize, exact: Option<usize>) -> Result<Vec<u8>> {
    // Preflight EOCD before ZipArchive allocates its central-directory entries.
    // SteamPipe's bounded single-entry ZIPs never need ZIP64 or multiple disks.
    let start = bytes.len().saturating_sub(65557);
    let end = bytes[start..]
        .windows(4)
        .rposition(|part| part == b"PK\x05\x06")
        .map(|at| start + at)
        .ok_or_else(|| failure("SteamPipe ZIP end record is missing"))?;
    let record = bytes
        .get(end..end + 22)
        .ok_or_else(|| failure("SteamPipe ZIP end record is truncated"))?;
    if record[4..8] != [0; 4] || record[8..12] != [1, 0, 1, 0] {
        return Err(failure(
            "SteamPipe ZIP requires one entry on one disk; ZIP64/multi-entry containers are not supported",
        ));
    }
    let comment = u16::from_le_bytes(record[20..22].try_into().unwrap()) as usize;
    if end + 22 + comment != bytes.len() {
        return Err(failure("SteamPipe ZIP has an inconsistent end record"));
    }
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes))
        .map_err(|_| failure("invalid SteamPipe ZIP container"))?;
    if archive.len() != 1 {
        return Err(failure("SteamPipe ZIP must contain exactly one entry"));
    }
    let entry = archive
        .by_index(0)
        .map_err(|_| failure("unreadable SteamPipe ZIP entry"))?;
    if entry.is_dir()
        || entry.size() > limit as u64
        || exact.is_some_and(|n| entry.size() != n as u64)
    {
        return Err(failure(
            "SteamPipe ZIP entry size differs from its manifest bounds",
        ));
    }
    let expected = entry.size() as usize;
    let crc = entry.crc32();
    let output = read_bounded(entry, expected)?;
    if output.len() != expected || crc32fast::hash(&output) != crc {
        return Err(failure("SteamPipe ZIP CRC/size mismatch"));
    }
    Ok(output)
}
fn decrypt(bytes: &[u8], key: &[u8; 32]) -> Result<BytesMut> {
    // The sibling primitive splits its input without checking; guard first.
    if bytes.len() < 32 || !bytes.len().is_multiple_of(16) {
        return Err(failure("invalid encrypted-IV AES256-CBC framing"));
    }
    steam_vent_crypto::symmetric_decrypt_without_hmac(BytesMut::from(bytes), key)
        .map_err(|_| failure("AES256-CBC padding is invalid; content or depot key is wrong"))
}
fn decode_name(raw: &str, encrypted: bool, key: &[u8; 32]) -> Result<String> {
    if raw.is_empty() || raw.len() > 32768 {
        return Err(failure(
            "manifest filename is empty or exceeds its length limit",
        ));
    }
    if !encrypted {
        return Ok(raw.to_owned());
    }
    // Valve's base64 strings commonly include a final LF (as in the public
    // SteamKit v4/v5 fixtures); .NET Convert.FromBase64String ignores whitespace.
    let bytes = if raw.bytes().any(|b| b.is_ascii_whitespace()) {
        let compact: Vec<u8> = raw.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
        STANDARD.decode(compact)
    } else {
        STANDARD.decode(raw)
    }
    .map_err(|_| failure("encrypted manifest filename has invalid base64"))?;
    let mut plain = decrypt(&bytes, key)?;
    // Steam filename strings can carry one terminating NUL, never an interior NUL.
    if plain.last() == Some(&0) {
        plain.truncate(plain.len() - 1);
    }
    let result = std::str::from_utf8(&plain)
        .map(str::to_owned)
        .map_err(|_| failure("manifest filename is not valid UTF-8"));
    plain.as_mut().zeroize();
    result
}
fn path(raw: &str) -> Result<PathBuf> {
    let normalized = raw.replace('\\', "/");
    if normalized.is_empty()
        || normalized.len() > 4096
        || normalized.starts_with('/')
        || normalized.bytes().any(|b| b == 0 || b < 32 || b == b':')
        || normalized
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return Err(failure("manifest contains an unsafe relative filename"));
    }
    Ok(PathBuf::from(normalized))
}
fn link(raw: &str, file: &std::path::Path) -> Result<PathBuf> {
    let normalized = raw.replace('\\', "/");
    if normalized.is_empty()
        || normalized.len() > 4096
        || normalized.starts_with('/')
        || normalized.bytes().any(|b| b == 0 || b < 32 || b == b':')
    {
        return Err(failure("manifest contains an unsafe symbolic link target"));
    }
    let mut depth = file.parent().map_or(0, |p| p.components().count());
    for part in normalized.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                if depth == 0 {
                    return Err(failure("manifest symbolic link escapes the install root"));
                }
                depth -= 1;
            }
            _ => depth += 1,
        }
    }
    Ok(PathBuf::from(normalized))
}
fn hash(bytes: &[u8], purpose: &str) -> Result<[u8; 20]> {
    bytes
        .try_into()
        .map_err(|_| failure(format!("manifest {purpose} must have a 20-byte SHA1")))
}
struct RawFile {
    name: String,
    name_hash: Vec<u8>,
    size: u64,
    flags: u32,
    sha: Vec<u8>,
    chunks: Vec<DepotChunk>,
    link: String,
}
struct RawManifest {
    depot: u32,
    gid: u64,
    encrypted: bool,
    files: Vec<RawFile>,
}

pub(super) fn manifest(bytes: &[u8], spec: &DepotSpec, key: &[u8; 32]) -> Result<DepotManifest> {
    if bytes.len() > MAX_MANIFEST {
        return Err(failure("manifest exceeds the 128 MiB size bound"));
    }
    let decoded;
    let bytes = if bytes.starts_with(b"PK\x03\x04") {
        decoded = unzip(bytes, MAX_MANIFEST, None)?;
        decoded.as_slice()
    } else {
        bytes
    };
    let mut input = BinaryReader { bytes, at: 0 };
    let magic = input.u32()?;
    let raw = if magic == BINARY {
        binary(&mut input)?
    } else {
        protobuf_manifest(magic, &mut input)?
    };
    if input.at != bytes.len() {
        return Err(failure("trailing bytes after manifest terminator"));
    }
    if raw.depot != spec.depot_id || raw.gid != spec.manifest_id {
        return Err(failure(
            "manifest depot/GID does not match the requested depot",
        ));
    }
    if raw.files.len() > MAX_FILES {
        return Err(failure("manifest exceeds the file count limit"));
    }
    let mut files = Vec::with_capacity(raw.files.len());
    let mut paths = BTreeMap::new();
    let mut count = 0usize;
    for raw_file in raw.files {
        let name = decode_name(&raw_file.name, raw.encrypted, key)?;
        let filename_hash = hash(&raw_file.name_hash, "filename hash")?;
        // Steam hashes the lowercase Windows-separated plaintext filename.
        let canonical = name.replace('/', "\\").to_lowercase();
        // Symlink entries (flag 0x200) carry a filename hash Steam does not derive from the name.
        if raw_file.flags & 0x200 == 0
            && Sha1::digest(canonical.as_bytes()).as_slice() != filename_hash
        {
            return Err(failure(format!(
                "manifest filename SHA1 mismatch for {name:?} (flags {:#x})",
                raw_file.flags
            )));
        }
        let file_path = path(if raw_file.flags & 64 != 0 {
            name.trim_end_matches(['/', '\\'])
        } else {
            &name
        })?;
        if raw_file.flags & !1023 != 0 {
            return Err(failure(format!(
                "manifest uses unknown file flags 0x{:x}",
                raw_file.flags
            )));
        }
        let directory = raw_file.flags & 64 != 0;
        let symlink = raw_file.flags & 512 != 0;
        if directory && symlink {
            return Err(failure(
                "manifest entry is both a directory and a symbolic link",
            ));
        }
        if paths.insert(file_path.clone(), directory).is_some() {
            return Err(failure("manifest has duplicate normalized filenames"));
        }
        let link_target = if raw_file.link.is_empty() {
            None
        } else {
            Some(link(
                &decode_name(&raw_file.link, raw.encrypted, key)?,
                &file_path,
            )?)
        };
        if symlink != link_target.is_some() {
            return Err(failure("manifest symbolic link flag and target disagree"));
        }
        let mut chunks = raw_file.chunks;
        count = count
            .checked_add(chunks.len())
            .ok_or_else(|| failure("manifest chunk count overflow"))?;
        if count > MAX_CHUNKS {
            return Err(failure("manifest exceeds the chunk count limit"));
        }
        chunks.sort_by_key(|chunk| chunk.offset);
        let mut offset = 0u64;
        for chunk in &chunks {
            validate_chunk(chunk)?;
            if chunk.offset != offset {
                return Err(failure("manifest chunks overlap or leave a hole in a file"));
            }
            offset = offset
                .checked_add(u64::from(chunk.uncompressed_size))
                .ok_or_else(|| failure("manifest file length overflow"))?;
        }
        if directory || symlink {
            if !chunks.is_empty() {
                return Err(failure(
                    "directory or symbolic link unexpectedly has content chunks",
                ));
            }
        } else if offset != raw_file.size {
            return Err(failure("manifest chunk lengths do not cover the file size"));
        }
        let sha1 = if directory || symlink {
            if !raw_file.sha.is_empty() && raw_file.sha.len() != 20 {
                return Err(failure("invalid special-file SHA1 length"));
            }
            None
        } else {
            Some(hash(&raw_file.sha, "content hash")?)
        };
        files.push(DepotFile {
            path: file_path,
            size: raw_file.size,
            sha1,
            flags: raw_file.flags,
            chunks,
            link_target,
        });
    }
    for file in &files {
        for parent in file
            .path
            .ancestors()
            .skip(1)
            .filter(|p| !p.as_os_str().is_empty())
        {
            if paths.get(parent) == Some(&false) {
                return Err(failure(
                    "manifest file or symlink is also used as a parent directory",
                ));
            }
        }
    }
    validate_links(&files)?;
    files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(DepotManifest {
        depot_id: raw.depot,
        manifest_id: raw.gid,
        files,
    })
}
fn validate_links(files: &[DepotFile]) -> Result<()> {
    use std::collections::VecDeque;
    let links: HashMap<_, _> = files
        .iter()
        .filter_map(|file| file.link_target.as_ref().map(|target| (&file.path, target)))
        .collect();
    for file in files {
        let Some(target) = &file.link_target else {
            continue;
        };
        let mut resolved = file
            .path
            .parent()
            .unwrap_or(std::path::Path::new(""))
            .to_path_buf();
        let mut pending: VecDeque<String> = target
            .to_str()
            .ok_or_else(|| failure("non-UTF8 symbolic link target"))?
            .split('/')
            .map(str::to_owned)
            .collect();
        let mut expansions = 0;
        while let Some(part) = pending.pop_front() {
            match part.as_str() {
                "" | "." => continue,
                ".." => {
                    if !resolved.pop() {
                        return Err(failure(
                            "manifest symbolic link chain escapes the install root",
                        ));
                    }
                }
                _ => {
                    resolved.push(part);
                    if let Some(target) = links.get(&resolved) {
                        expansions += 1;
                        if expansions > 256 {
                            return Err(failure(
                                "manifest symbolic link chain is cyclic or exceeds 256 links",
                            ));
                        }
                        resolved.pop();
                        for part in target
                            .to_str()
                            .ok_or_else(|| failure("non-UTF8 symbolic link target"))?
                            .split('/')
                            .rev()
                        {
                            pending.push_front(part.to_owned());
                        }
                    }
                }
            }
        }
    }
    Ok(())
}
// Count length-delimited repeated entries before generated protobuf decoding:
// a tiny wire payload of empty mappings must not allocate unbounded structs.
fn wire_fields(mut bytes: &[u8], mut visit: impl FnMut(u64, &[u8]) -> Result<()>) -> Result<()> {
    fn varint(bytes: &mut &[u8]) -> Result<u64> {
        let mut value = 0u64;
        for shift in (0..70).step_by(7) {
            let byte = *bytes
                .first()
                .ok_or_else(|| failure("truncated manifest protobuf varint"))?;
            *bytes = &bytes[1..];
            if shift == 63 && byte > 1 {
                return Err(failure("manifest protobuf varint overflow"));
            }
            value |= u64::from(byte & 127) << shift;
            if byte & 128 == 0 {
                return Ok(value);
            }
        }
        Err(failure("manifest protobuf varint overflow"))
    }
    while !bytes.is_empty() {
        let tag = varint(&mut bytes)?;
        if tag >> 3 == 0 {
            return Err(failure("invalid manifest protobuf field number"));
        }
        let len = match tag & 7 {
            0 => {
                varint(&mut bytes)?;
                continue;
            }
            1 => 8,
            2 => usize::try_from(varint(&mut bytes)?)
                .map_err(|_| failure("manifest protobuf length overflow"))?,
            5 => 4,
            _ => {
                return Err(failure(
                    "manifest protobuf contains unsupported group/wire framing",
                ));
            }
        };
        let field = bytes
            .get(..len)
            .ok_or_else(|| failure("truncated manifest protobuf field"))?;
        if tag & 7 == 2 {
            visit(tag >> 3, field)?;
        }
        bytes = &bytes[len..];
    }
    Ok(())
}
fn payload_bounds(payload: &[u8]) -> Result<()> {
    let (mut files, mut chunks) = (0usize, 0usize);
    wire_fields(payload, |field, mapping| {
        if field == 1 {
            files += 1;
            if files > MAX_FILES {
                return Err(failure("manifest exceeds the file count limit"));
            }
            wire_fields(mapping, |field, chunk| {
                if field == 6 {
                    chunks += 1;
                    if chunks > MAX_CHUNKS || chunk.len() > 4096 {
                        return Err(failure(
                            "manifest exceeds the chunk count/metadata size limit",
                        ));
                    }
                    wire_fields(chunk, |_, _| Ok(()))?;
                }
                Ok(())
            })?;
        }
        Ok(())
    })
}
fn protobuf_manifest(first: u32, input: &mut BinaryReader<'_>) -> Result<RawManifest> {
    let mut magic = first;
    let (mut payload, mut metadata, mut signature) = (None, None, false);
    loop {
        if magic == END {
            break;
        }
        let len = input.u32()? as usize;
        let section = input.take(len)?;
        match magic {
            PAYLOAD if payload.is_none() => {
                payload = Some(section);
            }
            METADATA if metadata.is_none() && len <= 16384 => {
                metadata = Some(
                    ContentManifestMetadata::parse_from_bytes(section)
                        .map_err(|_| failure("invalid manifest metadata protobuf"))?,
                );
            }
            SIGNATURE if !signature && len <= 4096 => {
                ContentManifestSignature::parse_from_bytes(section)
                    .map_err(|_| failure("invalid manifest signature protobuf"))?;
                signature = true;
            }
            PAYLOAD | METADATA | SIGNATURE => return Err(failure("duplicate manifest section")),
            _ => {
                return Err(failure(format!(
                    "unknown Steam manifest section 0x{magic:08x}; unsupported format"
                )));
            }
        }
        magic = input.u32()?;
    }
    let metadata = metadata.ok_or_else(|| failure("manifest is missing metadata"))?;
    let payload = payload.ok_or_else(|| failure("manifest is missing its payload"))?;
    if !signature {
        return Err(failure("manifest is missing its signature section"));
    }
    let crc = if metadata.filenames_encrypted() {
        metadata.crc_encrypted()
    } else {
        metadata.crc_clear()
    };
    if crc != 0 {
        let mut hasher = crc32fast::Hasher::new();
        hasher.update(&(payload.len() as u32).to_le_bytes());
        hasher.update(payload);
        if hasher.finalize() != crc {
            return Err(failure("manifest payload CRC32 mismatch"));
        }
    }
    payload_bounds(payload)?;
    let payload = ContentManifestPayload::parse_from_bytes(payload)
        .map_err(|_| failure("invalid manifest payload protobuf"))?;
    if payload.mappings.len() > MAX_FILES {
        return Err(failure("manifest exceeds the file count limit"));
    }
    let mut files = Vec::with_capacity(payload.mappings.len());
    for mapping in payload.mappings {
        let mut chunks = Vec::with_capacity(mapping.chunks.len());
        for chunk in mapping.chunks {
            chunks.push(DepotChunk {
                id: hash(chunk.sha(), "chunk ID")?,
                offset: chunk.offset(),
                uncompressed_size: chunk.cb_original(),
                compressed_size: chunk.cb_compressed(),
                adler: chunk.crc(),
            });
        }
        files.push(RawFile {
            name: mapping.filename.unwrap_or_default(),
            name_hash: mapping.sha_filename.unwrap_or_default(),
            size: mapping.size.unwrap_or(0),
            flags: mapping.flags.unwrap_or(0),
            sha: mapping.sha_content.unwrap_or_default(),
            chunks,
            link: mapping.linktarget.unwrap_or_default(),
        });
    }
    Ok(RawManifest {
        depot: metadata.depot_id(),
        gid: metadata.gid_manifest(),
        encrypted: metadata.filenames_encrypted(),
        files,
    })
}
struct BinaryReader<'a> {
    bytes: &'a [u8],
    at: usize,
}
impl<'a> BinaryReader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self
            .at
            .checked_add(n)
            .ok_or_else(|| failure("manifest size overflow"))?;
        let part = self
            .bytes
            .get(self.at..end)
            .ok_or_else(|| failure("manifest is truncated"))?;
        self.at = end;
        Ok(part)
    }
    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    fn string(&mut self) -> Result<String> {
        let remaining = &self.bytes[self.at..];
        let len = remaining
            .iter()
            .take(32769)
            .position(|b| *b == 0)
            .ok_or_else(|| failure("unterminated binary manifest filename"))?;
        let result = std::str::from_utf8(self.take(len)?)
            .map(str::to_owned)
            .map_err(|_| failure("binary manifest filename is not UTF-8"))?;
        self.take(1)?;
        Ok(result)
    }
}
fn binary(input: &mut BinaryReader<'_>) -> Result<RawManifest> {
    let version = input.u32()?;
    if version != 4 {
        return Err(failure(format!(
            "unsupported binary Steam manifest version {version}; expected v4"
        )));
    }
    let depot = input.u32()?;
    let gid = input.u64()?;
    input.u32()?; // creation timestamp
    let encrypted = input.u32()? != 0;
    input.u64()?;
    input.u64()?; // compressed/uncompressed depot totals
    let expected_chunks = input.u32()? as usize;
    let expected_files = input.u32()? as usize;
    let mapping_bytes = input.u32()? as usize;
    let encrypted_crc = input.u32()?;
    let clear_crc = input.u32()?;
    input.u32()?; // flags
    if expected_files > MAX_FILES || expected_chunks > MAX_CHUNKS {
        return Err(failure("binary manifest entry count exceeds the limit"));
    }
    let mapping = input.take(mapping_bytes)?;
    let crc = if encrypted { encrypted_crc } else { clear_crc };
    if crc != 0 && crc32fast::hash(mapping) != crc {
        return Err(failure("binary manifest mapping CRC32 mismatch"));
    }
    let mut reader = BinaryReader {
        bytes: mapping,
        at: 0,
    };
    let mut files = Vec::new();
    let mut count = 0usize;
    while reader.at < mapping.len() {
        if files.len() >= MAX_FILES {
            return Err(failure("binary manifest exceeds file limit"));
        }
        let name = reader.string()?;
        let size = reader.u64()?;
        let flags = reader.u32()?;
        let sha = reader.take(20)?.to_vec();
        let name_hash = reader.take(20)?.to_vec();
        let chunk_count = reader.u32()? as usize;
        count = count
            .checked_add(chunk_count)
            .ok_or_else(|| failure("binary chunk count overflow"))?;
        if count > MAX_CHUNKS || chunk_count > (mapping.len() - reader.at) / 40 {
            return Err(failure(
                "binary manifest chunk count exceeds available data",
            ));
        }
        let mut chunks = Vec::with_capacity(chunk_count);
        for _ in 0..chunk_count {
            chunks.push(DepotChunk {
                id: hash(reader.take(20)?, "chunk ID")?,
                adler: reader.u32()?,
                offset: reader.u64()?,
                uncompressed_size: reader.u32()?,
                compressed_size: reader.u32()?,
            });
        }
        files.push(RawFile {
            name,
            name_hash,
            size,
            flags,
            sha,
            chunks,
            link: String::new(),
        });
    }
    if files.len() != expected_files || count != expected_chunks || input.u32()? != BINARY {
        return Err(failure("binary manifest counts or end marker do not match"));
    }
    Ok(RawManifest {
        depot,
        gid,
        encrypted,
        files,
    })
}
pub(super) fn validate_chunk(chunk: &DepotChunk) -> Result<()> {
    if chunk.uncompressed_size == 0
        || chunk.uncompressed_size as usize > MAX_CHUNK
        || chunk.compressed_size < 32
        || chunk.compressed_size as usize > MAX_CHUNK
        || !chunk.compressed_size.is_multiple_of(16)
    {
        return Err(failure(
            "chunk sizes exceed the supported 16 MiB bound or have invalid AES framing",
        ));
    }
    Ok(())
}
pub(super) fn chunk(bytes: &[u8], chunk: &DepotChunk, key: &[u8; 32]) -> Result<Vec<u8>> {
    validate_chunk(chunk)?;
    if bytes.len() != chunk.compressed_size as usize {
        return Err(failure("encrypted chunk length mismatch"));
    }
    let plain = decrypt(bytes, key)?;
    let expected = chunk.uncompressed_size as usize;
    let output = if plain.starts_with(b"PK\x03\x04") {
        unzip(&plain, expected, Some(expected))?
    } else if plain.starts_with(b"VZa") {
        vzip(&plain, expected)?
    } else if plain.starts_with(b"VSZa") {
        vzstd(&plain, expected)?
    } else {
        return Err(failure(
            "unknown Steam chunk compression; expected ZIP, VZip/LZMA v'a', or VSZ/Zstd v'a'",
        ));
    };
    verify_chunk(chunk, &output)?;
    Ok(output)
}
fn u32_at(bytes: &[u8], at: usize) -> Result<u32> {
    Ok(u32::from_le_bytes(
        bytes
            .get(at..at + 4)
            .ok_or_else(|| failure("compressed frame is truncated"))?
            .try_into()
            .unwrap(),
    ))
}
pub(super) fn vzip(bytes: &[u8], expected: usize) -> Result<Vec<u8>> {
    if bytes.len() < 22 || !bytes.ends_with(b"zv") {
        return Err(failure("invalid VZip header/footer"));
    }
    let footer = bytes.len() - 10;
    let dictionary = u32_at(bytes, 8)?;
    if u64::from(dictionary) > MAX_WINDOW || bytes[7] >= 225 {
        return Err(failure(
            "VZip dictionary/properties exceed the decompression bound",
        ));
    }
    if u32_at(bytes, footer + 4)? as usize != expected {
        return Err(failure("VZip footer length differs from the manifest"));
    }
    let reader = lzma_rust2::LzmaReader::new_with_props(
        &bytes[12..footer],
        expected as u64,
        bytes[7],
        dictionary,
        None,
    )
    .map_err(|_| failure("invalid VZip LZMA properties"))?;
    let output = read_bounded(reader, expected)?;
    if output.len() != expected || crc32fast::hash(&output) != u32_at(bytes, footer)? {
        return Err(failure("VZip decompressed length/CRC32 mismatch"));
    }
    Ok(output)
}
fn vzstd(bytes: &[u8], expected: usize) -> Result<Vec<u8>> {
    if bytes.len() < 23 || !bytes.ends_with(b"zsv") {
        return Err(failure("invalid VSZ header/footer"));
    }
    let footer = bytes.len() - 15;
    let size = u64::from(u32_at(bytes, footer + 4)?);
    if size != expected as u64 || u32_at(bytes, 4)? != u32_at(bytes, footer)? {
        return Err(failure("VSZ length/CRC header and footer disagree"));
    }
    let frame = &bytes[8..footer];
    zstd_window(frame)?;
    let decoder = ruzstd::decoding::StreamingDecoder::new(Cursor::new(frame))
        .map_err(|_| failure("invalid VSZ Zstd frame"))?;
    let mut decoder = decoder;
    let output = read_bounded(&mut decoder, expected)?;
    if decoder.get_ref().position() != frame.len() as u64 {
        return Err(failure("VSZ contains trailing or multiple Zstd frames"));
    }
    if output.len() != expected || crc32fast::hash(&output) != u32_at(bytes, footer)? {
        return Err(failure("VSZ decompressed length/CRC32 mismatch"));
    }
    Ok(output)
}
pub(super) fn zstd_window(frame: &[u8]) -> Result<()> {
    if frame.len() < 6 || !frame.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]) {
        return Err(failure("VSZ does not contain a Zstd frame"));
    }
    let descriptor = frame[4];
    if descriptor & 0x08 != 0 {
        return Err(failure("Zstd frame has reserved header bits"));
    }
    let single = descriptor & 0x20 != 0;
    let mut at = 5;
    if !single {
        let window = frame[at];
        at += 1;
        let log = 10 + (window >> 3);
        if log > 26 {
            return Err(failure(
                "Zstd window exceeds the 64 MiB decompression bound",
            ));
        }
        let base = 1u64 << log;
        if base + (base >> 3) * u64::from(window & 7) > MAX_WINDOW {
            return Err(failure(
                "Zstd window exceeds the 64 MiB decompression bound",
            ));
        }
    }
    at += match descriptor & 3 {
        0 => 0,
        1 => 1,
        2 => 2,
        _ => 4,
    };
    let fcs = descriptor >> 6;
    let n = match fcs {
        0 if single => 1,
        0 => 0,
        1 => 2,
        2 => 4,
        _ => 8,
    };
    let data = frame
        .get(at..at + n)
        .ok_or_else(|| failure("Zstd frame header is truncated"))?;
    if single {
        let mut value = [0u8; 8];
        value[..n].copy_from_slice(data);
        let size = u64::from_le_bytes(value) + if n == 2 { 256 } else { 0 };
        if size > MAX_WINDOW {
            return Err(failure(
                "Zstd single-segment window exceeds the decompression bound",
            ));
        }
    }
    Ok(())
}
