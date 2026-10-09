//! Typed JSONL scan/copy engine shared by the standalone app and legacy test adapter.
//! No Codex state is mutated by `scan`; `copy` only creates new staging files.
use base64::{Engine, engine::general_purpose::STANDARD};
use memchr::{memchr, memrchr};
use rayon::prelude::*;

use serde::{
    Deserialize, Deserializer,
    de::{Error, MapAccess, Visitor},
};
use serde_json::{Value, json, value::RawValue};
use std::{
    collections::HashMap,
    fs::File,
    io::{BufWriter, Write},
};

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
const BUFFER: usize = 8 * 1024 * 1024;

struct UniqueObject<'a>(HashMap<String, &'a RawValue>);
impl<'de> Deserialize<'de> for UniqueObject<'de> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        struct ObjectVisitor;
        impl<'de> Visitor<'de> for ObjectVisitor {
            type Value = UniqueObject<'de>;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("an object with unique fields")
            }
            fn visit_map<A: MapAccess<'de>>(
                self,
                mut access: A,
            ) -> std::result::Result<Self::Value, A::Error> {
                let mut values = HashMap::new();
                while let Some((key, value)) = access.next_entry::<String, &RawValue>()? {
                    if values.insert(key, value).is_some() {
                        return Err(A::Error::custom("Duplicate migration field"));
                    }
                }
                Ok(UniqueObject(values))
            }
        }
        deserializer.deserialize_map(ObjectVisitor)
    }
}

fn field<'a>(raw: &'a str, keys: &[&str]) -> Result<&'a str> {
    let mut current = raw;
    for key in keys {
        let object: UniqueObject = serde_json::from_str(current)?;
        current = object.0.get(*key).ok_or("Missing migration field")?.get();
    }
    Ok(current)
}

fn patch(raw: &str, keys: &[&str], value: Value, offset: usize) -> Result<Patch> {
    let old = field(raw, keys)?;
    let start = old.as_ptr() as usize - raw.as_ptr() as usize + offset;
    Ok(Patch {
        start: start as u64,
        end: (start + old.len()) as u64,
        old: old.as_bytes().to_vec(),
        new: serde_json::to_vec(&value)?,
    })
}

pub fn scan(path: &str, provider: Option<&str>) -> Result<Scan> {
    let file = File::open(path)?;
    // Read-only map of an offline rollout. The orchestrator checks size/mtime again before writing.
    let data = unsafe { memmap2::MmapOptions::new().map(&file)? };
    if data.last() != Some(&b'\n') {
        return Err("Incomplete rollout".into());
    }
    // Referenced paginated histories need their lineage header, not a provider scan.
    let Some(provider) = provider else {
        let end = memchr(b'\n', &data).ok_or("Incomplete header")? + 1;
        return scan_range(&data[..end], 0, None);
    };
    let block = 8 * 1024 * 1024;
    if data.len() < 64 * 1024 * 1024 {
        return scan_range(&data, 0, Some(provider));
    }
    scan_partitioned(&data, provider, block)
}

fn scan_partitioned(data: &[u8], provider: &str, block: usize) -> Result<Scan> {
    if data.last() != Some(&b'\n') {
        return Err("Incomplete rollout".into());
    }
    if data.len() <= block || rayon::current_num_threads() == 1 {
        return scan_range(data, 0, Some(provider));
    }
    let cuts = record_cuts(data, block)?;
    let parts: Vec<Scan> = cuts
        .par_windows(2)
        .map(|range| scan_range(&data[range[0]..range[1]], range[0], Some(provider)))
        .collect::<Result<_>>()?;
    let mut combined = Scan {
        patches: Vec::with_capacity(parts.iter().map(|p| p.patches.len()).sum()),
        bases: Vec::with_capacity(parts.iter().map(|p| p.bases.len()).sum()),
    };
    for mut p in parts {
        combined.patches.append(&mut p.patches);
        combined.bases.append(&mut p.bases);
    }
    Ok(combined)
}

// Shared by provider-field scanning and ordinal recovery.
pub(crate) fn record_cuts(data: &[u8], block: usize) -> Result<Vec<usize>> {
    // Split only after complete records. A large single record stays in one job;
    // jumping past its newline avoids repeatedly searching through that record.
    let mut cuts = vec![0usize];
    while let Some(at) = cuts
        .last()
        .unwrap()
        .checked_add(block)
        .filter(|at| *at < data.len())
    {
        cuts.push(at + memchr(b'\n', &data[at..]).ok_or("Incomplete rollout")? + 1);
    }
    if cuts.last() != Some(&data.len()) {
        cuts.push(data.len());
    }
    Ok(cuts)
}

fn scan_range(data: &[u8], offset: usize, provider: Option<&str>) -> Result<Scan> {
    static MARKER: std::sync::OnceLock<regex::bytes::Regex> = std::sync::OnceLock::new();
    let marker = MARKER.get_or_init(|| {
        regex::bytes::Regex::new(r#""(?:session_meta|thread_settings_applied)""#).unwrap()
    });
    let mut previous_end = 0;
    let mut changes = Vec::new();
    let mut bases = Vec::new();
    for found in marker.find_iter(data) {
        let at = found.start();
        if at < previous_end {
            continue;
        }
        let start = memrchr(b'\n', &data[..at]).map_or(0, |i| i + 1);
        let end = at + memchr(b'\n', &data[at..]).ok_or("Incomplete record")? + 1;
        previous_end = end;
        let raw = std::str::from_utf8(&data[start..end])?;
        let start = offset + start;
        let record: Value = serde_json::from_str(raw)?;
        let payload = &record["payload"];
        if record["type"] == "session_meta" {
            if let Some(provider) = provider.filter(|p| payload["model_provider"] != *p) {
                if payload.get("model_provider").is_none() {
                    let part = field(raw, &["payload"])?;
                    let at = part.as_ptr() as usize - raw.as_ptr() as usize + start + 1;
                    let comma = if payload.as_object().is_some_and(|p| !p.is_empty()) {
                        ","
                    } else {
                        ""
                    };
                    let insert = format!(
                        "\"model_provider\":{}{comma}",
                        serde_json::to_string(provider)?
                    );
                    changes.push(Patch {
                        start: at as u64,
                        end: at as u64,
                        old: vec![],
                        new: insert.into_bytes(),
                    });
                } else {
                    changes.push(patch(
                        raw,
                        &["payload", "model_provider"],
                        json!(provider),
                        start,
                    )?);
                }
            }
            if let Some(base) = payload.get("history_base").filter(|v| !v.is_null()) {
                bases.push(Base {
                    physical: text(&base["thread_id"])?.to_owned(),
                    offset: number(&base["end_byte_offset"])?,
                    ordinal: base["end_ordinal_exclusive"]
                        .as_i64()
                        .ok_or("Invalid ordinal")?,
                    patch: patch(
                        raw,
                        &["payload", "history_base", "end_byte_offset"],
                        base["end_byte_offset"].clone(),
                        start,
                    )?,
                });
            }
        } else if record["type"] == "event_msg"
            && payload["type"] == "thread_settings_applied"
            && let Some(provider) = provider
        {
            let settings = &payload["thread_settings"];
            if settings.get("model_provider_id").is_none() {
                return Err("Unknown thread settings schema".into());
            }
            if settings["model_provider_id"] != provider {
                changes.push(patch(
                    raw,
                    &["payload", "thread_settings", "model_provider_id"],
                    json!(provider),
                    start,
                )?);
            }
        }
    }
    Ok(Scan {
        patches: changes,
        bases,
    })
}

fn number(v: &Value) -> Result<u64> {
    v.as_u64().ok_or_else(|| "Invalid byte offset".into())
}
fn text(v: &Value) -> Result<&str> {
    v.as_str().ok_or_else(|| "Missing string".into())
}

pub fn copy(path: &str, temp: &str, patches: &[Patch], parallel_threshold: u64) -> Result<()> {
    let src = File::open(path)?;
    let raw = unsafe { memmap2::MmapOptions::new().map(&src)? };
    let mut end = 0;
    for p in patches {
        if p.start < end
            || p.end < p.start
            || p.end > raw.len() as u64
            || raw[p.start as usize..p.end as usize] != p.old
        {
            return Err("Invalid or changed copy patch".into());
        }
        end = p.end;
    }
    let mapped = rayon::current_num_threads() > 1 && raw.len() as u64 >= parallel_threshold;
    if mapped {
        return copy_mapped(&src, &raw, temp, patches);
    }
    let mut dst = BufWriter::with_capacity(
        128 * 1024,
        crate::platform::staging_file(std::path::Path::new(temp), false)?,
    );
    let mut at = 0;
    for p in patches {
        for chunk in raw[at..p.start as usize].chunks(BUFFER) {
            dst.write_all(chunk)?;
        }
        dst.write_all(&p.new)?;
        at = p.end as usize;
    }
    for chunk in raw[at..].chunks(BUFFER) {
        dst.write_all(chunk)?;
    }
    dst.flush()?;
    crate::platform::finish_staging(&src, dst.get_ref())?;
    Ok(())
}

fn copy_mapped(source: &File, raw: &[u8], temp: &str, patches: &[Patch]) -> Result<()> {
    let mut spans = Vec::with_capacity(patches.len() * 2 + 1);
    let mut ends = Vec::with_capacity(patches.len() * 2 + 1);
    let mut length = 0usize;
    let mut push = |bytes| {
        let bytes: &[u8] = bytes;
        if !bytes.is_empty() {
            length += bytes.len();
            spans.push(bytes);
            ends.push(length);
        }
    };
    let mut at = 0;
    for p in patches {
        push(&raw[at..p.start as usize]);
        push(&p.new);
        at = p.end as usize;
    }
    push(&raw[at..]);
    let file = crate::platform::staging_file(std::path::Path::new(temp), true)?;
    file.set_len(length as u64)?;
    if length == 0 {
        crate::platform::finish_staging(source, &file)?;
        return Ok(());
    }
    // Exclusive newly-created staging file; mutable chunks do not overlap.
    let mut dst = unsafe { memmap2::MmapOptions::new().map_mut(&file)? };
    dst.par_chunks_mut(BUFFER)
        .enumerate()
        .for_each(|(i, output)| {
            let mut at = i * BUFFER;
            let mut span = ends.partition_point(|end| *end <= at);
            let mut written = 0;
            while written < output.len() {
                let start = if span == 0 { 0 } else { ends[span - 1] };
                let count = (ends[span] - at).min(output.len() - written);
                output[written..written + count]
                    .copy_from_slice(&spans[span][at - start..at - start + count]);
                at += count;
                written += count;
                span += 1;
            }
        });
    dst.flush()?;
    crate::platform::finish_staging(source, &file)?;
    Ok(())
}

pub(crate) mod bytes {
    use super::*;
    pub fn serialize<S: serde::Serializer>(v: &[u8], s: S) -> std::result::Result<S::Ok, S::Error> {
        s.serialize_str(&STANDARD.encode(v))
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> std::result::Result<Vec<u8>, D::Error> {
        STANDARD
            .decode(String::deserialize(d)?)
            .map_err(D::Error::custom)
    }
}
#[derive(Clone, Debug, serde::Serialize, Deserialize)]
pub struct Patch {
    pub start: u64,
    pub end: u64,
    #[serde(with = "bytes")]
    pub old: Vec<u8>,
    #[serde(with = "bytes")]
    pub new: Vec<u8>,
}
#[derive(Clone, Debug, serde::Serialize, Deserialize)]
pub struct Base {
    pub physical: String,
    pub offset: u64,
    pub ordinal: i64,
    pub patch: Patch,
}
#[derive(Clone, Debug, serde::Serialize, Deserialize)]
pub struct Scan {
    pub patches: Vec<Patch>,
    pub bases: Vec<Base>,
}

#[cfg(test)]
mod partition_tests {
    use super::*;
    #[test]
    fn every_boundary_matches_serial_scan() {
        let raw = concat!(
            "{\"type\":\"session_meta\",\"payload\":{\"id\":\"root\",\"model_provider\":\"openai\",\"note\":\"Русское имя\"}}\r\n",
            "{\"type\":\"event_msg\",\"payload\":{\"type\":\"thread_settings_applied\",\"thread_settings\":{\"model_provider_id\":\"openai\"},\"other\":\"session_meta\"}}\n",
            "{\"type\":\"session_meta\",\"payload\":{\"id\":\"child\",\"history_base\":{\"thread_id\":\"root\",\"end_byte_offset\":12,\"end_ordinal_exclusive\":2}}}\n"
        ).as_bytes();
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(4)
            .build()
            .unwrap();
        for provider in ["factory", "openai", "second"] {
            let expected =
                serde_json::to_value(scan_range(raw, 0, Some(provider)).unwrap()).unwrap();
            for block in 1..=raw.len() + 1 {
                let actual = pool
                    .install(|| scan_partitioned(raw, provider, block))
                    .unwrap();
                assert_eq!(
                    serde_json::to_value(actual).unwrap(),
                    expected,
                    "block {block}"
                );
            }
        }
        assert!(scan_partitioned(&raw[..raw.len() - 1], "factory", 32).is_err());
        let bad = b"{\"type\":\"event_msg\",\"payload\":{\"type\":\"thread_settings_applied\",\"thread_settings\":{}}}\n";
        assert!(scan_partitioned(bad, "factory", 1).is_err());
    }
}
