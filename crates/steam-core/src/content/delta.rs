//! Depot delta framing and reconstruction, traced in the original steamclient:
//! 021ac830 parses encrypted 502f15e5/protobuf/f3dea982 frames; 0136d690
//! dispatches method 1 (decompressed XOR) and 2 (prearranged dictionary), and
//! 02507730 selects VZd / VSZd dictionary streams. Unknown methods fall back.
use super::*;
use protobuf::Message;
use sha1::{Digest, Sha1};
use std::collections::BTreeMap;
use std::sync::Arc;
use steam_vent_proto_steam::content_manifest::{
    ContentDeltaChunks, EContentDeltaChunkDataLocation,
};

pub(crate) const MAX_PATCH: usize = 16 * 1024 * 1024;
const MAX_ENTRIES: usize = 100_000;
type Identity = (u32, u64, u64, u32);
#[derive(Default)]
pub(super) struct Cache {
    entries: HashMap<Identity, Option<Arc<Patch>>>,
    bytes: usize,
}
pub(crate) struct Patch {
    entries: BTreeMap<[u8; 20], Entry>,
    bytes: usize,
    wire_bytes: u64,
}
pub(crate) struct Entry {
    pub source: [u8; 20],
    size: usize,
    method: u32,
    data: Vec<u8>,
}
impl Patch {
    pub(crate) fn entry(&self, target: &[u8; 20]) -> Option<&Entry> {
        self.entries.get(target)
    }
    fn parse(bytes: &[u8], spec: &DepotSpec, source: u64) -> Result<Self> {
        if bytes.len() < 12
            || bytes.len() > MAX_PATCH
            || u32::from_le_bytes(bytes[..4].try_into().unwrap()) != 0x502f15e5
        {
            return Err(failure("Invalid depot patch header/size"));
        }
        let len = u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as usize;
        let end = 8usize
            .checked_add(len)
            .filter(|n| *n <= bytes.len().saturating_sub(4))
            .ok_or_else(|| failure("Truncated depot patch"))?;
        if u32::from_le_bytes(bytes[end..end + 4].try_into().unwrap()) != 0xf3dea982 {
            return Err(failure("Invalid depot patch trailer"));
        }
        protobuf_bounds(&bytes[8..end], true)?;
        let proto = ContentDeltaChunks::parse_from_bytes(&bytes[8..end])
            .map_err(|_| failure("Invalid depot patch protobuf"))?;
        if proto.depot_id() != spec.depot_id
            || proto.manifest_id_source() != source
            || proto.manifest_id_target() != spec.manifest_id
            || proto.deltaChunks.len() > MAX_ENTRIES
        {
            return Err(failure(
                "Depot patch identities/count do not match requested manifests",
            ));
        }
        let location = proto
            .chunk_data_location
            .map_or(
                Ok(EContentDeltaChunkDataLocation::k_EContentDeltaChunkDataLocationInProtobuf),
                |value| value.enum_value(),
            )
            .map_err(|_| failure("Unknown depot patch data location"))?;
        let mut after = end + 4;
        let mut entries = BTreeMap::new();
        let mut total = 0;
        for chunk in proto.deltaChunks {
            let source: [u8; 20] = chunk
                .sha_source()
                .try_into()
                .map_err(|_| failure("Invalid patch source SHA1"))?;
            let target: [u8; 20] = chunk
                .sha_target()
                .try_into()
                .map_err(|_| failure("Invalid patch target SHA1"))?;
            let size = chunk.size_original() as usize;
            if size == 0 || size > MAX_CHUNK {
                return Err(failure("Depot patch output exceeds chunk bound"));
            }
            let method = chunk.patch_method();
            let data = match location {
                EContentDeltaChunkDataLocation::k_EContentDeltaChunkDataLocationInProtobuf => {
                    if chunk
                        .size_delta
                        .is_some_and(|n| n as usize != chunk.chunk().len())
                    {
                        return Err(failure("Patch chunk size mismatch"));
                    }
                    chunk.chunk.unwrap_or_default()
                }
                EContentDeltaChunkDataLocation::k_EContentDeltaChunkDataLocationAfterProtobuf => {
                    if !chunk.chunk().is_empty() {
                        return Err(failure("Ambiguous patch chunk data"));
                    }
                    let next = after
                        .checked_add(chunk.size_delta() as usize)
                        .filter(|n| *n <= bytes.len())
                        .ok_or_else(|| failure("Truncated appended patch data"))?;
                    let data = bytes[after..next].to_vec();
                    after = next;
                    data
                }
            };
            if data.is_empty() || data.len() > MAX_CHUNK || total + data.len() > MAX_PATCH {
                return Err(failure("Patch data exceeds buffer bound"));
            }
            total += data.len();
            if entries
                .insert(
                    target,
                    Entry {
                        source,
                        size,
                        method,
                        data,
                    },
                )
                .is_some()
            {
                return Err(failure("Duplicate patch target"));
            }
        }
        if after != bytes.len() {
            return Err(failure("Trailing bytes in depot patch"));
        }
        Ok(Self {
            bytes: total + entries.len() * (std::mem::size_of::<Entry>() + 96),
            entries,
            wire_bytes: 0,
        })
    }
    pub(crate) fn reconstruct(&self, target: &DepotChunk, source: &[u8]) -> Result<VerifiedChunk> {
        let entry = self
            .entry(&target.id)
            .ok_or_else(|| failure("Missing delta chunk"))?;
        if entry.size != target.uncompressed_size as usize
            || source.is_empty()
            || source.len() > MAX_CHUNK
            || <[u8; 20]>::from(Sha1::digest(source)) != entry.source
        {
            return Err(failure("Delta source/output descriptor mismatch"));
        }
        let size = target.uncompressed_size as usize;
        let mut bytes = match entry.method {
            1 => {
                if source.len() != size {
                    return Err(failure("XOR patch requires equal source and target sizes"));
                }
                let mut delta = if entry.data.starts_with(b"VZa") {
                    formats::vzip(&entry.data, size)?
                } else if entry.data.starts_with(b"VSZa") {
                    formats::vzstd(&entry.data, size)?
                } else if entry.data.starts_with(b"PK\x03\x04") {
                    formats::unzip(&entry.data, size, Some(size))?
                } else {
                    return Err(failure("Unsupported XOR delta compression"));
                };
                for (delta, old) in delta.iter_mut().zip(source) {
                    *delta ^= old;
                }
                delta
            }
            2 if entry.data.starts_with(b"VZd") => {
                formats::vzip_dictionary(&entry.data, size, Some(source))?
            }
            2 if entry.data.starts_with(b"VSZd") => {
                formats::vzstd_dictionary(&entry.data, size, Some(source))?
            }
            _ => return Err(failure("Unsupported delta patch method/compression")),
        };
        if let Err(error) = verify_chunk(target, &bytes) {
            bytes.zeroize();
            return Err(error);
        }
        Ok(VerifiedChunk {
            chunk: target.clone(),
            bytes,
        })
    }
}
impl ContentClient {
    /// A failed optional patch is cached as unavailable for this install. Normal
    /// authorized chunks remain the fallback; cancellation is checked by callers.
    pub(crate) async fn patch(
        &self,
        spec: &DepotSpec,
        source: u64,
        missing_bytes: u64,
        cancel: &CancellationToken,
    ) -> Result<(Option<Arc<Patch>>, u64)> {
        validate_spec(spec)?;
        if source == 0 || source == spec.manifest_id {
            return Ok((None, 0));
        }
        let identity = (spec.depot_id, source, spec.manifest_id, spec.source_app_id);
        let mut cache = self.patches.lock().await;
        if let Some(patch) = cache.entries.get(&identity) {
            return Ok((patch.clone(), 0));
        }
        if cache.entries.len() >= 512 {
            return Ok((None, 0));
        }
        let patch = tokio::time::timeout(DEADLINE, async {
            if !self.cache.lock().await.chunks.contains_key(&(
                spec.depot_id,
                spec.manifest_id,
                spec.source_app_id,
            )) {
                return Err(failure("Patch target manifest is not authorized"));
            }
            let info = bounded(cancel, async {
                self.connection
                    .service_method(CContentServerDirectory_GetDepotPatchInfo_Request {
                        appid: Some(spec.source_app_id),
                        depotid: Some(spec.depot_id),
                        source_manifestid: Some(source),
                        target_manifestid: Some(spec.manifest_id),
                        ..Default::default()
                    })
                    .await
                    .map_err(|_| failure("Depot patch lookup failed"))
            })
            .await?;
            let size =
                usize::try_from(info.patch_size()).map_err(|_| failure("Patch size overflow"))?;
            if !info.is_available()
                || size == 0
                || size > MAX_PATCH
                || size as u64 >= missing_bytes
                || cache.bytes.saturating_add(size) > MAX_PATCH * 2
            {
                return Ok(None);
            }
            let key = self.key(spec, cancel).await?;
            let bytes = self
                .fetch(
                    spec,
                    &format!(
                        "depot/{}/patch/{source}/{}",
                        spec.depot_id, spec.manifest_id
                    ),
                    size,
                    Some(size),
                    cancel,
                )
                .await?;
            let spec = spec.clone();
            let patch = tokio::task::spawn_blocking(move || {
                let mut patch = if bytes.starts_with(&0x502f15e5u32.to_le_bytes()) {
                    Patch::parse(&bytes, &spec, source)?
                } else {
                    let decoded = formats::decrypt(&bytes, &key)?;
                    Patch::parse(&decoded, &spec, source)?
                };
                patch.wire_bytes = size as u64;
                Ok::<_, Error>(patch)
            })
            .await
            .map_err(|_| failure("Patch parsing worker failed"))??;
            Ok::<_, Error>(Some(Arc::new(patch)))
        })
        .await
        .ok()
        .and_then(Result::ok)
        .flatten();
        if cancel.is_cancelled() {
            return Err(cancelled());
        }
        let patch = patch.filter(|patch| cache.bytes.saturating_add(patch.bytes) <= MAX_PATCH * 2);
        if let Some(patch) = &patch {
            cache.bytes += patch.bytes;
        }
        let downloaded = patch.as_ref().map_or(0, |p| p.wire_bytes);
        cache.entries.insert(identity, patch.clone());
        Ok((patch, downloaded))
    }
}

fn protobuf_bounds(mut bytes: &[u8], outer: bool) -> Result<()> {
    fn varint(bytes: &mut &[u8]) -> Result<u64> {
        let mut value = 0u64;
        for i in 0..10 {
            let b = *bytes
                .first()
                .ok_or_else(|| failure("Truncated patch varint"))?;
            *bytes = &bytes[1..];
            if i == 9 && b > 1 {
                return Err(failure("Oversized patch varint"));
            }
            value |= u64::from(b & 127) << (i * 7);
            if b & 128 == 0 {
                return Ok(value);
            }
        }
        Err(failure("Oversized patch varint"))
    }
    let mut chunks = 0;
    while !bytes.is_empty() {
        let tag = varint(&mut bytes)?;
        if tag >> 3 == 0 {
            return Err(failure("Invalid patch protobuf tag"));
        }
        let n = match tag & 7 {
            0 => {
                varint(&mut bytes)?;
                continue;
            }
            1 => 8,
            5 => 4,
            2 => usize::try_from(varint(&mut bytes)?)
                .map_err(|_| failure("Patch field size overflow"))?,
            _ => return Err(failure("Unsupported patch protobuf wire type")),
        };
        let field = bytes
            .get(..n)
            .ok_or_else(|| failure("Patch field exceeds available data"))?;
        if outer && tag >> 3 == 4 {
            if tag & 7 != 2 {
                return Err(failure("Invalid patch chunk wire type"));
            }
            chunks += 1;
            if chunks > MAX_ENTRIES {
                return Err(failure("Too many patch chunks"));
            }
            protobuf_bounds(field, false)?;
        }
        bytes = &bytes[n..];
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use steam_vent_proto_steam::content_manifest::content_delta_chunks::DeltaChunk;
    fn target(bytes: &[u8]) -> DepotChunk {
        DepotChunk {
            id: Sha1::digest(bytes).into(),
            offset: 0,
            uncompressed_size: bytes.len() as u32,
            compressed_size: 32,
            adler: adler32(bytes),
        }
    }
    fn lzma(bytes: &[u8], dict: Option<&[u8]>) -> Vec<u8> {
        let options = lzma_rust2::LzmaOptions {
            dict_size: 4096,
            preset_dict: dict.map(<[u8]>::to_vec),
            ..Default::default()
        };
        let mut writer = lzma_rust2::LzmaWriter::new_no_header(Vec::new(), &options, true).unwrap();
        writer.write_all(bytes).unwrap();
        let compressed = writer.finish().unwrap();
        let mut frame = if dict.is_some() {
            b"VZd".to_vec()
        } else {
            b"VZa".to_vec()
        };
        frame.extend_from_slice(&[0; 4]);
        frame.push(options.get_props());
        frame.extend_from_slice(&options.dict_size.to_le_bytes());
        frame.extend(compressed);
        frame.extend_from_slice(&crc32fast::hash(bytes).to_le_bytes());
        frame.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
        frame.extend_from_slice(b"zv");
        frame
    }
    fn framed(source: &[u8], new: &[u8], method: u32, data: Vec<u8>, appended: bool) -> Vec<u8> {
        let proto = ContentDeltaChunks {
            depot_id: Some(1),
            manifest_id_source: Some(2),
            manifest_id_target: Some(3),
            chunk_data_location: Some(protobuf::EnumOrUnknown::new(if appended {
                EContentDeltaChunkDataLocation::k_EContentDeltaChunkDataLocationAfterProtobuf
            } else {
                EContentDeltaChunkDataLocation::k_EContentDeltaChunkDataLocationInProtobuf
            })),
            deltaChunks: vec![DeltaChunk {
                sha_source: Some(Sha1::digest(source).to_vec()),
                sha_target: Some(Sha1::digest(new).to_vec()),
                size_original: Some(new.len() as u32),
                patch_method: Some(method),
                size_delta: Some(data.len() as u32),
                chunk: (!appended).then(|| data.clone()),
                ..Default::default()
            }],
            ..Default::default()
        }
        .write_to_bytes()
        .unwrap();
        let mut frame = 0x502f15e5u32.to_le_bytes().to_vec();
        frame.extend_from_slice(&(proto.len() as u32).to_le_bytes());
        frame.extend(proto);
        frame.extend_from_slice(&0xf3dea982u32.to_le_bytes());
        if appended {
            frame.extend(data);
        }
        frame
    }
    fn spec() -> DepotSpec {
        DepotSpec {
            depot_id: 1,
            manifest_id: 3,
            source_app_id: 1,
        }
    }
    #[test]
    fn xor_and_dictionary_lzma_reconstruct_verified_targets_in_both_container_layouts() {
        let old = b"source dictionary old bytes";
        let new = b"target dictionary new bytes";
        for appended in [false, true] {
            for method in [1, 2] {
                let data = if method == 1 {
                    let xor: Vec<_> = old.iter().zip(new).map(|(a, b)| a ^ b).collect();
                    lzma(&xor, None)
                } else {
                    lzma(new, Some(old))
                };
                let frame = framed(old, new, method, data, appended);
                let patch = Patch::parse(&frame, &spec(), 2).unwrap();
                assert_eq!(
                    patch.reconstruct(&target(new), old).unwrap().as_bytes(),
                    new
                );
                assert!(patch.reconstruct(&target(new), b"modified source").is_err());
                assert!(Patch::parse(&frame, &spec(), 9).is_err());
                for len in 0..frame.len() {
                    assert!(Patch::parse(&frame[..len], &spec(), 2).is_err());
                }
                let mut trailing = frame.clone();
                trailing.push(0);
                assert!(Patch::parse(&trailing, &spec(), 2).is_err());
            }
        }
    }
    #[test]
    fn raw_dictionary_zstd_matches_independent_libzstd_fixture() {
        // Generated from these public test strings with libzstd
        // ZSTD_compress_usingDict(level=3), not this decoder implementation.
        let old = b"Verified source bytes and dictionary. ".repeat(128);
        let mut new = String::from_utf8(old.clone())
            .unwrap()
            .replace("source", "target")
            .into_bytes();
        new.extend_from_slice(b"update");
        let raw = "28b52ffd600612c50000607461726765747570646174650300ce92540f00d7a30520";
        let compressed: Vec<_> = raw
            .as_bytes()
            .as_chunks::<2>()
            .0
            .iter()
            .map(|p| u8::from_str_radix(std::str::from_utf8(p).unwrap(), 16).unwrap())
            .collect();
        let crc = crc32fast::hash(&new);
        let mut frame = b"VSZd".to_vec();
        frame.extend_from_slice(&crc.to_le_bytes());
        frame.extend(compressed);
        frame.extend_from_slice(&crc.to_le_bytes());
        frame.extend_from_slice(&(new.len() as u64).to_le_bytes());
        frame.extend_from_slice(b"zsv");
        let patch = Patch::parse(&framed(&old, &new, 2, frame, true), &spec(), 2).unwrap();
        assert_eq!(
            patch.reconstruct(&target(&new), &old).unwrap().as_bytes(),
            new
        );
    }
    #[test]
    fn corrupt_or_unknown_methods_are_rejected_before_any_staging_write() {
        let old = b"old";
        let new = b"new";
        let patch = Patch::parse(&framed(old, new, 999, vec![0], false), &spec(), 2).unwrap();
        assert!(patch.reconstruct(&target(new), old).is_err());
        let mut encoded = lzma(new, Some(old));
        encoded[7] = 255;
        let patch = Patch::parse(&framed(old, new, 2, encoded, false), &spec(), 2).unwrap();
        assert!(patch.reconstruct(&target(new), old).is_err());
        assert!(protobuf_bounds(&[0x22, 0xff, 0xff, 0xff, 0xff, 0x0f], true).is_err());
        assert!(protobuf_bounds(&[0x22, 1, 0x80], true).is_err());
    }
}
