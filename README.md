# overdraw

Draw on top of your X11 screen. Press F9, scribble over whatever is there, press F9 again to get your mouse back. Made for pointing at things while recording a screencast.

## Requirements

X11. No compositor needed: the strokes are shaped into an override-redirect window, so everything you did not paint stays click-through, and screen recorders such as OBS or `ffmpeg -f x11grab` capture the annotations.

## Install

A static x86_64 Linux binary is attached to every release:

```
wget -O ~/.local/bin/overdraw https://github.com/JafarAbdi/overdraw/releases/latest/download/overdraw
chmod +x ~/.local/bin/overdraw
```

Or build it yourself with `cargo install --path .`.

## Usage

Run `overdraw`. It starts invisible and grabs F9. Press F9 to start drawing; the pointer becomes a crosshair and the left button paints. Press F9 or Escape to stop drawing; the strokes stay on screen until you clear them. It keeps running until you log out or `pkill overdraw`; a second instance refuses to start because F9 is already grabbed.

## Autostart

In `~/.config/i3/config`:

```
exec --no-startup-id overdraw
```

## Keys

| Key         | Action              |
| ----------- | ------------------- |
| `F9`        | Toggle drawing mode |
| Left button | Draw                |
| `1`         | Red                 |
| `2`         | Green               |
| `3`         | Blue                |
| `4`         | Yellow              |
| `Backspace` | Clear all strokes   |
| `Ctrl+Z`    | Undo last stroke    |
| `Escape`    | Stop drawing        |

The colour and editing keys work while drawing mode is active.

## i3

Do not `bindsym F9` in your i3 config: the program grabs F9 itself and the two grabs conflict.

## Credits

- [gromit-mpx](https://github.com/bk138/gromit-mpx) by Christian Beier and Simon Budig. The design is theirs: a root-sized override-redirect window shaped to the painted pixels, pointer and keyboard grabs while drawing, and a hotkey grabbed on the root window. No code was copied; gromit-mpx is GPL-2.0.
- [drawonscreen_rust](https://github.com/enheit/drawonscreen_rust) by enheit. The starting point for this project and the source of the key layout. None of its code remains.
- [x11rb](https://github.com/psychon/x11rb) does the talking to the X server, [xkeysym](https://github.com/rust-windowing/xkeysym) names the keys.

## License

MIT, see [LICENSE](LICENSE).
