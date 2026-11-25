//! `rhema update`: the update flow outside the terminal, plain lines on
//! stdout, the same check and the same installer the settings' Update
//! page runs.

use crate::update;

/// Checks the channel, installs the newer release when there is one,
/// and prints what happened. The exit code is the answer: 0 when this
/// build is the newest or the install landed, 1 when the check or the
/// install failed.
pub async fn update_command() -> anyhow::Result<()> {
    update::clear_stale_install();
    let current = update::current_version();
    println!("rhema {current}, checking the channel");
    let latest = update::fetch_latest_version()
        .await
        .map_err(|err| anyhow::anyhow!("the version check failed: {err}"))?;
    if !update::is_newer(&latest, current) {
        println!("rhema {latest} is the channel's release; nothing to install");
        return Ok(());
    }
    println!("rhema {latest} is available, installing (this can take a minute)");
    let output = update::run_installer()
        .await
        .map_err(|err| anyhow::anyhow!("the install failed: {err}"))?;
    for line in output.lines() {
        println!("{line}");
    }
    println!("restart rhema to run {latest}");
    Ok(())
}
