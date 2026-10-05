//! Terminal lifecycle and effect execution. The model never performs I/O;
//! background workers never mutate picker state or draw to the terminal.
mod model;
mod view;
use crate::{backend, history::History, remote::Directory, sessions::Snapshot};
use anyhow::Result;
use crossterm::event::{self, DisableBracketedPaste, EnableBracketedPaste};
pub use model::Choice;
use model::{App, Effect, HostState};
use std::{sync::Arc, time::Duration};
use tokio::{
    runtime::Runtime,
    sync::{Semaphore, mpsc},
    task::JoinHandle,
};

struct TerminalGuard;
impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = crossterm::execute!(std::io::stdout(), DisableBracketedPaste);
        ratatui::restore();
    }
}

/// Owns every background job for one picker invocation. Dropping the picker
/// cancels the jobs, whose subprocesses are kill-on-drop.
struct Jobs {
    runtime: Runtime,
    limit: Arc<Semaphore>,
    tx: mpsc::UnboundedSender<(String, Result<Snapshot>)>,
    rx: mpsc::UnboundedReceiver<(String, Result<Snapshot>)>,
    folder: Option<JoinHandle<Result<(String, Directory)>>>,
}

impl Jobs {
    fn new() -> Result<Self> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()?;
        let (tx, rx) = mpsc::unbounded_channel();
        Ok(Self {
            runtime,
            limit: Arc::new(Semaphore::new(4)),
            tx,
            rx,
            folder: None,
        })
    }

    fn refresh(&self, hosts: Vec<String>) {
        for host in hosts {
            let tx = self.tx.clone();
            let sem = self.limit.clone();
            self.runtime.spawn(async move {
                let _permit = sem.acquire_owned().await;
                let result = backend::refresh(host.clone()).await;
                let _ = tx.send((host, result));
            });
        }
    }

    fn cancel_folder(&mut self) {
        if let Some(job) = self.folder.take() {
            job.abort();
        }
    }

    fn folder(&mut self, host: String, path: String, name: Option<String>) {
        self.cancel_folder();
        self.folder = Some(self.runtime.spawn(async move {
            let directory = match name {
                Some(name) => backend::mkdir(host.clone(), path, name).await?,
                None => backend::browse(host.clone(), path).await?,
            };
            Ok((host, directory))
        }));
    }

    fn drain(
        &mut self,
        app: &mut App,
        cache: &mut History,
        host_filter: Option<&str>,
    ) -> Result<()> {
        if self.folder.as_ref().is_some_and(|job| job.is_finished()) {
            app.folders(self.runtime.block_on(self.folder.take().unwrap())?);
        }
        while let Ok((host, result)) = self.rx.try_recv() {
            match result {
                Ok(snapshot) => {
                    if !snapshot.warnings.is_empty() {
                        app.message = Some(snapshot.warnings.join("; "));
                    }
                    app.states.insert(
                        host.clone(),
                        if snapshot.server_running {
                            HostState::Reachable
                        } else {
                            HostState::Stopped
                        },
                    );
                    if let Err(e) = cache.merge(&host, &snapshot) {
                        app.message = Some(e.to_string());
                    }
                    app.replace(rows(cache, host_filter));
                }
                Err(e) => {
                    app.states.insert(host, HostState::Offline(e.to_string()));
                }
            }
        }
        Ok(())
    }
}

fn rows(cache: &History, host_filter: Option<&str>) -> Vec<crate::history::Saved> {
    cache
        .sessions()
        .iter()
        .filter(|r| host_filter.is_none_or(|h| h == r.host))
        .cloned()
        .collect()
}

pub fn pick(cache: &mut History, host_filter: Option<&str>) -> Result<Option<Choice>> {
    let hosts = host_filter
        .map(|h| vec![h.to_owned()])
        .unwrap_or_else(|| cache.hosts().to_vec());
    let mut app = App::new(rows(cache, host_filter));
    let initial = app.refresh(hosts);
    let mut terminal = ratatui::try_init()?;
    let _guard = TerminalGuard;
    // Paint cached content before constructing the runtime or starting SSH.
    terminal.draw(|f| view::draw(&mut app, f))?;
    crossterm::execute!(std::io::stdout(), EnableBracketedPaste)?;
    let mut jobs = Jobs::new()?;
    if let Effect::Refresh(hosts) = initial {
        jobs.refresh(hosts);
    }
    loop {
        jobs.drain(&mut app, cache, host_filter)?;
        terminal.draw(|f| view::draw(&mut app, f))?;
        if !event::poll(Duration::from_millis(100))? {
            continue;
        }
        match app.event(event::read()?, cache.hosts(), host_filter) {
            Effect::None => {}
            Effect::Exit(choice) => {
                if let Some(Choice::Connect { host }) = &choice {
                    cache.add_host(host)?;
                }
                return Ok(choice);
            }
            Effect::Refresh(hosts) => jobs.refresh(hosts),
            Effect::Folder { host, path, name } => jobs.folder(host, path, name),
            Effect::CancelFolder => jobs.cancel_folder(),
        }
    }
}
