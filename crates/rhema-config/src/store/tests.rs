use super::*;

#[test]
#[cfg(unix)]
fn a_widened_config_file_tightens_on_the_next_save() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.yaml");
    std::fs::write(&path, "stale").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    Settings::save_to(&path, &Settings::default()).unwrap();
    let mode = std::fs::metadata(&path).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600, "credentials file must be 0600");
}

#[test]
fn settings_round_trip_through_a_foreign_store() {
    // The seam a second frontend implements: the section moves in
    // and out of any document, here a JSON envelope, the
    // framework never sees the format.
    struct GodCodeStore(std::sync::Mutex<String>);

    impl Store for GodCodeStore {
        fn load(&self) -> Result<Settings, ConfigError> {
            let doc = self
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if doc.is_empty() {
                return Ok(Settings::default());
            }
            let envelope: serde_json::Value =
                serde_json::from_str(&doc).map_err(|err| ConfigError::Store(err.to_string()))?;
            serde_json::from_value(envelope.get("rhema").cloned().unwrap_or_default())
                .map_err(|err| ConfigError::Store(err.to_string()))
        }

        fn save(&self, settings: &Settings) -> Result<(), ConfigError> {
            let envelope = serde_json::json!({ "rhema": settings, "accent": "#00ff00" });
            *self
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) =
                serde_json::to_string(&envelope)
                    .map_err(|err| ConfigError::Store(err.to_string()))?;
            Ok(())
        }
    }

    let store = GodCodeStore(std::sync::Mutex::new(String::new()));
    // An empty document is the default settings.
    assert_eq!(store.load().unwrap(), Settings::default());

    let mut settings = Settings::default();
    settings.set_for_profile(Some("work"), |p| p.model = Some("m".into()));
    store.save(&settings).unwrap();
    // The frontend's own key rode along, untouched by the reload.
    let raw = store
        .0
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    assert!(raw.contains("accent"));
    assert_eq!(store.load().unwrap(), settings);
}

#[test]
fn the_yaml_store_creates_the_directory_and_keeps_the_file_private() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("nested").join("config.yaml");
    YamlStore::at(&path).save(&Settings::default()).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(dir.path().join("nested"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700, "the directory must be 0700");
    }
    assert_eq!(YamlStore::at(&path).load().unwrap(), Settings::default());
}
