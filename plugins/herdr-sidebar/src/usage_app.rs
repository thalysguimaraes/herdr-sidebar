//! The Usage view: per-account quota meters from a tg-usage server, drawn in
//! a narrow sidebar pane. Data comes from [`herdr_sidebar::usage`]; the fetch
//! runs on a background thread so a slow `curl` never stalls the event loop.

use std::sync::mpsc::{Receiver, channel};
use std::time::Duration;

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use herdr_sidebar::ui::draw_scrollbar;
use herdr_sidebar::usage::{self, Account, Horizon, Snapshot, Window};

/// The background fetch re-runs this often and on demand (`r`).
const REFRESH_EVERY: Duration = Duration::from_secs(60);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Loading,
    /// The server sent a snapshot.
    Loaded,
    /// `load_connection()` returned None: tg-usage was never paired here.
    NotPaired,
    /// Fetch failed; the payload is the error text.
    Error,
}

enum Msg {
    Snapshot(Snapshot),
    /// Fetch failed; the view shows a fixed offline line.
    Offline,
    /// The thread found no pairing file on this host.
    NotPaired,
}

pub struct App {
    state: State,
    snapshot: Option<Snapshot>,
    rx: Option<Receiver<Msg>>,
    scroll: usize,
}

impl App {
    pub fn new() -> Self {
        let mut app = Self {
            state: State::Loading,
            snapshot: None,
            rx: None,
            scroll: 0,
        };
        app.spawn_worker();
        app
    }

    pub fn on_key(&mut self, key: crossterm::event::KeyEvent) {
        use crossterm::event::{KeyCode, KeyEventKind};
        if key.kind != KeyEventKind::Press {
            return;
        }
        match key.code {
            KeyCode::Char('r') | KeyCode::Char('R') => self.refresh_now(),
            KeyCode::Char('j') | KeyCode::Down => self.on_scroll(true),
            KeyCode::Char('k') | KeyCode::Up => self.on_scroll(false),
            KeyCode::PageDown => self.page(true),
            KeyCode::PageUp => self.page(false),
            _ => {}
        }
    }

    pub fn on_scroll(&mut self, down: bool) {
        for _ in 0..3 {
            self.step(down);
        }
    }

    /// A full viewport step; used by PageUp/PageDown.
    fn page(&mut self, down: bool) {
        for _ in 0..10 {
            self.step(down);
        }
    }

    fn step(&mut self, down: bool) {
        let last = self.row_count().saturating_sub(1);
        self.scroll = if down {
            (self.scroll + 1).min(last)
        } else {
            self.scroll.saturating_sub(1)
        };
    }

    /// Total rendered rows, for scroll clamping and the scrollbar.
    fn row_count(&self) -> usize {
        match (&self.state, &self.snapshot) {
            (State::Loaded, Some(s)) => {
                let rows: usize = s
                    .accounts
                    .iter()
                    .map(|a| 1 + a.windows.iter().filter(|w| !w.model_scoped).count() * 2)
                    .sum();
                rows + s.accounts.len().saturating_sub(1)
            }
            _ => 1,
        }
    }

    pub fn tick(&mut self) {
        let Some(rx) = &self.rx else { return };
        while let Ok(msg) = rx.try_recv() {
            match msg {
                Msg::Snapshot(s) => {
                    self.state = State::Loaded;
                    self.snapshot = Some(s);
                }
                Msg::Offline => {
                    self.state = State::Error;
                }
                Msg::NotPaired => {
                    self.state = State::NotPaired;
                    self.snapshot = None;
                }
            }
        }
    }

    fn refresh_now(&mut self) {
        self.state = State::Loading;
        self.spawn_worker();
    }

    /// Start the fetch thread if none is running. The thread loops on its own
    /// (REFRESH_EVERY), so `r` just re-enters the loading state and lets the
    /// next result land; a dead thread (receiver dropped earlier) is replaced.
    fn spawn_worker(&mut self) {
        if self.rx.is_none() {
            let (tx, rx) = channel::<Msg>();
            self.rx = Some(rx);
            std::thread::Builder::new()
                .name("usage-fetch".into())
                .spawn(move || fetch_loop(tx))
                .ok();
        }
    }

    pub fn draw_body(&mut self, frame: &mut Frame, area: Rect) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        // Two cells each side so bars breathe away from the pane border; the
        // scrollbar keeps the full-width edge.
        const PAD: u16 = 2;
        let inner = Rect::new(area.x + PAD, area.y, area.width.saturating_sub(PAD * 2), area.height);
        let rows = self.render_rows(usize::from(inner.width));
        let visible = usize::from(area.height);
        self.scroll = self.scroll.min(rows.len().saturating_sub(visible.max(1)));
        let total = rows.len();
        let slice: Vec<Line> = rows
            .into_iter()
            .skip(self.scroll)
            .take(visible)
            .collect();
        frame.render_widget(Paragraph::new(slice), inner);
        draw_scrollbar(frame, area, total, visible, self.scroll);
    }

    /// Build every logical row for the whole view; draw_body slices them.
    fn render_rows(&self, width: usize) -> Vec<Line<'static>> {
        match self.state {
            State::NotPaired => vec![Line::from(Span::styled(
                "tg-usage not paired on this host",
                Style::default().dim(),
            ))],
            State::Loading => {
                vec![Line::from(Span::styled("loading…", Style::default().dim()))]
            }
            State::Error => vec![Line::from(Span::styled(
                "usage offline · r to retry",
                Style::default().dim(),
            ))],
            State::Loaded => {
                let Some(s) = &self.snapshot else { return vec![] };
                let mut rows = Vec::new();
                for (i, account) in s.accounts.iter().enumerate() {
                    if i > 0 {
                        rows.push(Line::default());
                    }
                    rows.extend(account_rows(account, s.server_now_ms, width));
                }
                rows
            }
        }
    }
}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}

/// The background fetch thread: fetch immediately, then every REFRESH_EVERY.
/// The pairing decision is re-made each cycle so a file that appears or
/// disappears later is followed — a thread that cached `None` forever would
/// never see a later pairing.
fn fetch_loop(tx: std::sync::mpsc::Sender<Msg>) -> ! {
    loop {
        let msg = match usage::load_connection() {
            None => Msg::NotPaired,
            Some(conn) => match usage::fetch(&conn) {
                Ok(s) => Msg::Snapshot(s),
                Err(_) => Msg::Offline,
            },
        };
        if tx.send(msg).is_err() {
            std::process::exit(0);
        }
        std::thread::sleep(REFRESH_EVERY);
    }
}

fn account_rows(account: &Account, server_now_ms: i64, width: usize) -> Vec<Line<'static>> {
    let name = if account.fresh {
        Span::styled(account.display_name.clone(), Style::default().bold())
    } else {
        Span::styled(
            format!("{} stale", account.display_name),
            Style::default().dim(),
        )
    };
    let mut rows = vec![Line::from(name)];
    for win in account.windows.iter().filter(|w| !w.model_scoped) {
        rows.push(Line::from(window_label(win, server_now_ms, width)));
        rows.push(Line::from(bar_spans(win, width, server_now_ms)));
    }
    rows
}

/// "Session 74%" left, countdown right-aligned at `width`.
fn window_label(win: &Window, server_now_ms: i64, width: usize) -> Vec<Span<'static>> {
    let left = format!("{} {:.0}%", short_label(win), win.used.clamp(0.0, 1.0) * 100.0);
    let right = countdown_to(win.resets_at_ms, server_now_ms);
    let gap = width
        .saturating_sub(left.chars().count() + right.chars().count())
        .max(1);
    vec![
        Span::raw(left),
        Span::raw(" ".repeat(gap)),
        Span::raw(right),
    ]
}

fn short_label(win: &Window) -> String {
    match win.horizon {
        Horizon::Session => "Session".into(),
        Horizon::Weekly => "Weekly".into(),
        Horizon::Monthly => "Monthly".into(),
        Horizon::Other => win.label.clone(),
    }
}

/// "43m" / "2h 53m" / "4d 9h" / "—" (unknown reset). Past resets clamp to 0.
fn countdown_to(resets_at_ms: Option<i64>, now_ms: i64) -> String {
    let Some(resets_at) = resets_at_ms else {
        return "—".to_string();
    };
    let mins = (resets_at - now_ms).max(0) / 60_000;
    let days = mins / (60 * 24);
    let hours = (mins / 60) % 24;
    let rem = mins % 60;
    if days > 0 {
        format!("{days}d {hours}h")
    } else if hours > 0 {
        format!("{hours}h {rem}m")
    } else {
        format!("{rem}m")
    }
}

/// One rendered bar row: `width` cells of '█' (used share) and '░', plus a
/// dim '│' pace marker at the elapsed share when the window length is known.
fn bar_spans(win: &Window, width: usize, now_ms: i64) -> Vec<Span<'static>> {
    if width == 0 {
        return vec![];
    }
    let used = win.used.clamp(0.0, 1.0);
    let filled_n = ((used * width as f64).round() as usize).min(width);
    let marker = elapsed_position(win, width, now_ms); // 1-based cell, 0 = none
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut run = String::new();
    let mut cur: Option<(char, Style)> = None;
    for i in 0..width {
        let (ch, st) = if marker != 0 && i + 1 == marker {
            ('│', Style::default().dim())
        } else if i < filled_n {
            ('█', Style::default().fg(fill_color(used)))
        } else {
            ('░', Style::default().dim())
        };
        match cur {
            Some((c, s)) if c == ch && s == st => run.push(ch),
            Some((_, s)) => {
                spans.push(Span::styled(std::mem::take(&mut run), s));
                run.push(ch);
                cur = Some((ch, st));
            }
            None => {
                run.push(ch);
                cur = Some((ch, st));
            }
        }
    }
    if let Some((_, s)) = cur {
        spans.push(Span::styled(run, s));
    }
    spans
}

/// 1-based cell where the window's elapsed share lands, or 0 = no marker.
/// elapsed = 1 - remaining/length, from the reset time and window duration.
fn elapsed_position(win: &Window, width: usize, now_ms: i64) -> usize {
    let (Some(duration), Some(resets_at)) = (win.duration_ms, win.resets_at_ms) else {
        return 0;
    };
    if duration <= 0 {
        return 0;
    }
    let elapsed = 1.0 - (resets_at - now_ms) as f64 / duration as f64;
    let pos = (elapsed.clamp(0.0, 1.0) * width as f64).ceil() as usize;
    pos.clamp(1, width)
}

fn fill_color(used: f64) -> Color {
    if used < 0.60 {
        Color::Green
    } else if used <= 0.85 {
        Color::Yellow
    } else {
        Color::Red
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn win(used: f64, duration: Option<i64>, resets_at: Option<i64>) -> Window {
        Window {
            label: "Session".into(),
            horizon: Horizon::Session,
            used,
            duration_ms: duration,
            resets_at_ms: resets_at,
            model_scoped: false,
        }
    }

    #[test]
    fn countdown_formats() {
        let now = 1_000_000_000_000;
        assert_eq!(countdown_to(None, now), "—");
        assert_eq!(countdown_to(Some(now + 43 * 60_000), now), "43m");
        assert_eq!(
            countdown_to(Some(now + 2 * 3_600_000 + 53 * 60_000), now),
            "2h 53m"
        );
        assert_eq!(
            countdown_to(Some(now + 4 * 86_400_000 + 9 * 3_600_000), now),
            "4d 9h"
        );
        // A past reset clamps to zero.
        assert_eq!(countdown_to(Some(now - 5_000), now), "0m");
    }

    #[test]
    fn bar_proportions_and_marker() {
        let now = 1_000_000_000_000;
        // 74% of 50 cells = 37 filled.
        let mut w = win(0.74, None, None);
        let bar: String = bar_spans(&w, 50, now)
            .iter()
            .map(|s| s.content.to_string())
            .collect();
        assert_eq!(bar.chars().filter(|&c| c == '█').count(), 37);
        assert_eq!(bar.chars().filter(|&c| c == '░').count(), 13);
        assert_eq!(elapsed_position(&w, 50, now), 0);

        // Marker: 2h window, 1h elapsed, 20 cols -> elapsed 0.5 -> cell 10.
        w.duration_ms = Some(2 * 3_600_000);
        w.resets_at_ms = Some(now + 3_600_000);
        assert_eq!(elapsed_position(&w, 20, now), 10);

        // Elapsed 0 clamps to cell 1; elapsed 1 lands on the last cell.
        w.resets_at_ms = Some(now + 2 * 3_600_000);
        assert_eq!(elapsed_position(&w, 20, now), 1);
        w.resets_at_ms = Some(now);
        assert_eq!(elapsed_position(&w, 20, now), 20);

        // No duration or no reset -> no marker.
        assert_eq!(elapsed_position(&win(0.1, None, Some(now)), 20, now), 0);
        assert_eq!(elapsed_position(&win(0.1, Some(1), None), 20, now), 0);
    }

    #[test]
    fn bar_marker_is_its_own_cell() {
        let now = 1_000_000_000_000;
        // used 20%, elapsed 50%: '│' sits mid-bar on empty cells, as a dim run.
        let w = win(0.20, Some(2 * 3_600_000), Some(now + 3_600_000));
        let spans = bar_spans(&w, 10, now);
        let bar: String = spans.iter().map(|s| s.content.to_string()).collect();
        assert_eq!(bar, "██░░│░░░░░");
        assert!(
            spans
                .iter()
                .any(|s| s.content == "│" && s.style.add_modifier.contains(ratatui::style::Modifier::DIM))
        );
    }

    #[test]
    fn color_thresholds() {
        assert_eq!(fill_color(0.59), Color::Green);
        assert_eq!(fill_color(0.60), Color::Yellow);
        assert_eq!(fill_color(0.85), Color::Yellow);
        assert_eq!(fill_color(0.86), Color::Red);
    }

    #[test]
    fn window_label_layout() {
        let now = 1_000_000_000_000;
        let w = Window {
            label: "5h session".into(),
            horizon: Horizon::Session,
            used: 0.74,
            duration_ms: None,
            resets_at_ms: Some(now + 2 * 3_600_000 + 53 * 60_000),
            model_scoped: false,
        };
        let spans = window_label(&w, now, 46);
        let text: String = spans.iter().map(|s| s.content.to_string()).collect();
        assert_eq!(text.chars().count(), 46);
        assert!(text.starts_with("Session 74%"));
        assert!(text.ends_with("2h 53m"));
    }

    #[test]
    fn row_count_counts_account_blocks() {
        let mut app = App {
            state: State::Loading,
            snapshot: None,
            rx: None,
            scroll: 0,
        };
        assert_eq!(app.row_count(), 1);
        app.snapshot = Some(Snapshot {
            server_now_ms: 0,
            accounts: vec![
                Account {
                    provider: "claude".into(),
                    display_name: "Claude · Kanastra".into(),
                    fresh: true,
                    windows: vec![win(0.1, None, None), win(0.2, None, None)],
                },
                Account {
                    provider: "codex".into(),
                    display_name: "Codex".into(),
                    fresh: true,
                    windows: vec![win(0.3, None, None)],
                },
            ],
        });
        app.state = State::Loaded;
        // (1 header + 2*2 bars) + gap + (1 header + 2 bars) = 5 + 1 + 3 = 9.
        assert_eq!(app.row_count(), 9);
    }
}
