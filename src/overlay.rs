use gtk::prelude::*;
use gtk::{DrawingArea, Label, Window, cairo, glib};
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Instant;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum State {
    Hidden,
    Recording,
    Processing,
    Done,
    Error,
}

impl State {
    fn accent(self) -> (f64, f64, f64) {
        match self {
            State::Recording => (1.00, 0.36, 0.42),  // coral
            State::Processing => (0.45, 0.68, 1.00), // azure
            State::Done => (0.31, 0.85, 0.60),       // mint
            _ => (1.00, 0.62, 0.29),                 // amber
        }
    }
    fn label(self) -> &'static str {
        match self {
            State::Recording => "Listening",
            State::Processing => "Transcribing",
            State::Done => "Pasted",
            State::Error => "Failed",
            State::Hidden => "",
        }
    }
    fn css(self) -> &'static str {
        match self {
            State::Recording => "recording",
            State::Processing => "processing",
            State::Done => "done",
            State::Error => "error",
            State::Hidden => "hidden",
        }
    }
}

const BOTTOM_MARGIN: i32 = 56;
const SHADOW_PAD: i32 = 28; // room for the drop shadow inside the surface
const VIZ_W: i32 = 60;
const VIZ_H: i32 = 22;
const BARS: usize = 11;
const RISE_PX: f64 = 14.0; // slide distance of the entrance animation
const FADE_MS: f64 = 190.0;

const CSS: &str = "
window { background: none; }
.pill {
  background: linear-gradient(to bottom, rgba(34,34,42,0.88), rgba(16,16,20,0.94));
  border: 1px solid rgba(255,255,255,0.09);
  border-radius: 999px;
  box-shadow: 0 12px 32px rgba(0,0,0,0.55), inset 0 1px 0 rgba(255,255,255,0.07);
  padding: 7px 16px 7px 14px;
}
.pill.recording { border-color: rgba(255,92,107,0.30); }
.pill.processing { border-color: rgba(115,173,255,0.30); }
.pill.done { border-color: rgba(79,217,153,0.32); }
.pill.error { border-color: rgba(255,158,74,0.32); }
.title { color: rgba(240,240,248,0.94); font-size: 12px; font-weight: 600; }
.timer { color: rgba(240,240,248,0.42); font-size: 11px; font-family: monospace; }
.transcript { color: rgba(240,240,248,0.92); font-size: 12px; }
.outer { background: none; }
";

struct Anim {
    /// 0.0 hidden, 1.0 fully shown; eased every frame.
    shown: f64,
    target: f64,
    phase: f64,
    /// Newest sample last; scrolls right to left.
    levels: [f64; BARS],
}

pub struct Overlay {
    window: Window,
    viz: DrawingArea,
    pill: gtk::Box,
    title: Label,
    timer: Label,
    transcript: Label,
    state: Rc<Cell<State>>,
    since: Rc<Cell<Option<Instant>>>,
    anim: Rc<RefCell<Anim>>,
    ticking: Rc<Cell<bool>>,
}

impl Overlay {
    pub fn new(app: &gtk::Application) -> Self {
        let css = gtk::CssProvider::new();
        css.load_from_data(CSS);
        if let Some(display) = gtk::gdk::Display::default() {
            gtk::style_context_add_provider_for_display(
                &display,
                &css,
                gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
            );
        }

        let state = Rc::new(Cell::new(State::Hidden));
        let since = Rc::new(Cell::new(None));
        let anim = Rc::new(RefCell::new(Anim {
            shown: 0.0,
            target: 0.0,
            phase: 0.0,
            levels: [0.0; BARS],
        }));

        let viz = DrawingArea::new();
        viz.set_content_width(VIZ_W);
        viz.set_content_height(VIZ_H);
        viz.set_valign(gtk::Align::Center);
        {
            let (s, a) = (state.clone(), anim.clone());
            viz.set_draw_func(move |_, cr, w, h| draw_viz(cr, w, h, s.get(), &a.borrow()));
        }

        let title = Label::new(Some(State::Recording.label()));
        title.add_css_class("title");
        let timer = Label::new(Some("0:00"));
        timer.add_css_class("timer");
        let transcript = Label::new(None);
        transcript.add_css_class("transcript");
        transcript.set_wrap(true);
        transcript.set_wrap_mode(gtk::pango::WrapMode::WordChar);
        transcript.set_max_width_chars(64);
        transcript.set_justify(gtk::Justification::Left);
        transcript.set_halign(gtk::Align::Start);
        transcript.set_hexpand(true);
        transcript.set_visible(false);

        let pill = gtk::Box::new(gtk::Orientation::Horizontal, 9);
        pill.add_css_class("pill");
        pill.set_halign(gtk::Align::Center);
        pill.set_valign(gtk::Align::End);
        pill.set_margin_start(SHADOW_PAD);
        pill.set_margin_end(SHADOW_PAD);
        pill.set_margin_top(SHADOW_PAD);
        pill.append(&viz);
        pill.append(&title);
        pill.append(&timer);
        pill.append(&transcript);

        let window = Window::builder()
            .application(app)
            .title("Speak Spic")
            .resizable(false)
            .decorated(false)
            .focusable(false)
            .can_focus(false)
            .child(&pill)
            .build();

        window.init_layer_shell();
        window.set_layer(Layer::Overlay);
        window.set_keyboard_mode(KeyboardMode::None);
        window.set_anchor(Edge::Bottom, true);
        window.set_margin(Edge::Bottom, BOTTOM_MARGIN - SHADOW_PAD);

        Self {
            window,
            viz,
            pill,
            title,
            timer,
            transcript,
            state,
            since,
            anim,
            ticking: Rc::new(Cell::new(false)),
        }
    }

    pub fn state(&self) -> State {
        self.state.get()
    }

    pub fn set_state(&self, s: State) {
        let prev = self.state.replace(s);
        if prev == s {
            return;
        }
        self.pill.remove_css_class(prev.css());
        self.pill.add_css_class(s.css());

        if s == State::Hidden {
            self.anim.borrow_mut().target = 0.0;
            self.since.set(None);
            self.transcript.set_visible(false);
            self.transcript.set_text("");
        } else {
            self.title.set_text(s.label());
            self.timer.set_visible(s == State::Recording);
            if s == State::Recording {
                self.since.set(Some(Instant::now()));
                self.timer.set_text("0:00");
                self.anim.borrow_mut().levels = [0.0; BARS];
                // keep transcript visible if we already have interim text
            }
            let mut a = self.anim.borrow_mut();
            a.target = 1.0;
            a.phase = 0.0;
            drop(a);
            self.window.set_visible(true);
        }
        self.start_tick();
    }

    pub fn set_transcript(&self, text: &str) {
        let t = text.trim();
        if t.is_empty() {
            self.transcript.set_visible(false);
            self.transcript.set_text("");
        } else {
            // Show live WhisperFlow-style transcript that rewrites as you speak.
            // Cap by chars (not bytes) to keep the pill from blowing out, but
            // keep enough context to be useful and avoid mid-word cut.
            let capped: String = if t.chars().count() > 320 {
                t.chars().skip(t.chars().count() - 320).collect()
            } else {
                t.to_string()
            };
            self.transcript.set_text(&capped);
            self.transcript.set_visible(true);
        }
    }

    /// One frame-clock driven loop: eases the fade, scrolls the waveform,
    /// ticks the timer. Stops itself once hidden and fully faded out.
    fn start_tick(&self) {
        if self.ticking.replace(true) {
            return;
        }
        let (window, viz, pill, timer) = (
            self.window.clone(),
            self.viz.clone(),
            self.pill.clone(),
            self.timer.clone(),
        );
        let (state, since, anim, ticking) = (
            self.state.clone(),
            self.since.clone(),
            self.anim.clone(),
            self.ticking.clone(),
        );
        let last = Cell::new(None::<i64>);
        self.window.add_tick_callback(move |_, clock| {
            let now = clock.frame_time();
            let dt_ms = match last.get() {
                Some(prev) => ((now - prev) as f64 / 1000.0).clamp(0.0, 64.0),
                None => 16.0,
            };
            last.set(Some(now));

            let st = state.get();
            let mut a = anim.borrow_mut();

            // Ease toward the target with a fixed-duration linear ramp, then
            // shape it: fast-out for the entrance, gentle for the exit.
            let step = dt_ms / FADE_MS;
            a.shown = if a.target > a.shown {
                (a.shown + step).min(1.0)
            } else {
                (a.shown - step).max(0.0)
            };
            let eased = ease_out_cubic(a.shown);
            window.set_opacity(eased);
            pill.set_margin_bottom((SHADOW_PAD as f64 + RISE_PX * (1.0 - eased)) as i32);

            a.phase += dt_ms / 1000.0;
            let level = crate::audio::level();
            a.levels.rotate_left(1);
            let decayed = a.levels[BARS - 2] * 0.82;
            a.levels[BARS - 1] = if st == State::Recording {
                level.max(decayed)
            } else {
                0.0
            };
            drop(a);

            if let Some(t0) = since.get() {
                let s = t0.elapsed().as_secs();
                timer.set_text(&format!("{}:{:02}", s / 60, s % 60));
            }
            // The DrawingArea caches its render node; the window's own
            // queue_draw does not reach it.
            viz.queue_draw();

            if state.get() == State::Hidden && anim.borrow().shown <= 0.0 {
                window.set_visible(false);
                ticking.set(false);
                return glib::ControlFlow::Break;
            }
            glib::ControlFlow::Continue
        });
    }
}

fn ease_out_cubic(t: f64) -> f64 {
    1.0 - (1.0 - t).powi(3)
}

fn draw_viz(cr: &cairo::Context, w: i32, h: i32, state: State, a: &Anim) {
    let (w, h) = (w as f64, h as f64);
    let (r, g, b) = state.accent();
    let cy = h / 2.0;
    cr.set_line_cap(cairo::LineCap::Round);

    match state {
        State::Recording => {
            // Live scrolling waveform: newest sample enters at the right.
            let bw = 2.8;
            let gap = (w - bw) / (BARS - 1) as f64;
            cr.set_line_width(bw);
            for (i, &lv) in a.levels.iter().enumerate() {
                let x = bw / 2.0 + i as f64 * gap;
                // A little idle shimmer so the bar never flatlines to nothing.
                let idle = 0.13 + 0.05 * (a.phase * 3.4 + i as f64 * 0.8).sin();
                let amp = (lv.powf(0.65) + idle).min(1.0);
                let bh = (amp * (h - 3.0)).max(bw);
                let fade = 0.35 + 0.65 * (i as f64 / (BARS - 1) as f64);
                cr.set_source_rgba(r, g, b, fade);
                cr.move_to(x, cy - bh / 2.0);
                cr.line_to(x, cy + bh / 2.0);
                let _ = cr.stroke();
            }
        }
        State::Processing => {
            // Travelling sine wave through the same bar field.
            let bw = 2.8;
            let gap = (w - bw) / (BARS - 1) as f64;
            cr.set_line_width(bw);
            for i in 0..BARS {
                let x = bw / 2.0 + i as f64 * gap;
                let ph = a.phase * 6.0 - i as f64 * 0.45;
                let amp = 0.18 + 0.42 * (ph.sin() * 0.5 + 0.5).powi(2);
                let bh = (amp * (h - 3.0)).max(bw);
                cr.set_source_rgba(r, g, b, 0.30 + 0.60 * (ph.sin() * 0.5 + 0.5));
                cr.move_to(x, cy - bh / 2.0);
                cr.line_to(x, cy + bh / 2.0);
                let _ = cr.stroke();
            }
        }
        State::Done => {
            // Check mark with an expanding ring, centred in the viz area.
            let cx = w / 2.0;
            let pop = (a.phase * 3.2).min(1.0);
            let ring = 5.0 + 6.0 * ease_out_cubic(pop);
            cr.set_line_width(1.4);
            cr.set_source_rgba(r, g, b, 0.35 * (1.0 - pop));
            cr.arc(cx, cy, ring, 0.0, std::f64::consts::TAU);
            let _ = cr.stroke();

            cr.set_line_width(2.0);
            cr.set_line_join(cairo::LineJoin::Round);
            cr.set_source_rgba(r, g, b, 1.0);
            cr.move_to(cx - 5.0, cy + 0.5);
            cr.line_to(cx - 1.5, cy + 4.0);
            cr.line_to(cx + 5.5, cy - 4.0);
            let _ = cr.stroke();
        }
        State::Error => {
            let cx = w / 2.0;
            cr.set_line_width(2.2);
            cr.set_source_rgba(r, g, b, 1.0);
            cr.move_to(cx, cy - 5.0);
            cr.line_to(cx, cy + 1.5);
            let _ = cr.stroke();
            cr.arc(cx, cy + 5.0, 1.3, 0.0, std::f64::consts::TAU);
            let _ = cr.fill();
        }
        State::Hidden => {}
    }
}
