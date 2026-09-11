//! Integrity test for the pinned naming development corpus.
//!
//! The corpus is development evidence reduced from Sonarr/Radarr parser
//! fixtures. This test validates the corpus and its provenance. It does **not**
//! compare Nightjar output against the upstream expectations; the upstream
//! fixtures assert another parser's schema, so agreement is not ground truth.
//!
//! The validation runs in memory so the mutation controls can prove each guard
//! fires. Every control stays inside a declared local budget: the corpus file,
//! the case count and every input have hard bounds below, and the test reads no
//! network and spawns no process.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

const SCHEMA_VERSION: i64 = 1;
const SET: &str = "development";
const SET_ROOTS: [&str; 4] = ["regression", "development", "heldout", "stress"];
const CASE_FIELDS: [&str; 9] = [
    "id",
    "source",
    "test",
    "input",
    "expect",
    "dropped",
    "applicable",
    "reason",
    "category",
];
/// Local test budget. The checked-in corpus is far under every bound.
const MAX_CASE_COUNT: usize = 2000;
const MAX_INPUT_BYTES: usize = 1024;
const MAX_CORPUS_BYTES: u64 = 4 * 1024 * 1024;

fn naming_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/naming")
}

fn development_dir() -> PathBuf {
    naming_dir().join("development")
}

fn upstream_dir() -> PathBuf {
    development_dir().join("upstream")
}

fn read_json(path: &Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

/// SHA-256, hand-rolled because the core crate has no hash dependency and this
/// test must not add one. It is checked against the standard vectors below.
fn sha256_hex(data: &[u8]) -> String {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    let mut message = data.to_vec();
    let bit_len = (data.len() as u64).wrapping_mul(8);
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&bit_len.to_be_bytes());

    for chunk in message.as_chunks::<64>().0 {
        let mut w = [0u32; 64];
        for (word, bytes) in w.iter_mut().zip(chunk.as_chunks::<4>().0) {
            *word = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let (mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh) =
            (h[0], h[1], h[2], h[3], h[4], h[5], h[6], h[7]);
        for (&k, &wi) in K.iter().zip(w.iter()) {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let t1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(k)
                .wrapping_add(wi);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
        h[5] = h[5].wrapping_add(f);
        h[6] = h[6].wrapping_add(g);
        h[7] = h[7].wrapping_add(hh);
    }
    h.iter().map(|word| format!("{word:08x}")).collect()
}

fn declared_sources(sources: &Value) -> Result<BTreeMap<String, String>, String> {
    let mut declared = BTreeMap::new();
    let list = sources["sources"]
        .as_array()
        .ok_or("SOURCES.json has no sources")?;
    for entry in list {
        let file = entry["file"].as_str().ok_or("source entry lacks file")?;
        let hash = entry["sha256"]
            .as_str()
            .ok_or("source entry lacks sha256")?;
        if declared
            .insert(file.to_string(), hash.to_string())
            .is_some()
        {
            return Err(format!("duplicate declared source: {file}"));
        }
    }
    Ok(declared)
}

/// Validate the corpus against its manifest and its four set roots.
///
/// Returns the first failure. Every guard has a mutation control below.
fn validate(
    corpus_text: &str,
    sources: &Value,
    upstream: &Path,
    naming: &Path,
) -> Result<(), String> {
    let corpus: Value =
        serde_json::from_str(corpus_text).map_err(|e| format!("malformed JSON: {e}"))?;
    if corpus["schema_version"].as_i64() != Some(SCHEMA_VERSION) {
        return Err(format!("schema_version is not {SCHEMA_VERSION}"));
    }
    if corpus["set"].as_str() != Some(SET) {
        return Err(format!("set is not {SET}"));
    }
    let cases = corpus["cases"].as_array().ok_or("cases is not a list")?;
    if cases.len() > MAX_CASE_COUNT {
        return Err(format!(
            "{} cases over the {MAX_CASE_COUNT} budget",
            cases.len()
        ));
    }

    let declared = declared_sources(sources)?;
    let declared_files: BTreeSet<String> = declared.keys().cloned().collect();
    let mut present: BTreeSet<String> = BTreeSet::new();
    for entry in std::fs::read_dir(upstream).map_err(|e| format!("upstream dir: {e}"))? {
        let entry = entry.map_err(|e| format!("upstream dir: {e}"))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.ends_with(".cs") {
            present.insert(name);
        }
    }
    if let Some(file) = present.difference(&declared_files).next() {
        return Err(format!("undeclared source: {file}"));
    }
    if let Some(file) = declared_files.difference(&present).next() {
        return Err(format!("missing source: {file}"));
    }
    for (file, want) in &declared {
        let bytes = std::fs::read(upstream.join(file)).map_err(|e| format!("read {file}: {e}"))?;
        if &sha256_hex(&bytes) != want {
            return Err(format!("hash mismatch: {file}"));
        }
    }

    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut applicable = 0usize;
    let mut categories: BTreeMap<String, u64> = BTreeMap::new();
    for case in cases {
        let id = case["id"].as_str().unwrap_or("<missing id>");
        for field in CASE_FIELDS {
            if case.get(field).is_none() {
                return Err(format!("case {id} lacks field {field}"));
            }
        }
        if !seen.insert(id.to_string()) {
            return Err(format!("duplicate id: {id}"));
        }
        let source = case["source"].as_str().unwrap_or("");
        if !declared_files.contains(source) {
            return Err(format!("case {id} names an undeclared source {source}"));
        }
        if case["applicable"].as_bool() == Some(true) {
            applicable += 1;
        }
        let category = case["category"].as_str().unwrap_or("");
        *categories.entry(category.to_string()).or_default() += 1;
        let input = case["input"]
            .as_str()
            .ok_or_else(|| format!("case {id} input is not a string"))?;
        if input.len() > MAX_INPUT_BYTES {
            return Err(format!(
                "case {id} input is {} bytes, over {MAX_INPUT_BYTES}",
                input.len()
            ));
        }
    }

    let counts = &corpus["counts"];
    if counts["total"].as_u64() != Some(cases.len() as u64) {
        return Err(format!(
            "count drift: total {} != {}",
            counts["total"],
            cases.len()
        ));
    }
    if counts["applicable"].as_u64() != Some(applicable as u64) {
        return Err(format!(
            "count drift: applicable {} != {applicable}",
            counts["applicable"]
        ));
    }
    if counts["excluded"].as_u64() != Some((cases.len() - applicable) as u64) {
        return Err(format!(
            "count drift: excluded {} != {}",
            counts["excluded"],
            cases.len() - applicable
        ));
    }
    let want_categories: BTreeMap<String, u64> = categories;
    let got_categories: BTreeMap<String, u64> = corpus["categories"]
        .as_object()
        .ok_or("categories is not an object")?
        .iter()
        .map(|(k, v)| (k.clone(), v.as_u64().unwrap_or(0)))
        .collect();
    if got_categories != want_categories {
        return Err("count drift: category counts do not match the cases".into());
    }
    let listed: BTreeSet<String> = corpus["sources"]
        .as_array()
        .ok_or("sources is not a list")?
        .iter()
        .filter_map(|v| v.as_str().map(String::from))
        .collect();
    if listed != declared_files {
        return Err("corpus source list does not match the manifest".into());
    }

    let mut assigned: BTreeMap<String, String> = BTreeMap::new();
    for root in SET_ROOTS {
        let dir = naming.join(root);
        if !dir.is_dir() {
            return Err(format!("set root missing: {root}"));
        }
        for entry in std::fs::read_dir(&dir).map_err(|e| format!("set root {root}: {e}"))? {
            let entry = entry.map_err(|e| format!("set root {root}: {e}"))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if !name.ends_with(".json") {
                continue;
            }
            let text = std::fs::read_to_string(entry.path())
                .map_err(|e| format!("read {root}/{name}: {e}"))?;
            let other: Value = serde_json::from_str(&text)
                .map_err(|e| format!("set {root} holds malformed {name}: {e}"))?;
            for case in other["cases"].as_array().into_iter().flatten() {
                if let Some(cid) = case["id"].as_str()
                    && let Some(previous) = assigned.insert(cid.to_string(), root.to_string())
                {
                    return Err(format!("case {cid} is in both {previous} and {root}"));
                }
            }
        }
    }
    Ok(())
}

fn fixture() -> (String, Value) {
    let text = std::fs::read_to_string(development_dir().join("corpus.json")).unwrap();
    let sources = read_json(&upstream_dir().join("SOURCES.json"));
    (text, sources)
}

#[test]
fn sha256_matches_known_vectors() {
    assert_eq!(
        sha256_hex(b""),
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
    assert_eq!(
        sha256_hex(b"abc"),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
    assert_eq!(
        sha256_hex(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
        "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
    );
}

#[test]
fn development_corpus_is_intact() {
    let (text, sources) = fixture();
    validate(&text, &sources, &upstream_dir(), &naming_dir()).unwrap();
}

#[test]
fn corpus_stays_within_local_budget() {
    let path = development_dir().join("corpus.json");
    let bytes = std::fs::metadata(&path).unwrap().len();
    assert!(
        bytes <= MAX_CORPUS_BYTES,
        "corpus is {bytes} bytes, over {MAX_CORPUS_BYTES}"
    );
    let corpus = read_json(&path);
    let cases = corpus["cases"].as_array().unwrap().len();
    assert!(
        cases <= MAX_CASE_COUNT,
        "{cases} cases over {MAX_CASE_COUNT}"
    );
}

#[test]
fn malformed_json_is_rejected() {
    let (_, sources) = fixture();
    let err = validate("{ not json", &sources, &upstream_dir(), &naming_dir()).unwrap_err();
    assert!(err.contains("malformed JSON"), "{err}");
}

#[test]
fn duplicate_id_is_rejected() {
    let (text, sources) = fixture();
    let mut corpus: Value = serde_json::from_str(&text).unwrap();
    let first = corpus["cases"][0]["id"].clone();
    corpus["cases"][1]["id"] = first;
    let err = validate(
        &corpus.to_string(),
        &sources,
        &upstream_dir(),
        &naming_dir(),
    )
    .unwrap_err();
    assert!(err.contains("duplicate id"), "{err}");
}

#[test]
fn count_drift_is_rejected() {
    let (text, sources) = fixture();
    let mut corpus: Value = serde_json::from_str(&text).unwrap();
    let applicable = corpus["counts"]["applicable"].as_u64().unwrap();
    corpus["counts"]["applicable"] = json!(applicable + 1);
    let err = validate(
        &corpus.to_string(),
        &sources,
        &upstream_dir(),
        &naming_dir(),
    )
    .unwrap_err();
    assert!(err.contains("count drift"), "{err}");
}

#[test]
fn missing_source_is_rejected() {
    let (text, sources) = fixture();
    let mut sources = sources;
    sources["sources"]
        .as_array_mut()
        .unwrap()
        .push(json!({"file": "ghost.cs", "sha256": "00"}));
    let err = validate(&text, &sources, &upstream_dir(), &naming_dir()).unwrap_err();
    assert!(err.contains("missing source: ghost.cs"), "{err}");
}

#[test]
fn undeclared_source_is_rejected() {
    let (text, sources) = fixture();
    let mut sources = sources;
    let first = sources["sources"].as_array_mut().unwrap().remove(0);
    let file = first["file"].as_str().unwrap();
    let err = validate(&text, &sources, &upstream_dir(), &naming_dir()).unwrap_err();
    assert!(err.contains(&format!("undeclared source: {file}")), "{err}");
}

#[test]
fn changed_source_byte_is_rejected() {
    let (text, sources) = fixture();
    let mut sources = sources;
    sources["sources"][0]["sha256"] = json!("0".repeat(64));
    let err = validate(&text, &sources, &upstream_dir(), &naming_dir()).unwrap_err();
    assert!(err.contains("hash mismatch"), "{err}");
}

#[test]
fn oversized_input_is_rejected() {
    let (text, sources) = fixture();
    let mut corpus: Value = serde_json::from_str(&text).unwrap();
    corpus["cases"][0]["input"] = json!("x".repeat(MAX_INPUT_BYTES + 1));
    let err = validate(
        &corpus.to_string(),
        &sources,
        &upstream_dir(),
        &naming_dir(),
    )
    .unwrap_err();
    assert!(err.contains(&format!("over {MAX_INPUT_BYTES}")), "{err}");
}

#[test]
fn missing_set_root_is_rejected() {
    let (text, sources) = fixture();
    let err = validate(
        &text,
        &sources,
        &upstream_dir(),
        &naming_dir().join("does-not-exist"),
    )
    .unwrap_err();
    assert!(err.contains("set root missing"), "{err}");
}

#[test]
fn case_in_two_sets_is_rejected() {
    let (text, sources) = fixture();
    let tmp = std::env::temp_dir().join(format!("nightjar-naming-{}", std::process::id()));
    std::fs::create_dir_all(&tmp).unwrap();
    for root in SET_ROOTS {
        std::fs::create_dir_all(tmp.join(root)).unwrap();
    }
    std::fs::copy(
        development_dir().join("corpus.json"),
        tmp.join("development").join("corpus.json"),
    )
    .unwrap();
    std::fs::copy(
        development_dir().join("corpus.json"),
        tmp.join("regression").join("corpus.json"),
    )
    .unwrap();
    let result = validate(&text, &sources, &upstream_dir(), &tmp);
    std::fs::remove_dir_all(&tmp).unwrap();
    let err = result.unwrap_err();
    assert!(err.contains("is in both"), "{err}");
}
