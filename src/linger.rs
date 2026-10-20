//! systemd "linger" for the autostart service (Linux only).
//!
//! A systemd user service starts at login unless linger is on for the user.
//! `setup`/`enable` turn it on with plain `loginctl enable-linger` (no
//! arguments, so it acts on the caller; polkit allows that for a local active
//! session) and record that in `setup.json`. `disable`, `uninstall` and
//! `setup --undo` turn it off again only when that record exists, so a linger
//! the user enabled by hand is never touched.
//!
//! This code never runs `sudo`. Every call to `loginctl` goes through the
//! [`Loginctl`] trait so tests use a fake; [`system`] returns the real one only
//! on Linux and never when `CCP_SERVICE_DIR` puts the service in file-only mode.

use anyhow::{Context, Result, bail};
use std::process::Command;

/// The three `loginctl` operations this program needs.
pub trait Loginctl {
    /// `Some(true)` when linger is on for the current user; `None` when unknown.
    fn is_enabled(&self) -> Option<bool>;
    fn enable(&self) -> Result<()>;
    fn disable(&self) -> Result<()>;
}

/// The real `loginctl`.
pub struct SystemLoginctl;

fn current_user() -> Option<String> {
    if let Some(user) = std::env::var("USER").ok().filter(|u| !u.is_empty()) {
        return Some(user);
    }
    let out = Command::new("id").arg("-un").output().ok()?;
    let name = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (out.status.success() && !name.is_empty()).then_some(name)
}

fn run_loginctl(args: &[&str]) -> Result<()> {
    let out = Command::new("loginctl")
        .args(args)
        .output()
        .with_context(|| format!("failed to run `loginctl {}`", args.join(" ")))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        bail!("`loginctl {}` failed: {}", args.join(" "), stderr.trim());
    }
    Ok(())
}

impl Loginctl for SystemLoginctl {
    fn is_enabled(&self) -> Option<bool> {
        let user = current_user()?;
        let out = Command::new("loginctl")
            .args(["show-user", &user, "-p", "Linger"])
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        Some(String::from_utf8_lossy(&out.stdout).trim() == "Linger=yes")
    }

    fn enable(&self) -> Result<()> {
        run_loginctl(&["enable-linger"])
    }

    fn disable(&self) -> Result<()> {
        run_loginctl(&["disable-linger"])
    }
}

/// The real backend, or `None` where linger does not apply or must not be
/// touched: not Linux, or `CCP_SERVICE_DIR` is set (sandbox / file-only mode).
pub fn system() -> Option<SystemLoginctl> {
    (cfg!(target_os = "linux") && std::env::var_os(crate::service::SERVICE_DIR_ENV).is_none())
        .then_some(SystemLoginctl)
}

/// Manual commands shown when linger could not be enabled. The first needs no
/// privileges; `sudo` is only ever suggested, never run.
pub const MANUAL_HINT: &str = "run `loginctl enable-linger` (or, only if that is denied, `sudo loginctl enable-linger $USER`)";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnableOutcome {
    AlreadyOn,
    /// Dry run: linger is not on and would be enabled.
    WouldEnable,
    Enabled,
    Failed(String),
}

/// Turn linger on unless it already is. Reads, then enables, then reads again.
pub fn ensure(backend: &dyn Loginctl, dry_run: bool) -> EnableOutcome {
    if backend.is_enabled() == Some(true) {
        return EnableOutcome::AlreadyOn;
    }
    if dry_run {
        return EnableOutcome::WouldEnable;
    }
    if let Err(err) = backend.enable() {
        return EnableOutcome::Failed(format!("{err:#}"));
    }
    match backend.is_enabled() {
        Some(true) => EnableOutcome::Enabled,
        _ => EnableOutcome::Failed("loginctl succeeded but linger is not reported as on".into()),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReleaseOutcome {
    /// Setup did not enable linger: nothing is done.
    NotRecorded,
    WouldRelease,
    Released,
    /// Linger was already off (someone else turned it off); only the record goes.
    AlreadyOff,
    Failed(String),
}

/// Turn linger off, but only when `recorded` says setup turned it on.
pub fn release(backend: &dyn Loginctl, recorded: bool, dry_run: bool) -> ReleaseOutcome {
    if !recorded {
        return ReleaseOutcome::NotRecorded;
    }
    if dry_run {
        return ReleaseOutcome::WouldRelease;
    }
    if backend.is_enabled() == Some(false) {
        return ReleaseOutcome::AlreadyOff;
    }
    match backend.disable() {
        Ok(()) => ReleaseOutcome::Released,
        Err(err) => ReleaseOutcome::Failed(format!("{err:#}")),
    }
}

/// Text for a [`ReleaseOutcome`] in the commands' reports, or `None` when there
/// is nothing to say.
pub fn release_message(outcome: &ReleaseOutcome) -> Option<String> {
    match outcome {
        ReleaseOutcome::NotRecorded => None,
        ReleaseOutcome::WouldRelease => {
            Some("would turn linger off again (setup had turned it on)".into())
        }
        ReleaseOutcome::Released => Some("linger turned off again (setup had turned it on)".into()),
        ReleaseOutcome::AlreadyOff => Some("linger was already off; record cleared".into()),
        ReleaseOutcome::Failed(err) => Some(format!(
            "linger: {err}. Setup turned it on; run `loginctl disable-linger` to turn it off"
        )),
    }
}

#[cfg(test)]
pub(crate) mod fake {
    use super::*;
    use std::cell::{Cell, RefCell};

    /// Fake `loginctl`: remembers the calls it received.
    pub struct FakeLoginctl {
        pub on: Cell<Option<bool>>,
        pub enable_fails: bool,
        pub disable_fails: bool,
        pub calls: RefCell<Vec<&'static str>>,
    }

    impl FakeLoginctl {
        pub fn new(on: Option<bool>) -> Self {
            Self {
                on: Cell::new(on),
                enable_fails: false,
                disable_fails: false,
                calls: RefCell::new(Vec::new()),
            }
        }
    }

    impl Loginctl for FakeLoginctl {
        fn is_enabled(&self) -> Option<bool> {
            self.calls.borrow_mut().push("show");
            self.on.get()
        }
        fn enable(&self) -> Result<()> {
            self.calls.borrow_mut().push("enable");
            if self.enable_fails {
                bail!("Access denied");
            }
            self.on.set(Some(true));
            Ok(())
        }
        fn disable(&self) -> Result<()> {
            self.calls.borrow_mut().push("disable");
            if self.disable_fails {
                bail!("Access denied");
            }
            self.on.set(Some(false));
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::FakeLoginctl;
    use super::*;

    #[test]
    fn already_on_is_a_no_op() {
        let fake = FakeLoginctl::new(Some(true));
        assert_eq!(ensure(&fake, false), EnableOutcome::AlreadyOn);
        assert!(!fake.calls.borrow().contains(&"enable"));
    }

    #[test]
    fn enabling_checks_again() {
        let fake = FakeLoginctl::new(Some(false));
        assert_eq!(ensure(&fake, false), EnableOutcome::Enabled);
        assert_eq!(*fake.calls.borrow(), ["show", "enable", "show"]);
    }

    #[test]
    fn failure_is_reported_not_raised() {
        let mut fake = FakeLoginctl::new(Some(false));
        fake.enable_fails = true;
        assert!(matches!(ensure(&fake, false), EnableOutcome::Failed(e) if e.contains("denied")));
        // loginctl missing: unknown state, enable fails.
        let mut fake = FakeLoginctl::new(None);
        fake.enable_fails = true;
        assert!(matches!(ensure(&fake, false), EnableOutcome::Failed(_)));
    }

    #[test]
    fn success_that_does_not_show_as_on_is_a_failure() {
        let fake = FakeLoginctl::new(None);
        // enable() flips the fake to on, so emulate a backend that lies.
        struct Liar;
        impl Loginctl for Liar {
            fn is_enabled(&self) -> Option<bool> {
                Some(false)
            }
            fn enable(&self) -> Result<()> {
                Ok(())
            }
            fn disable(&self) -> Result<()> {
                Ok(())
            }
        }
        assert!(matches!(ensure(&Liar, false), EnableOutcome::Failed(_)));
        assert_eq!(ensure(&fake, false), EnableOutcome::Enabled);
    }

    #[test]
    fn dry_run_changes_nothing() {
        let fake = FakeLoginctl::new(Some(false));
        assert_eq!(ensure(&fake, true), EnableOutcome::WouldEnable);
        assert!(!fake.calls.borrow().contains(&"enable"));
        let fake = FakeLoginctl::new(Some(true));
        assert_eq!(release(&fake, true, true), ReleaseOutcome::WouldRelease);
        assert!(fake.calls.borrow().is_empty());
    }

    #[test]
    fn release_needs_the_record() {
        let fake = FakeLoginctl::new(Some(true));
        assert_eq!(release(&fake, false, false), ReleaseOutcome::NotRecorded);
        assert!(fake.calls.borrow().is_empty());
        assert_eq!(fake.on.get(), Some(true));
        assert_eq!(release(&fake, true, false), ReleaseOutcome::Released);
        assert_eq!(fake.on.get(), Some(false));
    }

    #[test]
    fn release_failure_and_already_off() {
        let mut fake = FakeLoginctl::new(Some(true));
        fake.disable_fails = true;
        assert!(matches!(
            release(&fake, true, false),
            ReleaseOutcome::Failed(_)
        ));
        let fake = FakeLoginctl::new(Some(false));
        assert_eq!(release(&fake, true, false), ReleaseOutcome::AlreadyOff);
        assert!(!fake.calls.borrow().contains(&"disable"));
    }
}
