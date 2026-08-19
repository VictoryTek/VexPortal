//! Watching a recipe run.
//!
//! One page per job: a status header, the live output, and a Cancel button that turns
//! into a result banner when the job ends.

use crate::app::App;
use crate::ui::{ansi, window::Window};

use adw::prelude::*;
use std::cell::RefCell;
use std::rc::Rc;
use vexportal_catalog::Recipe;

/// Beyond this the log view starts costing more than the scrollback is worth; the
/// oldest lines are dropped, and the journal has the full record either way.
const MAX_LINES: i32 = 5000;

struct State {
    job_id: Option<String>,
    finished: bool,
}

#[derive(Clone)]
pub struct RunPage {
    page: adw::NavigationPage,
    buffer: gtk::TextBuffer,
    view: gtk::TextView,
    scroller: gtk::ScrolledWindow,
    spinner: gtk::Spinner,
    status: gtk::Label,
    banner: adw::Banner,
    cancel: gtk::Button,
    state: Rc<RefCell<State>>,
    app: Rc<App>,
}

impl RunPage {
    pub fn new(app: &Rc<App>, window: &Window, recipe: &Recipe) -> Self {
        let buffer = gtk::TextBuffer::new(None);
        register_tags(&buffer);

        let view = gtk::TextView::builder()
            .buffer(&buffer)
            .editable(false)
            .cursor_visible(false)
            .monospace(true)
            .wrap_mode(gtk::WrapMode::WordChar)
            .build();
        view.add_css_class("vex-log");

        let scroller = gtk::ScrolledWindow::builder()
            .child(&view)
            .vexpand(true)
            .hscrollbar_policy(gtk::PolicyType::Automatic)
            .build();
        scroller.add_css_class("vex-log-view");

        let spinner = gtk::Spinner::new();
        spinner.start();
        let status = gtk::Label::new(Some("Waiting for authorization…"));
        status.set_xalign(0.0);
        status.set_hexpand(true);

        let cancel = gtk::Button::with_label("Cancel");
        cancel.add_css_class("destructive-action");
        cancel.set_valign(gtk::Align::Center);
        cancel.set_sensitive(false);

        let header_row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        header_row.set_margin_start(12);
        header_row.set_margin_end(12);
        header_row.set_margin_top(12);
        header_row.append(&spinner);
        header_row.append(&status);
        header_row.append(&cancel);

        let banner = adw::Banner::new("");

        let content = gtk::Box::new(gtk::Orientation::Vertical, 12);
        content.append(&banner);
        content.append(&header_row);

        let log_frame = gtk::Box::new(gtk::Orientation::Vertical, 0);
        log_frame.set_margin_start(12);
        log_frame.set_margin_end(12);
        log_frame.set_margin_bottom(12);
        log_frame.append(&scroller);
        content.append(&log_frame);

        let toolbar = adw::ToolbarView::new();
        let header = adw::HeaderBar::new();
        let copy = gtk::Button::from_icon_name("edit-copy-symbolic");
        copy.set_tooltip_text(Some("Copy output"));
        header.pack_end(&copy);
        toolbar.add_top_bar(&header);
        toolbar.set_content(Some(&content));

        let page = adw::NavigationPage::builder()
            .title(&recipe.title)
            .child(&toolbar)
            .build();

        let this = RunPage {
            page,
            buffer: buffer.clone(),
            view,
            scroller,
            spinner,
            status,
            banner,
            cancel: cancel.clone(),
            state: Rc::new(RefCell::new(State {
                job_id: None,
                finished: false,
            })),
            app: app.clone(),
        };

        copy.connect_clicked({
            let buffer = buffer.clone();
            let window = window.clone();
            move |_| {
                let text = buffer.text(&buffer.start_iter(), &buffer.end_iter(), false);
                if let Some(display) = gtk::gdk::Display::default() {
                    display.clipboard().set_text(&text);
                    window.toast("Output copied");
                }
            }
        });

        cancel.connect_clicked({
            let this = this.clone();
            move |_| this.request_cancel()
        });

        this
    }

    pub fn navigation_page(&self) -> &adw::NavigationPage {
        &self.page
    }

    /// The daemon accepted the request and gave it a job id.
    pub fn attach(&self, job_id: &str) {
        self.state.borrow_mut().job_id = Some(job_id.to_string());
        self.status.set_text("Running…");
        self.cancel.set_sensitive(true);
    }

    /// The request was refused before it started.
    pub fn failed_to_start(&self, message: &str) {
        self.stop();
        self.status.set_text("Could not start");
        self.banner.set_title(message);
        self.banner.set_revealed(true);
        self.append_plain(message);
    }

    /// The user dismissed the polkit prompt. Not an error — just say so and stop.
    pub fn declined(&self) {
        self.stop();
        self.status.set_text("Cancelled — not authorized");
        self.banner
            .set_title("This operation needs administrator authorization to run.");
        self.banner.set_revealed(true);
    }

    pub fn append(&self, stream: u32, line: &str) {
        let tag = (stream == crate::ui::run_page::STDERR).then_some("stderr");
        self.append_line(line, tag);
    }

    pub fn finished(&self, exit_code: i32) {
        self.state.borrow_mut().finished = true;
        self.stop();
        if exit_code == 0 {
            self.status.set_text("Finished");
        } else {
            self.status.set_text(&format!("Failed (exit code {exit_code})"));
            self.banner
                .set_title("This operation did not complete. The output above says why.");
            self.banner.set_revealed(true);
        }
    }

    fn request_cancel(&self) {
        let state = self.state.borrow();
        if let Some(job_id) = &state.job_id {
            if !state.finished {
                self.app.cancel(job_id);
                self.status.set_text("Cancelling…");
                self.cancel.set_sensitive(false);
            }
        }
    }

    fn stop(&self) {
        self.spinner.stop();
        self.spinner.set_visible(false);
        self.cancel.set_sensitive(false);
    }

    fn append_plain(&self, line: &str) {
        self.append_line(line, Some("stderr"));
    }

    /// Append one line, styling any escape sequences it carries.
    fn append_line(&self, line: &str, force_tag: Option<&str>) {
        let mut end = self.buffer.end_iter();
        if self.buffer.char_count() > 0 {
            self.buffer.insert(&mut end, "\n");
        }

        for segment in ansi::parse(line) {
            let mut end = self.buffer.end_iter();
            let tag = force_tag
                .map(str::to_string)
                .or_else(|| segment.style.tag_name());
            match tag {
                Some(tag) => {
                    self.buffer
                        .insert_with_tags_by_name(&mut end, &segment.text, &[&tag]);
                }
                None => self.buffer.insert(&mut end, &segment.text),
            }
        }

        self.trim();
        self.scroll_to_end();
    }

    fn trim(&self) {
        let overflow = self.buffer.line_count() - MAX_LINES;
        if overflow <= 0 {
            return;
        }
        let start = self.buffer.start_iter();
        if let Some(cut) = self.buffer.iter_at_line(overflow) {
            self.buffer.delete(&mut start.clone(), &mut cut.clone());
        }
    }

    /// Follow the output, but only while the user is already at the bottom — yanking
    /// the view back down while someone is reading earlier output is maddening.
    fn scroll_to_end(&self) {
        let adjustment = self.scroller.vadjustment();
        let at_bottom =
            adjustment.value() + adjustment.page_size() >= adjustment.upper() - 64.0;
        if !at_bottom {
            return;
        }
        let mark = self.buffer.create_mark(None, &self.buffer.end_iter(), false);
        self.view
            .scroll_to_mark(&mark, 0.0, false, 0.0, 0.0);
        self.buffer.delete_mark(&mark);
    }
}

/// Matches `executor::STREAM_STDERR` in the daemon.
pub const STDERR: u32 = 1;

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

    for (name, bold, dim) in [("-bold", true, false), ("-dim", false, true), ("-bold-dim", true, true)] {
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
