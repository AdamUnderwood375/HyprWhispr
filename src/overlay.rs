use gtk::prelude::*;
use gtk::{DrawingArea, Window, cairo, glib};
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use std::cell::Cell;
use std::rc::Rc;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum State {
    Hidden,
    Recording,
    Processing,
    Done,
    Error,
}

impl State {
    fn color(self) -> (f64, f64, f64) {
        match self {
            State::Recording => (0.93, 0.26, 0.26),
            State::Processing => (0.40, 0.58, 0.96),
            State::Done => (0.30, 0.78, 0.47),
            _ => (0.95, 0.55, 0.20),
        }
    }
    fn label(self) -> &'static str {
        match self {
            State::Recording => "Recording",
            State::Processing => "Processing…",
            State::Done => "Done",
            State::Error => "Error",
            State::Hidden => "",
        }
    }
}

const PILL_H: i32 = 30;
const PILL_W: i32 = 150;
const PAD_H: f64 = 14.0;
const DOT_R: f64 = 4.5;
const CORNER: f64 = 15.0;
const FONT: f64 = 11.0;
const BOTTOM_MARGIN: i32 = 48;
const BG: (f64, f64, f64, f64) = (0.08, 0.08, 0.10, 0.82);

pub struct Overlay {
    window: Window,
    state: Rc<Cell<State>>,
    tick: Rc<Cell<u32>>,
    anim: Rc<Cell<Option<glib::SourceId>>>,
}

impl Overlay {
    pub fn new(app: &gtk::Application) -> Self {
        let window = Window::builder()
            .application(app)
            .title("rwhispr overlay")
            .default_width(PILL_W)
            .default_height(PILL_H)
            .resizable(false)
            .decorated(false)
            .focusable(false)
            .can_focus(false)
            .build();

        window.init_layer_shell();
        window.set_layer(Layer::Overlay);
        window.set_keyboard_mode(KeyboardMode::None);
        window.set_anchor(Edge::Bottom, true);
        window.set_margin(Edge::Bottom, BOTTOM_MARGIN);

        let state = Rc::new(Cell::new(State::Hidden));
        let tick = Rc::new(Cell::new(0u32));

        let area = DrawingArea::new();
        area.set_content_width(PILL_W);
        area.set_content_height(PILL_H);
        {
            let (s, t) = (state.clone(), tick.clone());
            area.set_draw_func(move |_, cr, w, h| draw(cr, w, h, s.get(), t.get()));
        }
        window.set_child(Some(&area));

        let css = gtk::CssProvider::new();
        css.load_from_data("window, window * { background: none; background-color: transparent; }");
        if let Some(display) = gtk::gdk::Display::default() {
            gtk::style_context_add_provider_for_display(
                &display,
                &css,
                gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
            );
        }

        Self {
            window,
            state,
            tick,
            anim: Rc::new(Cell::new(None)),
        }
    }

    pub fn set_state(&self, s: State) {
        self.state.set(s);
        match s {
            State::Hidden => {
                self.stop_anim();
                self.window.set_visible(false);
            }
            _ => {
                self.window.set_visible(true);
                self.window.queue_draw();
                if matches!(s, State::Recording | State::Processing) {
                    self.start_anim();
                } else {
                    self.stop_anim();
                }
            }
        }
    }

    fn start_anim(&self) {
        if self.anim.get_ref_exists() {
            return;
        }
        let (win, tick, state, anim) = (
            self.window.clone(),
            self.tick.clone(),
            self.state.clone(),
            self.anim.clone(),
        );
        let id = glib::timeout_add_local(std::time::Duration::from_millis(33), move || {
            tick.set(tick.get().wrapping_add(1));
            win.queue_draw();
            if matches!(state.get(), State::Recording | State::Processing) {
                glib::ControlFlow::Continue
            } else {
                anim.set(None);
                glib::ControlFlow::Break
            }
        });
        self.anim.set(Some(id));
    }

    fn stop_anim(&self) {
        if let Some(id) = self.anim.take() {
            id.remove();
        }
    }
}

trait CellExt {
    fn get_ref_exists(&self) -> bool;
}
impl CellExt for Rc<Cell<Option<glib::SourceId>>> {
    fn get_ref_exists(&self) -> bool {
        let v = self.take();
        let exists = v.is_some();
        self.set(v);
        exists
    }
}

fn draw(cr: &cairo::Context, w: i32, h: i32, state: State, tick: u32) {
    if state == State::Hidden {
        return;
    }
    let (w, h) = (w as f64, h as f64);
    let (r, g, b) = state.color();

    rounded_rect(cr, 0.0, 0.0, w, h, CORNER);
    cr.set_source_rgba(BG.0, BG.1, BG.2, BG.3);
    let _ = cr.fill();

    let cx = PAD_H + DOT_R;
    let cy = h / 2.0;
    let t = tick as f64;

    match state {
        State::Recording => {
            // Halo pulses with the mic level so you can see it is hearing you.
            let pulse = 0.5 + 0.5 * (t * 0.15).sin();
            let level = crate::audio::level();
            let glow = DOT_R + (2.0 + 4.0 * level) * pulse;
            cr.arc(cx, cy, glow, 0.0, std::f64::consts::TAU);
            cr.set_source_rgba(r, g, b, 0.22 * pulse);
            let _ = cr.fill();
            cr.arc(cx, cy, DOT_R, 0.0, std::f64::consts::TAU);
            cr.set_source_rgba(r, g, b, 1.0);
            let _ = cr.fill();
        }
        State::Processing => {
            for i in 0..3 {
                let a = t * 0.10 + i as f64 * (std::f64::consts::TAU / 3.0);
                cr.arc(
                    cx + a.cos() * DOT_R * 0.85,
                    cy + a.sin() * DOT_R * 0.85,
                    2.0,
                    0.0,
                    std::f64::consts::TAU,
                );
                cr.set_source_rgba(r, g, b, 0.30 + 0.70 * ((i + 1) as f64 / 3.0));
                let _ = cr.fill();
            }
        }
        State::Done => {
            cr.set_line_width(2.0);
            cr.set_line_cap(cairo::LineCap::Round);
            cr.set_line_join(cairo::LineJoin::Round);
            cr.move_to(cx - 4.0, cy);
            cr.line_to(cx - 1.0, cy + 3.0);
            cr.line_to(cx + 4.0, cy - 3.0);
            cr.set_source_rgba(r, g, b, 1.0);
            let _ = cr.stroke();
        }
        _ => {
            cr.arc(cx, cy, DOT_R, 0.0, std::f64::consts::TAU);
            cr.set_source_rgba(r, g, b, 1.0);
            let _ = cr.fill();
        }
    }

    cr.select_font_face("sans-serif", cairo::FontSlant::Normal, cairo::FontWeight::Normal);
    cr.set_font_size(FONT);
    cr.set_source_rgba(0.92, 0.92, 0.94, 0.92);
    let label = state.label();
    if let Ok(ext) = cr.text_extents(label) {
        cr.move_to(cx + DOT_R + 8.0, h / 2.0 + ext.height() / 2.0);
        let _ = cr.show_text(label);
    }
}

fn rounded_rect(cr: &cairo::Context, x: f64, y: f64, w: f64, h: f64, r: f64) {
    use std::f64::consts::PI;
    cr.new_sub_path();
    cr.arc(x + w - r, y + r, r, -PI / 2.0, 0.0);
    cr.arc(x + w - r, y + h - r, r, 0.0, PI / 2.0);
    cr.arc(x + r, y + h - r, r, PI / 2.0, PI);
    cr.arc(x + r, y + r, r, PI, 1.5 * PI);
    cr.close_path();
}

impl Overlay {
    pub fn state(&self) -> State {
        self.state.get()
    }
}
