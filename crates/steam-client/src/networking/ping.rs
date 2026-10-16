//! Ping-location text and marker semantics.
use super::{pop_code, pop_id};

pub(super) const SIZE: usize = 512;
const MAX_MARKERS: usize = 12;
const MAX_PING: i32 = 5000;
const NO_DIRECT: i32 = i16::MAX as i32;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Sample {
    pub ping: i32,
    pub penalty: i32,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Marker {
    pub id: u32,
    pub direct: Sample,
    pub routed: Sample,
}
impl Marker {
    pub fn measured(id: u32, direct: i32, routed: i32) -> Self {
        Self {
            id,
            direct: Sample {
                ping: if (0..=MAX_PING).contains(&direct) {
                    direct
                } else {
                    NO_DIRECT
                },
                penalty: 0,
            },
            routed: Sample {
                ping: routed,
                penalty: 0,
            },
        }
    }
}

pub(super) fn encode(markers: &[Marker]) -> [u8; SIZE] {
    // Best routed markers, with common intercontinental anchors retained where
    // possible, then sort by POP id for the merge used by EstimatePingTime.
    let mut unique = std::collections::BTreeMap::new();
    for marker in markers {
        unique.entry(marker.id).or_insert(*marker);
    }
    let mut markers: Vec<_> = unique.into_values().collect();
    markers.sort_by_key(|m| {
        (
            m.routed.ping + m.routed.penalty,
            m.direct.ping + m.direct.penalty,
            m.id,
        )
    });
    markers.dedup_by_key(|m| m.id);
    if markers.len() > MAX_MARKERS {
        let mut selected: Vec<_> = markers.iter().take(4).copied().collect();
        for code in ["fra", "iad", "gru", "sgp"] {
            if let Some(m) = markers.iter().find(|m| Some(m.id) == pop_id(code)) {
                if !selected.iter().any(|s| s.id == m.id) {
                    selected.push(*m);
                }
            }
        }
        for m in markers {
            if selected.len() == MAX_MARKERS {
                break;
            }
            if !selected.iter().any(|s| s.id == m.id) {
                selected.push(m);
            }
        }
        markers = selected;
    }
    markers.sort_by_key(|m| m.id);
    let mut blob = [0; SIZE];
    for (m, chunk) in markers.iter().zip(blob.chunks_exact_mut(12)) {
        chunk[..4].copy_from_slice(&m.id.to_le_bytes());
        for (value, field) in [
            m.direct.ping,
            m.direct.penalty,
            m.routed.ping,
            m.routed.penalty,
        ]
        .iter()
        .zip(chunk[4..].chunks_exact_mut(2))
        {
            field.copy_from_slice(&(*value as i16).to_le_bytes());
        }
    }
    blob
}
pub(super) fn decode(blob: &[u8]) -> Vec<Marker> {
    let mut markers = Vec::new();
    for chunk in blob.chunks_exact(12).take(MAX_MARKERS) {
        let id = u32::from_le_bytes(chunk[..4].try_into().unwrap());
        if id == 0 {
            break;
        }
        let value = |at| i32::from(i16::from_le_bytes([chunk[at], chunk[at + 1]]));
        let m = Marker {
            id,
            direct: Sample {
                ping: value(4),
                penalty: value(6),
            },
            routed: Sample {
                ping: value(8),
                penalty: value(10),
            },
        };
        if pop_id(&pop_code(id)) != Some(id)
            || !valid(m.routed)
            || !(valid(m.direct)
                || m.direct
                    == (Sample {
                        ping: NO_DIRECT,
                        penalty: 0,
                    }))
            || markers.iter().any(|m: &Marker| m.id == id)
        {
            return Vec::new();
        }
        markers.push(m);
    }
    markers
}
fn valid(s: Sample) -> bool {
    (0..=MAX_PING).contains(&s.ping) && (-9999..=9999).contains(&s.penalty)
}
fn sample(text: &str) -> Option<Sample> {
    let text = text.trim();
    let split = text
        .char_indices()
        .skip(1)
        .find(|(_, c)| *c == '+' || *c == '-')
        .map(|(i, _)| i);
    let (ping, penalty) = split.map_or((text, "0"), |i| text.split_at(i));
    let s = Sample {
        ping: ping.trim().parse().ok()?,
        penalty: penalty.trim().parse().ok()?,
    };
    valid(s).then_some(s)
}
pub(super) fn parse(text: &str) -> Option<[u8; SIZE]> {
    if text.is_empty() || text.len() >= 1024 {
        return None;
    }
    let mut markers = Vec::new();
    for entry in text.split(',') {
        let (code, values) = entry.trim().split_once('=')?;
        let id = pop_id(code)?;
        let (direct, routed) = if let Some((d, r)) = values.split_once('/') {
            (
                if d.trim().is_empty() {
                    Sample {
                        ping: NO_DIRECT,
                        penalty: 0,
                    }
                } else {
                    sample(d)?
                },
                sample(r)?,
            )
        } else {
            let s = sample(values)?;
            (s, s)
        };
        // Steam ignores repeat markers, retaining the first one.
        if !markers.iter().any(|m: &Marker| m.id == id) {
            markers.push(Marker { id, direct, routed });
        }
    }
    (!markers.is_empty()).then(|| encode(&markers))
}
pub(super) fn format(markers: &[Marker]) -> String {
    let mut markers = markers.to_vec();
    markers.sort_by_key(|m| {
        (
            m.routed.ping,
            m.routed.penalty,
            m.direct.ping,
            m.direct.penalty,
            m.id,
        )
    });
    let sample = |s: Sample| format!("{}{:+}", s.ping, s.penalty);
    markers
        .iter()
        .map(|m| {
            let values = if m.direct.ping == NO_DIRECT {
                format!("/{}", sample(m.routed))
            } else if m.direct == m.routed {
                sample(m.direct)
            } else {
                format!("{}/{}", sample(m.direct), sample(m.routed))
            };
            format!("{}={values}", pop_code(m.id))
        })
        .collect::<Vec<_>>()
        .join(",")
}
pub(super) fn estimate(a: &[Marker], b: &[Marker]) -> i32 {
    let mut best = None;
    for a in a {
        for b in b.iter().filter(|b| b.id == a.id) {
            for (d, r) in [(a.direct, b.routed), (b.direct, a.routed)] {
                if d.ping == NO_DIRECT {
                    continue;
                }
                let ping = d.ping + r.ping;
                let score = ping + d.penalty + r.penalty;
                if best.is_none_or(|(old, _)| score < old) {
                    best = Some((score, ping));
                }
            }
        }
    }
    best.map_or(-2, |(_, ping)| ping)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn valve_markers_roundtrip_and_estimates() {
        let blob = parse("ams=21+3,fra=40+9/19+2,sto2=/35+4").unwrap();
        assert_eq!(parse(&format(&decode(&blob))), Some(blob));
        let b = decode(&parse("ams=5+0,fra=8+1,sto2=10+0").unwrap());
        assert_eq!(estimate(&decode(&blob), &b), 26);
        assert_eq!(estimate(&b, &decode(&blob)), 26);
        assert_eq!(
            estimate(
                &decode(&parse("ams=/20+0").unwrap()),
                &decode(&parse("ams=/30+0").unwrap())
            ),
            -2
        );
        // Variance selects the route, but is not added to the returned RTT.
        assert_eq!(
            estimate(&decode(&parse("ams=10+100,fra=30+0").unwrap()), &b),
            38
        );
    }
    #[test]
    fn invalid_and_edge_inputs() {
        for input in [
            "",
            "ams=-1",
            "ams=5001",
            "ams=10+10000",
            "ams=3/-1",
            "ams=10/",
            "am=2",
            "a_s=3",
            "ams=3,",
            "ams=3/4/5",
            "ams=1e2",
        ] {
            assert!(parse(input).is_none(), "{input}");
        }
        assert!(parse("ams=0-9999,sto2=5000+9999").is_some());
        assert_eq!(decode(&parse("ams=10,ams=20").unwrap()).len(), 1);
        let entries: Vec<_> = (0..30)
            .map(|i| {
                Marker::measured(
                    pop_id(&format!("ab{}", char::from(b'a' + i % 26))).unwrap(),
                    i32::from(i),
                    i32::from(i),
                )
            })
            .collect();
        assert_eq!(decode(&encode(&entries)).len(), 12);
        assert_eq!(estimate(&[], &entries), -2);
    }
}
