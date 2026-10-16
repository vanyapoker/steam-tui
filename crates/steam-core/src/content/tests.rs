//! Published SteamKit2 regression vectors (MIT; SteamRE/SteamKit,
//! SteamKit2/Tests/{DepotChunkFacts,DepotManifestFacts}.cs and Tests/Files).
//! Keys below are public regression-vector keys, never account credentials.
use super::*;
use protobuf::Message;
use sha1::{Digest, Sha1};
use std::io::{Cursor, Write};
use steam_vent_proto_steam::content_manifest::{
    ContentManifestMetadata, ContentManifestPayload, content_manifest_payload::FileMapping,
};

fn hex(text: &str) -> Vec<u8> {
    text.as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
        .collect()
}
fn key(text: &str) -> [u8; 32] {
    hex(text).try_into().unwrap()
}
fn spec() -> DepotSpec {
    DepotSpec {
        depot_id: 440,
        manifest_id: 1118032470228587934,
        source_app_id: 440,
    }
}
fn section(output: &mut Vec<u8>, magic: u32, bytes: &[u8]) {
    output.extend_from_slice(&magic.to_le_bytes());
    output.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
    output.extend_from_slice(bytes);
}
fn framed(mappings: Vec<FileMapping>, encrypted: bool) -> Vec<u8> {
    let payload = ContentManifestPayload {
        mappings,
        ..Default::default()
    }
    .write_to_bytes()
    .unwrap();
    let mut hash = crc32fast::Hasher::new();
    hash.update(&(payload.len() as u32).to_le_bytes());
    hash.update(&payload);
    let crc = hash.finalize();
    let metadata = ContentManifestMetadata {
        depot_id: Some(spec().depot_id),
        gid_manifest: Some(spec().manifest_id),
        filenames_encrypted: Some(encrypted),
        crc_clear: (!encrypted).then_some(crc),
        crc_encrypted: encrypted.then_some(crc),
        ..Default::default()
    }
    .write_to_bytes()
    .unwrap();
    let mut output = Vec::new();
    section(&mut output, 0x71f617d0, &payload);
    section(&mut output, 0x1f4812be, &metadata);
    section(&mut output, 0x1b81b817, &[]);
    output.extend_from_slice(&0x32c415abu32.to_le_bytes());
    output
}
fn empty_file(name: &str) -> FileMapping {
    FileMapping {
        filename: Some(name.into()),
        size: Some(0),
        flags: Some(0),
        sha_filename: Some(
            Sha1::digest(name.replace('/', "\\").to_lowercase().as_bytes()).to_vec(),
        ),
        sha_content: Some(Sha1::digest([]).to_vec()),
        ..Default::default()
    }
}
#[test]
fn zip_framed_manifest_and_encrypted_filename_are_real_formats() {
    let encrypted_name = "ke0QTBNkOl8ujYW7oYMyI3h0RYTpN+FaSk0DvQ3bBzVqbmKddipiJ2jT22F2eJFW\n";
    let mut mapping = empty_file("hl2\\media\\startupvids.txt");
    mapping.filename = Some(encrypted_name.into());
    let raw = framed(vec![mapping], true);
    let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
    zip.start_file(
        "manifest",
        zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated),
    )
    .unwrap();
    zip.write_all(&raw).unwrap();
    let archive = zip.finish().unwrap().into_inner();
    let depot_key = key("44ce5c5297a415a1a6f69c856037a5a2fdd82cd474fa659edfb4d59b2abc55fc");
    let parsed = formats::manifest(&archive, &spec(), &depot_key).unwrap();
    assert_eq!(
        parsed.files[0].path,
        PathBuf::from("hl2/media/startupvids.txt")
    );
    assert!(formats::manifest(&archive, &spec(), &[0; 32]).is_err());
}
#[test]
fn manifests_reject_wrong_identity_corruption_duplicates_and_unsafe_paths() {
    let valid = framed(vec![empty_file("bin/game")], false);
    let mut wrong = spec();
    wrong.depot_id += 1;
    assert!(formats::manifest(&valid, &wrong, &[0; 32]).is_err());
    wrong = spec();
    wrong.manifest_id += 1;
    assert!(formats::manifest(&valid, &wrong, &[0; 32]).is_err());
    let mut corrupt = valid.clone();
    corrupt[12] ^= 1;
    assert!(formats::manifest(&corrupt, &spec(), &[0; 32]).is_err());
    let mut trailing = valid;
    trailing.push(0);
    assert!(formats::manifest(&trailing, &spec(), &[0; 32]).is_err());
    for name in [
        "../escape",
        "bin/../../escape",
        "/absolute",
        "C:\\windows",
        "bin//game",
        "bin/./game",
        "bin\\..\\escape",
        "bad\0name",
    ] {
        assert!(
            formats::manifest(&framed(vec![empty_file(name)], false), &spec(), &[0; 32]).is_err(),
            "accepted {name:?}"
        );
    }
    assert!(
        formats::manifest(
            &framed(vec![empty_file("bin/game"), empty_file("bin\\game")], false),
            &spec(),
            &[0; 32]
        )
        .is_err()
    );
    assert!(
        formats::manifest(
            &framed(vec![empty_file("bin"), empty_file("bin/game")], false),
            &spec(),
            &[0; 32]
        )
        .is_err()
    );
}
#[test]
fn symlink_flags_and_targets_are_checked() {
    let mut file = empty_file("bin/game");
    file.flags = Some(512);
    file.linktarget = Some("../lib/real-game".into());
    let parsed = formats::manifest(&framed(vec![file.clone()], false), &spec(), &[0; 32]).unwrap();
    assert_eq!(
        parsed.files[0].link_target,
        Some(PathBuf::from("../lib/real-game"))
    );
    file.linktarget = Some("../../escape".into());
    assert!(formats::manifest(&framed(vec![file.clone()], false), &spec(), &[0; 32]).is_err());
    file.linktarget = None;
    assert!(formats::manifest(&framed(vec![file], false), &spec(), &[0; 32]).is_err());
    let mut first = empty_file("a");
    first.flags = Some(512);
    first.linktarget = Some(".".into());
    let mut second = empty_file("b");
    second.flags = Some(512);
    second.linktarget = Some("a/../escape".into());
    assert!(
        formats::manifest(
            &framed(vec![first.clone(), second.clone()], false),
            &spec(),
            &[0; 32]
        )
        .is_err()
    );
    first.linktarget = Some("b".into());
    second.linktarget = Some("a".into());
    assert!(formats::manifest(&framed(vec![first, second], false), &spec(), &[0; 32]).is_err());
}
#[test]
fn binary_v4_manifest_is_parsed_and_bounded() {
    let name = "bin/game";
    let mapping = empty_file(name);
    let mut records = name.as_bytes().to_vec();
    records.push(0);
    records.extend_from_slice(&0u64.to_le_bytes());
    records.extend_from_slice(&0u32.to_le_bytes());
    records.extend_from_slice(mapping.sha_content.as_ref().unwrap());
    records.extend_from_slice(mapping.sha_filename.as_ref().unwrap());
    records.extend_from_slice(&0u32.to_le_bytes());
    let mut output = Vec::new();
    for number in [0x16349781u32, 4, spec().depot_id] {
        output.extend_from_slice(&number.to_le_bytes());
    }
    output.extend_from_slice(&spec().manifest_id.to_le_bytes());
    output.extend_from_slice(&[0; 8]);
    output.extend_from_slice(&[0; 16]);
    for number in [0u32, 1, records.len() as u32, 0, 0, 0] {
        output.extend_from_slice(&number.to_le_bytes());
    }
    output.extend_from_slice(&records);
    output.extend_from_slice(&0x16349781u32.to_le_bytes());
    assert_eq!(
        formats::manifest(&output, &spec(), &[0; 32]).unwrap().files[0].path,
        PathBuf::from(name)
    );
    output[4] = 3;
    assert!(formats::manifest(&output, &spec(), &[0; 32]).is_err());
}
#[test]
fn steamkit_encrypted_zip_lzma_and_zstd_vectors_verify() {
    let vectors = [
        (
            "44ce5c5297a415a1a6f69c856037a5a2fdd82cd474fa659edfb4d59b2abc55fc",
            "bac8e2657470b2eb70d6ddcd6c07004be8738697",
            2130218374u32,
            544u32,
            "72b00e726de7cdbaddd1c2d9f8c5df21f8f7407100ef6fd4258bda51015c01096fd2348a0fa175583e942727579817de72caa174cc056df3e2a3da23eb6aec1da67a5fa36253553be71ed316208178a1c2c96e3b2de3360824d6127cf7a3fe1b227996f20a62419b4833053b7a9b09f38a1d40ceda9503dd2a2db73f415839ff04b5ca086a69ed8e5178e7c4dd933f44c15f535a91d2c78182bee93b630b9fdd89e09a8000d39fa88b639cd10dbe94c5ad8df19546199fa1b55e08628f7a4e373b6f525fabdd26ddb39b6663ae8457cc318e9d13e36d481fdb9c525e5c9e56523fe66bb1b7f4c41feb12817d22a7cc4ba2ab823793abfab190b3f788ec36a55ec00563cebe8f2a05892016826cb19946c77254ae402488a60bb6af046c0e5a3826a452dc3bf10f86ca66b76e05c158c27403d002c37d2961696d13f066128a53",
        ),
        (
            "e5f6aed55e9ece429e56b813fbf6bfe924f3cf72972fdbd0571ffcad9f2f7daa",
            "7b8567d9b3c09295cdbf4978c32b348d8e76c750",
            2894626744,
            798,
            "bdd0c894b719592591164f592ea669d690cfadd49012c58ea0c58d82a2f50ae763acf2c4df2abbac2c870d4e3e99106ecf54734fd8a2aab6cb9d090cf9a006dad8deaa586c88b86fd3602c9234dfb185708a452f975bcb86a3f983238ff09209543dc93c1f15fa7f931679d383bbd952a922e38014f91323f220d5ecd4fe83063c8e46bd379817f12e1cb2177791bd9313620f4b1d8d29a79a2a7dafdd4349d39ed3993194d6867447c0b1f25bd5a18dd9115e4e6da0294f272b91dba5ab5ab6681ce47375f428d6bb5510683755e331739267d8612ae53146f61d9dc68b2f19f5093e3724e419915ebe39ca743fa15989f9b7b52f5f59199955759c3bf97350627738201ab58077f2deaa0f299420cf5c517cfdca07c641f1beb509dc8b2d0ff91d81fe220943ceb91ba2315a575024",
        ),
        (
            "0102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f20",
            "9e72678e305540630a665b93e1463bc3983eb55a",
            3753325726,
            156,
            "d5cb9a62c37caa88fda4047446235f0bfd3c204a108e97ddb7b88e7a1aa28307082847f1a04fc35880f32539347cf907f0a08458a91f1ff4e0dce9f9ec01fdedda29ffd15a9cf3a0cfab526edb4d60dcf68910c42286cb26e9ed3e95d6ceab1803545b4e45ac8150b4e585216b7fc7e3d08c8539044bb98d9428cdae1cc58ee6a5b9da9fe7de79a7ef98eac3d5ab84e8e93d3fc16cac4911301ca7696087a8206909f6d9b194c9c1c14d9f1ad878daea",
        ),
    ];
    for (depot_key, id, adler, size, ciphertext) in vectors {
        let data = hex(ciphertext);
        let chunk = DepotChunk {
            id: hex(id).try_into().unwrap(),
            offset: 0,
            uncompressed_size: size,
            compressed_size: data.len() as u32,
            adler,
        };
        let plain = formats::chunk(&data, &chunk, &key(depot_key)).unwrap();
        verify_chunk(&chunk, &plain).unwrap();
        let verified = VerifiedChunk {
            chunk: chunk.clone(),
            bytes: plain.clone(),
        };
        assert_eq!(verified.bytes_for(&chunk).unwrap(), plain.as_slice());
        let mut another_offset = chunk.clone();
        another_offset.offset += 1;
        assert!(verified.bytes_for(&another_offset).is_err());
        let mut damaged = data;
        damaged[32] ^= 0x80;
        assert!(formats::chunk(&damaged, &chunk, &key(depot_key)).is_err());
        let mut wrong = chunk.clone();
        wrong.id[0] ^= 1;
        assert!(verify_chunk(&wrong, &plain).is_err());
        wrong = chunk;
        wrong.adler ^= 1;
        assert!(verify_chunk(&wrong, &plain).is_err());
    }
}
#[test]
fn guards_reject_truncated_aes_and_oversized_chunk_claims() {
    let chunk = DepotChunk {
        id: [0; 20],
        offset: 0,
        uncompressed_size: 1,
        compressed_size: 32,
        adler: 0,
    };
    for len in 0..32 {
        assert!(formats::chunk(&vec![0; len], &chunk, &[0; 32]).is_err());
    }
    let mut large = chunk;
    large.uncompressed_size = MAX_CHUNK as u32 + 1;
    assert!(formats::chunk(&[0; 32], &large, &[0; 32]).is_err());
    assert_eq!(adler32(b""), 0);
    assert_eq!(adler32(b"a"), 0x00610061);
}

#[test]
fn decompression_bombs_are_rejected_before_dictionary_allocation() {
    let mut vzip = b"VZa".to_vec();
    vzip.extend_from_slice(&[0; 4]);
    vzip.push(93);
    vzip.extend_from_slice(&u32::MAX.to_le_bytes());
    vzip.extend_from_slice(&[0; 4]);
    vzip.extend_from_slice(&1u32.to_le_bytes());
    vzip.extend_from_slice(b"zv");
    assert!(
        formats::vzip(&vzip, 1)
            .unwrap_err()
            .to_string()
            .contains("dictionary")
    );
    // Non-single-segment Zstd frame: window exponent 31 means 2^41 bytes.
    assert!(formats::zstd_window(&[0x28, 0xb5, 0x2f, 0xfd, 0, 0xf8]).is_err());
    let mut single = vec![0x28, 0xb5, 0x2f, 0xfd, 0xe0];
    single.extend_from_slice(&u64::MAX.to_le_bytes());
    assert!(formats::zstd_window(&single).is_err());
}
