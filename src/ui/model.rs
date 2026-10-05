//! Picker state transitions. No terminal, filesystem, runtime, or network I/O.
use crate::{history::Saved, remote::Directory, sessions::Conversation};
use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::widgets::ListState;
use std::{collections::HashMap, path::Path};
use tui_input::{Input, backend::crossterm::EventHandler};

#[derive(Clone, Debug)]
pub enum Choice {
    Resume {
        host: String,
        conversation: Conversation,
    },
    New {
        host: String,
        path: String,
    },
    Connect {
        host: String,
    },
    Shell {
        host: String,
        path: String,
    },
}

#[derive(Clone)]
pub(super) enum HostState {
    Checking,
    Reachable,
    Stopped,
    Offline(String),
}

pub(super) enum Mode {
    Loading,
    List,
    Search,
    Host(Input),
    Folders {
        host: String,
        directory: Directory,
        filter: Input,
        list: ListState,
    },
    Mkdir {
        host: String,
        directory: Directory,
        input: Input,
    },
}

pub(super) enum Effect {
    None,
    Exit(Option<Choice>),
    Refresh(Vec<String>),
    Folder {
        host: String,
        path: String,
        name: Option<String>,
    },
    CancelFolder,
}

pub(super) struct App {
    pub rows: Vec<Saved>,
    pub list: ListState,
    pub search: Input,
    pub mode: Mode,
    pub states: HashMap<String, HostState>,
    pub message: Option<String>,
}

impl App {
    pub fn new(rows: Vec<Saved>) -> Self {
        Self {
            rows,
            list: ListState::default().with_selected(Some(0)),
            search: Input::default(),
            mode: Mode::List,
            states: HashMap::new(),
            message: None,
        }
    }

    pub fn visible(&self) -> Vec<&Saved> {
        let q = self.search.value().to_lowercase();
        self.rows
            .iter()
            .filter(|r| {
                format!(
                    "{} {} {} {}",
                    r.host, r.conversation.title, r.conversation.cwd, r.conversation.id
                )
                .to_lowercase()
                .contains(&q)
            })
            .collect()
    }

    pub fn selected(&self) -> Option<&Saved> {
        self.visible()
            .get(self.list.selected().unwrap_or(0))
            .copied()
    }

    pub fn replace(&mut self, rows: Vec<Saved>) {
        let key = self
            .selected()
            .map(|r| (r.host.clone(), r.conversation.id.clone()));
        self.rows = rows;
        let index = key
            .and_then(|(host, id)| {
                self.visible()
                    .iter()
                    .position(|r| r.host == host && r.conversation.id == id)
            })
            .unwrap_or(0);
        self.list
            .select(Some(index.min(self.visible().len().saturating_sub(1))));
    }

    pub fn refresh(&mut self, hosts: Vec<String>) -> Effect {
        let hosts: Vec<_> = hosts
            .into_iter()
            .filter(|h| !matches!(self.states.get(h), Some(HostState::Checking)))
            .collect();
        for host in &hosts {
            self.states.insert(host.clone(), HostState::Checking);
        }
        Effect::Refresh(hosts)
    }

    pub fn folders(&mut self, result: anyhow::Result<(String, Directory)>) {
        match result {
            Ok((host, directory)) => {
                self.message = None;
                self.mode = Mode::Folders {
                    host,
                    directory,
                    filter: Input::default(),
                    list: ListState::default().with_selected(Some(0)),
                };
            }
            Err(e) => {
                self.message = Some(format!("{e:#}"));
                self.mode = Mode::List;
            }
        }
    }

    pub fn event(&mut self, event: Event, hosts: &[String], host_filter: Option<&str>) -> Effect {
        if let Event::Paste(text) = event {
            let input = match &mut self.mode {
                Mode::Search => {
                    self.list.select(Some(0));
                    &mut self.search
                }
                Mode::Host(input) | Mode::Mkdir { input, .. } => input,
                Mode::Folders { filter, list, .. } => {
                    list.select(Some(0));
                    filter
                }
                _ => return Effect::None,
            };
            for c in text.chars().filter(|c| !c.is_control()) {
                input.handle(tui_input::InputRequest::InsertChar(c));
            }
            return Effect::None;
        }
        let Event::Key(key) = event else {
            return Effect::None;
        };
        if key.kind != KeyEventKind::Press {
            return Effect::None;
        }
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return Effect::Exit(None);
        }
        let effect = match &mut self.mode {
            Mode::Loading => {
                if key.code == KeyCode::Esc {
                    self.mode = Mode::List;
                    Effect::CancelFolder
                } else {
                    Effect::None
                }
            }
            Mode::Search => {
                match key.code {
                    KeyCode::Esc => {
                        self.search = Input::default();
                        self.mode = Mode::List;
                    }
                    KeyCode::Enter => self.mode = Mode::List,
                    _ => {
                        self.search.handle_event(&event);
                        self.list.select(Some(0));
                    }
                }
                Effect::None
            }
            Mode::Host(input) => {
                match key.code {
                    KeyCode::Esc => self.mode = Mode::List,
                    KeyCode::Tab if !hosts.is_empty() => {
                        let next = hosts
                            .iter()
                            .position(|h| h == input.value())
                            .map_or(0, |i| (i + 1) % hosts.len());
                        *input = Input::new(hosts[next].clone());
                    }
                    KeyCode::Enter if !input.value().trim().is_empty() => {
                        return Effect::Exit(Some(Choice::Connect {
                            host: input.value().trim().into(),
                        }));
                    }
                    _ => {
                        input.handle_event(&event);
                    }
                }
                Effect::None
            }
            Mode::Mkdir {
                host,
                directory,
                input,
            } => match key.code {
                KeyCode::Esc => {
                    self.mode = Mode::List;
                    Effect::None
                }
                KeyCode::Enter => Effect::Folder {
                    host: host.clone(),
                    path: directory.path.clone(),
                    name: Some(input.value().into()),
                },
                _ => {
                    input.handle_event(&event);
                    Effect::None
                }
            },
            Mode::Folders {
                host,
                directory,
                filter,
                list,
            } => {
                let names = folder_names(directory, filter);
                let i = list.selected().unwrap_or(0);
                match key.code {
                    KeyCode::Esc => {
                        self.mode = Mode::List;
                        Effect::None
                    }
                    KeyCode::Char(' ') => Effect::Exit(Some(Choice::New {
                        host: host.clone(),
                        path: directory.path.clone(),
                    })),
                    KeyCode::Char('n') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        self.mode = Mode::Mkdir {
                            host: host.clone(),
                            directory: directory.clone(),
                            input: Input::default(),
                        };
                        Effect::None
                    }
                    KeyCode::Left => {
                        directory
                            .parent
                            .as_ref()
                            .map_or(Effect::None, |path| Effect::Folder {
                                host: host.clone(),
                                path: path.clone(),
                                name: None,
                            })
                    }
                    KeyCode::Enter | KeyCode::Right => {
                        names.get(i).map_or(Effect::None, |name| Effect::Folder {
                            host: host.clone(),
                            path: Path::new(&directory.path)
                                .join(name)
                                .to_string_lossy()
                                .into(),
                            name: None,
                        })
                    }
                    KeyCode::Up if !names.is_empty() => {
                        list.select(Some((i + names.len() - 1) % names.len()));
                        Effect::None
                    }
                    KeyCode::Down if !names.is_empty() => {
                        list.select(Some((i + 1) % names.len()));
                        Effect::None
                    }
                    _ => {
                        filter.handle_event(&event);
                        list.select(Some(0));
                        Effect::None
                    }
                }
            }
            Mode::List => {
                let count = self.visible().len();
                let i = self.list.selected().unwrap_or(0);
                match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => Effect::Exit(None),
                    KeyCode::Char('/') => {
                        self.mode = Mode::Search;
                        Effect::None
                    }
                    KeyCode::Char('h') => {
                        self.mode = Mode::Host(Input::new(
                            self.selected()
                                .map(|r| r.host.clone())
                                .or_else(|| host_filter.map(str::to_owned))
                                .unwrap_or_default(),
                        ));
                        Effect::None
                    }
                    KeyCode::Up | KeyCode::Char('k') if count > 0 => {
                        self.list.select(Some((i + count - 1) % count));
                        Effect::None
                    }
                    KeyCode::Down | KeyCode::Char('j') if count > 0 => {
                        self.list.select(Some((i + 1) % count));
                        Effect::None
                    }
                    KeyCode::Char('r') => self.refresh(
                        host_filter
                            .map(|h| vec![h.into()])
                            .unwrap_or_else(|| hosts.to_vec()),
                    ),
                    KeyCode::Enter => self.selected().map_or(Effect::None, |r| {
                        Effect::Exit(Some(Choice::Resume {
                            host: r.host.clone(),
                            conversation: r.conversation.clone(),
                        }))
                    }),
                    KeyCode::Char('t') => self.selected().map_or(Effect::None, |r| {
                        Effect::Exit(Some(Choice::Shell {
                            host: r.host.clone(),
                            path: r.conversation.cwd.clone(),
                        }))
                    }),
                    KeyCode::Char('n') => {
                        let target = self
                            .selected()
                            .map(|r| (r.host.clone(), r.conversation.cwd.clone()))
                            .or_else(|| host_filter.map(|h| (h.into(), "~".into())))
                            .or_else(|| hosts.first().map(|h| (h.clone(), "~".into())));
                        match target {
                            Some((host, path)) => Effect::Folder {
                                host,
                                path,
                                name: None,
                            },
                            None => {
                                self.mode = Mode::Host(Input::default());
                                Effect::None
                            }
                        }
                    }
                    _ => Effect::None,
                }
            }
        };
        if matches!(effect, Effect::Folder { .. }) {
            self.mode = Mode::Loading;
        }
        effect
    }
}

pub(super) fn folder_names<'a>(directory: &'a Directory, filter: &Input) -> Vec<&'a str> {
    let query = filter.value().to_lowercase();
    directory
        .folders
        .iter()
        .filter(|n| n.to_lowercase().contains(&query))
        .map(String::as_str)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyEvent;

    fn key(app: &mut App, code: KeyCode) -> Effect {
        app.event(
            Event::Key(KeyEvent::new(code, KeyModifiers::NONE)),
            &[],
            None,
        )
    }

    fn row(host: &str, id: &str) -> Saved {
        Saved {
            host: host.into(),
            visited: 1,
            conversation: Conversation {
                id: id.into(),
                cwd: "/projects/api".into(),
                ..Default::default()
            },
        }
    }

    #[test]
    fn refresh_preserves_selection_by_host_and_id_not_index() {
        let selected = row("devbox", "same-id");
        let mut app = App::new(vec![selected.clone()]);
        app.replace(vec![row("devbox", "other-id"), selected.clone()]);
        assert_eq!(app.list.selected(), Some(1));
        app.replace(vec![row("another-host", "same-id"), selected]);
        assert!(
            matches!(key(&mut app, KeyCode::Enter), Effect::Exit(Some(Choice::Resume { host, conversation }))
            if host == "devbox" && conversation.id == "same-id")
        );
    }

    #[test]
    fn folder_filter_navigation_and_launch_use_current_directory() {
        let mut app = App::new(vec![]);
        let directory = Directory {
            path: "/projects".into(),
            parent: Some("/".into()),
            folders: vec!["api".into(), "website".into()],
        };
        app.folders(Ok(("host".into(), directory.clone())));
        app.event(Event::Paste("WEB\n".into()), &[], None);
        assert!(
            matches!(key(&mut app, KeyCode::Enter), Effect::Folder { path, name: None, .. } if path == "/projects/website")
        );
        assert!(matches!(app.mode, Mode::Loading));
        app.folders(Ok(("host".into(), directory)));
        // Space chooses the current folder, not its highlighted child.
        assert!(
            matches!(key(&mut app, KeyCode::Char(' ')), Effect::Exit(Some(Choice::New { path, .. })) if path == "/projects")
        );
        assert!(matches!(key(&mut app, KeyCode::Left), Effect::Folder { path, .. } if path == "/"));
    }

    #[test]
    fn loading_can_cancel_without_entering_or_losing_selected_conversation() {
        let mut app = App::new(vec![row("host", "id")]);
        assert!(
            matches!(key(&mut app, KeyCode::Char('n')), Effect::Folder { path, .. } if path == "/projects/api")
        );
        assert!(matches!(key(&mut app, KeyCode::Enter), Effect::None));
        assert!(matches!(key(&mut app, KeyCode::Esc), Effect::CancelFolder));
        assert!(matches!(app.mode, Mode::List));
        assert_eq!(app.selected().unwrap().conversation.id, "id");
    }

    #[test]
    fn retry_deduplicates_running_hosts_and_respects_host_filter() {
        let mut app = App::new(vec![]);
        assert!(
            matches!(app.refresh(vec!["busy".into()]), Effect::Refresh(hosts) if hosts == ["busy"])
        );
        let hosts = vec!["busy".into(), "offline".into()];
        let retry = Event::Key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE));
        assert!(
            matches!(app.event(retry.clone(), &hosts, Some("busy")), Effect::Refresh(hosts) if hosts.is_empty())
        );
        assert!(
            matches!(app.event(retry, &hosts, None), Effect::Refresh(hosts) if hosts == ["offline"])
        );
    }

    #[test]
    fn search_and_host_entry_do_not_trigger_list_shortcuts() {
        let mut app = App::new(vec![row("host", "id")]);
        key(&mut app, KeyCode::Char('/'));
        key(&mut app, KeyCode::Char('q'));
        assert_eq!(app.search.value(), "q");
        assert!(matches!(app.mode, Mode::Search));
        key(&mut app, KeyCode::Esc);
        assert_eq!(app.search.value(), "");
        key(&mut app, KeyCode::Char('h'));
        let tab = Event::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        app.event(tab, &["host".into(), "other".into()], None);
        assert!(
            matches!(key(&mut app, KeyCode::Enter), Effect::Exit(Some(Choice::Connect { host })) if host == "other")
        );
    }
}
