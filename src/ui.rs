use crate::{
    remote::{Connection, Directory, Request},
    ssh::Client,
};
use anyhow::Result;
use crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers,
};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Margin, Rect},
    style::{Color, Style, Stylize},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Padding, Paragraph, Wrap},
};
use std::{
    path::Path,
    sync::mpsc::{self, Receiver},
    thread,
    time::Duration,
};
use tui_input::{Input, backend::crossterm::EventHandler};

const BG: Color = Color::Rgb(16, 20, 28);
const FG: Color = Color::Rgb(221, 229, 240);
const MUTED: Color = Color::Rgb(130, 147, 168);
const ACCENT: Color = Color::Rgb(105, 220, 205);
const SELECT: Color = Color::Rgb(31, 48, 61);
const RED: Color = Color::Rgb(255, 133, 133);

#[derive(Default)]
enum Mode {
    #[default]
    Browse,
    New,
    Confirm,
}
enum Action {
    Nothing,
    Quit,
    Connect(Connection),
    Remote(Request),
    Browse(String),
}
#[derive(Default)]
struct App {
    rows: Vec<Connection>,
    list: ListState,
    mode: Mode,
    error: Option<String>,
    busy: bool,
    tick: usize,
    directory: Directory,
    folders: ListState,
    filter: Input,
}
impl App {
    fn visible_folders(&self) -> Vec<&String> {
        let query = self.filter.value().to_lowercase();
        self.directory
            .folders
            .iter()
            .filter(|name| name.to_lowercase().contains(&query))
            .collect()
    }
    fn key(&mut self, key: KeyEvent, direct: bool) -> Action {
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return Action::Quit;
        }
        if self.busy {
            return Action::Nothing;
        }
        let index = self.list.selected().unwrap_or(0);
        match &mut self.mode {
            Mode::Confirm => {
                self.mode = Mode::Browse;
                if matches!(key.code, KeyCode::Char('y' | 'Y')) {
                    return Action::Remote(Request::Stop {
                        id: self.rows[index].id.clone(),
                    });
                }
            }
            Mode::New => match key.code {
                KeyCode::Esc => {
                    self.mode = Mode::Browse;
                    self.error = None;
                }
                KeyCode::Char(' ') if !self.directory.path.is_empty() => {
                    return Action::Remote(Request::Start {
                        path: self.directory.path.clone(),
                        direct,
                        server_name: None,
                    });
                }
                KeyCode::Left => {
                    if let Some(parent) = &self.directory.parent {
                        return Action::Browse(parent.clone());
                    }
                }
                KeyCode::Home => return Action::Browse("~".into()),
                KeyCode::Enter | KeyCode::Right => {
                    if let Some(name) = self
                        .visible_folders()
                        .get(self.folders.selected().unwrap_or(0))
                    {
                        return Action::Browse(
                            Path::new(&self.directory.path)
                                .join(name)
                                .to_string_lossy()
                                .into(),
                        );
                    }
                }
                KeyCode::Up | KeyCode::Down => {
                    let count = self.visible_folders().len();
                    if count > 0 {
                        let selected = self.folders.selected().unwrap_or(0);
                        let step = if key.code == KeyCode::Up {
                            count - 1
                        } else {
                            1
                        };
                        self.folders.select(Some((selected + step) % count));
                    }
                }
                _ => {
                    self.filter.handle_event(&Event::Key(key));
                    self.folders.select(Some(0));
                }
            },
            Mode::Browse => match key.code {
                KeyCode::Char('q') | KeyCode::Esc => return Action::Quit,
                KeyCode::Up | KeyCode::Char('k') => self
                    .list
                    .select(Some((index + self.rows.len()) % (self.rows.len() + 1))),
                KeyCode::Down | KeyCode::Char('j') => {
                    self.list.select(Some((index + 1) % (self.rows.len() + 1)))
                }
                KeyCode::Char('r') => return Action::Remote(Request::List),
                KeyCode::Char('X' | 'Q') if index < self.rows.len() => self.mode = Mode::Confirm,
                KeyCode::Char('n') => {
                    self.mode = Mode::New;
                    self.error = None;
                    return Action::Browse("~".into());
                }
                KeyCode::Enter if index == self.rows.len() => {
                    self.mode = Mode::New;
                    self.error = None;
                    return Action::Browse("~".into());
                }
                KeyCode::Enter => {
                    if direct && !self.rows[index].direct {
                        self.error = Some(
                            "This server is loopback-only. Reconnect without --direct.".into(),
                        );
                    } else {
                        return Action::Connect(self.rows[index].clone());
                    }
                }
                _ => {}
            },
        }
        Action::Nothing
    }
    fn draw(&mut self, f: &mut Frame, host: &str, direct: bool) {
        f.render_widget(Block::new().style(Style::default().bg(BG).fg(FG)), f.area());
        let area = f.area().inner(Margin {
            horizontal: 3,
            vertical: 1,
        });
        let [header, subtitle, _, body, status, footer] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(2),
            Constraint::Length(2),
        ])
        .areas(area);
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("RCODEX", Style::default().fg(ACCENT).bold()),
                Span::styled("  /  remote workspaces", Style::default().fg(MUTED)),
            ])),
            header,
        );
        let transport = if direct {
            "DIRECT · trusted network only"
        } else {
            "SSH encrypted"
        };
        f.render_widget(
            Paragraph::new(format!("{host}  ·  {transport}")).fg(MUTED),
            subtitle,
        );
        if matches!(self.mode, Mode::New) {
            let [path, filter, folders] = Layout::vertical([
                Constraint::Length(2),
                Constraint::Length(2),
                Constraint::Min(1),
            ])
            .areas(body);
            f.render_widget(
                Paragraph::new(format!("Choose a project  /  {}", self.directory.path))
                    .fg(ACCENT)
                    .wrap(Wrap { trim: false }),
                path,
            );
            f.render_widget(
                Paragraph::new(format!("Filter: {}", self.filter.value())).fg(MUTED),
                filter,
            );
            let names = self.visible_folders();
            let items: Vec<_> = names
                .iter()
                .map(|name| ListItem::new(format!("  {name}/")))
                .collect();
            if items.is_empty() {
                f.render_widget(
                    Paragraph::new("No matching folders. Space launches in the current directory.")
                        .fg(MUTED)
                        .wrap(Wrap { trim: false }),
                    folders,
                );
            } else {
                f.render_stateful_widget(
                    List::new(items)
                        .highlight_style(Style::default().bg(SELECT).fg(ACCENT))
                        .highlight_symbol("▎ "),
                    folders,
                    &mut self.folders,
                );
            }
            let message = self.error.as_deref().unwrap_or(if self.busy {
                "Loading remote directory…"
            } else {
                "Space launches in the current directory. Enter opens the selected folder."
            });
            f.render_widget(
                Paragraph::new(message)
                    .fg(if self.error.is_some() { RED } else { MUTED })
                    .wrap(Wrap { trim: false }),
                status,
            );
            f.render_widget(Paragraph::new("↑↓ select   enter/→ open   ← parent   home ~   space launch   esc cancel\nType to filter folders · backspace to edit").fg(MUTED).wrap(Wrap { trim: false }), footer);
            return;
        }
        let items: Vec<ListItem> = self
            .rows
            .iter()
            .map(|r| {
                let name = Path::new(&r.path)
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy();
                let mode = if r.direct { "DIRECT" } else { "SSH" };
                ListItem::new(vec![
                    Line::from(vec![
                        Span::styled(" ● ", Style::default().fg(ACCENT)),
                        Span::raw(name.into_owned()).bold(),
                        Span::styled(
                            format!("   :{} · {mode}", r.port),
                            Style::default().fg(MUTED),
                        ),
                    ]),
                    Line::from(format!("   {}", r.path)).fg(MUTED),
                    Line::from(""),
                ])
            })
            .chain(std::iter::once(ListItem::new(vec![
                Line::from(" + Launch a new project").fg(ACCENT),
                Line::from("   Start a persistent Codex app-server").fg(MUTED),
                Line::from(""),
            ])))
            .collect();
        let list = List::new(items)
            .block(
                Block::new()
                    .borders(Borders::TOP)
                    .border_style(Style::default().fg(SELECT))
                    .title(format!(" WORKSPACES  {:02} ", self.rows.len()))
                    .title_style(Style::default().fg(MUTED))
                    .padding(Padding::top(1)),
            )
            .highlight_style(Style::default().bg(SELECT))
            .highlight_symbol("▎ ");
        f.render_stateful_widget(list, body, &mut self.list);
        if let Some(error) = &self.error {
            f.render_widget(
                Paragraph::new(error.as_str())
                    .fg(RED)
                    .wrap(Wrap { trim: false }),
                status,
            );
        } else if self.busy {
            let spinner = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
            f.render_widget(
                Paragraph::new(format!("{} Working…", spinner[self.tick % spinner.len()]))
                    .fg(ACCENT),
                status,
            );
        } else {
            f.render_widget(
                Paragraph::new("Servers keep running when you disconnect.").fg(MUTED),
                status,
            );
        }
        f.render_widget(
            Paragraph::new("↑↓ select   enter connect   n new   X/Q stop   r refresh   q quit")
                .fg(MUTED)
                .wrap(Wrap { trim: false }),
            footer,
        );
        match &self.mode {
            Mode::Confirm => {
                let popup = popup(body, 68, 8);
                f.render_widget(Clear, popup);
                let name = &self.rows[self.list.selected().unwrap_or(0)].path;
                f.render_widget(
                    Paragraph::new(vec![
                        Line::from(name.as_str()).bold(),
                        Line::from("All connected clients will disconnect.").fg(MUTED),
                        Line::from(""),
                        Line::from("y stop server   any other key cancel").fg(RED),
                    ])
                    .block(
                        Block::bordered()
                            .title(" STOP SERVER? ")
                            .border_style(Style::default().fg(RED))
                            .padding(Padding::uniform(1)),
                    )
                    .style(Style::default().bg(BG).fg(FG)),
                    popup,
                );
            }
            Mode::Browse | Mode::New => {}
        }
    }
}
fn popup(area: Rect, width: u16, height: u16) -> Rect {
    let w = width.min(area.width);
    let h = height.min(area.height);
    Rect::new(
        area.x + (area.width - w) / 2,
        area.y + (area.height - h) / 2,
        w,
        h,
    )
}
enum Loaded {
    Rows(bool, Vec<Connection>),
    Directory(Directory),
}
fn dispatch(client: Client, action: Action) -> Receiver<Result<Loaded>> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        if let Action::Browse(path) = action {
            let _ = tx.send(client.browse(&path).map(Loaded::Directory));
            return;
        }
        let Action::Remote(request) = action else {
            return;
        };
        let start = matches!(request, Request::Start { .. });
        let stop = matches!(request, Request::Stop { .. });
        let result = client.call(request).and_then(|rows| {
            if stop {
                client.call(Request::List)
            } else {
                Ok(rows)
            }
        });
        let _ = tx.send(result.map(|rows| Loaded::Rows(start, rows)));
    });
    rx
}
pub fn pick(client: Client, direct: bool) -> Result<Option<Connection>> {
    let mut terminal = ratatui::try_init()?;
    let result = (|| -> Result<Option<Connection>> {
        crossterm::execute!(std::io::stdout(), EnableBracketedPaste)?;
        let mut app = App {
            busy: true,
            ..App::default()
        };
        app.list.select(Some(0));
        let mut pending = Some(dispatch(client.clone(), Action::Remote(Request::List)));
        loop {
            if let Some(rx) = &pending
                && let Ok(result) = rx.try_recv()
            {
                pending = None;
                app.busy = false;
                match result {
                    Ok(Loaded::Directory(directory)) => {
                        app.directory = directory;
                        app.filter = Input::default();
                        app.folders = ListState::default().with_selected(Some(0));
                    }
                    Ok(Loaded::Rows(true, rows)) => return Ok(rows.into_iter().next()),
                    Ok(Loaded::Rows(false, rows)) => {
                        app.list
                            .select(Some(app.list.selected().unwrap_or(0).min(rows.len())));
                        app.rows = rows;
                    }
                    Err(e) => app.error = Some(format!("{e:#}")),
                }
            }
            terminal.draw(|f| app.draw(f, &client.host, direct))?;
            if event::poll(Duration::from_millis(80))? {
                match event::read()? {
                    Event::Key(key) if key.kind == KeyEventKind::Press => {
                        match app.key(key, direct) {
                            Action::Nothing => {}
                            Action::Quit => return Ok(None),
                            Action::Connect(c) => return Ok(Some(c)),
                            action @ (Action::Remote(_) | Action::Browse(_)) => {
                                app.error = None;
                                app.busy = true;
                                pending = Some(dispatch(client.clone(), action));
                            }
                        }
                    }
                    Event::Paste(text) if matches!(app.mode, Mode::New) && !app.busy => {
                        app.filter.handle_event(&Event::Paste(text));
                        app.folders.select(Some(0));
                    }
                    _ => {}
                }
            }
            app.tick += 1;
        }
    })();
    let _ = crossterm::execute!(std::io::stdout(), DisableBracketedPaste);
    ratatui::restore();
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> App {
        let mut app = App {
            rows: vec![
                Connection {
                    id: "a".repeat(32),
                    path: "/home/dev/projects/example-app".into(),
                    pid: 123,
                    start: "123".into(),
                    port: 43127,
                    direct: false,
                    token: None,
                    certificate: None,
                    log: "log".into(),
                    created: 0,
                },
                Connection {
                    id: "b".repeat(32),
                    path: "/home/dev/projects/rcodex".into(),
                    pid: 456,
                    start: "456".into(),
                    port: 39841,
                    direct: true,
                    token: None,
                    certificate: None,
                    log: "log".into(),
                    created: 0,
                },
            ],
            ..App::default()
        };
        app.list.select(Some(0));
        app
    }
    #[test]
    fn uppercase_kills_lowercase_quits_and_cancel_preserves_server() {
        for key in ['X', 'Q'] {
            let mut app = fixture();
            assert!(matches!(
                app.key(KeyCode::Char(key).into(), false),
                Action::Nothing
            ));
            assert!(matches!(app.mode, Mode::Confirm));
            assert!(matches!(
                app.key(KeyCode::Char('n').into(), false),
                Action::Nothing
            ));
            assert!(matches!(app.mode, Mode::Browse));
            app.key(KeyCode::Char(key).into(), false);
            assert!(
                matches!(app.key(KeyCode::Char('y').into(),false),Action::Remote(Request::Stop{id}) if id=="a".repeat(32))
            );
        }
        assert!(matches!(
            fixture().key(KeyCode::Char('q').into(), false),
            Action::Quit
        ));
    }
    #[test]
    fn direct_mode_rejects_loopback_but_selects_direct_server() {
        let mut app = fixture();
        assert!(matches!(
            app.key(KeyCode::Enter.into(), true),
            Action::Nothing
        ));
        assert!(app.error.is_some());
        app.key(KeyCode::Down.into(), true);
        assert!(matches!(app.key(KeyCode::Enter.into(),true),Action::Connect(c) if c.port==39841));
    }
    #[test]
    fn browser_filters_opens_and_launches_current_directory() {
        let mut app = fixture();
        assert!(
            matches!(app.key(KeyCode::Char('n').into(), false), Action::Browse(path) if path == "~")
        );
        app.directory = Directory {
            path: "/projects".into(),
            parent: Some("/".into()),
            folders: vec!["alpha".into(), "café".into()],
        };
        app.key(KeyCode::Down.into(), false);
        assert!(
            matches!(app.key(KeyCode::Enter.into(), false), Action::Browse(path) if path == "/projects/café")
        );
        app.key(KeyCode::Char('a').into(), false);
        app.key(KeyCode::Char('l').into(), false);
        assert!(
            matches!(app.key(KeyCode::Enter.into(), false), Action::Browse(path) if path == "/projects/alpha")
        );
        assert!(
            matches!(app.key(KeyCode::Char(' ').into(), true), Action::Remote(Request::Start {path, direct:true, ..}) if path == "/projects")
        );
        assert!(
            matches!(app.key(KeyCode::Left.into(), false), Action::Browse(path) if path == "/")
        );
        app.filter = Input::new("missing".into());
        assert!(matches!(
            app.key(KeyCode::Enter.into(), false),
            Action::Nothing
        ));
        app.directory.parent = None;
        assert!(matches!(
            app.key(KeyCode::Left.into(), false),
            Action::Nothing
        ));
        app.key(KeyCode::Esc.into(), false);
        assert!(matches!(app.mode, Mode::Browse));
    }
    #[test]
    fn layouts_tolerate_small_terminals_and_long_paths() {
        use ratatui::{Terminal, backend::TestBackend};
        for (width, height) in [(20, 8), (60, 20), (120, 40)] {
            let mut app = fixture();
            app.directory.path = "/日本語".repeat(40);
            for mode in [Mode::Browse, Mode::Confirm, Mode::New] {
                app.mode = mode;
                Terminal::new(TestBackend::new(width, height))
                    .unwrap()
                    .draw(|f| app.draw(f, "devbox", true))
                    .unwrap();
            }
        }
    }
    #[test]
    fn render_states() {
        use ratatui::{Terminal, backend::TestBackend};
        for (name, mode) in [
            ("workspaces", Mode::Browse),
            ("new-project", Mode::New),
            ("browser-empty", Mode::New),
            ("browser-error", Mode::New),
            ("stop-server", Mode::Confirm),
        ] {
            let mut app = fixture();
            app.mode = mode;
            app.directory = Directory {
                path: "/home/dev/projects".into(),
                parent: Some("/home/dev".into()),
                folders: vec![
                    "example-app".into(),
                    "rcodex".into(),
                    "server".into(),
                    "website".into(),
                ],
            };
            app.folders.select(Some(1));
            if name == "browser-empty" {
                app.filter = Input::new("no-match".into());
            }
            if name == "browser-error" {
                app.error = Some("read remote directory: Permission denied (os error 13)".into());
            }
            let mut terminal = Terminal::new(TestBackend::new(96, 25)).unwrap();
            terminal.draw(|f| app.draw(f, "devbox", false)).unwrap();
            let buffer = terminal.backend().buffer();
            let text = (0..25)
                .map(|y| (0..96).map(|x| buffer[(x, y)].symbol()).collect::<String>())
                .collect::<Vec<_>>()
                .join("\n");
            assert!(text.contains("RCODEX"));
            assert!(text.contains(match name {
                "new-project" | "browser-empty" | "browser-error" => "space launch",
                "stop-server" => "y stop server   any other key cancel",
                _ => "example-app",
            }));
            if let Ok(dir) = std::env::var("RCODEX_RENDER_DIR") {
                let mut svg = String::from(
                    "<svg xmlns='http://www.w3.org/2000/svg' width='1152' height='600'><rect width='100%' height='100%' fill='#10141c'/><g font-family='DejaVu Sans Mono' font-size='18'>",
                );
                for y in 0..25 {
                    for x in 0..96 {
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
                        svg.push_str(&format!("<rect x='{}' y='{}' width='12' height='24' fill='{bg}'/><text x='{}' y='{}' fill='{fg}'>{value}</text>",x*12,y*24,x*12,y*24+19));
                    }
                }
                svg.push_str("</g></svg>");
                std::fs::create_dir_all(&dir).unwrap();
                std::fs::write(format!("{dir}/{name}.svg"), svg).unwrap();
            }
        }
    }
}
