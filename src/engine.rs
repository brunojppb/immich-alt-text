//! Discovery, worker pool, and retries. Talks to the UI only through `Event`s.

use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{mpsc, watch, Mutex};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::config::Config;
use crate::events::{Command, Event, Stage};
use crate::immich::{Asset, ImmichClient, ImmichError};
use crate::llm::{LlmClient, LlmError};

/// Knobs that tests change. Production uses `Default`.
#[derive(Debug, Clone)]
pub struct EngineOptions {
    /// Sleep before the first retry. Doubles on each further retry.
    pub backoff_base: Duration,
}

impl Default for EngineOptions {
    fn default() -> Self {
        Self {
            backoff_base: Duration::from_secs(2),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error(transparent)]
    Immich(#[from] ImmichError),
    #[error(transparent)]
    Llm(#[from] LlmError),
}

/// Handle to a running engine task.
pub struct EngineHandle {
    cmd_tx: mpsc::Sender<Command>,
    cancel: CancellationToken,
    task: JoinHandle<()>,
}

impl EngineHandle {
    /// Sends a command. Dropped silently if the engine is gone.
    pub async fn send(&self, cmd: Command) {
        let _ = self.cmd_tx.send(cmd).await;
    }

    /// Cancels the engine and waits up to `grace` for it to stop.
    pub async fn shutdown(self, grace: Duration) {
        self.cancel.cancel();
        let _ = tokio::time::timeout(grace, self.task).await;
    }
}

/// Starts the engine with production options.
pub fn spawn(config: Config, events: mpsc::Sender<Event>) -> Result<EngineHandle, EngineError> {
    spawn_with(config, events, EngineOptions::default())
}

/// Starts the engine. It waits for `Command::Start` before doing any work.
pub fn spawn_with(
    config: Config,
    events: mpsc::Sender<Event>,
    options: EngineOptions,
) -> Result<EngineHandle, EngineError> {
    let immich = ImmichClient::new(
        &config.immich.url,
        &config.immich.api_key,
        Duration::from_secs(config.immich.timeout_secs),
    )?;
    let llm = LlmClient::new(
        &config.llm.base_url,
        &config.llm.api_key,
        &config.llm.model,
        config.llm.max_tokens,
        Duration::from_secs(config.llm.timeout_secs),
    )?;
    let (cmd_tx, cmd_rx) = mpsc::channel(16);
    let cancel = CancellationToken::new();
    let engine = Arc::new(Engine {
        immich,
        llm,
        config,
        options,
        events,
        cancel: cancel.clone(),
    });
    let task = tokio::spawn(engine.control_loop(cmd_rx));
    Ok(EngineHandle {
        cmd_tx,
        cancel,
        task,
    })
}

struct Engine {
    immich: ImmichClient,
    llm: LlmClient,
    config: Config,
    options: EngineOptions,
    events: mpsc::Sender<Event>,
    cancel: CancellationToken,
}

/// One run: discovery plus workers. Dropped when the next run starts.
struct Run {
    pause_tx: watch::Sender<bool>,
    token: CancellationToken,
    /// Cleared just before `RunFinished` so a new `Start` is accepted at once.
    active: Arc<AtomicBool>,
    task: JoinHandle<()>,
}

enum Outcome {
    Done,
    Failed,
    Cancelled,
}

#[derive(Debug, thiserror::Error)]
enum StageError {
    #[error("{0}")]
    Transient(String),
    #[error("{0}")]
    Permanent(String),
    #[error("{0}")]
    Fatal(String),
}

impl From<ImmichError> for StageError {
    fn from(error: ImmichError) -> Self {
        let message = error.to_string();
        match error {
            ImmichError::Transient(_) => Self::Transient(message),
            ImmichError::Permanent(_) => Self::Permanent(message),
            ImmichError::Fatal(_) => Self::Fatal(message),
        }
    }
}

impl From<LlmError> for StageError {
    fn from(error: LlmError) -> Self {
        let message = error.to_string();
        match error {
            LlmError::Transient(_) => Self::Transient(message),
            LlmError::Permanent(_) => Self::Permanent(message),
            LlmError::Fatal(_) => Self::Fatal(message),
        }
    }
}

impl Engine {
    async fn control_loop(self: Arc<Self>, mut cmd_rx: mpsc::Receiver<Command>) {
        let mut run: Option<Run> = None;

        loop {
            tokio::select! {
                _ = self.cancel.cancelled() => break,
                cmd = cmd_rx.recv() => {
                    let Some(cmd) = cmd else { break };
                    match cmd {
                        Command::Start => {
                            let busy = run.as_ref().is_some_and(|run| {
                                run.active.load(Ordering::Acquire) && !run.token.is_cancelled()
                            });
                            if !busy {
                                if let Some(run) = run.take() {
                                    let _ = run.task.await;
                                }
                                run = Some(self.clone().start_run());
                            }
                        }
                        Command::Pause => {
                            if let Some(run) = &run {
                                let _ = run.pause_tx.send(true);
                            }
                        }
                        Command::Resume => {
                            if let Some(run) = &run {
                                let _ = run.pause_tx.send(false);
                            }
                        }
                        Command::Quit => {
                            self.cancel.cancel();
                            break;
                        }
                    }
                }
            }
        }

        if let Some(run) = run {
            let _ = run.task.await;
        }
    }

    fn start_run(self: Arc<Self>) -> Run {
        let token = self.cancel.child_token();
        let (pause_tx, pause_rx) = watch::channel(false);
        let active = Arc::new(AtomicBool::new(true));
        let task = tokio::spawn(self.run(token.clone(), pause_rx, active.clone()));
        Run {
            pause_tx,
            token,
            active,
            task,
        }
    }

    async fn run(
        self: Arc<Self>,
        token: CancellationToken,
        pause_rx: watch::Receiver<bool>,
        active: Arc<AtomicBool>,
    ) {
        let started = Instant::now();
        let workers = self.config.run.workers.max(1);
        let (asset_tx, asset_rx) = mpsc::channel::<Asset>(workers * 4);
        let asset_rx = Arc::new(Mutex::new(asset_rx));
        let done = Arc::new(AtomicU64::new(0));
        let failed = Arc::new(AtomicU64::new(0));

        let mut handles = Vec::with_capacity(workers);
        for _ in 0..workers {
            handles.push(tokio::spawn(self.clone().worker(
                token.clone(),
                pause_rx.clone(),
                asset_rx.clone(),
                done.clone(),
                failed.clone(),
            )));
        }

        self.clone().discover(token.clone(), asset_tx).await;

        for handle in handles {
            let _ = handle.await;
        }

        active.store(false, Ordering::Release);
        if !token.is_cancelled() {
            self.emit(Event::RunFinished {
                done: done.load(Ordering::Relaxed),
                failed: failed.load(Ordering::Relaxed),
                elapsed: started.elapsed(),
            })
            .await;
        }
    }

    /// Pages through Immich and queues assets that need a description.
    /// Dropping `asset_tx` at the end tells the workers to stop.
    async fn discover(self: Arc<Self>, token: CancellationToken, asset_tx: mpsc::Sender<Asset>) {
        let mut page = 1u32;
        let mut scanned = 0u64;
        let mut queued = 0u64;

        loop {
            let result = tokio::select! {
                _ = token.cancelled() => return,
                result = self.retry(|| self.immich.list_images(page, self.config.run.page_size)) => result,
            };
            let page_data = match result {
                Ok(page_data) => page_data,
                Err(error) => {
                    self.fail_run(&token, error.to_string()).await;
                    return;
                }
            };

            scanned += page_data.items.len() as u64;
            let wanted: Vec<Asset> = page_data
                .items
                .into_iter()
                .filter(|asset| asset.needs_description())
                .collect();
            queued += wanted.len() as u64;
            self.emit(Event::PageLoaded { scanned, queued }).await;

            for asset in wanted {
                tokio::select! {
                    _ = token.cancelled() => return,
                    result = asset_tx.send(asset) => {
                        if result.is_err() {
                            return;
                        }
                    }
                }
            }

            match page_data.next_page {
                Some(next_page) if next_page > page => page = next_page,
                _ => break,
            }
        }

        self.emit(Event::DiscoveryDone {
            total_queued: queued,
        })
        .await;
    }

    async fn worker(
        self: Arc<Self>,
        token: CancellationToken,
        mut pause_rx: watch::Receiver<bool>,
        asset_rx: Arc<Mutex<mpsc::Receiver<Asset>>>,
        done: Arc<AtomicU64>,
        failed: Arc<AtomicU64>,
    ) {
        loop {
            while *pause_rx.borrow() {
                tokio::select! {
                    _ = token.cancelled() => return,
                    result = pause_rx.changed() => {
                        if result.is_err() {
                            return;
                        }
                    }
                }
            }

            let asset = {
                let mut rx = asset_rx.lock().await;
                tokio::select! {
                    _ = token.cancelled() => return,
                    asset = rx.recv() => match asset {
                        Some(asset) => asset,
                        None => return,
                    },
                }
            };

            match self.process(&token, &asset).await {
                Outcome::Done => {
                    done.fetch_add(1, Ordering::Relaxed);
                }
                Outcome::Failed => {
                    failed.fetch_add(1, Ordering::Relaxed);
                }
                Outcome::Cancelled => return,
            }
        }
    }

    async fn process(&self, token: &CancellationToken, asset: &Asset) -> Outcome {
        let started = Instant::now();
        let id = asset.id.clone();
        let name = asset.name.clone();
        self.emit(Event::AssetStarted {
            id: id.clone(),
            name: name.clone(),
        })
        .await;

        self.stage(&id, Stage::Fetching).await;
        let jpeg = tokio::select! {
            _ = token.cancelled() => return Outcome::Cancelled,
            result = self.retry(|| self.immich.preview_jpeg(&id)) => result,
        };
        let jpeg = match jpeg {
            Ok(jpeg) => jpeg,
            Err(error) => return self.fail_asset(token, id, name, error).await,
        };

        self.stage(&id, Stage::CallingLlm).await;
        let llm_started = Instant::now();
        let text = tokio::select! {
            _ = token.cancelled() => return Outcome::Cancelled,
            result = self.retry(|| self.llm.describe(&jpeg, &self.config.llm.prompt)) => result,
        };
        let text = match text {
            Ok(text) => text,
            Err(error) => return self.fail_asset(token, id, name, error).await,
        };
        let llm_took = llm_started.elapsed();

        self.stage(&id, Stage::Writing).await;
        if let Err(error) = self.retry(|| self.immich.set_description(&id, &text)).await {
            return self.fail_asset(token, id, name, error).await;
        }

        self.emit(Event::AssetDone {
            id,
            name,
            description: text,
            took: started.elapsed(),
            llm_took,
        })
        .await;
        Outcome::Done
    }

    async fn retry<T, E, F, Fut>(&self, mut op: F) -> Result<T, StageError>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = Result<T, E>>,
        E: Into<StageError>,
    {
        let attempts = self.config.run.retries + 1;
        let mut attempt = 1u32;

        loop {
            match op().await.map_err(Into::into) {
                Ok(value) => return Ok(value),
                Err(StageError::Transient(message)) if attempt < attempts => {
                    let delay = self.options.backoff_base * 2u32.pow(attempt - 1);
                    tracing::warn!(
                        attempt,
                        delay_ms = delay.as_millis() as u64,
                        %message,
                        "retrying"
                    );
                    tokio::time::sleep(delay).await;
                    attempt += 1;
                }
                Err(StageError::Transient(message)) => {
                    return Err(StageError::Transient(format!(
                        "{message} ({attempts} tries)"
                    )));
                }
                Err(error) => return Err(error),
            }
        }
    }

    async fn fail_asset(
        &self,
        token: &CancellationToken,
        id: String,
        name: String,
        error: StageError,
    ) -> Outcome {
        match error {
            StageError::Fatal(message) => {
                self.fail_run(token, message).await;
                Outcome::Cancelled
            }
            other => {
                tracing::warn!(%id, %name, error = %other, "asset failed");
                self.emit(Event::AssetFailed {
                    id,
                    name,
                    error: other.to_string(),
                })
                .await;
                Outcome::Failed
            }
        }
    }

    async fn fail_run(&self, token: &CancellationToken, message: String) {
        if token.is_cancelled() {
            return;
        }

        tracing::error!(error = %message, "run stopped");
        self.emit(Event::Fatal { error: message }).await;
        token.cancel();
    }

    async fn stage(&self, id: &str, stage: Stage) {
        self.emit(Event::AssetStage {
            id: id.to_string(),
            stage,
        })
        .await;
    }

    async fn emit(&self, event: Event) {
        let _ = self.events.send(event).await;
    }
}
