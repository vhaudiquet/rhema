//! The stored document: the framework's [`Settings`] section beside
//! this frontend's own settings, one file this frontend owns at
//! `<config-base>/config.yaml`. The app's state machine calls the
//! [`ConfigStore`] trait, never the filesystem, so login and model
//! persistence is testable without touching `$XDG_CONFIG_HOME`.

use rhema_config::privacy::{private_dir, private_write_atomic};
use rhema_config::{ConfigError, Home, Settings};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The `custom` palette's token overrides, per appearance. A shell
/// process (a wallpaper watcher, a pywal-style generator) writes these;
/// the TUI reads them at startup. Any token left out — or written in a
/// form that is not a `#RRGGBB` color — reads as the rhema default for
/// that slot, so a partial file is an overlay, not a broken theme.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CustomPalette {
    /// The dark appearance's tokens.
    pub dark: Option<CustomTokens>,
    /// The light appearance's tokens.
    pub light: Option<CustomTokens>,
}

/// One appearance of the `custom` palette: the design tokens by name,
/// each an optional `#RRGGBB` (or `RRGGBB`) string. Unknown keys are
/// rejected: a typo'd token must not silently read as unset.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CustomTokens {
    /// Page canvas.
    pub ground: Option<String>,
    /// Panels and cards.
    pub surface: Option<String>,
    /// Popovers, hovered rows.
    pub elevated: Option<String>,
    /// Code blocks, log view, the transcript well.
    pub sunken: Option<String>,
    /// Hairlines.
    pub border: Option<String>,
    /// Dividers that must read.
    pub border_strong: Option<String>,
    /// Body text.
    pub text: Option<String>,
    /// Secondary text.
    pub text2: Option<String>,
    /// Hints only.
    pub text3: Option<String>,
    /// Fills, rules, the mark.
    pub accent: Option<String>,
    /// Accent as text or icon.
    pub accent_text: Option<String>,
    /// Labels ON an accent fill.
    pub on_accent: Option<String>,
    /// A call in flight.
    pub running: Option<String>,
    /// Success.
    pub ok: Option<String>,
    /// Caution.
    pub warn: Option<String>,
    /// A real failure.
    pub error: Option<String>,
    /// Queued, idle, neutral notice.
    pub info: Option<String>,
}

/// The file this frontend owns: the framework's [`Settings`] section —
/// flattened, its keys at the top level, byte-compatible with the
/// layout the file has always had — beside this frontend's own
/// settings. The framework reads only its section; everything else
/// here is the terminal UI's.
///
/// The strictness the framework's section has on its own does not
/// survive serde's flatten — unknown keys would be dropped silently —
/// so [`Document::load_from`] re-enforces it: every key that is not
/// this frontend's own must parse as the section, whose unknown-key
/// rejection is loud. A typo'd `profiles:` is an error naming the key
/// wherever it lands; the price of one file under two vocabularies is
/// that the error's field list names only the section.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "snake_case")]
pub struct Document {
    /// The framework's section: profiles, credentials, the persisted
    /// off switches.
    #[serde(flatten)]
    pub settings: Settings,
    /// The footer's icon flavor: `nerd` (the default) for Nerd Font
    /// glyphs, or `unicode` for the everywhere-safe set. Anything
    /// unparseable reads as the default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icons: Option<String>,
    /// The color theme: `dark` (the default) or `light`. Validated by
    /// the consumer, not here — anything unparseable reads as the
    /// default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub theme: Option<String>,
    /// What paints behind the ink: `native` (the default) leaves the
    /// terminal's own background in place, `rhema` paints the palette's
    /// fills over the whole screen. Validated by the consumer, not
    /// here — anything unparseable reads as the default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub background: Option<String>,
    /// The UI color palette: `rhema` (the default), `aquamarine`,
    /// `black-and-white`, `dracula`, `catppuccin-frappe`,
    /// `catppuccin-macchiato`, `catppuccin-mocha`, `nord`, `gruvbox`,
    /// `solarized`, `m3-you`, or `custom`. Validated by the consumer,
    /// not here — anything unparseable reads as the default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub colors: Option<String>,
    /// The seed color of the `m3-you` palette, `#RRGGBB`: pick one from
    /// your wallpaper (a terminal cannot see it). Unset means Material's
    /// baseline purple. Ignored by every other palette.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub m3_source: Option<String>,
    /// The `custom` palette's token overrides, per appearance: the hook
    /// a wallpaper watcher or scheme generator writes. Read by the
    /// consumer when `colors` is `custom`; anything it leaves out reads
    /// as the rhema default for that slot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom: Option<CustomPalette>,
    /// The last profile that served each workspace a turn, keyed by the
    /// workspace's absolute path. Startup reopens a directory on its
    /// last-used profile — a per-directory memory, never a global
    /// switch, so parallel instances in different directories stay
    /// independent.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub last_profile_by_workspace: BTreeMap<String, String>,
    /// The last sandbox kind each workspace ran `bash` under, keyed by
    /// the workspace's absolute path — the sandbox twin of
    /// `last_profile_by_workspace`. A directory reopens on the kind it
    /// last used, never overriding an explicit flag or `RHEMA_SANDBOX`.
    /// The kind is stored as given; whether this machine still offers
    /// it is the consumer's question.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub last_sandbox_by_workspace: BTreeMap<String, String>,
    /// Where the first-run onboarding stands. `Some(false)` — the
    /// wizard opened but did not finish — reopens it on the next
    /// start; `Some(true)` never shows it again. `None`, the default,
    /// is a document that predates the wizard and reads as onboarded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub onboarded: Option<bool>,
}

/// The document under its historical name — the one the session state
/// machine, the screens, and the run loop speak.
pub type Config = Document;

impl std::ops::Deref for Document {
    type Target = Settings;

    fn deref(&self) -> &Settings {
        &self.settings
    }
}

impl std::ops::DerefMut for Document {
    fn deref_mut(&mut self) -> &mut Settings {
        &mut self.settings
    }
}

impl Default for Document {
    /// The framework section ships its two profiles; everything
    /// frontend-owned starts unset.
    fn default() -> Self {
        Self {
            settings: Settings::default(),
            icons: None,
            theme: None,
            background: None,
            colors: None,
            m3_source: None,
            custom: None,
            last_profile_by_workspace: BTreeMap::new(),
            last_sandbox_by_workspace: BTreeMap::new(),
            onboarded: None,
        }
    }
}

impl Document {
    /// Loads the document; a missing file is the default document, not
    /// an error.
    pub fn load() -> Result<Document, ConfigError> {
        Self::load_from(&Self::path()?)
    }

    /// Persists the document, creating the directory `0700` and the
    /// file `0600` on Unix because the framework section can hold
    /// credentials. Both modes are re-asserted on every save, so a
    /// file that predates the rule — or was widened elsewhere —
    /// tightens again.
    pub fn save(&self) -> Result<(), ConfigError> {
        let dir = Home::discover()?.config;
        private_dir(&dir)?;
        Self::save_to(&dir.join("config.yaml"), self)
    }

    /// The document file (`<config-base>/config.yaml`).
    pub fn path() -> Result<PathBuf, ConfigError> {
        Ok(Home::discover()?.config_file())
    }

    /// [`Document::load`] against an explicit file (missing file is
    /// the default document).
    ///
    /// The parse is lenient — serde's flatten drops keys it cannot
    /// place — so the framework section's strictness is re-asserted
    /// here, key by key: see `reject_unknown_keys`.
    pub fn load_from(path: &impl AsRef<Path>) -> Result<Document, ConfigError> {
        let raw = match std::fs::read_to_string(path) {
            Ok(raw) => raw,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Document::default());
            }
            Err(err) => return Err(err.into()),
        };
        let doc: Document = serde_yaml::from_str(&raw)?;
        Self::reject_unknown_keys(&raw)?;
        Ok(doc)
    }

    /// Rejects a key that belongs to neither vocabulary — a typo'd
    /// `profiles:` must not silently read as unset just because the
    /// flatten swallowed it. The frontend's own keys are checked by
    /// name; everything left over must parse as the framework's
    /// [`Settings`], whose unknown-key rejection names the offender.
    fn reject_unknown_keys(raw: &str) -> Result<(), ConfigError> {
        const OWN: &[&str] = &[
            "icons",
            "theme",
            "background",
            "colors",
            "m3_source",
            "custom",
            "last_profile_by_workspace",
            "last_sandbox_by_workspace",
            "onboarded",
        ];
        let value: serde_yaml::Value = serde_yaml::from_str(raw)?;
        let serde_yaml::Value::Mapping(map) = value else {
            return Ok(());
        };
        let section: serde_yaml::Mapping = map
            .into_iter()
            .filter(|(key, _)| key.as_str().map(|key| !OWN.contains(&key)).unwrap_or(true))
            .collect();
        serde_yaml::from_value::<Settings>(serde_yaml::Value::Mapping(section))?;
        Ok(())
    }

    /// [`Document::save`] to an explicit file, with the same `0600`
    /// handling.
    pub fn save_to(path: &impl AsRef<Path>, doc: &Document) -> Result<(), ConfigError> {
        let yaml = serde_yaml::to_string(doc)?;
        private_write_atomic(path.as_ref(), yaml.as_bytes())?;
        Ok(())
    }

    /// The profile `workspace` last served a turn under, when one was
    /// recorded. The name is returned as stored — the caller decides
    /// whether that profile still exists and can serve turns.
    pub fn last_profile_in(&self, workspace: &Path) -> Option<String> {
        self.last_profile_by_workspace
            .get(&workspace.to_string_lossy().into_owned())
            .cloned()
    }

    /// Records `profile` as the last one `workspace` served a turn
    /// under. A no-op when the map already holds the name, so a turn per
    /// keystroke never rewrites the file.
    pub fn note_profile_in(&mut self, workspace: &Path, profile: &str) {
        let changed = self.last_profile_in(workspace).as_deref() != Some(profile);
        if changed {
            self.last_profile_by_workspace
                .insert(workspace.to_string_lossy().into_owned(), profile.to_owned());
        }
    }

    /// The sandbox kind `workspace` last ran `bash` under, when one was
    /// recorded. The kind is returned as stored — the caller decides
    /// whether this machine still offers it.
    pub fn last_sandbox_in(&self, workspace: &Path) -> Option<String> {
        self.last_sandbox_by_workspace
            .get(&workspace.to_string_lossy().into_owned())
            .cloned()
    }

    /// Records `kind` as the last one `workspace` ran `bash` under. A
    /// no-op when the map already holds the kind, so a turn per
    /// keystroke never rewrites the file.
    pub fn note_sandbox_in(&mut self, workspace: &Path, kind: &str) {
        let changed = self.last_sandbox_in(workspace).as_deref() != Some(kind);
        if changed {
            self.last_sandbox_by_workspace
                .insert(workspace.to_string_lossy().into_owned(), kind.to_owned());
        }
    }
}

/// Reads and writes the persisted rhema document.
pub trait ConfigStore: Send + Sync {
    /// Loads the document (a missing file is the default document, not
    /// an error, matching [`Document::load`]).
    fn load(&self) -> Result<Config, ConfigError>;

    /// Persists the document, creating the directory.
    fn save(&self, config: &Config) -> Result<(), ConfigError>;
}

/// The real store: the rhema document on disk.
#[derive(Debug, Clone, Copy, Default)]
pub struct DiskStore;

impl ConfigStore for DiskStore {
    fn load(&self) -> Result<Config, ConfigError> {
        Document::load()
    }

    fn save(&self, config: &Config) -> Result<(), ConfigError> {
        config.save()
    }
}

/// An in-memory store for tests.
#[derive(Debug, Default)]
pub struct MemoryStore {
    config: std::sync::Mutex<Config>,
}

impl MemoryStore {
    /// A store holding `config`.
    pub fn new(config: Config) -> Self {
        Self {
            config: std::sync::Mutex::new(config),
        }
    }
}

impl ConfigStore for MemoryStore {
    fn load(&self) -> Result<Config, ConfigError> {
        Ok(self
            .config
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone())
    }

    fn save(&self, config: &Config) -> Result<(), ConfigError> {
        *self
            .config
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = config.clone();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_background_choice_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");
        std::fs::write(&path, "background: native\n").unwrap();
        let loaded = Document::load_from(&path).unwrap();
        assert_eq!(loaded.background.as_deref(), Some("native"));
        // Unset reads as None, the rhema fills.
        std::fs::write(&path, "theme: dark\n").unwrap();
        let loaded = Document::load_from(&path).unwrap();
        assert_eq!(loaded.background, None);
    }

    #[test]
    fn the_custom_palette_round_trips_and_rejects_typos() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");
        std::fs::write(
            &path,
            "colors: custom\ncustom:\n  dark:\n    accent: \"#FF8800\"\n",
        )
        .unwrap();
        let loaded = Document::load_from(&path).unwrap();
        assert_eq!(
            loaded
                .custom
                .as_ref()
                .and_then(|c| c.dark.as_ref())
                .and_then(|t| t.accent.as_deref()),
            Some("#FF8800")
        );
        assert!(loaded.custom.as_ref().unwrap().light.is_none());
        // A typo'd token name is an error, not a silent default.
        std::fs::write(
            &path,
            "colors: custom\ncustom:\n  dark:\n    graond: \"#FF8800\"\n",
        )
        .unwrap();
        let err = Document::load_from(&path).expect_err("a typo'd token must not read as unset");
        assert!(err.to_string().contains("graond"), "names the key: {err}");
    }

    #[test]
    fn workspaces_remember_their_last_profile_and_survive_a_round_trip() {
        let mut config = Document::default();
        assert_eq!(config.last_profile_in(Path::new("/home/u/proj")), None);

        config.note_profile_in(Path::new("/home/u/proj"), "work");
        assert_eq!(
            config.last_profile_in(Path::new("/home/u/proj")).as_deref(),
            Some("work")
        );
        // Another directory stays untouched: the memory is per workspace.
        assert_eq!(config.last_profile_in(Path::new("/home/u/other")), None);

        config.note_profile_in(Path::new("/home/u/proj"), "personal");
        assert_eq!(
            config.last_profile_in(Path::new("/home/u/proj")).as_deref(),
            Some("personal")
        );

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");
        Document::save_to(&path, &config).unwrap();
        let loaded = Document::load_from(&path).unwrap();
        assert_eq!(loaded, config);
        assert_eq!(
            loaded.last_profile_in(Path::new("/home/u/proj")).as_deref(),
            Some("personal")
        );
    }

    #[test]
    fn workspaces_remember_their_last_sandbox_and_survive_a_round_trip() {
        let mut config = Document::default();
        assert_eq!(config.last_sandbox_in(Path::new("/home/u/proj")), None);

        config.note_sandbox_in(Path::new("/home/u/proj"), "docker");
        assert_eq!(
            config.last_sandbox_in(Path::new("/home/u/proj")).as_deref(),
            Some("docker")
        );

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");
        Document::save_to(&path, &config).unwrap();
        let loaded = Document::load_from(&path).unwrap();
        assert_eq!(loaded, config);
        assert_eq!(
            loaded.last_sandbox_in(Path::new("/home/u/proj")).as_deref(),
            Some("docker")
        );
    }

    #[test]
    fn one_file_carries_both_vocabularies_and_stays_strict() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");
        // The framework's section and this frontend's, one file: the
        // section's keys read through the flatten, the UI's beside
        // them, and the saved layout matches the historical one —
        // both sections at the top level, no nesting.
        let mut config = Document::default();
        config.settings.set_for_profile(Some("work"), |p| {
            p.model = Some("m".into());
        });
        config.theme = Some("light".into());
        config.onboarded = Some(true);
        Document::save_to(&path, &config).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("model:"), "framework section on disk: {raw}");
        assert!(raw.contains("theme:"), "frontend section on disk: {raw}");
        assert!(!raw.contains("settings:"), "no nested section: {raw}");
        let loaded = Document::load_from(&path).unwrap();
        assert_eq!(loaded, config);
        assert_eq!(loaded.effective(Some("work")).model.as_deref(), Some("m"));

        // A typo'd framework key stays loud inside the bigger document.
        std::fs::write(&path, "profles:\n  personal: {}\n").unwrap();
        let err =
            Document::load_from(&path).expect_err("a typo'd section key must not read as unset");
        assert!(err.to_string().contains("profles"), "names the key: {err}");
    }
}
