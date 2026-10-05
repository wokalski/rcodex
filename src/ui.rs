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
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
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
    Search,
    New,
    Confirm(Connection),
    Rename(Connection),
    Mkdir,
    Logs(Connection),
}
#[derive(Clone, Copy)]
pub enum Open {
    New,
    Resume,
    Last,
}
enum Action {
    Nothing,
    Quit,
    Connect(Connection, Open),
    Remote(Request),
    Browse(String),
    Mkdir(String, String),
    Logs(String),
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
    search: Input,
    edit: Input,
    log: String,
    log_scroll: u16,
    log_x: u16,
    log_follow: bool,
    help: bool,
    help_scroll: u16,
    favorites_only: bool,
}
impl App {
    fn visible_rows(&self) -> Vec<&Connection> {
        let query = self.search.value().to_lowercase();
        self.rows
            .iter()
            .filter(|r| {
                (!self.favorites_only || r.favorite)
                    && format!("{} {} {}", r.label(), r.path, r.id)
                        .to_lowercase()
                        .contains(&query)
            })
            .collect()
    }
    fn selected(&self) -> Option<&Connection> {
        self.visible_rows()
            .get(self.list.selected().unwrap_or(0))
            .copied()
    }
    fn replace_rows(&mut self, rows: Vec<Connection>) {
        let selected = self.selected().map(|r| r.id.clone());
        let launch_selected = !self.rows.is_empty() && selected.is_none();
        self.rows = rows;
        let visible = self.visible_rows();
        let index = selected
            .and_then(|id| visible.iter().position(|r| r.id == id))
            .unwrap_or_else(|| {
                if launch_selected {
                    visible.len()
                } else {
                    self.list.selected().unwrap_or(0).min(visible.len())
                }
            });
        self.list.select(Some(index));
    }
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
        if self.help {
            match key.code {
                KeyCode::Up => self.help_scroll = self.help_scroll.saturating_sub(1),
                KeyCode::Down => self.help_scroll = self.help_scroll.saturating_add(1),
                KeyCode::Home => self.help_scroll = 0,
                _ => self.help = false,
            }
            return Action::Nothing;
        }
        if key.code == KeyCode::F(1)
            || (matches!(self.mode, Mode::Browse) && key.code == KeyCode::Char('?'))
        {
            self.help = true;
            self.help_scroll = 0;
            return Action::Nothing;
        }
        if self.busy {
            return Action::Nothing;
        }
        let selected = self.selected().cloned();
        let index = self.list.selected().unwrap_or(0);
        let count = self.visible_rows().len();
        match &mut self.mode {
            Mode::Confirm(c) => {
                let id = c.id.clone();
                self.mode = Mode::Browse;
                if matches!(key.code, KeyCode::Char('y' | 'Y')) {
                    return Action::Remote(Request::Stop { id });
                }
            }
            Mode::Rename(c) => match key.code {
                KeyCode::Esc => self.mode = Mode::Browse,
                KeyCode::Enter => {
                    let id = c.id.clone();
                    self.mode = Mode::Browse;
                    return Action::Remote(Request::Rename {
                        id,
                        name: self.edit.value().into(),
                    });
                }
                _ => {
                    self.edit.handle_event(&Event::Key(key));
                }
            },
            Mode::Mkdir => match key.code {
                KeyCode::Esc => self.mode = Mode::New,
                KeyCode::Enter => {
                    return Action::Mkdir(self.directory.path.clone(), self.edit.value().into());
                }
                _ => {
                    self.edit.handle_event(&Event::Key(key));
                }
            },
            Mode::Search => match key.code {
                KeyCode::Esc => {
                    self.search = Input::default();
                    self.list.select(Some(0));
                    self.mode = Mode::Browse;
                }
                KeyCode::Enter => self.mode = Mode::Browse,
                _ => {
                    self.search.handle_event(&Event::Key(key));
                    self.list.select(Some(0));
                }
            },
            Mode::Logs(c) => match key.code {
                KeyCode::Esc | KeyCode::Char('q') => self.mode = Mode::Browse,
                KeyCode::Char('r') => return Action::Logs(c.id.clone()),
                KeyCode::Char('f') => self.log_follow = !self.log_follow,
                KeyCode::Up | KeyCode::Char('k') | KeyCode::PageUp => {
                    self.log_follow = false;
                    self.log_scroll = self
                        .log_scroll
                        .saturating_sub(if key.code == KeyCode::PageUp { 10 } else { 1 });
                }
                KeyCode::Down | KeyCode::Char('j') | KeyCode::PageDown => {
                    self.log_follow = false;
                    self.log_scroll = self
                        .log_scroll
                        .saturating_add(if key.code == KeyCode::PageDown { 10 } else { 1 });
                }
                KeyCode::Home => {
                    self.log_follow = false;
                    self.log_scroll = 0;
                }
                KeyCode::End => self.log_follow = true,
                KeyCode::Left => self.log_x = self.log_x.saturating_sub(8),
                KeyCode::Right => self.log_x = self.log_x.saturating_add(8),
                _ => {}
            },
            Mode::New => match key.code {
                KeyCode::Char('n') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.edit = Input::default();
                    self.mode = Mode::Mkdir;
                }
                KeyCode::Esc => {
                    self.mode = Mode::Browse;
                    self.error = None;
                }
                KeyCode::Char(' ') if !self.directory.path.is_empty() => {
                    return Action::Remote(Request::Start {
                        path: self.directory.path.clone(),
                        direct,
                        server_name: None,
                        name: None,
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
                KeyCode::Char('f') => {
                    if let Some(c) = selected {
                        return Action::Remote(Request::Favorite {
                            id: c.id,
                            favorite: !c.favorite,
                        });
                    }
                }
                KeyCode::Char('F') => {
                    self.favorites_only = !self.favorites_only;
                    self.list.select(Some(0));
                }
                KeyCode::Esc if !self.search.value().is_empty() => {
                    self.search = Input::default();
                    self.list.select(Some(0));
                }
                KeyCode::Char('q') | KeyCode::Esc => return Action::Quit,
                KeyCode::Char('/') => self.mode = Mode::Search,
                KeyCode::Up | KeyCode::Char('k') => {
                    self.list.select(Some((index + count) % (count + 1)))
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    self.list.select(Some((index + 1) % (count + 1)))
                }
                KeyCode::Char('r') => return Action::Remote(Request::List),
                KeyCode::Char('b') => {
                    if let Some(c) = selected {
                        self.mode = Mode::New;
                        self.error = None;
                        return Action::Browse(c.path);
                    }
                }
                KeyCode::Char('X' | 'Q') => {
                    if let Some(c) = selected {
                        self.mode = Mode::Confirm(c);
                    }
                }
                KeyCode::Char('e') => {
                    if let Some(c) = selected {
                        self.edit = Input::new(c.name.clone().unwrap_or_default());
                        self.mode = Mode::Rename(c);
                    }
                }
                KeyCode::Char('l') => {
                    if let Some(c) = selected {
                        let action = Action::Logs(c.id.clone());
                        self.mode = Mode::Logs(c);
                        self.log.clear();
                        self.log_x = 0;
                        self.log_follow = true;
                        return action;
                    }
                }
                KeyCode::Char('n') => {
                    self.mode = Mode::New;
                    self.error = None;
                    return Action::Browse("~".into());
                }
                KeyCode::Enter if index == count => {
                    self.mode = Mode::New;
                    self.error = None;
                    return Action::Browse("~".into());
                }
                KeyCode::Enter | KeyCode::Char('s' | 'c') => {
                    let Some(c) = selected else {
                        return Action::Nothing;
                    };
                    if direct && !c.direct {
                        self.error = Some(
                            "This server is loopback-only. Reconnect without --direct.".into(),
                        );
                    } else {
                        let open = match key.code {
                            KeyCode::Char('s') => Open::Resume,
                            KeyCode::Char('c') => Open::Last,
                            _ => Open::New,
                        };
                        return Action::Connect(c, open);
                    }
                }
                _ => {}
            },
        }
        Action::Nothing
    }
    fn draw(&mut self, f: &mut Frame, host: &str, direct: bool) {
        self.draw_content(f, host, direct);
        if self.help {
            let area = popup(f.area(), 86, 24);
            f.render_widget(Clear, area);
            f.render_widget(
                Paragraph::new(concat!(
                    "WORKSPACES\n",
                    "↑↓ / j k    Select         enter    New conversation\n",
                    "c           Continue      s        Conversation picker\n",
                    "/           Search        esc      Clear search / quit\n",
                    "f           Star/unstar   F        Favorites only\n",
                    "e           Rename        l        Live server logs\n",
                    "n           New project   b        Browse selected project\n",
                    "X / Q       Stop (asks)   r        Refresh\n\n",
                    "FOLDER BROWSER\n",
                    "enter / →   Open folder   ←        Parent folder\n",
                    "space       Launch here   ctrl+n   Create folder\n",
                    "home        Home folder   typing   Filter folders\n\n",
                    "LOGS\n",
                    "↑↓ / pgup / pgdn scroll   ←→ pan   f follow/pause\n",
                    "home top    end follow    esc back\n\n",
                    "F1 help · ↑↓ scroll · home top · any other key closes"
                ))
                .wrap(Wrap { trim: false })
                .scroll((self.help_scroll, 0))
                .block(
                    Block::bordered()
                        .title(" KEYBOARD GUIDE ")
                        .padding(Padding::uniform(1))
                        .border_style(Style::default().fg(ACCENT)),
                )
                .style(Style::default().bg(BG).fg(FG)),
                area,
            );
        }
    }
    fn draw_content(&mut self, f: &mut Frame, host: &str, direct: bool) {
        f.render_widget(Block::new().style(Style::default().bg(BG).fg(FG)), f.area());
        let area = f.area().inner(Margin {
            horizontal: 3,
            vertical: 1,
        });
        let [header, subtitle, search, body, status, footer] = Layout::vertical([
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
            "DIRECT · TLS encrypted"
        } else {
            "SSH encrypted"
        };
        f.render_widget(
            Paragraph::new(format!("{host}  ·  {transport}")).fg(MUTED),
            subtitle,
        );
        if let Mode::Logs(c) = &self.mode {
            let height = body.height.saturating_sub(2) as usize;
            let bottom = self
                .log
                .lines()
                .count()
                .saturating_sub(height)
                .min(u16::MAX as usize) as u16;
            self.log_scroll = if self.log_follow {
                bottom
            } else {
                self.log_scroll.min(bottom)
            };
            f.render_widget(
                Paragraph::new(self.log.as_str())
                    .block(
                        Block::bordered()
                            .border_style(Style::default().fg(SELECT))
                            .title_style(Style::default().fg(ACCENT))
                            .title(format!(" LOG / {} ", c.label())),
                    )
                    .scroll((self.log_scroll, self.log_x)),
                body,
            );
            let message = self.error.as_deref().unwrap_or(if self.log_follow {
                "Following · refreshes every 2s · latest 500 lines / 128 KiB · server log, not chat history"
            } else { "Paused · server log, not chat history" });
            f.render_widget(
                Paragraph::new(message)
                    .fg(if self.error.is_some() { RED } else { MUTED })
                    .wrap(Wrap { trim: false }),
                status,
            );
            f.render_widget(Paragraph::new("↑↓/pgup/pgdn scroll   ←→ pan   home top   end follow\nf follow/pause   r refresh   esc back").fg(MUTED), footer);
            return;
        }
        if matches!(self.mode, Mode::New | Mode::Mkdir) {
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
            f.render_widget(Paragraph::new("↑↓ select   enter/→ open   ← parent   home ~   space launch   esc cancel\nType to filter · ctrl+n create folder").fg(MUTED).wrap(Wrap { trim: false }), footer);
            if matches!(self.mode, Mode::Mkdir) {
                self.draw_edit(
                    f,
                    body,
                    " NEW FOLDER ",
                    "Creates a folder here and opens it. No files are overwritten.",
                );
            }
            return;
        }
        if matches!(self.mode, Mode::Search) || !self.search.value().is_empty() {
            f.render_widget(
                Paragraph::new(format!(
                    "/ {}{}",
                    self.search.value(),
                    if matches!(self.mode, Mode::Search) {
                        "▏  enter apply · esc clear"
                    } else {
                        "  · esc clear"
                    }
                ))
                .fg(ACCENT),
                search,
            );
        }
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let items: Vec<ListItem> = self
            .visible_rows()
            .iter()
            .map(|r| {
                let mode = if r.certificate.is_some() {
                    "TLS"
                } else if r.direct {
                    "LEGACY"
                } else {
                    "SSH"
                };
                ListItem::new(vec![
                    Line::from(vec![
                        Span::styled(
                            if r.favorite { " ★ " } else { " ● " },
                            Style::default().fg(ACCENT),
                        ),
                        Span::raw(r.label().to_owned()).bold(),
                        Span::styled(
                            format!(
                                "   {} · {mode} · up {}",
                                &r.id[..8.min(r.id.len())],
                                age(now.saturating_sub(r.created))
                            ),
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
                    .title(format!(
                        " {}  {} / {} ",
                        if self.favorites_only {
                            "FAVORITES"
                        } else {
                            "WORKSPACES"
                        },
                        self.visible_rows().len(),
                        self.rows.len()
                    ))
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
                Paragraph::new("Servers survive disconnects · list refreshes every 5s").fg(MUTED),
                status,
            );
        }
        f.render_widget(
            Paragraph::new("↑↓ select   enter new chat   c continue   s sessions   / search   ? help\nf star   F favorites   n new   b browse   e rename   l logs   X/Q stop   q quit")
                .fg(MUTED)
                .wrap(Wrap { trim: false }),
            footer,
        );
        match &self.mode {
            Mode::Confirm(c) => {
                let popup = popup(body, 68, 9);
                f.render_widget(Clear, popup);
                f.render_widget(
                    Paragraph::new(vec![
                        Line::from(c.label()).bold(),
                        Line::from(c.id.as_str()).fg(MUTED),
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
            Mode::Rename(_) => self.draw_edit(
                f,
                body,
                " RENAME SERVER ",
                "A label for this server. Leave empty to use the folder name.",
            ),
            _ => {}
        }
    }
    fn draw_edit(&self, f: &mut Frame, body: Rect, title: &str, hint: &str) {
        let area = popup(body, 72, 8);
        f.render_widget(Clear, area);
        let block = Block::bordered()
            .title(title)
            .padding(Padding::uniform(1))
            .border_style(Style::default().fg(ACCENT))
            .style(Style::default().bg(BG).fg(FG));
        let inner = block.inner(area);
        f.render_widget(block, area);
        let [input, _, hint_area, help] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .areas(inner);
        let scroll = self
            .edit
            .visual_scroll(input.width.saturating_sub(1) as usize);
        f.render_widget(
            Paragraph::new(self.edit.value())
                .fg(ACCENT)
                .scroll((0, scroll.min(u16::MAX as usize) as u16)),
            input,
        );
        f.render_widget(Paragraph::new(hint).fg(MUTED), hint_area);
        f.render_widget(Paragraph::new("enter save   esc cancel").fg(MUTED), help);
        if input.width > 0 && input.height > 0 {
            let cursor = self
                .edit
                .visual_cursor()
                .saturating_sub(scroll)
                .min(input.width as usize - 1);
            f.set_cursor_position((input.x + cursor as u16, input.y));
        }
    }
}
fn age(seconds: u64) -> String {
    match seconds {
        0..60 => format!("{seconds}s"),
        60..3600 => format!("{}m", seconds / 60),
        3600..86400 => format!("{}h {}m", seconds / 3600, seconds % 3600 / 60),
        _ => format!("{}d {}h", seconds / 86400, seconds % 86400 / 3600),
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
    Logs(String),
}
fn dispatch(client: Client, action: Action) -> Receiver<Result<Loaded>> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        if let Action::Browse(path) = action {
            let _ = tx.send(client.browse(&path).map(Loaded::Directory));
            return;
        }
        if let Action::Mkdir(parent, name) = action {
            let _ = tx.send(client.mkdir(&parent, &name).map(Loaded::Directory));
            return;
        }
        if let Action::Logs(id) = action {
            let _ = tx.send(client.logs(&id, 500).map(Loaded::Logs));
            return;
        }
        let Action::Remote(request) = action else {
            return;
        };
        let start = matches!(request, Request::Start { .. });
        let refresh = matches!(
            request,
            Request::Stop { .. } | Request::Rename { .. } | Request::Favorite { .. }
        );
        let result = client.call(request).and_then(|rows| {
            if refresh {
                client.call(Request::List)
            } else {
                Ok(rows)
            }
        });
        let _ = tx.send(result.map(|rows| Loaded::Rows(start, rows)));
    });
    rx
}
pub fn pick(client: Client, direct: bool) -> Result<Option<(Connection, Open)>> {
    let mut terminal = ratatui::try_init()?;
    let result = (|| -> Result<Option<(Connection, Open)>> {
        crossterm::execute!(std::io::stdout(), EnableBracketedPaste)?;
        let mut app = App {
            busy: true,
            ..App::default()
        };
        app.list.select(Some(0));
        let mut pending = Some(dispatch(client.clone(), Action::Remote(Request::List)));
        let mut refreshed = Instant::now();
        loop {
            if let Some(rx) = &pending
                && let Ok(result) = rx.try_recv()
            {
                pending = None;
                app.busy = false;
                refreshed = Instant::now();
                match result {
                    Ok(Loaded::Directory(directory)) => {
                        app.mode = Mode::New;
                        app.directory = directory;
                        app.filter = Input::default();
                        app.folders = ListState::default().with_selected(Some(0));
                    }
                    Ok(Loaded::Rows(true, rows)) => {
                        return Ok(rows.into_iter().next().map(|c| (c, Open::New)));
                    }
                    Ok(Loaded::Rows(false, rows)) => app.replace_rows(rows),
                    Ok(Loaded::Logs(text)) => app.log = text,
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
                            Action::Connect(c, open) => return Ok(Some((c, open))),
                            action @ (Action::Remote(_)
                            | Action::Browse(_)
                            | Action::Mkdir(_, _)
                            | Action::Logs(_)) => {
                                app.error = None;
                                app.busy = true;
                                pending = Some(dispatch(client.clone(), action));
                            }
                        }
                    }
                    Event::Paste(text) if !app.busy && !app.help => {
                        let event =
                            Event::Paste(text.chars().filter(|c| !c.is_control()).collect());
                        match app.mode {
                            Mode::New => {
                                app.filter.handle_event(&event);
                                app.folders.select(Some(0));
                            }
                            Mode::Search => {
                                app.search.handle_event(&event);
                                app.list.select(Some(0));
                            }
                            Mode::Rename(_) | Mode::Mkdir => {
                                app.edit.handle_event(&event);
                            }
                            _ => {}
                        }
                    }
                    _ => {}
                }
            }
            if pending.is_none() {
                let refresh = match &app.mode {
                    Mode::Browse if refreshed.elapsed() >= Duration::from_secs(5) => {
                        Some(Action::Remote(Request::List))
                    }
                    Mode::Logs(c)
                        if app.log_follow && refreshed.elapsed() >= Duration::from_secs(2) =>
                    {
                        Some(Action::Logs(c.id.clone()))
                    }
                    _ => None,
                };
                if let Some(action) = refresh {
                    // Background reads must not swallow navigation or text input.
                    // An explicit action can replace this read-only receiver.
                    app.error = None;
                    pending = Some(dispatch(client.clone(), action));
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
                    name: None,
                    favorite: false,
                    last_used: 0,
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
                    name: Some("Overnight refactor".into()),
                    favorite: true,
                    last_used: 0,
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
    fn favorites_filter_actions_and_help_do_not_target_hidden_rows() {
        let mut app = fixture();
        app.key(KeyCode::Char('F').into(), false);
        assert_eq!(app.visible_rows().len(), 1);
        assert!(
            matches!(app.key(KeyCode::Char('f').into(), false), Action::Remote(Request::Favorite { id, favorite: false }) if id == "b".repeat(32))
        );
        app.key(KeyCode::Char('?').into(), false);
        assert!(app.help);
        assert!(matches!(
            app.key(KeyCode::Char('X').into(), false),
            Action::Nothing
        ));
        assert!(!app.help);
        assert!(matches!(app.mode, Mode::Browse));
        app.search = Input::new("example".into());
        assert!(app.visible_rows().is_empty());
        assert!(matches!(
            app.key(KeyCode::Char('f').into(), false),
            Action::Nothing
        ));
        app.mode = Mode::New;
        app.key(KeyCode::F(1).into(), false);
        app.key(KeyCode::Esc.into(), false);
        assert!(matches!(app.mode, Mode::New));
    }

    #[test]
    fn uppercase_kills_lowercase_quits_and_cancel_preserves_server() {
        for key in ['X', 'Q'] {
            let mut app = fixture();
            assert!(matches!(
                app.key(KeyCode::Char(key).into(), false),
                Action::Nothing
            ));
            assert!(matches!(app.mode, Mode::Confirm(_)));
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
        assert!(
            matches!(app.key(KeyCode::Enter.into(),true),Action::Connect(c, Open::New) if c.port==39841)
        );
    }
    #[test]
    fn search_resume_rename_and_stop_target_the_visible_server() {
        let mut app = fixture();
        app.key(KeyCode::Char('/').into(), false);
        for c in "OVERNIGHT".chars() {
            app.key(KeyCode::Char(c).into(), false);
        }
        assert_eq!(app.visible_rows().len(), 1);
        app.key(KeyCode::Enter.into(), false);
        assert!(
            matches!(app.key(KeyCode::Char('s').into(), false), Action::Connect(c, Open::Resume) if c.id == "b".repeat(32))
        );
        assert!(
            matches!(app.key(KeyCode::Char('c').into(), false), Action::Connect(c, Open::Last) if c.id == "b".repeat(32))
        );
        app.key(KeyCode::Char('e').into(), false);
        app.edit = Input::new("Renamed".into());
        assert!(
            matches!(app.key(KeyCode::Enter.into(), false), Action::Remote(Request::Rename { id, name }) if id == "b".repeat(32) && name == "Renamed")
        );
        app.key(KeyCode::Char('X').into(), false);
        // Even a list change after opening the confirmation cannot change its target.
        app.rows.reverse();
        assert!(
            matches!(app.key(KeyCode::Char('y').into(), false), Action::Remote(Request::Stop { id }) if id == "b".repeat(32))
        );
        app.key(KeyCode::Esc.into(), false);
        assert!(app.search.value().is_empty());
        assert!(matches!(app.mode, Mode::Browse));
    }

    #[test]
    fn refresh_preserves_identity_and_the_new_project_entry() {
        let mut app = fixture();
        app.list.select(Some(1));
        let mut rows = app.rows.clone();
        rows.reverse();
        app.replace_rows(rows);
        assert_eq!(app.list.selected(), Some(0));
        assert_eq!(app.selected().unwrap().id, "b".repeat(32));
        app.list.select(Some(2));
        app.replace_rows(vec![app.rows[0].clone()]);
        assert_eq!(app.list.selected(), Some(1));
        assert!(app.selected().is_none());
        app.replace_rows(vec![]);
        assert_eq!(app.list.selected(), Some(0));
    }

    #[test]
    fn folder_creation_and_log_navigation_do_not_launch_or_stop_servers() {
        let mut app = fixture();
        app.key(KeyCode::Char('n').into(), false);
        app.directory.path = "/projects".into();
        app.key(
            KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL),
            false,
        );
        app.edit = Input::new("café".into());
        assert!(
            matches!(app.key(KeyCode::Enter.into(), false), Action::Mkdir(parent, name) if parent == "/projects" && name == "café")
        );
        app.key(KeyCode::Esc.into(), false);
        assert!(matches!(app.mode, Mode::New));
        app.key(KeyCode::Esc.into(), false);
        assert!(
            matches!(app.key(KeyCode::Char('l').into(), false), Action::Logs(id) if id == "a".repeat(32))
        );
        assert!(app.log_follow);
        app.log_scroll = 15;
        app.key(KeyCode::PageUp.into(), false);
        assert_eq!(app.log_scroll, 5);
        assert!(!app.log_follow);
        app.key(KeyCode::End.into(), false);
        assert!(app.log_follow);
        app.key(KeyCode::Esc.into(), false);
        assert!(matches!(app.mode, Mode::Browse));
        assert_eq!(age(59), "59s");
        assert_eq!(age(60), "1m");
        assert_eq!(age(7320), "2h 2m");
        assert_eq!(age(90000), "1d 1h");
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
            for mode in [
                Mode::Browse,
                Mode::Confirm(app.rows[0].clone()),
                Mode::New,
                Mode::Search,
                Mode::Rename(app.rows[0].clone()),
                Mode::Mkdir,
                Mode::Logs(app.rows[0].clone()),
            ] {
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
            ("help", Mode::Browse),
            ("favorites", Mode::Browse),
            ("new-project", Mode::New),
            ("browser-empty", Mode::New),
            ("browser-error", Mode::New),
            ("stop-server", Mode::Confirm(fixture().rows[0].clone())),
            ("search", Mode::Search),
            ("rename", Mode::Rename(fixture().rows[1].clone())),
            ("create-folder", Mode::Mkdir),
            ("logs", Mode::Logs(fixture().rows[1].clone())),
        ] {
            let mut app = fixture();
            app.mode = mode;
            app.help = name == "help";
            app.favorites_only = name == "favorites";
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs();
            app.rows[0].created = now - 7320;
            app.rows[1].created = now - 480;
            app.rows[1].certificate = Some("fixture".into());
            app.edit = Input::new(
                if name == "rename" {
                    "Overnight refactor"
                } else {
                    "new-project"
                }
                .into(),
            );
            app.log = "codex app-server (WebSockets)\n  listening on: ws://127.0.0.1:39841\n  readyz: http://127.0.0.1:39841/readyz\n  healthz: http://127.0.0.1:39841/healthz\nConnected client: codex-tui\nThread resumed successfully".into();
            app.log_follow = true;
            if name == "search" {
                app.search = Input::new("overnight".into());
            }
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
            assert!(name == "help" || text.contains("RCODEX"));
            assert!(text.contains(match name {
                "new-project" | "browser-empty" | "browser-error" => "space launch",
                "stop-server" => "y stop server   any other key cancel",
                "search" => "Overnight refactor",
                "help" => "KEYBOARD GUIDE",
                "favorites" => "FAVORITES",
                "rename" => "RENAME SERVER",
                "create-folder" => "NEW FOLDER",
                "logs" => "Thread resumed successfully",
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
