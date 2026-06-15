use std::io::Cursor;
use std::time::{Duration, Instant};

use notify_rust::{Notification, Timeout};

use color_eyre::Result;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::{
    prelude::*,
    widgets::{canvas::Canvas, Block, Borders, Gauge, Paragraph},
    DefaultTerminal,
};

// ── TokyoNight Storm palette ──────────────────────────────────────────────────
const TN_BG: Color = Color::Rgb(0x1a, 0x1b, 0x26);
const TN_BG_DARK: Color = Color::Rgb(0x16, 0x17, 0x21);
const TN_BG_HL: Color = Color::Rgb(0x29, 0x2e, 0x42);
const TN_FG: Color = Color::Rgb(0xc0, 0xca, 0xf5);
const TN_COMMENT: Color = Color::Rgb(0x56, 0x5f, 0x89);
const TN_BLUE: Color = Color::Rgb(0x7a, 0xa2, 0xf7);
const TN_CYAN: Color = Color::Rgb(0x7d, 0xcf, 0xff);
const TN_GREEN: Color = Color::Rgb(0x9e, 0xce, 0x6a);
const TN_YELLOW: Color = Color::Rgb(0xe0, 0xaf, 0x68);
const TN_RED: Color = Color::Rgb(0xf7, 0x76, 0x8e);

// ── Entry point ───────────────────────────────────────────────────────────────
fn main() -> Result<()> {
    color_eyre::install()?;
    let mut args = std::env::args().skip(1);
    let duration_arg = args.find(|a| !a.starts_with('-'));
    let duration = if let Some(arg) = duration_arg {
        parse_hms(&arg)?
    } else {
        60.0
    };
    let terminal = ratatui::init();
    let result = App::new(duration).run(terminal);
    ratatui::restore();
    result
}

/// Parse "HH:MM:SS", "MM:SS", or plain seconds.
fn parse_hms(s: &str) -> Result<f64> {
    let parts: Vec<&str> = s.split(':').collect();
    let secs = match parts.as_slice() {
        [h, m, sec] => {
            h.parse::<f64>()? * 3600.0 + m.parse::<f64>()? * 60.0 + sec.parse::<f64>()?
        }
        [m, sec] => m.parse::<f64>()? * 60.0 + sec.parse::<f64>()?,
        [sec] => sec.parse::<f64>()?,
        _ => 60 as f64,
    };
    Ok(secs)
}

// ── WAV beep generator ────────────────────────────────────────────────────────
/// Synthesise a simple WAV file (PCM 16-bit, 44 100 Hz, mono) in memory.
/// Plays three short beep tones: 880 Hz, 1046 Hz, 1318 Hz ("ding-ding-DING").
fn generate_beep_wav() -> Vec<u8> {
    const SAMPLE_RATE: u32 = 44_100;
    let tones: &[(f64, f64)] = &[
        (880.0, 0.15),  // A5 – 150 ms
        (1046.5, 0.15), // C6
        (1318.5, 0.30), // E6 – 300 ms (held)
    ];

    let mut samples: Vec<i16> = Vec::new();
    for &(freq, dur_secs) in tones {
        let n = (SAMPLE_RATE as f64 * dur_secs) as usize;
        for i in 0..n {
            let t = i as f64 / SAMPLE_RATE as f64;
            let wave = (2.0 * std::f64::consts::PI * freq * t).sin();
            // Fade out last 10% to avoid clicks
            let fade = if i > (n * 9 / 10) {
                1.0 - (i - n * 9 / 10) as f64 / (n / 10).max(1) as f64
            } else {
                1.0
            };
            samples.push((wave * fade * i16::MAX as f64) as i16);
        }
        // 50 ms silence between notes
        for _ in 0..(SAMPLE_RATE / 20) {
            samples.push(0);
        }
    }

    // Build a minimal WAV file in memory
    let data_bytes = samples.len() * 2;
    let mut wav = Vec::with_capacity(44 + data_bytes);
    // RIFF header
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&((36 + data_bytes) as u32).to_le_bytes());
    wav.extend_from_slice(b"WAVE");
    // fmt  chunk
    wav.extend_from_slice(b"fmt ");
    wav.extend_from_slice(&16u32.to_le_bytes()); // chunk size
    wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
    wav.extend_from_slice(&1u16.to_le_bytes()); // mono
    wav.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
    wav.extend_from_slice(&(SAMPLE_RATE * 2).to_le_bytes()); // byte rate
    wav.extend_from_slice(&2u16.to_le_bytes()); // block align
    wav.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
                                                 // data chunk
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&(data_bytes as u32).to_le_bytes());
    for s in &samples {
        wav.extend_from_slice(&s.to_le_bytes());
    }
    wav
}

/// Play the beep in a background thread so the TUI isn't blocked.
fn play_beep_async() {
    std::thread::spawn(|| {
        // rodio needs an output stream created on the same thread it plays on.
        if let Ok((_stream, stream_handle)) = rodio::OutputStream::try_default() {
            let wav_data = generate_beep_wav();
            let cursor = Cursor::new(wav_data);
            if let Ok(source) = rodio::Decoder::new(cursor) {
                if let Ok(sink) = rodio::Sink::try_new(&stream_handle) {
                    sink.append(source);
                    sink.sleep_until_end(); // block *this* thread until done
                }
            }
        }
    });
}

/// Send a desktop notification (non-blocking).
fn send_notification(title: &str, body: &str) {
    let title = title.to_owned();
    let body = body.to_owned();
    std::thread::spawn(move || {
        let _ = Notification::new()
            .summary(&title)
            .body(&body)
            .icon("alarm")
            .timeout(Timeout::Milliseconds(6_000))
            .show();
    });
}

// ── App ───────────────────────────────────────────────────────────────────────
#[derive(Debug)]
pub struct App {
    running: bool,
    start_time: Instant,
    total_duration: f64,
    /// Ensures we only fire sound + notification once.
    alerted: bool,
    /// Set to true for the "done" banner flash.
    done: bool,
}

impl App {
    pub fn new(duration: f64) -> Self {
        Self {
            running: false,
            start_time: Instant::now(),
            total_duration: duration,
            alerted: false,
            done: false,
        }
    }

    pub fn run(mut self, mut terminal: DefaultTerminal) -> Result<()> {
        self.running = true;
        self.start_time = Instant::now();

        while self.running {
            terminal.draw(|frame| self.render(frame))?;
            self.handle_crossterm_events()?;

            if self.elapsed() >= self.total_duration && !self.alerted {
                self.alerted = true;
                self.done = true;
                play_beep_async();
                send_notification("⏱ Timer complete!", "Your countdown has finished.");
            }

            // Auto-quit 3 s after finishing
            if self.done && self.start_time.elapsed().as_secs_f64() >= self.total_duration + 3.0 {
                self.running = false;
            }
        }
        Ok(())
    }

    fn elapsed(&self) -> f64 {
        self.start_time
            .elapsed()
            .as_secs_f64()
            .min(self.total_duration)
    }

    fn remaining(&self) -> f64 {
        (self.total_duration - self.elapsed()).max(0.0)
    }

    fn progress(&self) -> f64 {
        (self.elapsed() / self.total_duration).clamp(0.0, 1.0)
    }

    // ── render ────────────────────────────────────────────────────────────────
    fn render(&mut self, frame: &mut Frame) {
        let area = frame.area();
        frame.render_widget(Block::default().style(Style::default().bg(TN_BG)), area);

        let vertical = Layout::vertical([
            Constraint::Percentage(8),
            Constraint::Percentage(84),
            Constraint::Percentage(8),
        ])
        .split(area);

        let horizontal = Layout::horizontal([
            Constraint::Percentage(10),
            Constraint::Percentage(80),
            Constraint::Percentage(10),
        ])
        .split(vertical[1]);

        let content = horizontal[1];

        // [0] title  [1] clock  [2] gauge  [3] time label  [4] done/hint
        let rows = Layout::vertical([
            Constraint::Length(3), // 0 title
            Constraint::Min(10),   // 1 clock
            Constraint::Length(1), // 2 guage
            Constraint::Length(1), // 3 gap
            Constraint::Length(1), // 4 time lable
            Constraint::Length(1), // 5 gap
            Constraint::Length(2), // 6 hint
        ])
        .split(content);

        self.render_title(frame, rows[0]);
        self.render_clock(frame, rows[1]);
        self.render_gauge(frame, rows[2]);
        self.render_time_label(frame, rows[4]);

        if self.done {
            self.render_done_banner(frame, rows[6]);
        } else {
            self.render_hint(frame, rows[6]);
        }
    }

    fn render_title(&self, frame: &mut Frame, area: Rect) {
        let title = Paragraph::new(Line::from(vec![
            Span::styled("  ⏱  ", Style::default().fg(TN_CYAN)),
            Span::styled(
                "COUNTDOWN TIMER",
                Style::default().fg(TN_BLUE).add_modifier(Modifier::BOLD),
            ),
            Span::styled("  ⏱  ", Style::default().fg(TN_CYAN)),
        ]))
        .alignment(Alignment::Center)
        .block(
            Block::default()
                .borders(Borders::BOTTOM)
                .border_style(Style::default().fg(TN_BG_HL))
                .style(Style::default().bg(TN_BG)),
        );
        frame.render_widget(title, area);
    }

    fn render_clock(&self, frame: &mut Frame, area: Rect) {
        let progress = self.progress();
        let remaining = self.remaining();
        let done = self.done;

        let accent = if done {
            TN_CYAN
        } else if remaining > self.total_duration * 0.5 {
            TN_GREEN
        } else if remaining > self.total_duration * 0.2 {
            TN_YELLOW
        } else {
            TN_RED
        };

        let canvas = Canvas::default()
            .block(Block::default().style(Style::default().bg(TN_BG_DARK)))
            .x_bounds([-1.0, 1.0])
            .y_bounds([-1.0, 1.0])
            .paint(move |ctx| {
                use ratatui::widgets::canvas::Circle;
                use std::f64::consts::{FRAC_PI_2, PI};

                // Outer ring
                ctx.draw(&Circle {
                    x: 0.0,
                    y: 0.0,
                    radius: 0.90,
                    color: TN_BG_HL,
                });
                // Track
                ctx.draw(&Circle {
                    x: 0.0,
                    y: 0.0,
                    radius: 0.78,
                    color: TN_COMMENT,
                });

                // Progress arc (dots)
                for i in 0..360usize {
                    let frac = i as f64 / 360.0;
                    if frac > progress {
                        continue;
                    }
                    let theta = FRAC_PI_2 - frac * 2.0 * PI;
                    let (x, y) = (0.78 * theta.cos(), 0.78 * theta.sin());
                    ctx.print(x, y, Span::styled("•", Style::default().fg(accent)));
                }

                // Inner clock face
                ctx.draw(&Circle {
                    x: 0.0,
                    y: 0.0,
                    radius: 0.62,
                    color: TN_BG,
                });

                // Hour tick marks
                for h in 0..12 {
                    let theta = FRAC_PI_2 - (h as f64 / 12.0) * 2.0 * PI;
                    ctx.draw(&ratatui::widgets::canvas::Line {
                        x1: 0.50 * theta.cos(),
                        y1: 0.50 * theta.sin(),
                        x2: 0.58 * theta.cos(),
                        y2: 0.58 * theta.sin(),
                        color: TN_COMMENT,
                    });
                }

                // Sweep hand
                {
                    let theta = FRAC_PI_2 - progress * 2.0 * PI;
                    ctx.draw(&ratatui::widgets::canvas::Line {
                        x1: 0.0,
                        y1: 0.0,
                        x2: 0.55 * theta.cos(),
                        y2: 0.55 * theta.sin(),
                        color: accent,
                    });
                }

                // ✓ checkmark when done
                if done {
                    ctx.print(
                        -0.10,
                        0.05,
                        Span::styled(
                            "✓",
                            Style::default().fg(TN_CYAN).add_modifier(Modifier::BOLD),
                        ),
                    );
                }

                // Centre dot
                ctx.draw(&Circle {
                    x: 0.0,
                    y: 0.0,
                    radius: 0.04,
                    color: TN_FG,
                });
            });

        frame.render_widget(canvas, area);
    }

    fn render_gauge(&self, frame: &mut Frame, area: Rect) {
        let progress = self.progress();
        let remaining = self.remaining();

        let gauge_color = if self.done {
            TN_CYAN
        } else if remaining > self.total_duration * 0.5 {
            TN_GREEN
        } else if remaining > self.total_duration * 0.2 {
            TN_YELLOW
        } else {
            TN_RED
        };

        let gauge = Gauge::default()
            .block(Block::default().style(Style::default().bg(TN_BG)))
            .gauge_style(Style::default().fg(gauge_color).bg(TN_BG_HL))
            .ratio(progress)
            .label(Span::styled(
                format!("{:.1}%", progress * 100.0),
                Style::default().fg(TN_FG).add_modifier(Modifier::BOLD),
            ))
            .use_unicode(true);

        frame.render_widget(gauge, area);
    }

    fn render_time_label(&self, frame: &mut Frame, area: Rect) {
        let remaining = self.remaining();
        let h = (remaining / 3600.0) as u64;
        let m = ((remaining % 3600.0) / 60.0) as u64;
        let s = (remaining % 60.0) as u64;

        let time_str = if h > 0 {
            format!("{:02}:{:02}:{:02}", h, m, s)
        } else {
            format!("{:02}:{:02}", m, s)
        };

        let label = Paragraph::new(Line::from(vec![
            Span::styled("remaining  ", Style::default().fg(TN_COMMENT)),
            Span::styled(
                time_str,
                Style::default().fg(TN_CYAN).add_modifier(Modifier::BOLD),
            ),
        ]))
        .alignment(Alignment::Center);

        frame.render_widget(label, area);
    }

    fn render_done_banner(&self, frame: &mut Frame, area: Rect) {
        let banner = Paragraph::new(Line::from(vec![Span::styled(
            " ✓  TIMER COMPLETE — closing in 3 s ",
            Style::default()
                .fg(TN_BG)
                .bg(TN_CYAN)
                .add_modifier(Modifier::BOLD),
        )]))
        .alignment(Alignment::Center);
        frame.render_widget(banner, area);
    }

    fn render_hint(&self, frame: &mut Frame, area: Rect) {
        let hint = Paragraph::new(Line::from(vec![
            Span::styled(" q ", Style::default().fg(TN_BG).bg(TN_COMMENT)),
            Span::styled("  quit    ", Style::default().fg(TN_COMMENT)),
            Span::styled(" Esc ", Style::default().fg(TN_BG).bg(TN_COMMENT)),
            Span::styled("  quit", Style::default().fg(TN_COMMENT)),
        ]))
        .alignment(Alignment::Center);
        frame.render_widget(hint, area);
    }

    // ── events ────────────────────────────────────────────────────────────────
    fn handle_crossterm_events(&mut self) -> Result<()> {
        if event::poll(Duration::from_millis(50))? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => self.on_key_event(key),
                _ => {}
            }
        }
        Ok(())
    }

    fn on_key_event(&mut self, key: KeyEvent) {
        match (key.modifiers, key.code) {
            (_, KeyCode::Esc | KeyCode::Char('q'))
            | (KeyModifiers::CONTROL, KeyCode::Char('c') | KeyCode::Char('C')) => self.quit(),
            _ => {}
        }
    }

    fn quit(&mut self) {
        self.running = false;
    }
}
