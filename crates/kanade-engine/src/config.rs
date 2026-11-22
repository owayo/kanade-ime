use serde::{Deserialize, Serialize};
use std::{
    io,
    path::{Path, PathBuf},
};
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub learning_excluded_apps: Vec<String>,
    pub learning: bool,
    pub live_conversion: bool,
    pub llm: bool,
    pub writing_assistance: bool,
    pub project_dictionary: bool,
    pub calculated_dates: bool,
    pub emoji_dictionary: bool,
    pub page_size: u32,
    pub font_size: f32,
    pub punctuation_commit: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LearningExclusionsEdit {
    pub previous: Vec<String>,
    pub apps: Vec<String>,
}
impl LearningExclusionsEdit {
    pub fn apply(&self, current: &Config) -> io::Result<Config> {
        if current.learning_excluded_apps != self.previous {
            return Err(io::ErrorKind::WouldBlock.into());
        }
        if self.apps.len() > 64
            || self.apps.iter().any(|app| {
                app.is_empty()
                    || app.len() > 512
                    || app.trim() != app
                    || app.chars().any(char::is_control)
            })
            || self
                .apps
                .iter()
                .enumerate()
                .any(|(i, app)| self.apps[..i].contains(app))
        {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        let mut settings = current.clone();
        settings.learning_excluded_apps.clone_from(&self.apps);
        settings.validate()?;
        Ok(settings)
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            learning_excluded_apps: vec![],
            learning: true,
            live_conversion: false,
            llm: false,
            writing_assistance: false,
            project_dictionary: false,
            calculated_dates: true,
            emoji_dictionary: true,
            page_size: kanade_ipc::PAGE_SIZE as u32,
            font_size: 17.0,
            punctuation_commit: false,
        }
    }
}
impl Config {
    /// What the engine uses while its settings file exists but cannot be read.
    /// The file may say that a feature is off, so every feature a user
    /// switches stays off rather than taking its default. Learning off also
    /// keeps Mozc from learning. The candidate count and the text size, which
    /// switch nothing on, keep their defaults.
    pub fn unreadable() -> Self {
        Self {
            learning: false,
            llm: false,
            project_dictionary: false,
            calculated_dates: false,
            emoji_dictionary: false,
            punctuation_commit: false,
            ..Self::default()
        }
    }
    pub fn learning_allowed(&self, app: &str) -> bool {
        self.learning && kanade_ipc::learning_app_allowed(app, &self.learning_excluded_apps)
    }
    pub fn validate(&self) -> io::Result<()> {
        // `llm` is accepted here; administration additionally requires a
        // qualified model and runtime before it can be switched on.
        if self.live_conversion || self.writing_assistance {
            return Err(io::ErrorKind::Unsupported.into());
        }
        if !(12.0..=40.0).contains(&self.font_size)
            || !(1..=9).contains(&self.page_size)
            || self.learning_excluded_apps.len() > 64
            || self
                .learning_excluded_apps
                .iter()
                .any(|app| !kanade_ipc::learning_app_allowed(app, &[]))
            || self
                .learning_excluded_apps
                .iter()
                .enumerate()
                .any(|(i, app)| self.learning_excluded_apps[..i].contains(app))
        {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        Ok(())
    }
    pub fn save(&self, path: &Path) -> io::Result<()> {
        self.validate()?;
        let parent = path.parent().ok_or(io::ErrorKind::InvalidInput)?;
        std::fs::create_dir_all(parent)?;
        let temp = path.with_extension("next");
        let data = toml::to_string_pretty(self).map_err(|_| io::ErrorKind::InvalidInput)?;
        let mut options = std::fs::File::options();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temp)?;
        std::io::Write::write_all(&mut file, data.as_bytes())?;
        file.sync_all()?;
        std::fs::rename(temp, path)?;
        Ok(())
    }
    pub fn load(path: &Path) -> io::Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(data) => {
                let mut table: toml::Table =
                    toml::from_str(&data).map_err(|_| io::ErrorKind::InvalidData)?;
                // The former allowlist is not an exclusion list. Retire it on
                // load; all other settings and explicit new exclusions survive.
                if let Some(legacy) = table.remove("learning_apps") {
                    let apps = legacy.as_array().ok_or(io::ErrorKind::InvalidData)?;
                    if apps.len() > 64
                        || apps
                            .iter()
                            .any(|app| app.as_str().is_none_or(|s| s.len() > 512))
                    {
                        return Err(io::ErrorKind::InvalidData.into());
                    }
                }
                let config: Self = table.try_into().map_err(|_| io::ErrorKind::InvalidData)?;
                config.validate()?;
                Ok(config)
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e),
        }
    }
}
/// The pipe this engine listens on. `KANADE_INSTANCE` names an isolated
/// instance, which a test or development engine sets together with its own
/// data directory. Frontends never read it: they reach the installed engine.
#[cfg(windows)]
pub fn endpoint() -> io::Result<String> {
    match std::env::var("KANADE_INSTANCE") {
        Ok(label) => kanade_ipc::windows::instance_endpoint(&label),
        Err(std::env::VarError::NotPresent) => kanade_ipc::windows::endpoint(),
        Err(std::env::VarError::NotUnicode(_)) => Err(io::ErrorKind::InvalidInput.into()),
    }
}
pub fn data_directory() -> io::Result<PathBuf> {
    if let Some(path) = std::env::var_os("KANADE_DATA_DIR") {
        return Ok(path.into());
    }
    #[cfg(target_os = "macos")]
    {
        std::env::var_os("HOME")
            .map(|p| PathBuf::from(p).join("Library/Application Support/kanade-ime"))
            .ok_or(io::ErrorKind::NotFound.into())
    }
    #[cfg(windows)]
    {
        std::env::var_os("LOCALAPPDATA")
            .map(|p| PathBuf::from(p).join("kanade-ime"))
            .ok_or(io::ErrorKind::NotFound.into())
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|p| PathBuf::from(p).join(".local/share")))
            .map(|p| p.join("kanade-ime"))
            .ok_or(io::ErrorKind::NotFound.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn default_learning_and_legacy_allowlists_use_exclusions() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("settings.toml");
        for old in ["[]", "[\"com.apple.TextEdit\"]", "[\"com.example.Editor\"]"] {
            std::fs::write(&path, format!("learning_apps = {old}\nfont_size = 24.0\n")).unwrap();
            let config = Config::load(&path).unwrap();
            assert!(config.learning_allowed("com.example.Editor"));
            assert!(config.learning_allowed("com.example.Other"));
            assert!(config.learning_allowed("com.apple.TextEdit"));
            assert!(config.learning_excluded_apps.is_empty());
            assert_eq!(config.font_size, 24.0);
            config.save(&path).unwrap();
            let saved: toml::Table =
                toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
            assert!(!saved.contains_key("learning_apps"));
        }
        std::fs::write(&path, "learning_apps = [\"com.apple.TextEdit\"]\nlearning = false\nlearning_excluded_apps = [\"com.example.Secret\"]\n").unwrap();
        let config = Config::load(&path).unwrap();
        assert!(!config.learning);
        assert_eq!(config.learning_excluded_apps, ["com.example.Secret"]);
        assert!(!config.learning_allowed("com.example.Other"));
        let config = Config {
            learning: true,
            ..config
        };
        assert!(config.learning_allowed("com.example.Other"));
        assert!(!config.learning_allowed("com.example.Secret"));
        assert!(!config.learning_allowed(""));
        std::fs::write(&path, "learning_apps = false").unwrap();
        assert!(Config::load(&path).is_err());
        std::fs::write(&path, "learning_excluded_app = []").unwrap();
        assert!(Config::load(&path).is_err());
        assert!(serde_json::from_str::<Config>(r#"{"learning_apps":[]}"#).is_err());
    }
    #[test]
    fn unreadable_settings_switch_every_feature_off() {
        let config = Config::unreadable();
        // Every switch, including any added later, so none takes its default.
        for (key, value) in toml::Table::try_from(&config).unwrap() {
            assert_ne!(value.as_bool(), Some(true), "{key}");
        }
        assert!(!config.learning_allowed("com.example.Editor"));
        assert_eq!(config.page_size, Config::default().page_size);
        assert_eq!(config.font_size, Config::default().font_size);
        config.validate().unwrap();
    }
    #[test]
    fn app_scope_is_bound_to_its_snapshot_and_preserves_other_settings() {
        let mut current = Config::default();
        let edit = LearningExclusionsEdit {
            previous: current.learning_excluded_apps.clone(),
            apps: vec!["com.example.Editor".into()],
        };
        current.font_size = 24.;
        let changed = edit.apply(&current).unwrap();
        assert_eq!(changed.font_size, 24.);
        assert_eq!(changed.learning_excluded_apps, edit.apps);
        assert!(edit.apply(&changed).is_err());
        for apps in [
            vec!["".into()],
            vec!["com.example.Editor\n".into()],
            vec!["a".into(), "a".into()],
            vec!["a".into(); 65],
        ] {
            assert!(
                LearningExclusionsEdit {
                    previous: current.learning_excluded_apps.clone(),
                    apps
                }
                .apply(&current)
                .is_err()
            );
        }
        assert!(
            LearningExclusionsEdit {
                previous: current.learning_excluded_apps.clone(),
                apps: vec![]
            }
            .apply(&current)
            .is_ok()
        );
    }
}
