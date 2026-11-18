//! Reproducible ingestion; this module is never linked into the input engine.
use super::*;
use kanade_emoji::{Data, Hit, LIMIT, MAX_KEY_BYTES, identity, normalize};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Deserialize)]
struct Lock {
    unicode: String,
    files: BTreeMap<String, Source>,
}
#[derive(Deserialize)]
struct Source {
    url: String,
    sha256: String,
    bytes: u64,
}
#[derive(Serialize, Deserialize)]
struct Manifest {
    sha256: String,
    entries: usize,
    readings: usize,
    source_hits: [usize; 6],
}

fn lock(root: &Path) -> Result<Lock> {
    Ok(serde_json::from_slice(&fs::read(
        root.join("locks/emoji.lock.json"),
    )?)?)
}
fn target(root: &Path) -> PathBuf {
    root.join("crates/kanade-emoji/data")
}
fn dictionary_digest(root: &Path) -> Result<String> {
    let lock: toml::Value = toml::from_str(&fs::read_to_string(
        root.join("locks/dictionary.lock.toml"),
    )?)?;
    Ok(lock["dictionary"]["sha256"]
        .as_str()
        .context("dictionary digest missing")?
        .to_owned())
}

pub fn verify(root: &Path) -> Result<()> {
    let folder = target(root);
    verify_at(root, &folder)
}
fn verify_at(root: &Path, folder: &Path) -> Result<()> {
    let data: Data = serde_json::from_slice(&fs::read(folder.join("catalog.json"))?)?;
    let manifest: Manifest = serde_json::from_slice(&fs::read(folder.join("manifest.json"))?)?;
    let lock = lock(root)?;
    let dictionary_lock: toml::Value = toml::from_str(&fs::read_to_string(
        root.join("locks/dictionary.lock.toml"),
    )?)?;
    ensure!(
        hash(&folder.join("notices/IPAdic-and-hasami-NOTICE.md"))?
            == dictionary_lock["dictionary"]["notices_sha256"]
                .as_str()
                .context("dictionary notice digest missing")?,
        "emoji dictionary notice differs"
    );
    ensure!(
        hash(&folder.join("catalog.json"))? == manifest.sha256,
        "emoji catalogue digest differs"
    );
    ensure!(
        data.format == 1 && data.unicode == lock.unicode,
        "emoji catalogue format differs"
    );
    ensure!(
        data.dictionary_sha256 == dictionary_digest(root)?,
        "emoji reading dictionary pin differs"
    );
    ensure!(
        data.entries.len() == manifest.entries && data.readings.len() == manifest.readings,
        "emoji catalogue counts differ"
    );
    ensure!(
        manifest.source_hits.iter().all(|n| *n > 100),
        "emoji source coverage is incomplete"
    );
    for (name, source) in lock.files {
        ensure!(
            data.sources.get(&name) == Some(&source.sha256),
            "emoji source pin differs"
        );
        if name.ends_with("LICENSE.txt") {
            ensure!(
                hash(&folder.join("notices").join(name))? == source.sha256,
                "emoji license digest differs"
            );
        }
    }
    let mut identities = std::collections::BTreeSet::new();
    ensure!(
        data.entries
            .iter()
            .all(|e| !e.is_empty() && identities.insert(identity(e))),
        "duplicate emoji sequences"
    );
    let mut reachable = std::collections::BTreeSet::new();
    for (key, hits) in &data.readings {
        ensure!(
            !key.is_empty() && normalize(key) == *key && key.len() <= MAX_KEY_BYTES,
            "invalid emoji reading"
        );
        let mut ids = std::collections::BTreeSet::new();
        ensure!(
            !hits.is_empty()
                && hits.len() <= LIMIT
                && hits.iter().all(|h| h.sources > 0
                    && h.sources < 64
                    && (h.emoji as usize) < data.entries.len()
                    && ids.insert(h.emoji)),
            "invalid emoji index"
        );
        reachable.extend(hits.iter().map(|h| h.emoji));
    }
    ensure!(
        reachable.len() == data.entries.len(),
        "emoji catalogue contains unreachable sequences"
    );
    Ok(())
}

fn qualified(text: &str) -> Result<Vec<String>> {
    let mut result = Vec::new();
    for line in text.lines() {
        let Some((points, status)) = line.split_once(';') else {
            continue;
        };
        if status.split_whitespace().next() != Some("fully-qualified") {
            continue;
        }
        let value: Option<String> = points
            .split_whitespace()
            .map(|s| u32::from_str_radix(s, 16).ok().and_then(char::from_u32))
            .collect();
        result.push(value.context("invalid Unicode emoji sequence")?);
    }
    ensure!(!result.is_empty(), "Unicode emoji list is empty");
    Ok(result)
}

fn from_hex(text: &str) -> Result<String> {
    text.split('-')
        .map(|s| u32::from_str_radix(s, 16).ok().and_then(char::from_u32))
        .collect::<Option<String>>()
        .context("invalid shortcode hex sequence")
}

struct Builder {
    entries: Vec<String>,
    ids: BTreeMap<String, u16>,
    readings: BTreeMap<String, BTreeMap<u16, (u8, u16)>>,
    derived: BTreeMap<String, Option<String>>,
    analyzer: hasami::Analyzer,
}
impl Builder {
    fn new(entries: Vec<String>, analyzer: hasami::Analyzer) -> Result<Self> {
        ensure!(entries.len() < u16::MAX as usize, "too many emoji entries");
        let ids: BTreeMap<_, _> = entries
            .iter()
            .enumerate()
            .map(|(i, e)| (identity(e), i as u16))
            .collect();
        ensure!(
            ids.len() == entries.len(),
            "ambiguous fully qualified sequences"
        );
        Ok(Self {
            entries,
            ids,
            readings: BTreeMap::new(),
            derived: BTreeMap::new(),
            analyzer,
        })
    }
    fn insert(&mut self, key: String, id: u16, rank: u8, source: u16) {
        if key.is_empty() || key.len() > MAX_KEY_BYTES || key.chars().any(char::is_control) {
            return;
        }
        self.readings
            .entry(key)
            .or_default()
            .entry(id)
            .and_modify(|(old, mask)| {
                *old = (*old).min(rank);
                *mask |= source;
            })
            .or_insert((rank, source));
    }
    fn alias(&mut self, emoji: &str, text: &str, rank: u8, source: u16) -> Result<()> {
        let Some(&id) = self.ids.get(&identity(emoji)) else {
            return Ok(());
        };
        if text.len() > MAX_KEY_BYTES {
            return Ok(());
        }
        if emoji
            .chars()
            .any(|c| ('\u{1f3fb}'..='\u{1f3ff}').contains(&c))
            && !tone_specific(text)
        {
            return Ok(());
        }
        self.insert(normalize(text), id, rank, source);
        // Labels may contain separators rather than lexical characters. Keep the
        // full alias as well as each explicit component; never take substrings.
        for part in text
            .split([':', '：', '、'])
            .filter(|s| !s.trim().is_empty())
        {
            self.insert(normalize(part), id, rank.saturating_add(1), source);
            if !part.chars().any(han) {
                continue;
            }
            if !self.derived.contains_key(part) {
                let tokens = self.analyzer.try_tokenize(part)?;
                let covered: String = tokens.iter().map(|t| &*t.surface).collect();
                let valid = covered == part
                    && !tokens.is_empty()
                    && tokens.iter().all(|t| {
                        t.is_known
                            && !t.reading.is_empty()
                            && t.reading.as_ref() != "*"
                            && !(t.surface.chars().count() == 1
                                && han(t.surface.chars().next().unwrap())
                                && t.pos.starts_with("名詞,固有名詞,人名"))
                    });
                self.derived.insert(
                    part.to_owned(),
                    valid.then(|| tokens.iter().map(|t| &*t.reading).collect()),
                );
            }
            if let Some(reading) = self.derived.get(part).cloned().flatten() {
                self.insert(normalize(&reading), id, rank.saturating_add(1), source);
            }
        }
        Ok(())
    }
    fn emojibase(&mut self, rows: &[Value], source: u16) -> Result<()> {
        for row in rows {
            let emoji = row["emoji"].as_str().context("emojibase emoji missing")?;
            self.alias(
                emoji,
                row["label"].as_str().context("emojibase label missing")?,
                0,
                source,
            )?;
            if let Some(tags) = row["tags"].as_array() {
                for tag in tags {
                    self.alias(emoji, tag.as_str().context("invalid emoji tag")?, 2, source)?;
                }
            }
            if let Some(skins) = row["skins"].as_array() {
                self.emojibase(skins, source)?;
            }
        }
        Ok(())
    }
}
fn han(c: char) -> bool {
    matches!(c, '\u{3400}'..='\u{9fff}' | '\u{f900}'..='\u{faff}' | '\u{20000}'..='\u{323af}')
}
fn tone_specific(text: &str) -> bool {
    let text = text.to_ascii_lowercase();
    ["肌", "はだ", "ハダ", "スキン"]
        .iter()
        .any(|s| text.contains(s))
        || text
            .split(|c: char| !c.is_ascii_alphabetic())
            .any(|s| matches!(s, "skin" | "tone"))
}

pub fn generate(root: &Path) -> Result<()> {
    let mut check = false;
    let mut offline = false;
    let mut dictionary = None;
    let mut args = std::env::args().skip(2);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--check" => check = true,
            "--offline" => offline = true,
            "--dictionary" => {
                dictionary = Some(PathBuf::from(
                    args.next().context("dictionary path required")?,
                ))
            }
            _ => bail!(
                "usage: cargo xtask emoji-data [--check] [--offline] --dictionary <pinned-ipadic.hsd>"
            ),
        }
    }
    let dictionary = dictionary.context("--dictionary <pinned-ipadic.hsd> is required")?;
    let dictionary_sha256 = dictionary_digest(root)?;
    ensure!(
        hash(&dictionary)? == dictionary_sha256,
        "emoji reading dictionary digest differs"
    );
    let dictionary_notice = dictionary.with_file_name("THIRD_PARTY_LICENSES.md");
    let dictionary_lock: toml::Value = toml::from_str(&fs::read_to_string(
        root.join("locks/dictionary.lock.toml"),
    )?)?;
    ensure!(
        hash(&dictionary_notice)?
            == dictionary_lock["dictionary"]["notices_sha256"]
                .as_str()
                .context("dictionary notice digest missing")?,
        "emoji dictionary notice differs"
    );
    let lock = lock(root)?;
    let cache = root.join("artifacts/emoji");
    fs::create_dir_all(&cache)?;
    for (name, source) in &lock.files {
        ensure!(
            name.bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"-._".contains(&c))
                && !name.starts_with('.'),
            "invalid emoji cache filename"
        );
        let file = cache.join(name);
        let valid = file.is_file() && hash(&file)? == source.sha256;
        if !valid {
            ensure!(!offline, "emoji source absent or corrupt in offline cache");
            let temp = file.with_extension("download");
            run(Command::new("curl")
                .args([
                    "--fail",
                    "--location",
                    "--silent",
                    "--show-error",
                    "--proto",
                    "=https",
                    "--proto-redir",
                    "=https",
                    "--max-time",
                    "120",
                    "--max-filesize",
                    "3000000",
                    "--output",
                ])
                .arg(&temp)
                .arg(&source.url))?;
            ensure!(hash(&temp)? == source.sha256, "emoji source digest differs");
            fs::rename(temp, &file)?;
        }
        ensure!(
            fs::metadata(&file)?.len() == source.bytes,
            "emoji source size differs"
        );
    }
    let entries = qualified(&fs::read_to_string(cache.join("emoji-test.txt"))?)?;
    ensure!(
        entries.len() >= 3000,
        "Unicode emoji coverage is incomplete"
    );
    let mut builder = Builder::new(entries, hasami::Analyzer::load(dictionary)?)?;
    let json =
        |name: &str| -> Result<Value> { Ok(serde_json::from_slice(&fs::read(cache.join(name))?)?) };
    for (file, source) in [("emojibase-ja.json", 1), ("emojibase-en.json", 2)] {
        let data = json(file)?;
        builder.emojibase(data.as_array().context("emojibase array required")?, source)?;
    }
    let data = json("emoji-ja.json")?;
    for (emoji, entry) in data.as_object().context("emoji-ja map required")? {
        for tag in entry["keywords"]
            .as_array()
            .context("emoji-ja keywords required")?
        {
            builder.alias(
                emoji,
                tag.as_str().context("invalid emoji-ja keyword")?,
                1,
                4,
            )?;
        }
    }
    let data = json("emojilib.json")?;
    for (emoji, tags) in data.as_object().context("emojilib map required")? {
        for tag in tags.as_array().context("emojilib tags required")? {
            builder.alias(emoji, tag.as_str().context("invalid emojilib tag")?, 2, 8)?;
        }
    }
    for (file, source) in [("shortcodes-github.json", 16), ("shortcodes-ja.json", 32)] {
        let data = json(file)?;
        for (hex, tags) in data.as_object().context("shortcodes map required")? {
            let emoji = from_hex(hex)?;
            if let Some(tag) = tags.as_str() {
                builder.alias(&emoji, tag, 0, source)?;
            } else {
                for tag in tags.as_array().context("shortcode list required")? {
                    builder.alias(
                        &emoji,
                        tag.as_str().context("invalid shortcode")?,
                        0,
                        source,
                    )?;
                }
            }
        }
    }
    let mut source_hits = [0; 6];
    let readings: BTreeMap<_, _> = builder
        .readings
        .into_iter()
        .map(|(key, values)| {
            let mut values: Vec<_> = values.into_iter().collect();
            values.sort_by_key(|(id, (rank, _))| (*rank, *id));
            let hits = values
                .into_iter()
                .take(LIMIT)
                .map(|(emoji, (_, sources))| {
                    for (i, count) in source_hits.iter_mut().enumerate() {
                        if sources & (1 << i) != 0 {
                            *count += 1;
                        }
                    }
                    Hit { emoji, sources }
                })
                .collect();
            (key, hits)
        })
        .collect();
    ensure!(
        readings.len() > 5000 && source_hits.iter().all(|n| *n > 100),
        "emoji reading coverage is incomplete"
    );
    let data = Data {
        format: 1,
        unicode: lock.unicode,
        entries: builder.entries,
        readings,
        sources: lock
            .files
            .iter()
            .map(|(name, s)| (name.clone(), s.sha256.clone()))
            .collect(),
        dictionary_sha256,
    };
    let bytes = serde_json::to_vec(&data)?;
    let folder = target(root);
    if check {
        ensure!(
            fs::read(folder.join("catalog.json"))? == bytes,
            "emoji catalogue needs regeneration"
        );
        verify(root)?;
        println!("emoji catalogue reproduced from six pinned sources");
        return Ok(());
    }
    let staging = folder.with_extension("next");
    ensure!(
        !staging.exists(),
        "previous emoji staging directory needs inspection"
    );
    fs::create_dir_all(staging.join("notices"))?;
    fs::write(staging.join("catalog.json"), bytes)?;
    for name in lock.files.keys().filter(|s| s.ends_with("LICENSE.txt")) {
        fs::copy(cache.join(name), staging.join("notices").join(name))?;
    }
    // The derived readings carry the same IPAdic notices as the pinned dictionary.
    fs::copy(
        dictionary_notice,
        staging.join("notices/IPAdic-and-hasami-NOTICE.md"),
    )?;
    let manifest = Manifest {
        sha256: hash(&staging.join("catalog.json"))?,
        entries: data.entries.len(),
        readings: data.readings.len(),
        source_hits,
    };
    fs::write(
        staging.join("manifest.json"),
        serde_json::to_vec_pretty(&manifest)?,
    )?;
    verify_at(root, &staging)?;
    let previous = folder.with_extension("previous");
    ensure!(
        !previous.exists(),
        "previous emoji catalogue backup needs inspection"
    );
    fs::rename(&folder, &previous)?;
    if let Err(error) = fs::rename(&staging, &folder) {
        fs::rename(previous, folder)?;
        return Err(error.into());
    }
    fs::remove_dir_all(previous)?;
    verify(root)?;
    println!(
        "emoji catalogue generated: {} sequences, {} readings, six sources",
        manifest.entries, manifest.readings
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn qualification_keeps_joiners_selectors_tags_and_rejects_components() {
        let input = "2764 FE0F ; fully-qualified # E0.6\n2764 ; unqualified\n1F3FB ; component\n1F3F3 FE0F 200D 1F308 ; fully-qualified\n1F1EF 1F1F5 ; fully-qualified";
        assert_eq!(qualified(input).unwrap(), ["❤️", "🏳️‍🌈", "🇯🇵"]);
        assert!(qualified("invalid ; fully-qualified").is_err());
        assert_eq!(from_hex("0031-FE0F-20E3").unwrap(), "1️⃣");
        assert!(from_hex("../invalid").is_err());
        assert!(tone_specific("明るい肌色"));
        assert!(!tone_specific("いいね"));
        assert!(!tone_specific("stone"));
    }
}
