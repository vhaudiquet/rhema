use super::*;

#[test]
fn home_points_every_convention_at_its_root() {
    let home = Home::at("/home/u".into(), "/home/u/.godcode".into());
    assert_eq!(
        home.config_file(),
        PathBuf::from("/home/u/.godcode/config.yaml")
    );
    assert_eq!(home.skills(), PathBuf::from("/home/u/.godcode/skills"));
    assert_eq!(
        home.subagents(),
        PathBuf::from("/home/u/.godcode/subagents")
    );
    assert_eq!(
        home.system_prompt(),
        PathBuf::from("/home/u/.godcode/SYSTEM.md")
    );
    // The cross-agent convention stays machine-global: every
    // frontend shares `~/.agents/skills`, whatever its config base.
    assert_eq!(
        home.agents_skills(),
        PathBuf::from("/home/u/.agents/skills")
    );
    // State and cache take the frontend's own name, a dot dir
    // names it without the dot, so nothing of godcode's lands
    // under the family's `rhema/`.
    assert_eq!(home.state, PathBuf::from("/home/u/.local/state/godcode"));
    assert_eq!(home.cache, PathBuf::from("/home/u/.cache/godcode"));
}

#[test]
fn the_reference_layout_nests_the_family_under_rhema() {
    let home = Home::under_home(Path::new("/home/u"));
    assert_eq!(home.config, PathBuf::from("/home/u/.config/rhema"));
    assert_eq!(home.state, PathBuf::from("/home/u/.local/state/rhema"));
    assert_eq!(home.cache, PathBuf::from("/home/u/.cache/rhema"));
}

#[test]
fn the_legacy_keyring_service_follows_the_frontend_name() {
    assert_eq!(
        Home::under_home(Path::new("/home/u")).legacy_keyring_service(),
        "rhema"
    );
    assert_eq!(
        Home::at("/home/u".into(), "/home/u/.godcode".into()).legacy_keyring_service(),
        "godcode"
    );
}
