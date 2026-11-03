//! What this machine offers: the per-backend CLI probes, the budget each
//! obeys, and the offered set a profile turns into choices.

use super::*;

/// The wall-clock bound on one `docker info` probe: the CLI answers in
/// well under a second when the daemon is up, and hangs or retries on
/// its own when it is not — a few seconds is the whole patience the
/// startup path can afford.
const DOCKER_PROBE_BUDGET: Duration = Duration::from_secs(3);

/// The same bound on one `lxc info` probe: the CLI answers fast when
/// the local daemon answers, and the startup path's patience is the
/// same few seconds.
const LXD_PROBE_BUDGET: Duration = Duration::from_secs(3);

/// The same bound on one `ecrou probe`: the CLI dlopens libkrun and
/// checks KVM, answering in well under a second when the host can
/// boot machines.
const ECROU_PROBE_BUDGET: Duration = Duration::from_secs(5);

/// The machine half of the availability probe: the profile-independent
/// facts. Probed once at startup and carried by the app, because a
/// profile switch must re-derive the offered set for the profile
/// moving in — `ssh` and `http` answer on the section's facts, not the
/// machine's alone — without paying for the CLI probes again.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SandboxMachines {
    /// `docker info` answered within the budget.
    pub docker: bool,
    /// `lxc info` answered within the budget.
    pub lxd: bool,
    /// `ecrou probe` answered within the budget: KVM working,
    /// libkrun loadable, the cache writable.
    pub ecrou: bool,
    /// The `ssh` binary exists on `PATH`.
    pub ssh: bool,
}

impl SandboxMachines {
    /// Probes this machine: one `docker info` run, one `lxc info` run,
    /// one `ecrou probe` run, one `PATH` scan for `ssh`. Bounded work —
    /// each CLI probe at most its budget — so startup waits seconds,
    /// never minutes.
    pub fn probe() -> Self {
        Self {
            docker: docker_info_answers(),
            lxd: lxd_info_answers(),
            ecrou: ecrou_probe_answers(),
            ssh: binary_on_path("ssh"),
        }
    }

    /// The offered set for `profile` from these facts: `local` always,
    /// the machine answers as they stand, and `ssh`/`http` only when
    /// the profile's own `sandbox` section names a host or a URL.
    pub fn offered_for(self, profile: &Profile) -> SandboxAvailability {
        SandboxAvailability::from_parts(self.docker, self.lxd, self.ecrou, self.ssh, profile)
    }
}

/// The sandbox kinds this machine and this profile can run `bash`
/// under right now: the picker's rows, and the gates the startup
/// resolution filters every tier through. `local` needs no probing and
/// is always offered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SandboxAvailability {
    /// `docker info` answered within the budget.
    pub docker: bool,
    /// `lxc info` answered within the budget.
    pub lxd: bool,
    /// `ecrou probe` answered within the budget: KVM working,
    /// libkrun loadable, the cache writable.
    pub ecrou: bool,
    /// The `ssh` binary exists and the profile names a host.
    pub ssh: bool,
    /// The profile names a sandbox URL. Never network-probed: a daemon
    /// that is down must still be selectable, and the turn reports the
    /// failure in-band.
    pub http: bool,
}

impl SandboxAvailability {
    /// The offered set from injected machine facts — the testable seam
    /// the probe path goes through. `ssh` is offered only when the binary
    /// exists AND the profile names a host; `http` only when a URL is
    /// set (`RHEMA_SANDBOX_URL` included).
    pub(crate) fn from_parts(
        docker_answers: bool,
        lxd_answers: bool,
        ecrou_answers: bool,
        ssh_present: bool,
        profile: &Profile,
    ) -> Self {
        Self {
            docker: docker_answers,
            lxd: lxd_answers,
            ecrou: ecrou_answers,
            ssh: ssh_present && profile.sandbox_host().is_some(),
            http: profile.sandbox_url().is_some(),
        }
    }

    /// Whether `choice` can run here: `local` always, the rest only
    /// when their probe answered.
    pub fn offers(self, choice: SandboxChoice) -> bool {
        match choice {
            SandboxChoice::Local => true,
            SandboxChoice::Docker => self.docker,
            SandboxChoice::Lxd => self.lxd,
            SandboxChoice::Ecrou => self.ecrou,
            SandboxChoice::Ssh => self.ssh,
            SandboxChoice::Http => self.http,
        }
    }

    /// The offered kinds, in the spec's canonical order — the picker's
    /// row order.
    pub fn offered(self) -> Vec<SandboxChoice> {
        [
            SandboxChoice::Local,
            SandboxChoice::Docker,
            SandboxChoice::Lxd,
            SandboxChoice::Ecrou,
            SandboxChoice::Ssh,
            SandboxChoice::Http,
        ]
        .into_iter()
        .filter(|choice| self.offers(*choice))
        .collect()
    }
}

/// Whether `lxc` answers at all: `lxc info` exiting zero within the
/// budget. A missing binary is the common "no" — treated exactly like
/// a refusing daemon.
pub(crate) fn lxd_info_answers() -> bool {
    let Ok(mut child) = std::process::Command::new("lxc")
        .arg("info")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return false;
    };
    let deadline = Instant::now() + LXD_PROBE_BUDGET;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                // Reap the killed child: the TUI is long-lived and an
                // unreaped probe lingers as a zombie until exit.
                let _ = child.wait();
                return false;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(25)),
            Err(_) => return false,
        }
    }
}

/// Whether `ecrou` answers at all: `ecrou probe` exiting zero within
/// the budget. A missing binary is the common "no" — the host cannot
/// boot machines, and the picker says so.
pub(crate) fn ecrou_probe_answers() -> bool {
    let Ok(mut child) = std::process::Command::new("ecrou")
        .arg("probe")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return false;
    };
    let deadline = Instant::now() + ECROU_PROBE_BUDGET;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                // Reap the killed child: the TUI is long-lived and an
                // unreaped probe lingers as a zombie until exit.
                let _ = child.wait();
                return false;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(25)),
            Err(_) => return false,
        }
    }
}

/// Whether `docker` answers at all: `docker info` exiting zero within
/// the budget. A missing binary is the common "no" — treated exactly
/// like a refusing daemon.
pub(crate) fn docker_info_answers() -> bool {
    let Ok(mut child) = std::process::Command::new("docker")
        .arg("info")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return false;
    };
    let deadline = Instant::now() + DOCKER_PROBE_BUDGET;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
            // Timed out or the wait failed: kill and read as "no".
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
        }
    }
}

/// Whether a binary of this name sits on `PATH` — the ssh probe, which
/// must not spawn a process to learn the spawn would work. Both
/// spellings Windows installs answer.
pub(crate) fn binary_on_path(name: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path)
        .any(|dir| dir.join(name).is_file() || dir.join(format!("{name}.exe")).is_file())
}
