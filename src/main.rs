mod algo;
mod art;

use algo::{effective_score, pick_next, scan_library, AlgorithmState, Song};
use art::load_art;
use crossterm::{
    event::{self, Event, KeyCode},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    widgets::{Block, Borders, Gauge, List, ListItem, Paragraph},
};
use ratatui_image::{StatefulImage, picker::Picker, protocol::StatefulProtocol};
use std::io::{stdout, BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

const STATE_FILE: &str = "algorithm_state.json";
const SIM_SECS: u64 = 180;

// ponytail: external player instead of rodio (no alsa/pkg-config system deps).
// In WSL, Linux audio clients have no sound card, so drive Windows' own
// MediaPlayer via powershell.exe (UNC path \\wsl$\...). Else mpv/ffplay/timer.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Backend {
    WinMedia, // WSL -> Windows speakers via powershell.exe, no installs
    Mpv,
    Ffplay,
    Simulated,
}

fn is_wsl() -> bool {
    std::env::var("WSL_DISTRO_NAME").is_ok()
        || std::fs::read_to_string("/proc/version")
            .map(|v| v.to_lowercase().contains("microsoft"))
            .unwrap_or(false)
}

fn powershell_ok() -> bool {
    Command::new("powershell.exe")
        .args(["-NoProfile", "-Command", "exit"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn detect_backend() -> Backend {
    if is_wsl() && powershell_ok() {
        return Backend::WinMedia;
    }    if Command::new("mpv").arg("--version").output().is_ok() {
        return Backend::Mpv;
    }
    if Command::new("ffplay").arg("-version").output().is_ok() {
        return Backend::Ffplay;
    }
    Backend::Simulated
}

/// One persistent powershell.exe hosting System.Windows.Media.MediaPlayer.
/// Line protocol on stdin, one reply line on stdout per command:
/// OPEN <unc> | PAUSE | RESUME | POS | DUR | ENDED | QUIT
const PS_SCRIPT: &str = r#"
Add-Type -AssemblyName PresentationCore
$p = New-Object System.Windows.Media.MediaPlayer
foreach ($line in $input) {
  if ($line.StartsWith('OPEN ')) { $p.Open([uri]$line.Substring(5)); $p.Play(); 'OK' }
  elseif ($line -eq 'PAUSE') { $p.Pause(); 'OK' }
  elseif ($line -eq 'RESUME') { $p.Play(); 'OK' }
  elseif ($line -eq 'POS') { [string]$p.Position.TotalSeconds }
  elseif ($line -eq 'DUR') { if ($p.NaturalDuration.HasTimeSpan) { [string]$p.NaturalDuration.TimeSpan.TotalSeconds } else { '0' } }
  elseif ($line -eq 'ENDED') { if ($p.NaturalDuration.HasTimeSpan -and $p.Position -ge $p.NaturalDuration.TimeSpan) { '1' } else { '0' } }
  elseif ($line -eq 'QUIT') { break }
}
$p.Close()
"#;

fn wsl_unc(path: &Path) -> Option<String> {
    let out = Command::new("wslpath").arg("-w").arg(path).output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

struct WinPlayer {
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    child: Child,
}

impl WinPlayer {
    fn spawn() -> Option<Self> {
        // script via argv (-Command <script>); $input then reads our piped stdin
        let mut child = Command::new("powershell.exe")
            .args(["-NoProfile", "-Command", PS_SCRIPT])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        let stdin = child.stdin.take()?;
        let stdout = BufReader::new(child.stdout.take()?);
        Some(Self { stdin, stdout, child })
    }
    fn cmd(&mut self, line: &str) -> String {
        let _ = writeln!(self.stdin, "{line}");
        let _ = self.stdin.flush();
        let mut s = String::new();
        let _ = self.stdout.read_line(&mut s);
        s.trim().to_string()
    }
    fn quit(mut self) {
        let _ = writeln!(self.stdin, "QUIT");
        let _ = self.stdin.flush();
        // powershell.exe won't exit until stdin closes — drop it before wait
        drop(self.stdin);
        let _ = self.child.wait();
    }
}

struct Player {
    backend: Backend,
    child: Option<Child>,
    win: Option<WinPlayer>,
    paused: bool,
    base_secs: u64, // elapsed before current run segment
    started: Instant,
}

impl Player {
    fn new(backend: Backend) -> Self {
        Self { backend, child: None, win: None, paused: false, base_secs: 0, started: Instant::now() }
    }
    fn elapsed(&mut self) -> u64 {
        if self.backend == Backend::WinMedia {
            return self.win.as_mut()
                .map(|w| w.cmd("POS").parse::<f64>().unwrap_or(0.0) as u64)
                .unwrap_or(0);
        }
        if self.paused {
            self.base_secs
        } else {
            self.base_secs + self.started.elapsed().as_secs()
        }
    }
    /// Real track length when known (simulated fallback or Windows metadata).
    fn duration(&mut self) -> Option<u64> {
        match self.backend {
            Backend::Simulated => Some(SIM_SECS),
            Backend::WinMedia => self.win.as_mut()
                .and_then(|w| w.cmd("DUR").parse::<f64>().ok())
                .filter(|d| *d > 0.0)
                .map(|d| d as u64),
            _ => None,
        }
    }
    fn stop_child(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
    fn shutdown(&mut self) {
        self.stop_child();
        if let Some(w) = self.win.take() {
            w.quit();
        }
    }
    fn play(&mut self, song: &Song) {
        self.stop_child();
        self.base_secs = 0;
        self.started = Instant::now();
        self.paused = false;
        if self.backend == Backend::WinMedia {
            if self.win.is_none() {
                self.win = WinPlayer::spawn();
            }
            let ok = self.win.as_mut()
                .and_then(|w| wsl_unc(&song.path).map(|u| (w, u)))
                .map(|(w, u)| w.cmd(&format!("OPEN {u}")) == "OK")
                .unwrap_or(false);
            if !ok {
                self.win = None;
                self.backend = Backend::Simulated; // windows side failed -> timer
            }
            return;
        }
        self.spawn(song, 0);
    }
    fn spawn(&mut self, song: &Song, seek: u64) {
        let r = match self.backend {
            Backend::WinMedia => return, // driven via WinPlayer in play()/toggle()
            Backend::Mpv => Command::new("mpv")
                .args(["--no-video", "--really-quiet", &format!("--start={seek}"), &song.path.to_string_lossy()])
                .spawn(),
            Backend::Ffplay => Command::new("ffplay")
                .args(["-nodisp", "-autoexit", "-loglevel", "quiet", "-ss", &seek.to_string(), &song.path.to_string_lossy()])
                .spawn(),
            Backend::Simulated => return,
        };
        match r {
            Ok(c) => self.child = Some(c),
            Err(_) => self.backend = Backend::Simulated, // player vanished -> simulate
        }
    }
    fn toggle(&mut self, song: &Song) {
        if self.backend == Backend::WinMedia {
            if let Some(w) = self.win.as_mut() {
                w.cmd(if self.paused { "RESUME" } else { "PAUSE" });
                self.paused = !self.paused;
            }
            return;
        }        if self.paused {
            self.paused = false;
            self.started = Instant::now();
            self.spawn(song, self.base_secs);
        } else {
            self.base_secs = self.elapsed();
            self.paused = true;
            self.stop_child();
        }
    }
    /// true when current track finished on its own
    fn finished(&mut self) -> bool {
        if self.paused {
            return false;
        }
        match self.backend {
            Backend::Simulated => self.elapsed() >= SIM_SECS,
            Backend::WinMedia => {
                // respawn if the daemon died underneath us
                let dead = self.win.as_mut()
                    .map(|w| matches!(w.child.try_wait(), Ok(Some(_))))
                    .unwrap_or(true);
                if dead {
                    self.win = None;
                    return true;
                }
                self.win.as_mut().map(|w| w.cmd("ENDED") == "1").unwrap_or(true)
            }
            _ => match self.child.as_mut() {
                Some(c) => matches!(c.try_wait(), Ok(Some(_))),
                None => true, // failed to launch counts as finished -> skip ahead
            },
        }
    }
}

fn default_music_dir() -> String {
    // ponytail: std only, no dirs crate — HOME else current dir fallback
    if let Ok(home) = std::env::var("HOME") {
        let p = format!("{home}/Music");
        if Path::new(&p).is_dir() {
            return p;
        }
    }
    if Path::new("Music").is_dir() {
        return "Music".into();
    }
    if Path::new("music").is_dir() {
        return "music".into();
    }
    std::env::var("HOME").map(|h| format!("{h}/Music")).unwrap_or("Music".into())
}

struct App {
    songs: Vec<Song>,
    idx: usize,      // now playing
    cursor: usize,   // library cursor
    toast: String,
    picker: Picker,
    art: Option<(String, StatefulProtocol)>, // cached for songs[idx].id
    gfx: String, // detected graphics protocol for the header
}

/// Switch track: scoring-excluded plumbing (session log, art refresh, audio) in one place.
fn set_track(app: &mut App, player: &mut Player, st: &mut AlgorithmState, idx: usize) {
    app.idx = idx;
    st.note_played(&app.songs[idx].id);
    let song = &app.songs[idx];
    let fresh = app.art.as_ref().is_none_or(|(id, _)| *id != song.id);
    if fresh {
        app.art = load_art(&song.path).map(|img| (song.id.clone(), app.picker.new_resize_protocol(img)));
    }
    player.play(&app.songs[idx]);
}

impl App {
    /// Up-next ranked by effective score, excluding now playing.
    fn ranked(&self, st: &AlgorithmState) -> Vec<(f32, usize)> {
        let mut r: Vec<(f32, usize)> = self
            .songs
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != self.idx)
            .map(|(i, s)| (effective_score(s, st), i))
            .collect();
        r.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap());
        r
    }
    fn top_artists(st: &AlgorithmState) -> Vec<(String, f32)> {
        let mut v: Vec<(String, f32)> = st.artist_scores.iter().map(|(k, v)| (k.clone(), *v)).collect();
        v.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
        v.truncate(3);
        v
    }
}

fn main() -> anyhow::Result<()> {
    let music_dir = default_music_dir();
    let songs = scan_library(&music_dir);
    if songs.is_empty() {
        println!("No audio in {music_dir}/. Add mp3/wav/flac/ogg as `Artist - Title.mp3`.");
        return Ok(());
    }
    let mut st = AlgorithmState::load(STATE_FILE);
    let backend = detect_backend();
    let backend_name = match backend {
        Backend::WinMedia => "windows",
        Backend::Mpv => "mpv",
        Backend::Ffplay => "ffplay",
        Backend::Simulated => "simulated (install mpv for real audio)",
    };
    let mut player = Player::new(backend);

    enable_raw_mode()?;
    let mut out = stdout();
    execute!(out, EnterAlternateScreen)?;
    let mut term = Terminal::new(CrosstermBackend::new(out))?;
    // query after entering the alternate screen, before reading events
    let mut app = App {
        idx: pick_next(&songs, &st, None).unwrap_or(0),
        cursor: 0,
        toast: "j/k browse · enter play · l love · s skip · d dislike · q quit".into(),
        songs,
        // ponytail: query real caps (kitty/sixel); halfblocks if the terminal stays silent
        picker: Picker::from_query_stdio().unwrap_or_else(|_| Picker::halfblocks()),
        art: None,
        gfx: String::new(),
    };
    app.gfx = format!("{:?}", app.picker.protocol_type()).to_lowercase();
    let first = app.idx;
    set_track(&mut app, &mut player, &mut st, first);

    let res = run(&mut term, &mut app, &mut st, &mut player, backend_name);

    disable_raw_mode()?;
    execute!(term.backend_mut(), LeaveAlternateScreen)?;
    term.show_cursor()?;
    player.shutdown();
    st.save(STATE_FILE);
    if let Err(e) = res {
        eprintln!("error: {e:#}");
    }
    println!("Saved to {STATE_FILE}. Bye!");
    Ok(())
}

fn run(
    term: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    app: &mut App,
    st: &mut AlgorithmState,
    player: &mut Player,
    backend_name: &str,
) -> anyhow::Result<()> {
    loop {
        if player.finished() {
            let s = &app.songs[app.idx];
            st.apply("complete", &s.id, &s.artist, &s.genre, 1.0);
            st.save(STATE_FILE);
            app.toast = format!("✓ completed {} (+20)", s.title);
            let next = pick_next(&app.songs, st, Some(&s.id)).unwrap_or(app.idx);
            set_track(app, player, st, next);
        }
        term.draw(|f| ui(f, app, st, player, backend_name))?;

        if !event::poll(Duration::from_millis(200))? {
            continue;
        }
        let key = match event::read()? {
            Event::Key(k) => k.code,
            _ => continue,
        };
        let n = app.songs.len();
        match key {
            KeyCode::Char('q') | KeyCode::Esc => break,
            KeyCode::Char('p') | KeyCode::Char(' ') => {
                let cur = app.songs[app.idx].clone();
                player.toggle(&cur);
                app.toast = if player.paused { "⏸ paused".into() } else { "▶ playing".into() };
            }
            KeyCode::Char('j') | KeyCode::Down => app.cursor = (app.cursor + 1).min(n - 1),
            KeyCode::Char('k') | KeyCode::Up => app.cursor = app.cursor.saturating_sub(1),
            KeyCode::Enter => {
                let cur = app.songs[app.idx].clone();
                let sel = app.cursor;
                if sel != app.idx {
                    st.apply("next", &cur.id, &cur.artist, &cur.genre, 1.0);
                    set_track(app, player, st, sel);
                    app.toast = format!("→ playing {}", app.songs[app.idx].title);
                }
            }
            KeyCode::Char('n') => {
                let cur = app.songs[app.idx].clone();
                st.apply("next", &cur.id, &cur.artist, &cur.genre, 1.0);
                let next = pick_next(&app.songs, st, Some(&cur.id)).unwrap_or(app.idx);
                set_track(app, player, st, next);
                app.toast = format!("→ next: {}", app.songs[app.idx].title);
            }
            KeyCode::Char('s') => {
                let cur = app.songs[app.idx].clone();
                // position-aware: early skip stings, late skip counts as liked.
                // Unknown length (mpv/ffplay) -> classic -15 middle tier.
                let prog = match player.duration().filter(|t| *t > 0) {
                    Some(total) => player.elapsed() as f32 / total as f32,
                    None => 0.5,
                };
                st.apply("skip", &cur.id, &cur.artist, &cur.genre, prog);
                st.save(STATE_FILE);
                let next = pick_next(&app.songs, st, Some(&cur.id)).unwrap_or(app.idx);
                set_track(app, player, st, next);
                app.toast = if prog >= 0.8 {
                    format!("↷ nearly finished {} (+10)", cur.title)
                } else if prog < 0.3 {
                    format!("↷ skipped early {} (-25)", cur.title)
                } else {
                    format!("↷ skipped {} (-15)", cur.title)
                };
            }
            KeyCode::Char('l') => {
                let cur = app.songs[app.idx].clone();
                st.apply("love", &cur.id, &cur.artist, &cur.genre, 1.0);
                st.save(STATE_FILE);
                app.toast = format!("❤ loved {} (+50)", cur.title);
            }
            KeyCode::Char('d') => {
                let cur = app.songs[app.idx].clone();
                st.apply("dislike", &cur.id, &cur.artist, &cur.genre, 1.0);
                st.quarantine_song(&cur.id);
                st.save(STATE_FILE);
                let next = pick_next(&app.songs, st, Some(&cur.id)).unwrap_or(app.idx);
                set_track(app, player, st, next);
                app.toast = format!("👎 disliked {} (-40, back in 10)", cur.title);
            }
            _ => {}
        }
    }
    Ok(())
}

fn fmt_time(s: u64) -> String {
    format!("{:02}:{:02}", s / 60, s % 60)
}

fn ui(
    f: &mut ratatui::Frame,
    app: &mut App,
    st: &AlgorithmState,
    player: &mut Player,
    backend_name: &str,
) {
    let cur = &app.songs[app.idx];
    let area = f.area();

    let root = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(0), Constraint::Length(3)])
        .split(area);

    // header
    let status = if player.paused { "⏸ PAUSED" } else { "▶ PLAYING" };
    let header = Paragraph::new(format!(
        "🎵 terminal_music  {status}  [{backend_name} · {}]   {} tracks",
        app.gfx,
        app.songs.len()
    ))
    .style(Style::default().fg(Color::Green).add_modifier(Modifier::BOLD))
    .block(Block::default().borders(Borders::ALL));
    f.render_widget(header, root[0]);

    let mid = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(45), Constraint::Percentage(55)])
        .split(root[1]);

    let left = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(12), Constraint::Min(4), Constraint::Length(3)])
        .split(mid[0]);

    // album art (kitty/sixel when the terminal speaks them, else halfblocks)
    let art_block = Block::default()
        .borders(Borders::ALL)
        .title("Art")
        .border_style(Style::default().fg(Color::Magenta));
    let art_area = art_block.inner(left[0]);
    f.render_widget(art_block, left[0]);
    if art_area.width > 2 && art_area.height > 0 {
        if let Some((_, proto)) = app.art.as_mut() {
            f.render_stateful_widget(StatefulImage::new(), art_area, proto);
        } else {
            f.render_widget(Paragraph::new("♪ no art"), art_area);
        }
    }

    // now playing
    let score = effective_score(cur, st);
    let song_adj = st.song_scores.get(&cur.id).copied().unwrap_or(0.0);
    let artist_adj = st.artist_scores.get(&cur.artist).copied().unwrap_or(0.0);
    let np_text = format!(
        "{} — {}\n[{}]   score {score:.1}  (song {song_adj:+.0} / artist {artist_adj:+.0})\n\n{}",
        cur.artist, cur.title, cur.genre, app.toast
    );
    let np = Paragraph::new(np_text).block(
        Block::default()
            .borders(Borders::ALL)
            .title("Now playing")
            .border_style(Style::default().fg(Color::Cyan)),
    );
    f.render_widget(np, left[1]);

    // progress (real duration when the backend knows it)
    let elapsed = player.elapsed();
    if let Some(total) = player.duration().filter(|t| *t > 0) {
        let ratio = (elapsed.min(total) as f64 / total as f64).clamp(0.0, 1.0);
        let g = Gauge::default()
            .block(Block::default().borders(Borders::ALL).title(format!(
                "{} / {}",
                fmt_time(elapsed),
                fmt_time(total)
            )))
            .gauge_style(Style::default().fg(Color::Green))
            .ratio(ratio);
        f.render_widget(g, left[2]);
    } else {
        let p = Paragraph::new(format!("elapsed {} (via {})", fmt_time(elapsed), backend_name))
            .block(Block::default().borders(Borders::ALL).title("Progress"));
        f.render_widget(p, left[2]);
    }

    let right = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
        .split(mid[1]);

    // up next
    let ranked = app.ranked(st);
    let items: Vec<ListItem> = ranked
        .iter()
        .take((right[0].height as usize).saturating_sub(2).max(1))
        .map(|(w, i)| {
            let s = &app.songs[*i];
            ListItem::new(format!("{w:6.1}  {} — {}", s.artist, s.title))
        })
        .collect();
    let upnext = List::new(items).block(
        Block::default()
            .borders(Borders::ALL)
            .title("Up next (algorithm)")
            .border_style(Style::default().fg(Color::Yellow)),
    );
    f.render_widget(upnext, right[0]);

    // library
    let h = (right[1].height as usize).saturating_sub(2).max(1);
    let start = app.cursor.saturating_sub(h - 1).min(app.cursor);
    let items: Vec<ListItem> = app
        .songs
        .iter()
        .enumerate()
        .skip(start)
        .take(h)
        .map(|(i, s)| {
            let marker = if i == app.idx { "▶" } else { " " };
            let style = if i == app.cursor {
                Style::default().bg(Color::DarkGray).add_modifier(Modifier::BOLD)
            } else if i == app.idx {
                Style::default().fg(Color::Green)
            } else {
                Style::default()
            };
            ListItem::new(format!("{marker} {} — {}", s.artist, s.title)).style(style)
        })
        .collect();
    let lib = List::new(items).block(
        Block::default()
            .borders(Borders::ALL)
            .title("Library (j/k + enter)")
            .border_style(Style::default().fg(Color::Blue)),
    );
    f.render_widget(lib, right[1]);

    // footer: taste stats + keys
    let tops = App::top_artists(st);
    let tops_s = if tops.is_empty() {
        "no taste data yet — love/skip songs".to_string()
    } else {
        tops.iter().map(|(a, v)| format!("{a} {v:+.0}")).collect::<Vec<_>>().join(" · ")
    };
    let footer = Paragraph::new(format!(
        "taste: {tops_s}\n[n]ext [s]kip(-15) [p]ause [l]ove(+50) [d]islike(-40) [q]uit+save"
    ))
    .block(Block::default().borders(Borders::ALL).title("Stats / keys"));
    f.render_widget(footer, root[2]);
}

#[cfg(test)]
mod ui_tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};

    fn demo_app() -> (App, AlgorithmState, Player) {
        let songs = vec![
            Song { id: "a".into(), title: "Hello".into(), artist: "Adele".into(), genre: "Pop".into(), path: "a".into() },
            Song { id: "b".into(), title: "Timeless".into(), artist: "Carti".into(), genre: "Rap".into(), path: "b".into() },
        ];
        let mut st = AlgorithmState::default();
        st.apply("love", "a", "Adele", "Pop", 1.0);
        let picker = Picker::halfblocks();
        let art_img = image::DynamicImage::new_rgb8(64, 64);
        let app = App {
            songs, idx: 0, cursor: 1, toast: "hi".into(),
            art: Some(("a".into(), picker.new_resize_protocol(art_img))),
            gfx: "halfblocks".into(),
            picker,
        };
        (app, st, Player::new(Backend::Simulated))
    }

    #[test]
    fn renders_wide_and_narrow_without_panic() {
        use ratatui_image::picker::ProtocolType;
        let (mut app, st, mut player) = demo_app();
        for (w, h) in [(100, 30), (80, 24), (40, 10)] {
            let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
            term.draw(|f| ui(f, &mut app, &st, &mut player, "simulated")).unwrap();
        }
        // force the graphics encode paths (protocol selection itself happens live
        // via from_query_stdio on the user's terminal)
        for proto in [ProtocolType::Kitty, ProtocolType::Sixel] {
            app.picker.set_protocol_type(proto);
            let img = image::DynamicImage::new_rgb8(64, 64);
            app.art = Some(("a".into(), app.picker.new_resize_protocol(img)));
            let mut term = Terminal::new(TestBackend::new(100, 30)).unwrap();
            term.draw(|f| ui(f, &mut app, &st, &mut player, "simulated")).unwrap();
        }
    }

    #[test]
    #[ignore] // plays ~5s of REAL audio through Windows speakers
    fn winmedia_plays_unc_flac() {
        if !(is_wsl() && powershell_ok()) {
            return;
        }
        let songs = scan_library(&default_music_dir());
        assert!(!songs.is_empty(), "need a real file in ~/Music");
        let mut w = WinPlayer::spawn().expect("powershell spawn");
        let unc = wsl_unc(&songs[0].path).expect("wslpath");
        assert_eq!(w.cmd(&format!("OPEN {unc}")), "OK");
        std::thread::sleep(Duration::from_secs(3));
        let pos: f64 = w.cmd("POS").parse().unwrap_or(0.0);
        assert!(pos > 1.0, "playback advancing, pos={pos}");
        let dur: f64 = w.cmd("DUR").parse().unwrap_or(0.0);
        assert!(dur > 10.0, "duration metadata, dur={dur}");
        w.cmd("PAUSE");
        let p1: f64 = w.cmd("POS").parse().unwrap_or(-1.0);
        std::thread::sleep(Duration::from_secs(2));
        let p2: f64 = w.cmd("POS").parse().unwrap_or(-2.0);
        assert!((p2 - p1).abs() < 0.6, "pause froze position: {p1} -> {p2}");
        w.quit();
    }
}
