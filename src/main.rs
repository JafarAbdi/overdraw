use anyhow::{Context, Result, bail, ensure};
use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::Event;
use x11rb::protocol::shape::{self, SK, SO};
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
    color: u32,
    points: Vec<Point>,
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
}

impl Overlay {
    fn new() -> Result<Self> {
        let (conn, screen_num) = x11rb::connect(None)?;
        ensure!(
            conn.extension_information(shape::X11_EXTENSION_NAME)?.is_some(),
            "the X server lacks the SHAPE extension"
        );
        let screen = &conn.setup().roots[screen_num];
        let (width, height) = (screen.width_in_pixels, screen.height_in_pixels);

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

        Ok(Self {
            conn,
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
            Event::ButtonPress(e) if e.detail == 1 => {
                let Mode::Drawing { current } = &mut self.mode else { return Ok(Outcome::Idle) };
                let p = Point { x: e.event_x, y: e.event_y };
                // Two identical points: a zero-length wide line with round caps is a dot.
                *current = Some(Stroke { color: self.color, points: vec![p, p] });
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
                if let Mode::Drawing { current } = &mut self.mode {
                    self.strokes.extend(current.take());
                }
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
            self.color = color;
            return Ok(Outcome::Idle);
        }
        match keysym {
            key::F9 | key::Escape => self.set_mode(Mode::PassThrough)?,
            key::BackSpace => {
                self.strokes.clear();
                self.redraw_all()?;
            }
            key::z if state.contains(KeyButMask::CONTROL) => {
                self.strokes.pop();
                self.redraw_all()?;
            }
            _ => {}
        }
        Ok(Outcome::Idle)
    }

    fn set_mode(&mut self, mode: Mode) -> Result<()> {
        if let Mode::Drawing { current } = &mut self.mode {
            self.strokes.extend(current.take());
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
        self.mode = mode;
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

fn main() -> Result<()> {
    Overlay::new()?.run()
}
