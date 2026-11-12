//! Updating the app (Settings, General, "On this computer"): the version
//! that runs, and a button that looks for a newer one.
//!
//! Every step is the user's click: looking, downloading, restarting. The
//! app never looks for a release by itself, and a version that was
//! downloaded waits until the user restarts into it. What a release must
//! be to be taken is `atroxabot_core::update`'s.

use atroxabot_core::update::{DOWNLOADS, Job, Release, Report, Staged};

use super::Shell;
use super::settings::Settings;
use super::widgets::row;
use crate::app::Action;
use crate::paint::{Painter, Rect};
use crate::ui::{ButtonStyle, Ui, button, icon};

/// How far updating has come since the app started.
#[derive(Debug, Default)]
pub(super) enum Updater {
    /// Nobody asked yet.
    #[default]
    Idle,
    /// Looking for a newer version.
    Checking,
    /// Looked: this is the newest.
    Newest,
    /// A newer version is out.
    Available(Release),
    /// It is being downloaded.
    Downloading {
        /// The version.
        release: Release,
        /// Bytes so far.
        done: u64,
        /// Bytes in all.
        size: u64,
    },
    /// It is downloaded and checked: a restart away.
    Ready(Staged),
    /// It is in place, and the app is closing to start it.
    Restarting,
    /// Something went wrong; why.
    Failed(String),
}

impl Updater {
    /// Takes in what a job came to, or how far a download is.
    fn report(&mut self, report: Report) {
        *self = match (std::mem::take(self), report) {
            (_, Report::Checked(Ok(Some(release)))) => Self::Available(release),
            (_, Report::Checked(Ok(None))) => Self::Newest,
            (Self::Downloading { release, .. }, Report::Progress { done, size }) => {
                Self::Downloading { release, done, size }
            }
            (_, Report::Downloaded(Ok(staged))) => Self::Ready(staged),
            (_, Report::Checked(Err(why)) | Report::Downloaded(Err(why))) => Self::Failed(why),
            // Progress of a download nobody waits for any more.
            (state, Report::Progress { .. }) => state,
        };
    }

    /// The line under the app's name.
    fn detail(&self) -> String {
        match self {
            Self::Idle => "Looks for a newer version when you ask, never by itself.".into(),
            Self::Checking => "Looking for a newer version…".into(),
            Self::Newest => "This is the newest version.".into(),
            Self::Available(release) => format!("Version {} is out.", release.version),
            Self::Downloading { release, done, size } => {
                format!("Downloading version {}: {}%", release.version, done * 100 / (*size).max(1))
            }
            Self::Ready(staged) => format!("Version {} is ready: restart to use it.", staged.version),
            Self::Restarting => "Restarting…".into(),
            Self::Failed(_) => "The update did not work.".into(),
        }
    }
}

impl Shell {
    /// What an update job came to.
    pub fn update_report(&mut self, report: Report) {
        self.updater.report(report);
    }

    /// The new version could not be moved in: nothing changed.
    pub fn update_failed(&mut self, why: String) {
        self.updater = Updater::Failed(why);
    }
}

/// The app's own row in the settings: its version, and the button that
/// updates it. Returns where what follows starts.
pub(super) fn draw(
    p: &mut Painter,
    ui: &mut Ui,
    (x, y, w): (f32, f32, f32),
    shell: &mut Shell,
    actions: &mut Vec<Action>,
) -> f32 {
    let rect = Rect::new(x, y, w, 58.0);
    let title = concat!("AtroxaBot ", env!("CARGO_PKG_VERSION"));
    let at = |width: f32| Rect::new(rect.right() - 12.0 - width, rect.y + 13.0, width, 32.0);
    let room = match shell.updater {
        Updater::Failed(_) => 250.0,
        Updater::Restarting => 0.0,
        _ => 180.0,
    };
    row(p, rect, (icon::REFRESH, None), title, &shell.updater.detail(), room);
    let mut next = None;
    match &shell.updater {
        Updater::Idle | Updater::Newest | Updater::Checking => {
            let idle = !matches!(shell.updater, Updater::Checking);
            if button(p, ui, at(160.0), "Check for updates", ButtonStyle::Secondary, idle) {
                actions.push(Action::Update(Job::Check));
                next = Some(Updater::Checking);
            }
        }
        Updater::Available(release) => {
            if button(p, ui, at(100.0), "Update", ButtonStyle::Primary, true) {
                actions.push(Action::Update(Job::Download(release.clone())));
                next = Some(Updater::Downloading { release: release.clone(), done: 0, size: 1 });
            }
        }
        Updater::Downloading { done, size, .. } => {
            let track = Rect::new(rect.right() - 172.0, rect.y + 26.0, 160.0, 6.0);
            let (trough, filled) = (p.theme.hover, p.theme.text_muted);
            p.rect(track, trough, 3.0);
            let part = (*done as f32 / (*size).max(1) as f32).clamp(0.0, 1.0);
            p.rect(Rect::new(track.x, track.y, (track.w * part).max(6.0), track.h), filled, 3.0);
        }
        Updater::Ready(staged) => {
            if button(p, ui, at(160.0), "Restart to update", ButtonStyle::Primary, true) {
                actions.push(Action::InstallUpdate(staged.clone()));
                next = Some(Updater::Restarting);
            }
        }
        Updater::Restarting => {}
        Updater::Failed(_) => {
            if button(p, ui, at(100.0), "Try again", ButtonStyle::Secondary, true) {
                actions.push(Action::Update(Job::Check));
                next = Some(Updater::Checking);
            }
            let page = Rect::new(rect.right() - 12.0 - 100.0 - 8.0 - 124.0, rect.y + 13.0, 124.0, 32.0);
            if button(p, ui, page, "Download page", ButtonStyle::Ghost, true) {
                actions.push(Action::OpenLink(DOWNLOADS.into()));
            }
        }
    }
    let mut y = rect.bottom();
    // Under the row, in full: why it failed, or what a restart costs.
    let under = match &shell.updater {
        Updater::Failed(why) => Some(why.as_str()),
        Updater::Ready(_) if shell.is_busy() => Some("An agent is working: restarting stops it."),
        _ => None,
    };
    if let Some(text) = under {
        y = Settings::note(p, (x + 2.0, y + 8.0, w - 4.0), text);
    }
    if let Some(next) = next {
        shell.updater = next;
    }
    y
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_move_the_update_along() {
        let mut updater = Updater::Checking;
        updater.report(Report::Checked(Ok(None)));
        assert!(matches!(updater, Updater::Newest));
        assert_eq!(updater.detail(), "This is the newest version.");
        // Progress with no download running changes nothing.
        updater.report(Report::Progress { done: 1, size: 2 });
        assert!(matches!(updater, Updater::Newest));
        updater.report(Report::Checked(Err("offline".into())));
        assert!(matches!(&updater, Updater::Failed(why) if why == "offline"));
        updater.report(Report::Downloaded(Err("no room".into())));
        assert!(matches!(&updater, Updater::Failed(why) if why == "no room"));
    }
}
