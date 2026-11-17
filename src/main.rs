//! fancytop: a flashy, fast terminal monitor for CPU, GPU, memory and DRAM bandwidth on
//! Apple Silicon Macs. No root needed: everything comes from Mach, IOKit and IOReport.

#[cfg(not(target_os = "macos"))]
compile_error!("fancytop only supports macOS.");

mod app;
mod ffi;
mod gpu;
mod sys;
mod theme;
mod throughput;
mod ui;
mod wm;

use std::io::{self, IsTerminal};
use std::process::exit;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use ratatui::DefaultTerminal;
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{BeginSynchronizedUpdate, EndSynchronizedUpdate};
use ratatui::layout::Rect;

use crate::app::App;
use crate::theme::{THEMES, Theme};
use crate::ui::Canvas;

const MAX_FPS: u32 = 120;

/// Set by the SIGINT/SIGTERM/SIGHUP handler so the main loop can quit and restore the terminal.
static QUIT: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_: libc::c_int) {
    QUIT.store(true, Ordering::Relaxed);
}

fn catch_signals() {
    for sig in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
        unsafe { libc::signal(sig, on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t) };
    }
}

const HELP: &str = "\
fancytop - flashy real-time CPU / GPU / memory / DRAM bandwidth / I/O monitor for Apple Silicon

USAGE:
    fancytop [OPTIONS]

OPTIONS:
    -f, --fps <N>        frames per second, 1-120 (default 30)
    -w, --window <SECS>  seconds of CPU history each core reading averages over (default 0.5)
    -t, --theme <NAME>   colour theme to start with (default Neon); one of
                         {THEMES}
    --snapshot <WxH>     print one frame as plain text at the given size (e.g. 160x50) and exit
    -h, --help           print this help
    -V, --version        print version

KEYS:
    q / Esc / Ctrl-C     quit
    + / -                frame rate up / down by 1 fps
    1-5                  show / hide CPU, GPU, memory, I/O, processes
    arrows               focus the window in that direction
    Shift + arrows       swap the focused window with the next one that way, or move it to that
                         edge of the screen
    Tab / Shift-Tab      focus the next / previous window
    [ ]                  make the focused window narrower / wider
    { }                  make the focused window shorter / taller
    t                    switch the focused window's split between side by side and stacked
    z                    zoom the focused window to fill the screen, or unzoom
    r                    reset to the automatic layout
    m                    turn mouse capture off / on (off lets the terminal select text)
    c / C                next / previous colour theme

MOUSE:
    drag the border between two windows to resize them; it snaps into line with other borders
    drag a title bar onto another window to tile beside it on the side you point at, onto its
    middle to swap the two, or onto the header, footer or a screen edge to span that edge";

/// The help text with the theme names filled in.
fn help() -> String {
    let mut names = String::new();
    let mut line = 0;
    for (i, t) in THEMES.iter().enumerate() {
        let sep = if i + 1 < THEMES.len() { "," } else { "" };
        if line > 0 && line + t.name.len() + 2 > 70 {
            names.push_str("\n                         ");
            line = 0;
        } else if line > 0 {
            names.push(' ');
            line += 1;
        }
        names.push_str(t.name);
        names.push_str(sep);
        line += t.name.len() + 1;
    }
    HELP.replace("{THEMES}", &names)
}

struct Args {
    fps: u32,
    window: f64,
    theme: usize,
    snapshot: Option<(u16, u16)>,
}

fn parse_args() -> Args {
    let (mut fps, mut window, mut theme, mut snapshot) = (30u32, 0.5f64, 0, None);
    let mut args = std::env::args().skip(1);
    let fail = |msg: String| -> ! {
        eprintln!("fancytop: {msg}\n\n{}", help());
        exit(2)
    };
    while let Some(arg) = args.next() {
        let (flag, inline) = match arg.split_once('=') {
            Some((f, v)) if f.starts_with("--") => (f.to_string(), Some(v.to_string())),
            _ => (arg.clone(), None),
        };
        let mut value = |name: &str| {
            inline
                .clone()
                .or_else(|| args.next())
                .unwrap_or_else(|| fail(format!("{name} needs a value")))
        };
        match flag.as_str() {
            "-f" | "--fps" => {
                fps = value("--fps")
                    .parse()
                    .unwrap_or_else(|_| fail("--fps must be a whole number".into()))
            }
            "-w" | "--window" => {
                window = value("--window")
                    .parse()
                    .unwrap_or_else(|_| fail("--window must be a number".into()))
            }
            "-t" | "--theme" => {
                let v = value("--theme");
                theme = Theme::find(&v).unwrap_or_else(|| fail(format!("no theme called '{v}'")));
            }
            "--snapshot" => {
                let v = value("--snapshot");
                let size = v.split_once('x').and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?)));
                snapshot = Some(
                    size.filter(|&(w, h)| w > 0 && h > 0)
                        .unwrap_or_else(|| fail("--snapshot wants a size like 160x50".into())),
                );
            }
            "-h" | "--help" => {
                println!("{}", help());
                exit(0)
            }
            "-V" | "--version" => {
                println!("fancytop {}", env!("CARGO_PKG_VERSION"));
                exit(0)
            }
            other => fail(format!("unknown option '{other}'")),
        }
    }
    Args {
        fps: fps.clamp(1, MAX_FPS),
        window: window.max(0.1),
        theme,
        snapshot,
    }
}

/// Warm up for a moment so rates, graphs and the bandwidth sampler have data, then print one
/// frame as plain text.
fn snapshot(app: &mut App, w: u16, h: u16) {
    let mut buf = Buffer::empty(Rect::new(0, 0, w, h));
    let start = Instant::now();
    let mut last = start;
    while start.elapsed() < Duration::from_millis(1500) {
        app.sample();
        buf.reset();
        let now = Instant::now();
        app.draw(&mut Canvas::new(&mut buf, true), (now - last).as_secs_f64());
        last = now;
        std::thread::sleep(Duration::from_millis(50));
    }
    for y in 0..h {
        let line: String = (0..w).map(|x| buf[(x, y)].symbol().to_string()).collect();
        println!("{}", line.trim_end());
    }
}

fn main() {
    let args = parse_args();
    let new_app = || {
        let mut app = App::new(args.fps, args.window);
        app.theme = args.theme;
        app
    };
    if let Some((w, h)) = args.snapshot {
        snapshot(&mut new_app(), w, h);
        return;
    }
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        eprintln!("fancytop needs an interactive terminal.");
        exit(1);
    }
    let mut app = new_app();
    let mut terminal = ratatui::init();
    catch_signals();
    // ratatui's panic hook restores the terminal but doesn't know about mouse capture.
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = execute!(io::stdout(), DisableMouseCapture);
        hook(info);
    }));
    let result = execute!(terminal.backend_mut(), EnableMouseCapture).and_then(|_| run(&mut terminal, &mut app));
    let _ = execute!(terminal.backend_mut(), DisableMouseCapture);
    let _ = terminal.show_cursor();
    ratatui::restore();
    if let Err(e) = result {
        eprintln!("fancytop: {e}");
        exit(1);
    }
}

fn run(terminal: &mut DefaultTerminal, app: &mut App) -> io::Result<()> {
    let mut last = Instant::now();
    loop {
        if QUIT.load(Ordering::Relaxed) {
            return Ok(());
        }
        let frame_start = Instant::now();
        let dt = (frame_start - last).as_secs_f64();
        last = frame_start;

        app.sample();
        let truecolor = app.truecolor;
        execute!(terminal.backend_mut(), BeginSynchronizedUpdate)?;
        terminal.draw(|f| {
            let mut cv = Canvas::new(f.buffer_mut(), truecolor);
            app.draw(&mut cv, dt);
        })?;
        execute!(terminal.backend_mut(), EndSynchronizedUpdate)?;
        app.render_ms = app.render_ms * 0.9 + frame_start.elapsed().as_secs_f64() * 1000.0 * 0.1;

        // Handle input until the next frame is due; a resize redraws immediately, and window
        // moves show at up to MAX_FPS however slow the refresh, a burst of drag events making
        // one frame.
        let mut deadline = frame_start + Duration::from_secs_f64(1.0 / app.fps as f64);
        loop {
            let now = Instant::now();
            if QUIT.load(Ordering::Relaxed) || now >= deadline || !event::poll(deadline - now)? {
                break;
            }
            let changed = match event::read()? {
                Event::Key(k) if k.kind == KeyEventKind::Press => match k.code {
                    KeyCode::Char('q' | 'Q') | KeyCode::Esc => return Ok(()),
                    KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => return Ok(()),
                    KeyCode::Char('+' | '=') => {
                        app.fps = (app.fps + 1).min(MAX_FPS);
                        false
                    }
                    KeyCode::Char('-' | '_') => {
                        app.fps = app.fps.saturating_sub(1).max(1);
                        false
                    }
                    KeyCode::Char('m' | 'M') => {
                        app.mouse = !app.mouse;
                        if app.mouse {
                            execute!(terminal.backend_mut(), EnableMouseCapture)?;
                        } else {
                            execute!(terminal.backend_mut(), DisableMouseCapture)?;
                            app.wm.cancel();
                        }
                        true
                    }
                    code => app.on_key(code, k.modifiers),
                },
                Event::Mouse(m) if app.mouse => app.on_mouse(m),
                Event::Resize(..) => break,
                _ => false,
            };
            if changed {
                deadline = deadline.min(frame_start + Duration::from_secs_f64(1.0 / MAX_FPS as f64));
            }
        }
    }
}
