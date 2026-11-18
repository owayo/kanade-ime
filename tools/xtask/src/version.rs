use super::*;
use std::time::{SystemTime, UNIX_EPOCH};

const PLISTS: [&str; 2] = [
    "packaging/macos/Info.plist",
    "packaging/macos/Settings.plist",
];
const KEYS: [&str; 2] = ["CFBundleShortVersionString", "CFBundleVersion"];

fn parts(value: &str) -> Result<[u32; 3]> {
    let values = value
        .split('.')
        .map(str::parse)
        .collect::<std::result::Result<Vec<u32>, _>>()?;
    let values: [u32; 3] = values
        .try_into()
        .map_err(|_| anyhow::anyhow!("three version components required"))?;
    ensure!(
        format!("{}.{}.{}", values[0], values[1], values[2]) == value,
        "version must use unpadded decimal components"
    );
    Ok(values)
}

fn current(root: &Path) -> Result<String> {
    let manifest: toml::Value = toml::from_str(&fs::read_to_string(root.join("Cargo.toml"))?)?;
    Ok(manifest
        .get("workspace")
        .and_then(|v| v.get("package"))
        .and_then(|v| v.get("version"))
        .and_then(toml::Value::as_str)
        .context("workspace version missing")?
        .to_owned())
}

// Release months use Japan's fixed UTC+9 offset, independently of the host TZ.
fn period(seconds: u64) -> Result<(u32, u32)> {
    let mut days = seconds
        .checked_add(9 * 3600)
        .context("release date overflow")?
        / 86_400;
    for year in 1970..2100 {
        let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
        let length = if leap { 366 } else { 365 };
        if days >= length {
            days -= length;
            continue;
        }
        ensure!(year >= 2000, "release year must be between 2000 and 2099");
        for (index, length) in [
            31,
            if leap { 29 } else { 28 },
            31,
            30,
            31,
            30,
            31,
            31,
            30,
            31,
            30,
            31,
        ]
        .into_iter()
        .enumerate()
        {
            if days < length {
                return Ok((year as u32 % 100, index as u32 + 1));
            }
            days -= length;
        }
    }
    bail!("release year must be between 2000 and 2099")
}

fn next(current: &str, year: u32, month: u32) -> Result<String> {
    let [old_year, old_month, old_counter] = parts(current)?;
    ensure!(
        year < 100 && (1..=12).contains(&month),
        "invalid release period"
    );
    let old_year = if (2000..2100).contains(&old_year) {
        old_year % 100
    } else {
        old_year
    };
    ensure!(
        (old_year, old_month) <= (year, month),
        "release date precedes the current version"
    );
    let counter = if (old_year, old_month) == (year, month) {
        old_counter
            .checked_add(1)
            .context("release counter overflow")?
            .max(100)
    } else {
        100
    };
    Ok(format!("{year}.{month}.{counter}"))
}

fn workspace_version(data: &str, value: &str) -> Result<String> {
    let mut section = false;
    let mut replaced = false;
    let mut result = String::new();
    for line in data.split_inclusive('\n') {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            section = trimmed == "[workspace.package]";
        }
        if section
            && let Some((key, rest)) = line.split_once('=')
            && key.trim() == "version"
        {
            ensure!(!replaced, "duplicate workspace version");
            let start = rest
                .find('"')
                .context("quoted workspace version required")?
                + 1;
            let end = rest[start..]
                .find('"')
                .context("unterminated workspace version")?
                + start;
            result.push_str(key);
            result.push('=');
            result.push_str(&rest[..start]);
            result.push_str(value);
            result.push_str(&rest[end..]);
            replaced = true;
        } else {
            result.push_str(line);
        }
    }
    ensure!(replaced, "workspace version missing");
    Ok(result)
}

fn plist_value(data: &str, key: &str) -> Result<std::ops::Range<usize>> {
    let token = format!("<key>{key}</key>");
    ensure!(
        data.matches(&token).count() == 1,
        "one {key} entry required"
    );
    let after = data.find(&token).context("plist version key missing")? + token.len();
    let leading = data[after..].len() - data[after..].trim_start().len();
    let start = after + leading;
    ensure!(
        data[start..].starts_with("<string>"),
        "plist version must be a string"
    );
    let start = start + "<string>".len();
    let end = start
        + data[start..]
            .find("</string>")
            .context("plist version string is unterminated")?;
    Ok(start..end)
}

pub fn check(root: &Path) -> Result<String> {
    let value = current(root)?;
    let [year, month, counter] = parts(&value)?;
    ensure!(
        year < 100 && (1..=12).contains(&month) && counter >= 100,
        "workspace version must be yy.m.counter, starting at 100"
    );
    for path in PLISTS {
        let data = fs::read_to_string(root.join(path))?;
        for key in KEYS {
            ensure!(
                data[plist_value(&data, key)?] == value,
                "{path}: {key} differs from the workspace version"
            );
        }
    }
    Ok(value)
}

pub fn bump(root: &Path, dry_run: bool) -> Result<()> {
    let seconds = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let (year, month) = period(seconds)?;
    if dry_run {
        println!("{}", next(&current(root)?, year, month)?);
        return Ok(());
    }
    fs::create_dir_all(root.join("target"))?;
    let guard = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(root.join("target/version.lock"))?;
    guard
        .try_lock()
        .context("another version update is running")?;
    let value = next(&current(root)?, year, month)?;
    let paths = ["Cargo.toml", PLISTS[0], PLISTS[1], "Cargo.lock"];
    let originals: Vec<_> = paths
        .iter()
        .map(|path| Ok((root.join(path), fs::read(root.join(path))?)))
        .collect::<Result<_>>()?;
    let result = (|| -> Result<()> {
        fs::write(
            root.join("Cargo.toml"),
            workspace_version(std::str::from_utf8(&originals[0].1)?, &value)?,
        )?;
        for path in PLISTS {
            let mut data = fs::read_to_string(root.join(path))?;
            for key in KEYS {
                data.replace_range(plist_value(&data, key)?, &value);
            }
            fs::write(root.join(path), data)?;
        }
        run(Command::new("cargo")
            .args(["update", "--workspace", "--offline"])
            .current_dir(root))?;
        check(root)?;
        Ok(())
    })();
    if let Err(error) = result {
        for (path, data) in originals {
            fs::write(path, data).context("could not restore version metadata")?;
        }
        return Err(error);
    }
    println!("{value}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn release_counter_follows_the_reference_scheme() {
        for (old, year, month, expected) in [
            ("0.1.0", 26, 11, "26.11.100"),
            ("26.11.100", 26, 11, "26.11.101"),
            ("2026.11.107", 26, 11, "26.11.108"),
            ("26.11.108", 26, 12, "26.12.100"),
            ("26.12.108", 27, 1, "27.1.100"),
        ] {
            assert_eq!(next(old, year, month).unwrap(), expected);
        }
        assert!(next("27.1.100", 26, 12).is_err());
        assert!(next("26.11.4294967295", 26, 11).is_err());
        assert!(parts("26.01.100").is_err());
    }
    #[test]
    fn japan_month_boundary_does_not_follow_utc() {
        assert_eq!(period(1_796_050_799).unwrap(), (26, 11));
        assert_eq!(period(1_796_050_800).unwrap(), (26, 12));
        assert_eq!(period(1_798_729_200).unwrap(), (27, 1));
        assert_eq!(period(1_803_826_800).unwrap(), (27, 3));
        assert_eq!(period(1_835_362_800).unwrap(), (28, 2));
        assert_eq!(period(1_835_449_200).unwrap(), (28, 3));
        assert!(period(u64::MAX).is_err());
    }
    #[test]
    fn metadata_updates_preserve_dependencies_and_other_plist_keys() {
        let manifest = "[workspace.dependencies]\ndep = { version = \"0.1.0\" }\n[workspace.package]\nversion = \"0.1.0\" # keep\nedition = \"2024\"\n";
        assert_eq!(
            workspace_version(manifest, "26.11.100").unwrap(),
            manifest.replace(
                "version = \"0.1.0\" # keep",
                "version = \"26.11.100\" # keep"
            )
        );
        let mut plist = "<key>Other</key><string>keep</string>\n<key>CFBundleVersion</key>\n <string>30</string>".to_string();
        plist.replace_range(plist_value(&plist, "CFBundleVersion").unwrap(), "26.11.100");
        assert!(plist.contains("<key>Other</key><string>keep</string>"));
        assert!(plist.contains("<string>26.11.100</string>"));
        assert!(
            plist_value(
                "<key>CFBundleVersion</key><integer>30</integer>",
                "CFBundleVersion"
            )
            .is_err()
        );
        assert!(plist_value("", "CFBundleVersion").is_err());
    }
    #[test]
    fn drift_is_rejected_and_a_failed_bump_restores_metadata() {
        struct Fixture(PathBuf);
        impl Drop for Fixture {
            fn drop(&mut self) {
                let _ = fs::remove_dir_all(&self.0);
            }
        }
        let fixture = Fixture(std::env::temp_dir().join(format!(
                "kanade-version-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            )));
        let root = &fixture.0;
        fs::create_dir_all(root.join("packaging/macos")).unwrap();
        fs::write(
            root.join("Cargo.toml"),
            "[workspace.package]\nversion = \"26.11.100\"\n",
        )
        .unwrap();
        let plist = "<key>CFBundleVersion</key><string>26.11.100</string><key>CFBundleShortVersionString</key><string>26.11.100</string>";
        for path in PLISTS {
            fs::write(root.join(path), plist).unwrap();
        }
        assert_eq!(check(root).unwrap(), "26.11.100");
        fs::write(
            root.join(PLISTS[1]),
            plist.replace("26.11.100", "26.11.101"),
        )
        .unwrap();
        assert!(check(root).is_err());
        fs::write(
            root.join("Cargo.toml"),
            "[workspace.package]\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        fs::write(root.join(PLISTS[0]), plist.replace("26.11.100", "0.1.0")).unwrap();
        fs::write(root.join(PLISTS[1]), "<dict/>").unwrap();
        fs::write(root.join("Cargo.lock"), "original lock").unwrap();
        let paths = ["Cargo.toml", PLISTS[0], PLISTS[1], "Cargo.lock"];
        let before: Vec<_> = paths
            .iter()
            .map(|path| fs::read(root.join(path)).unwrap())
            .collect();
        assert!(bump(root, false).is_err());
        for (path, expected) in paths.iter().zip(before) {
            assert_eq!(fs::read(root.join(path)).unwrap(), expected);
        }
    }
}
