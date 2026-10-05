//! Ratatui rendering only. Input handling and effects live in the picker model.
use super::model::{App, HostState, Mode, folder_names};
use crate::history::Saved;
use ratatui::{
    Frame,
    layout::{Constraint, Layout},
    style::{Color, Style, Stylize},
    text::Line,
    widgets::{Block, List, ListItem, Paragraph, Wrap},
};
use std::{
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

const BG: Color = Color::Rgb(16, 20, 28);
const FG: Color = Color::Rgb(221, 229, 240);
const MUTED: Color = Color::Rgb(130, 147, 168);
const ACCENT: Color = Color::Rgb(105, 220, 205);
const SELECT: Color = Color::Rgb(31, 48, 61);
const RED: Color = Color::Rgb(255, 133, 133);

fn clean(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() { '�' } else { c })
        .collect()
}

fn age(seconds: u64) -> String {
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3600 {
        format!("{}m", seconds / 60)
    } else if seconds < 86400 {
        format!("{}h", seconds / 3600)
    } else {
        format!("{}d", seconds / 86400)
    }
}

fn conversation_item(
    row: &Saved,
    state: Option<&HostState>,
    compact: bool,
    now: u64,
) -> ListItem<'static> {
    let state = match state {
        Some(HostState::Checking) => "checking",
        Some(HostState::Reachable) => "reachable",
        Some(HostState::Stopped) => "server stopped",
        Some(HostState::Offline(_)) => "offline",
        None => "cached",
    };
    let c = &row.conversation;
    let title = if c.title.is_empty() {
        "New conversation"
    } else {
        &c.title
    };
    let location = if compact {
        format!(
            " {} · {state} · {}",
            clean(&row.host),
            clean(
                Path::new(&c.cwd)
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or(&c.cwd)
            )
        )
    } else {
        format!(" {}  ·  {}  ·  {state}", clean(&c.cwd), clean(&row.host))
    };
    let mut lines = vec![
        Line::from(format!(" {}  ·  {}", clean(title), clean(&c.status))).bold(),
        Line::from(location).fg(MUTED),
    ];
    if !compact {
        let when = if row.visited == 0 {
            "not visited here".into()
        } else {
            format!(
                "visited {} ago",
                age(now.saturating_sub(row.visited / 1000))
            )
        };
        lines.extend([
            Line::from(format!(" {when}  ·  {}", clean(&c.id))).fg(MUTED),
            Line::from(""),
        ]);
    }
    ListItem::new(lines)
}

fn detail(app: &App) -> String {
    match &app.mode {
        Mode::Host(input) => format!(
            "SSH host: {}▏\nTab cycles saved hosts · Enter connects · Esc cancels",
            clean(input.value())
        ),
        Mode::Folders { directory, .. } => format!(
            "{}\nEnter opens folder · Space creates conversation · Ctrl-N mkdir",
            clean(&directory.path)
        ),
        Mode::Mkdir { directory, .. } => format!(
            "Create inside {}\nEnter create · Esc cancel",
            clean(&directory.path)
        ),
        _ => app
            .message
            .as_ref()
            .map(|text| clean(text))
            .or_else(|| {
                app.states.values().find_map(|s| {
                    if let HostState::Offline(e) = s {
                        Some(clean(e))
                    } else {
                        None
                    }
                })
            })
            .unwrap_or_else(|| {
                let mut hosts: Vec<_> = app
                    .states
                    .iter()
                    .map(|(host, state)| {
                        format!(
                            "{}: {}",
                            clean(host),
                            match state {
                                HostState::Checking => "checking…",
                                HostState::Reachable => "connected",
                                HostState::Stopped => "no server · h to connect",
                                HostState::Offline(_) => "offline",
                            }
                        )
                    })
                    .collect();
                hosts.sort();
                hosts.join(" · ")
            }),
    }
}

pub(super) fn draw(app: &mut App, f: &mut Frame) {
    f.render_widget(Block::new().style(Style::default().bg(BG).fg(FG)), f.area());
    let compact = f.area().width < 60 || f.area().height < 18;
    let [title, sub, search, body, status, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(if compact { 0 } else { 2 }),
        Constraint::Length(1),
        Constraint::Min(2),
        Constraint::Length(if compact { 1 } else { 3 }),
        Constraint::Length(if compact { 3 } else { 2 }),
    ])
    .margin(if compact { 1 } else { 2 })
    .areas(f.area());
    f.render_widget(
        Paragraph::new("RCODEX  /  conversations").fg(ACCENT).bold(),
        title,
    );
    f.render_widget(
        Paragraph::new("All Codex threads · local visits first · discovery runs in the background")
            .fg(MUTED)
            .wrap(Wrap { trim: false }),
        sub,
    );
    let query = match &app.mode {
        Mode::Folders { filter, .. } => filter.value(),
        _ => app.search.value(),
    };
    f.render_widget(
        Paragraph::new(format!(
            "/ {}{}",
            query,
            if matches!(app.mode, Mode::Search) {
                "▏"
            } else {
                ""
            }
        ))
        .fg(ACCENT),
        search,
    );
    match &mut app.mode {
        Mode::Loading => f.render_widget(
            Paragraph::new("Opening remote directory…  Esc cancels").fg(ACCENT),
            body,
        ),
        Mode::Folders {
            host,
            directory,
            filter,
            list,
        } => {
            let items = folder_names(directory, filter)
                .into_iter()
                .map(|n| ListItem::new(format!(" 📁 {}", clean(n))));
            f.render_stateful_widget(
                List::new(items)
                    .block(Block::bordered().title(format!(
                        " {} · {} ",
                        clean(host),
                        clean(&directory.path)
                    )))
                    .highlight_symbol("▎ ")
                    .highlight_style(Style::default().bg(SELECT).fg(ACCENT)),
                body,
                list,
            );
        }
        Mode::Mkdir { input, .. } => f.render_widget(
            Paragraph::new(format!("New folder name: {}▏", clean(input.value()))).fg(ACCENT),
            body,
        ),
        _ => {
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            let items: Vec<_> = app
                .visible()
                .iter()
                .map(|r| conversation_item(r, app.states.get(&r.host), compact, now))
                .collect();
            if items.is_empty() {
                f.render_widget(Paragraph::new("No conversations found. Press h to add/connect a host, or r to retry discovery.").fg(MUTED).wrap(Wrap { trim: false }), body);
            } else {
                f.render_stateful_widget(
                    List::new(items)
                        .highlight_symbol("▎ ")
                        .highlight_style(Style::default().bg(SELECT).fg(ACCENT)),
                    body,
                    &mut app.list,
                );
            }
        }
    }
    f.render_widget(
        Paragraph::new(detail(app))
            .fg(if app.message.is_some() { RED } else { ACCENT })
            .wrap(Wrap { trim: false }),
        status,
    );
    let help = if compact {
        "↑↓ select  ↵ resume  n new\n/ find  t shell  h host\nr retry  q quit"
    } else {
        "↑↓ select  enter resume  n new  t shell  / search\nh connect host  r retry  q quit"
    };
    f.render_widget(Paragraph::new(help).fg(MUTED), footer);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{remote::Directory, sessions::Conversation};
    use ratatui::{Terminal, backend::TestBackend, widgets::ListState};
    use tui_input::Input;

    #[test]
    fn ages_and_sanitizes() {
        assert_eq!(age(3600), "1h");
        assert_eq!(clean("a\u{1b}b"), "a�b");
    }

    #[test]
    fn render_states() {
        for state in ["cached", "checking", "offline", "folders", "small"] {
            let mut app = App::new(vec![Saved {
                host: "devbox".into(),
                visited: SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_millis() as u64
                    - 180_000,
                conversation: Conversation {
                    id: "019abcdef".into(),
                    title: "Fix remote conversation picker".into(),
                    cwd: "/home/dev/projects/rcodex".into(),
                    status: "idle".into(),
                    updated_at: 10,
                    ..Default::default()
                },
            }]);
            app.states.insert("devbox".into(), HostState::Reachable);
            match state {
                "checking" => {
                    app.states.insert("devbox".into(), HostState::Checking);
                }
                "offline" => {
                    app.states.insert(
                        "devbox".into(),
                        HostState::Offline("SSH unavailable".into()),
                    );
                }
                "folders" => {
                    app.mode = Mode::Folders {
                        host: "devbox".into(),
                        directory: Directory {
                            path: "/home/dev/projects".into(),
                            parent: Some("/home/dev".into()),
                            folders: vec!["rcodex".into(), "website".into()],
                        },
                        filter: Input::default(),
                        list: ListState::default().with_selected(Some(0)),
                    }
                }
                _ => {}
            }
            let (w, h) = if state == "small" { (40, 12) } else { (96, 25) };
            let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
            terminal.draw(|f| draw(&mut app, f)).unwrap();
            let buffer = terminal.backend().buffer();
            let text = (0..h)
                .map(|y| (0..w).map(|x| buffer[(x, y)].symbol()).collect::<String>())
                .collect::<Vec<_>>()
                .join("\n");
            assert!(text.contains("RCODEX"));
            if state == "small" {
                assert!(text.contains("Fix remote conversation"));
                assert!(text.contains("t shell"));
                assert!(text.contains("q quit"));
            }
            if let Ok(dir) = std::env::var("RCODEX_RENDER_DIR") {
                std::fs::create_dir_all(&dir).unwrap();
                let mut svg = format!(
                    "<svg xmlns='http://www.w3.org/2000/svg' width='{}' height='{}'><rect width='100%' height='100%' fill='#10141c'/><g font-family='DejaVu Sans Mono' font-size='18'>",
                    w * 12,
                    h * 24
                );
                for y in 0..h {
                    for x in 0..w {
                        let cell = &buffer[(x, y)];
                        let color = |c: Color, default: &str| match c {
                            Color::Rgb(r, g, b) => format!("#{r:02x}{g:02x}{b:02x}"),
                            _ => default.into(),
                        };
                        let bg = color(cell.bg, "#10141c");
                        let fg = color(cell.fg, "#dde5f0");
                        let value = cell
                            .symbol()
                            .replace('&', "&amp;")
                            .replace('<', "&lt;")
                            .replace('>', "&gt;");
                        svg.push_str(&format!("<rect x='{}' y='{}' width='12' height='24' fill='{bg}'/><text x='{}' y='{}' fill='{fg}'>{value}</text>", x*12, y*24, x*12, y*24+19));
                    }
                }
                svg.push_str("</g></svg>");
                std::fs::write(format!("{dir}/conversations-{state}.svg"), svg).unwrap();
            }
        }
    }
}
