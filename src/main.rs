use std::ffi::OsString;
use std::fs::File;
use std::io::Write;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail, ensure};
use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::Event;
use x11rb::protocol::shape::{self, SK, SO};
use x11rb::protocol::xinput::{self, Device, XIEventMask};
use x11rb::protocol::xproto::{
    CapStyle, ChangeGCAux, ClipOrdering, ConfigureWindowAux, ConnectionExt as _, CoordMode,
    CreateGCAux, CreateWindowAux, Cursor, EventMask, Gcontext, GrabMode, GrabStatus, JoinStyle,
    KeyButMask, Keysym, ModMask, Pixmap, Point, Rectangle, StackMode, Window, WindowClass,
};
use x11rb::rust_connection::RustConnection;
use x11rb::{CURRENT_TIME, NONE};
use xkeysym::key;

const LINE_WIDTH: u32 = 5;
const PEN_COLORS: [(Keysym, u32); 4] =
    [(key::_1, 0xFF0000), (key::_2, 0x00FF00), (key::_3, 0x0080FF), (key::_4, 0xFFFF00)];

struct Stroke {
    start_unix_us: u128,
    end_unix_us: u128,
    color: u32,
    points: Vec<Point>,
}

enum Action<'a> {
    DrawOn,
    DrawOff,
    Color(u32),
    Click { button: u32, x: i16, y: i16 },
    Stroke(&'a Stroke),
    Undo,
    Clear(usize),
}

impl Action<'_> {
    fn write(self, file: &mut File, unix_us: u128) -> Result<()> {
        let mut line = Vec::new();
        write!(line, "{{\"unix_us\":{unix_us},\"action\":")?;
        match self {
            Self::DrawOn => write!(line, "\"draw_on\"")?,
            Self::DrawOff => write!(line, "\"draw_off\"")?,
            Self::Color(color) => write!(line, "\"color\",\"color\":\"#{color:06x}\"")?,
            Self::Click { button, x, y } => {
                write!(line, "\"click\",\"button\":{button},\"x\":{x},\"y\":{y}")?;
            }
            Self::Stroke(stroke) => {
                write!(
                    line,
                    "\"stroke\",\"stroke\":{{\"start_unix_us\":{},\"end_unix_us\":{},\"color\":\"#{:06x}\",\"points\":[",
                    stroke.start_unix_us, stroke.end_unix_us, stroke.color
                )?;
                for (i, p) in stroke.points.iter().enumerate() {
                    write!(line, "{}[{},{}]", if i > 0 { "," } else { "" }, p.x, p.y)?;
                }
                write!(line, "]}}")?;
            }
            Self::Undo => write!(line, "\"undo\"")?,
            Self::Clear(count) => write!(line, "\"clear\",\"count\":{count}")?,
        }
        writeln!(line, "}}")?;
        file.write_all(&line).context("appending overdraw event")
    }
}

fn unix_us() -> Result<u128> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before Unix epoch")?
        .as_micros())
}

enum Mode {
    PassThrough,
    Drawing { current: Option<Stroke> },
}

enum Outcome {
    Idle,
    Painted,
}

struct Overlay {
    conn: RustConnection,
    root: Window,
    win: Window,
    mask: Pixmap,
    canvas: Pixmap,
    black: u32,
    whole: Rectangle,
    cursor: Cursor,
    gc_canvas: Gcontext,
    gc_mask_set: Gcontext,
    gc_mask_clear: Gcontext,
    keysyms: Vec<Keysym>,
    color: u32,
    strokes: Vec<Stroke>,
    mode: Mode,
    events: Option<File>,
}

impl Overlay {
    fn new(events: Option<PathBuf>) -> Result<Self> {
        let (conn, screen_num) = x11rb::connect(None)?;
        ensure!(
            conn.extension_information(shape::X11_EXTENSION_NAME)?.is_some(),
            "the X server lacks the SHAPE extension"
        );
        let screen = &conn.setup().roots[screen_num];
        let (width, height) = (screen.width_in_pixels, screen.height_in_pixels);
        let root = screen.root;
        if events.is_some() {
            let version = xinput::xi_query_version(&conn, 2, 0)?
                .reply()
                .context("the X server lacks XInput")?;
            ensure!(version.major_version >= 2, "the X server lacks XInput 2");
            xinput::xi_select_events(
                &conn,
                root,
                &[xinput::EventMask {
                    deviceid: Device::ALL_MASTER.into(),
                    mask: vec![XIEventMask::RAW_BUTTON_PRESS],
                }],
            )?
            .check()?;
        }

        let canvas = conn.generate_id()?;
        conn.create_pixmap(screen.root_depth, canvas, screen.root, width, height)?;
        let win = conn.generate_id()?;
        conn.create_window(
            screen.root_depth,
            win,
            screen.root,
            0,
            0,
            width,
            height,
            0,
            WindowClass::INPUT_OUTPUT,
            screen.root_visual,
            &CreateWindowAux::new().override_redirect(1).background_pixmap(canvas),
        )?;
        let mask = conn.generate_id()?;
        conn.create_pixmap(1, mask, win, width, height)?;

        let line = CreateGCAux::new()
            .line_width(LINE_WIDTH)
            .cap_style(CapStyle::ROUND)
            .join_style(JoinStyle::ROUND);
        let gc_canvas = conn.generate_id()?;
        conn.create_gc(gc_canvas, canvas, &line.foreground(screen.black_pixel))?;
        let gc_mask_set = conn.generate_id()?;
        conn.create_gc(gc_mask_set, mask, &line.foreground(1))?;
        let gc_mask_clear = conn.generate_id()?;
        conn.create_gc(gc_mask_clear, mask, &CreateGCAux::new().foreground(0))?;

        let font = conn.generate_id()?;
        conn.open_font(font, b"cursor")?;
        let cursor = conn.generate_id()?;
        // Glyphs 34/35 are XC_crosshair and its mask in the standard cursor font.
        conn.create_glyph_cursor(cursor, font, font, 34, 35, 0, 0, 0, !0, !0, !0)?;
        conn.close_font(font)?;

        let whole = Rectangle { x: 0, y: 0, width, height };
        let black = screen.black_pixel;
        conn.poly_fill_rectangle(canvas, gc_canvas, &[whole])?;
        shape::rectangles(&conn, SO::SET, SK::INPUT, ClipOrdering::UNSORTED, win, 0, 0, &[])?;
        shape::rectangles(&conn, SO::SET, SK::BOUNDING, ClipOrdering::UNSORTED, win, 0, 0, &[])?;
        conn.map_window(win)?;

        let min_keycode = conn.setup().min_keycode;
        let count = conn.setup().max_keycode - min_keycode + 1;
        let mapping = conn.get_keyboard_mapping(min_keycode, count)?.reply()?;
        let per_keycode = usize::from(mapping.keysyms_per_keycode);
        ensure!(per_keycode > 0, "keyboard mapping reports zero keysyms per keycode");
        let keysyms: Vec<Keysym> = mapping.keysyms.chunks(per_keycode).map(|k| k[0]).collect();
        let f9 = keysyms.iter().position(|&k| k == key::F9).context("no keycode maps to F9")?;
        let f9 = min_keycode + u8::try_from(f9)?;
        conn.grab_key(false, screen.root, ModMask::ANY, f9, GrabMode::ASYNC, GrabMode::ASYNC)?
            .check()
            .context("grabbing F9 on the root window (is something else bound to it?)")?;
        conn.flush()?;

        let events = events
            .map(|path| File::create(&path).with_context(|| format!("creating {}", path.display())))
            .transpose()?;

        Ok(Self {
            conn,
            root,
            win,
            mask,
            canvas,
            black,
            whole,
            cursor,
            gc_canvas,
            gc_mask_set,
            gc_mask_clear,
            keysyms,
            color: 0xFF0000,
            strokes: Vec::new(),
            mode: Mode::PassThrough,
            events,
        })
    }

    fn run(&mut self) -> Result<()> {
        loop {
            let mut event = Some(self.conn.wait_for_event()?);
            let mut painted = false;
            while let Some(ev) = event {
                match self.handle(ev)? {
                    Outcome::Idle => {}
                    Outcome::Painted => painted = true,
                }
                event = self.conn.poll_for_event()?;
            }
            if painted {
                shape::mask(&self.conn, SO::SET, SK::BOUNDING, self.win, 0, 0, self.mask)?;
            }
            self.conn.flush()?;
        }
    }

    fn handle(&mut self, event: Event) -> Result<Outcome> {
        match event {
            Event::KeyPress(e) => {
                let Some(keysym) = e
                    .detail
                    .checked_sub(self.conn.setup().min_keycode)
                    .and_then(|i| self.keysyms.get(usize::from(i)).copied())
                else {
                    return Ok(Outcome::Idle);
                };
                self.handle_key(keysym, e.state)
            }
            // X reports each scroll wheel step as a press of buttons 4 to 7.
            Event::XinputRawButtonPress(e)
                if matches!(self.mode, Mode::PassThrough) && matches!(e.detail, 1..=3 | 8..=9) =>
            {
                let time = unix_us()?;
                let pointer = self.conn.query_pointer(self.root)?.reply()?;
                self.log(
                    Action::Click { button: e.detail, x: pointer.root_x, y: pointer.root_y },
                    time,
                )?;
                Ok(Outcome::Idle)
            }
            Event::ButtonPress(e) if e.detail == 1 => {
                let Mode::Drawing { current } = &mut self.mode else { return Ok(Outcome::Idle) };
                let p = Point { x: e.event_x, y: e.event_y };
                // Two identical points: a zero-length wide line with round caps is a dot.
                let start_unix_us = unix_us()?;
                *current = Some(Stroke {
                    start_unix_us,
                    end_unix_us: start_unix_us,
                    color: self.color,
                    points: vec![p, p],
                });
                self.draw_segment(self.color, &[p, p])?;
                Ok(Outcome::Painted)
            }
            Event::MotionNotify(e) => {
                let Mode::Drawing { current: Some(stroke) } = &mut self.mode else {
                    return Ok(Outcome::Idle);
                };
                let p = Point { x: e.event_x, y: e.event_y };
                let segment = [stroke.points[stroke.points.len() - 1], p];
                let color = stroke.color;
                stroke.points.push(p);
                self.draw_segment(color, &segment)?;
                Ok(Outcome::Painted)
            }
            Event::ButtonRelease(e) if e.detail == 1 => {
                self.commit_stroke()?;
                Ok(Outcome::Idle)
            }
            Event::Error(err) => bail!("X11 error: {err:?}"),
            _ => Ok(Outcome::Idle),
        }
    }

    fn handle_key(&mut self, keysym: Keysym, state: KeyButMask) -> Result<Outcome> {
        if !matches!(self.mode, Mode::Drawing { .. }) {
            if keysym == key::F9 {
                self.set_mode(Mode::Drawing { current: None })?;
            }
            return Ok(Outcome::Idle);
        }
        if let Some(&(_, color)) = PEN_COLORS.iter().find(|(k, _)| *k == keysym) {
            if self.color != color {
                self.color = color;
                self.log(Action::Color(color), unix_us()?)?;
            }
            return Ok(Outcome::Idle);
        }
        match keysym {
            key::F9 | key::Escape => self.set_mode(Mode::PassThrough)?,
            key::BackSpace => {
                if !self.strokes.is_empty() {
                    let count = self.strokes.len();
                    self.strokes.clear();
                    self.log(Action::Clear(count), unix_us()?)?;
                }
                self.redraw_all()?;
            }
            key::z if state.contains(KeyButMask::CONTROL) => {
                if self.strokes.pop().is_some() {
                    self.log(Action::Undo, unix_us()?)?;
                }
                self.redraw_all()?;
            }
            _ => {}
        }
        Ok(Outcome::Idle)
    }

    fn log(&mut self, action: Action<'_>, unix_us: u128) -> Result<()> {
        match &mut self.events {
            Some(file) => action.write(file, unix_us),
            None => Ok(()),
        }
    }

    fn commit_stroke(&mut self) -> Result<()> {
        if let Mode::Drawing { current } = &mut self.mode
            && let Some(mut stroke) = current.take()
        {
            stroke.end_unix_us = unix_us()?;
            self.log(Action::Stroke(&stroke), stroke.end_unix_us)?;
            self.strokes.push(stroke);
        }
        Ok(())
    }

    fn set_mode(&mut self, mode: Mode) -> Result<()> {
        if matches!(self.mode, Mode::Drawing { .. }) {
            self.commit_stroke()?;
            self.conn.ungrab_pointer(CURRENT_TIME)?;
            self.conn.ungrab_keyboard(CURRENT_TIME)?;
        }
        if let Mode::Drawing { .. } = mode {
            self.conn.configure_window(
                self.win,
                &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE),
            )?;
            let pointer = self
                .conn
                .grab_pointer(
                    false,
                    self.win,
                    EventMask::BUTTON_PRESS | EventMask::BUTTON_RELEASE | EventMask::POINTER_MOTION,
                    GrabMode::ASYNC,
                    GrabMode::ASYNC,
                    NONE,
                    self.cursor,
                    CURRENT_TIME,
                )?
                .reply()?;
            ensure!(
                pointer.status == GrabStatus::SUCCESS,
                "pointer grab failed: {:?}",
                pointer.status
            );
            let keyboard = self
                .conn
                .grab_keyboard(false, self.win, CURRENT_TIME, GrabMode::ASYNC, GrabMode::ASYNC)?
                .reply()?;
            ensure!(
                keyboard.status == GrabStatus::SUCCESS,
                "keyboard grab failed: {:?}",
                keyboard.status
            );
        }
        let action = match mode {
            Mode::Drawing { .. } => Action::DrawOn,
            Mode::PassThrough => Action::DrawOff,
        };
        self.mode = mode;
        self.log(action, unix_us()?)?;
        Ok(())
    }

    fn all_strokes(&self) -> impl Iterator<Item = &Stroke> {
        let current = match &self.mode {
            Mode::Drawing { current } => current.as_ref(),
            Mode::PassThrough => None,
        };
        self.strokes.iter().chain(current)
    }

    // The mask and the canvas are caches derived from `strokes`. The canvas is the
    // window's background pixmap, so the server repaints exposed areas from it and
    // the client never redraws old strokes.
    fn redraw_all(&self) -> Result<()> {
        self.conn.poly_fill_rectangle(self.mask, self.gc_mask_clear, &[self.whole])?;
        self.conn.change_gc(self.gc_canvas, &ChangeGCAux::new().foreground(self.black))?;
        self.conn.poly_fill_rectangle(self.canvas, self.gc_canvas, &[self.whole])?;
        for stroke in self.all_strokes() {
            self.draw_segment(stroke.color, &stroke.points)?;
        }
        shape::mask(&self.conn, SO::SET, SK::BOUNDING, self.win, 0, 0, self.mask)?;
        self.conn.clear_area(false, self.win, 0, 0, 0, 0)?;
        Ok(())
    }

    fn draw_segment(&self, color: u32, points: &[Point]) -> Result<()> {
        self.conn.poly_line(CoordMode::ORIGIN, self.mask, self.gc_mask_set, points)?;
        self.conn.change_gc(self.gc_canvas, &ChangeGCAux::new().foreground(color))?;
        self.conn.poly_line(CoordMode::ORIGIN, self.canvas, self.gc_canvas, points)?;
        Ok(())
    }
}

fn events_path() -> Result<Option<PathBuf>> {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    match args.as_slice() {
        [] => Ok(None),
        [flag, path] if flag == "--events" => Ok(Some(path.into())),
        _ => bail!("usage: overdraw [--events PATH]"),
    }
}

fn main() -> Result<()> {
    Overlay::new(events_path()?)?.run()
}
