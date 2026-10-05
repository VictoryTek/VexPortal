//! Jobs: one operation the user started, in the daemon or in the built-in terminal.
//!
//! A job outlives whatever view first showed it. Its log lives here rather than in a
//! widget, so closing a dialog loses nothing and the Activity page can reopen it.

use crate::dbus_client::Event;
use crate::ui::ansi;

use gtk::prelude::*;

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::{Duration, Instant};

/// Beyond this the log costs more than the scrollback is worth; the oldest lines are
/// dropped, and the journal has the full record either way.
const MAX_LINES: i32 = 5000;

/// How long output for an unknown job id is kept waiting for its `Started`.
const ORPHAN_TTL: Duration = Duration::from_secs(60);

/// Matches `executor::STREAM_STDERR` in the daemon.
pub const STDERR: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobState {
    /// Sent to the daemon, waiting for polkit and a job id.
    Starting,
    Running,
    Finished(i32),
    /// The user dismissed the authorization prompt.
    Declined,
    /// Refused before it started: validation, the daemon, or a spawn failure.
    NotStarted(String),
    /// The built-in terminal could not start, so the command went to the desktop's
    /// own terminal, where VexPortal cannot follow it.
    External,
}

impl JobState {
    pub fn is_active(&self) -> bool {
        matches!(self, JobState::Starting | JobState::Running)
    }

    pub fn succeeded(&self) -> bool {
        matches!(self, JobState::Finished(0))
    }

    /// One short line for status rows and toasts.
    pub fn label(&self) -> String {
        match self {
            JobState::Starting => "Waiting for authorization…".to_string(),
            JobState::Running => "Running…".to_string(),
            JobState::Finished(0) => "Finished".to_string(),
            JobState::Finished(code) => format!("Failed (exit code {code})"),
            JobState::Declined => "Not authorized".to_string(),
            JobState::NotStarted(_) => "Could not start".to_string(),
            JobState::External => "Opened in a terminal window".to_string(),
        }
    }
}

type Listener = Box<dyn Fn(&Job)>;

pub struct Job {
    pub title: String,
    /// What actually runs, e.g. `just service restart plex`.
    pub command_line: String,
    pub terminal: bool,
    pub started_at: Instant,
    state: RefCell<JobState>,
    job_id: RefCell<Option<String>>,
    /// The daemon job's output. Terminal jobs keep theirs in the terminal widget.
    pub log: gtk::TextBuffer,
    /// The dialog showing this job, while one is open, so it is raised rather than
    /// opened twice.
    pub dialog: RefCell<Option<glib::WeakRef<adw::Dialog>>>,
    listeners: RefCell<Vec<Listener>>,
}

impl Job {
    pub fn new(title: &str, command_line: String, terminal: bool) -> Rc<Self> {
        let log = gtk::TextBuffer::new(None);
        register_tags(&log);
        Rc::new(Job {
            title: title.to_string(),
            command_line,
            terminal,
            started_at: Instant::now(),
            state: RefCell::new(if terminal {
                JobState::Running
            } else {
                JobState::Starting
            }),
            job_id: RefCell::new(None),
            log,
            dialog: RefCell::new(None),
            listeners: RefCell::new(Vec::new()),
        })
    }

    pub fn state(&self) -> JobState {
        self.state.borrow().clone()
    }

    pub fn job_id(&self) -> Option<String> {
        self.job_id.borrow().clone()
    }

    /// Called whenever the state changes. Listeners must not hold strong references to
    /// widgets they do not own: jobs live for the whole session.
    pub fn connect_changed(&self, listener: impl Fn(&Job) + 'static) {
        self.listeners.borrow_mut().push(Box::new(listener));
    }

    pub fn set_state(&self, state: JobState) {
        *self.state.borrow_mut() = state;
        // Take the listeners out while calling them, so one may register another
        // (a view opened from a toast) without a re-entrant borrow.
        let listeners = std::mem::take(&mut *self.listeners.borrow_mut());
        for listener in &listeners {
            listener(self);
        }
        let mut current = self.listeners.borrow_mut();
        let added = std::mem::replace(&mut *current, listeners);
        current.extend(added);
    }

    /// Apply one daemon event routed to this job.
    pub fn apply(&self, event: &Event) {
        match event {
            Event::Started { job_id, .. } => {
                *self.job_id.borrow_mut() = Some(job_id.clone());
                self.set_state(JobState::Running);
            }
            Event::Failed { message, .. } => {
                if crate::dbus_client::is_declined(message) {
                    self.set_state(JobState::Declined);
                } else {
                    self.append(message, Some("stderr"));
                    self.set_state(JobState::NotStarted(message.clone()));
                }
            }
            Event::Output { stream, line, .. } => {
                self.append(line, (*stream == STDERR).then_some("stderr"));
            }
            Event::Finished { exit_code, .. } => self.set_state(JobState::Finished(*exit_code)),
        }
    }

    /// Append one line, styling any escape sequences it carries.
    pub fn append(&self, line: &str, force_tag: Option<&str>) {
        let buffer = &self.log;
        let mut end = buffer.end_iter();
        if buffer.char_count() > 0 {
            buffer.insert(&mut end, "\n");
        }
        for segment in ansi::parse(line) {
            let mut end = buffer.end_iter();
            let tag = force_tag
                .map(str::to_string)
                .or_else(|| segment.style.tag_name());
            match tag {
                Some(tag) => buffer.insert_with_tags_by_name(&mut end, &segment.text, &[&tag]),
                None => buffer.insert(&mut end, &segment.text),
            }
        }

        let overflow = buffer.line_count() - MAX_LINES;
        if overflow > 0 {
            if let Some(mut cut) = buffer.iter_at_line(overflow) {
                buffer.delete(&mut buffer.start_iter(), &mut cut);
            }
        }
    }

    /// Plain text of the log, for the copy button.
    pub fn text(&self) -> String {
        self.log
            .text(&self.log.start_iter(), &self.log.end_iter(), false)
            .to_string()
    }
}

/// Routes daemon events to whatever is waiting for them.
///
/// The daemon starts a job before it replies to `RunRecipe`, and the reply and the
/// output signals reach the GUI on independent tasks, so `Output` and even `Finished`
/// can arrive before `Started` names the job. Those are held here and replayed once
/// the job id is known, instead of being dropped (and a page left spinning forever).
pub struct Router<T: Clone> {
    pending: HashMap<u64, T>,
    running: HashMap<String, T>,
    orphans: HashMap<String, (Instant, Vec<Event>)>,
}

impl<T: Clone> Default for Router<T> {
    fn default() -> Self {
        Self {
            pending: HashMap::new(),
            running: HashMap::new(),
            orphans: HashMap::new(),
        }
    }
}

impl<T: Clone> Router<T> {
    /// A request has been sent; its `Started` or `Failed` will carry `request_id`.
    pub fn expect(&mut self, request_id: u64, target: T) {
        self.pending.insert(request_id, target);
    }

    /// Route one event, returning every delivery it unlocks, in order.
    pub fn route(&mut self, event: Event) -> Vec<(T, Event)> {
        self.orphans
            .retain(|_, (since, _)| since.elapsed() < ORPHAN_TTL);

        match &event {
            Event::Started { request_id, job_id } => {
                let Some(target) = self.pending.remove(request_id) else {
                    log::warn!("`Started` for unknown request {request_id}");
                    return Vec::new();
                };
                let mut deliveries = vec![(target.clone(), event.clone())];
                let mut finished = false;
                if let Some((_, held)) = self.orphans.remove(job_id) {
                    for held in held {
                        finished |= matches!(held, Event::Finished { .. });
                        deliveries.push((target.clone(), held));
                    }
                }
                if !finished {
                    self.running.insert(job_id.clone(), target);
                }
                deliveries
            }
            Event::Failed { request_id, .. } => match self.pending.remove(request_id) {
                Some(target) => vec![(target, event)],
                None => Vec::new(),
            },
            Event::Output { job_id, .. } => match self.running.get(job_id) {
                Some(target) => vec![(target.clone(), event)],
                None => {
                    self.hold(event);
                    Vec::new()
                }
            },
            Event::Finished { job_id, .. } => match self.running.remove(job_id) {
                Some(target) => vec![(target, event)],
                None => {
                    self.hold(event);
                    Vec::new()
                }
            },
        }
    }

    fn hold(&mut self, event: Event) {
        let job_id = match &event {
            Event::Output { job_id, .. } | Event::Finished { job_id, .. } => job_id.clone(),
            _ => return,
        };
        self.orphans
            .entry(job_id)
            .or_insert_with(|| (Instant::now(), Vec::new()))
            .1
            .push(event);
    }
}

fn register_tags(buffer: &gtk::TextBuffer) {
    let table = buffer.tag_table();

    let stderr = gtk::TextTag::builder()
        .name("stderr")
        .foreground(ansi::Color::Red.css())
        .build();
    table.add(&stderr);

    // One tag per style the ANSI parser can produce, named the same way.
    for color in ansi::Color::ALL {
        for (suffix, bold, dim) in [
            ("", false, false),
            ("-bold", true, false),
            ("-dim", false, true),
            ("-bold-dim", true, true),
        ] {
            let tag = gtk::TextTag::builder()
                .name(format!("{}{suffix}", color_tag_name(color)))
                .foreground(color.css())
                .build();
            if bold {
                tag.set_weight(700);
            }
            if dim {
                tag.set_foreground_rgba(Some(&dimmed(color)));
            }
            table.add(&tag);
        }
    }

    for (name, bold, dim) in [
        ("-bold", true, false),
        ("-dim", false, true),
        ("-bold-dim", true, true),
    ] {
        let tag = gtk::TextTag::builder().name(name).build();
        if bold {
            tag.set_weight(700);
        }
        if dim {
            tag.set_foreground(Some("#9a9996"));
        }
        table.add(&tag);
    }
}

fn color_tag_name(color: ansi::Color) -> &'static str {
    match color {
        ansi::Color::Red => "red",
        ansi::Color::Green => "green",
        ansi::Color::Yellow => "yellow",
        ansi::Color::Blue => "blue",
        ansi::Color::Magenta => "magenta",
        ansi::Color::Cyan => "cyan",
        ansi::Color::Grey => "grey",
    }
}

fn dimmed(color: ansi::Color) -> gtk::gdk::RGBA {
    let mut rgba: gtk::gdk::RGBA = color.css().parse().unwrap_or(gtk::gdk::RGBA::BLACK);
    rgba.set_alpha(0.65);
    rgba
}

#[cfg(test)]
mod tests {
    use super::*;

    fn output(job: &str, line: &str) -> Event {
        Event::Output {
            job_id: job.into(),
            stream: 0,
            line: line.into(),
        }
    }

    fn finished(job: &str, exit_code: i32) -> Event {
        Event::Finished {
            job_id: job.into(),
            exit_code,
        }
    }

    fn started(request_id: u64, job: &str) -> Event {
        Event::Started {
            request_id,
            job_id: job.into(),
        }
    }

    #[test]
    fn events_in_order_are_delivered_directly() {
        let mut router = Router::default();
        router.expect(1, "page");
        assert_eq!(router.route(started(1, "j")).len(), 1);
        assert_eq!(router.route(output("j", "hello")).len(), 1);
        assert_eq!(router.route(finished("j", 0)).len(), 1);
        // Finished releases the job; later strays are held, not delivered.
        assert!(router.route(output("j", "late")).is_empty());
    }

    #[test]
    fn output_and_finish_before_started_are_replayed_in_order() {
        // L1: the daemon's signals can overtake its method reply.
        let mut router = Router::default();
        router.expect(7, "page");
        assert!(router.route(output("j", "one")).is_empty());
        assert!(router.route(output("j", "two")).is_empty());
        assert!(router.route(finished("j", 0)).is_empty());

        let deliveries = router.route(started(7, "j"));
        let lines: Vec<String> = deliveries
            .iter()
            .map(|(_, e)| match e {
                Event::Started { .. } => "started".to_string(),
                Event::Output { line, .. } => line.clone(),
                Event::Finished { exit_code, .. } => format!("finished {exit_code}"),
                Event::Failed { .. } => "failed".to_string(),
            })
            .collect();
        assert_eq!(lines, ["started", "one", "two", "finished 0"]);

        // The job already finished, so it must not be left registered as running.
        assert!(router.running.is_empty());
    }

    #[test]
    fn a_failed_request_is_delivered_once() {
        let mut router = Router::default();
        router.expect(3, "page");
        let failed = Event::Failed {
            request_id: 3,
            message: "nope".into(),
        };
        assert_eq!(router.route(failed.clone()).len(), 1);
        assert!(router.route(failed).is_empty());
    }

    #[test]
    fn output_for_someone_else_never_reaches_this_page() {
        let mut router = Router::default();
        router.expect(1, "mine");
        router.route(started(1, "j1"));
        assert!(router.route(output("other", "x")).is_empty());
    }
}
