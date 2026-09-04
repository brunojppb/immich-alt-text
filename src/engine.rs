//! Discovery, worker pool, and retries. Talks to the UI only through `Event`s.

use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{mpsc, oneshot, watch, Mutex};
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
    cmd_tx: mpsc::Sender<ControlMessage>,
    cancel: CancellationToken,
    task: JoinHandle<()>,
}

impl EngineHandle {
    /// Sends a command. Dropped silently if the engine is gone.
    pub async fn send(&self, cmd: Command) {
        let (ack_tx, ack_rx) = oneshot::channel();
        if self
            .cmd_tx
            .send(ControlMessage { cmd, ack: ack_tx })
            .await
            .is_ok()
        {
            let _ = ack_rx.await;
        }
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
    let handoff = Arc::new(Mutex::new(()));
    let engine = Arc::new(Engine {
        immich,
        llm,
        config,
        options,
        events,
        cancel: cancel.clone(),
        handoff: handoff.clone(),
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
    handoff: Arc<Mutex<()>>,
}

/// One run: discovery plus workers. Dropped when the next run starts.
struct Run {
    token: CancellationToken,
    terminal_cancel: CancellationToken,
    /// Cleared just before `RunFinished` so a new `Start` is accepted at once.
    active: Arc<AtomicBool>,
    pause_tx: watch::Sender<bool>,
    task: JoinHandle<()>,
}

struct ControlMessage {
    cmd: Command,
    ack: oneshot::Sender<()>,
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
    #[error("cancelled")]
    Cancelled,
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
    async fn control_loop(self: Arc<Self>, mut cmd_rx: mpsc::Receiver<ControlMessage>) {
        let mut run: Option<Run> = None;
        let mut retired_runs = Vec::new();

        loop {
            tokio::select! {
                _ = self.cancel.cancelled() => break,
                cmd = cmd_rx.recv() => {
                    let Some(ControlMessage { cmd, ack }) = cmd else { break };
                    match cmd {
                        Command::Start => {
                            let busy = run.as_ref().is_some_and(|run| {
                                run.active.load(Ordering::Acquire) && !run.token.is_cancelled()
                            });
                            if !busy {
                                if let Some(run) = run.take() {
                                    run.token.cancel();
                                    run.terminal_cancel.cancel();
                                    retired_runs.push(run.task);
                                }
                                run = Some(self.clone().start_run());
                            }
                            let _ = ack.send(());
                            Self::reap_finished(&mut retired_runs).await;
                        }
                        Command::Pause => {
                            if let Some(run) = run.as_ref() {
                                let _ = run.pause_tx.send(true);
                                let handoff_guard = self.handoff.lock().await;
                                drop(handoff_guard);
                            }
                            let _ = ack.send(());
                        }
                        Command::Resume => {
                            if let Some(run) = run.as_ref() {
                                let _ = run.pause_tx.send(false);
                            }
                            let _ = ack.send(());
                        }
                        Command::Quit => {
                            self.cancel.cancel();
                            let _ = ack.send(());
                            break;
                        }
                    }
                }
            }
        }

        if let Some(run) = run {
            run.token.cancel();
            run.terminal_cancel.cancel();
            let _ = run.task.await;
        }
        for task in retired_runs {
            let _ = task.await;
        }
    }

    async fn reap_finished(tasks: &mut Vec<JoinHandle<()>>) {
        let mut index = 0;
        while index < tasks.len() {
            if tasks[index].is_finished() {
                let task = tasks.swap_remove(index);
                let _ = task.await;
            } else {
                index += 1;
            }
        }
    }

    fn start_run(self: Arc<Self>) -> Run {
        let token = self.cancel.child_token();
        let terminal_cancel = CancellationToken::new();
        let (pause_tx, pause_rx) = watch::channel(false);
        let active = Arc::new(AtomicBool::new(true));
        let task = tokio::spawn(self.run(
            token.clone(),
            terminal_cancel.clone(),
            pause_rx,
            active.clone(),
        ));
        Run {
            token,
            terminal_cancel,
            active,
            pause_tx,
            task,
        }
    }

    async fn run(
        self: Arc<Self>,
        token: CancellationToken,
        terminal_cancel: CancellationToken,
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
                terminal_cancel.clone(),
                pause_rx.clone(),
                asset_rx.clone(),
                done.clone(),
                failed.clone(),
            )));
        }

        self.clone()
            .discover(token.clone(), terminal_cancel, asset_tx)
            .await;

        for handle in handles {
            let _ = handle.await;
        }

        active.store(false, Ordering::Release);
        if !token.is_cancelled() {
            let _ = self
                .emit_run(
                    &token,
                    Event::RunFinished {
                        done: done.load(Ordering::Relaxed),
                        failed: failed.load(Ordering::Relaxed),
                        elapsed: started.elapsed(),
                    },
                )
                .await;
        }
    }

    /// Pages through Immich and queues assets that need a description.
    /// Dropping `asset_tx` at the end tells the workers to stop.
    async fn discover(
        self: Arc<Self>,
        token: CancellationToken,
        terminal_cancel: CancellationToken,
        asset_tx: mpsc::Sender<Asset>,
    ) {
        let mut page = 1u32;
        let mut scanned = 0u64;
        let mut queued = 0u64;

        loop {
            let result = self
                .retry(&token, true, || {
                    self.immich.list_images(page, self.config.run.page_size)
                })
                .await;
            let page_data = match result {
                Ok(page_data) => page_data,
                Err(error) => {
                    self.fail_run(&token, &terminal_cancel, error.to_string())
                        .await;
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
            let _ = self
                .emit_run(&token, Event::PageLoaded { scanned, queued })
                .await;

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

        let _ = self
            .emit_run(
                &token,
                Event::DiscoveryDone {
                    total_queued: queued,
                },
            )
            .await;
    }

    async fn worker(
        self: Arc<Self>,
        token: CancellationToken,
        terminal_cancel: CancellationToken,
        mut pause_rx: watch::Receiver<bool>,
        asset_rx: Arc<Mutex<mpsc::Receiver<Asset>>>,
        done: Arc<AtomicU64>,
        failed: Arc<AtomicU64>,
    ) {
        let mut pending = None;

        loop {
            if self
                .wait_until_resumed(&token, &mut pause_rx)
                .await
                .is_err()
            {
                return;
            }

            let asset = match pending.take() {
                Some(asset) => asset,
                None => {
                    let mut rx = asset_rx.lock().await;
                    tokio::select! {
                        _ = token.cancelled() => return,
                        asset = rx.recv() => match asset {
                            Some(asset) => asset,
                            None => return,
                        },
                    }
                }
            };

            {
                let handoff_guard = self.handoff.lock().await;
                if token.is_cancelled() {
                    drop(handoff_guard);
                    return;
                }
                if *pause_rx.borrow() {
                    pending = Some(asset);
                    continue;
                }
                if !self
                    .emit_run(
                        &token,
                        Event::AssetStarted {
                            id: asset.id.clone(),
                            name: asset.name.clone(),
                        },
                    )
                    .await
                {
                    drop(handoff_guard);
                    return;
                }
                drop(handoff_guard);
            }

            match self.process(&token, &terminal_cancel, &asset).await {
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

    async fn process(
        &self,
        token: &CancellationToken,
        terminal_cancel: &CancellationToken,
        asset: &Asset,
    ) -> Outcome {
        if token.is_cancelled() {
            return Outcome::Cancelled;
        }

        let started = Instant::now();
        let id = asset.id.clone();
        let name = asset.name.clone();

        if !self.stage(token, &id, Stage::Fetching).await {
            return Outcome::Cancelled;
        }
        let jpeg = self
            .retry(token, true, || self.immich.preview_jpeg(&id))
            .await;
        let jpeg = match jpeg {
            Ok(jpeg) => jpeg,
            Err(error) => {
                return self
                    .fail_asset(token, terminal_cancel, id, name, error)
                    .await
            }
        };

        if !self.stage(token, &id, Stage::CallingLlm).await {
            return Outcome::Cancelled;
        }
        let llm_started = Instant::now();
        let text = self
            .retry(token, true, || {
                self.llm.describe(&jpeg, &self.config.llm.prompt)
            })
            .await;
        let text = match text {
            Ok(text) => text,
            Err(error) => {
                return self
                    .fail_asset(token, terminal_cancel, id, name, error)
                    .await
            }
        };
        let llm_took = llm_started.elapsed();

        if !self.stage(token, &id, Stage::Writing).await {
            return Outcome::Cancelled;
        }
        if let Err(error) = self
            .retry(token, false, || self.immich.set_description(&id, &text))
            .await
        {
            return self
                .fail_asset(token, terminal_cancel, id, name, error)
                .await;
        }

        if token.is_cancelled() {
            return Outcome::Cancelled;
        }

        let _ = self
            .emit_run(
                token,
                Event::AssetDone {
                    id,
                    name,
                    description: text,
                    took: started.elapsed(),
                    llm_took,
                },
            )
            .await;
        Outcome::Done
    }

    async fn retry<T, E, F, Fut>(
        &self,
        token: &CancellationToken,
        cancel_in_flight: bool,
        mut op: F,
    ) -> Result<T, StageError>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = Result<T, E>>,
        E: Into<StageError>,
    {
        let attempts = self.config.run.retries + 1;
        let mut attempt = 1u32;

        loop {
            if token.is_cancelled() {
                return Err(StageError::Cancelled);
            }

            let result = if cancel_in_flight {
                tokio::select! {
                    _ = token.cancelled() => return Err(StageError::Cancelled),
                    result = op() => result.map_err(Into::into),
                }
            } else {
                op().await.map_err(Into::into)
            };

            match result {
                Ok(value) => return Ok(value),
                Err(StageError::Transient(message)) if attempt < attempts => {
                    let delay = self.options.backoff_base * 2u32.pow(attempt - 1);
                    tracing::warn!(
                        attempt,
                        delay_ms = delay.as_millis() as u64,
                        %message,
                        "retrying"
                    );
                    tokio::select! {
                        _ = token.cancelled() => return Err(StageError::Cancelled),
                        _ = tokio::time::sleep(delay) => {}
                    }
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
        terminal_cancel: &CancellationToken,
        id: String,
        name: String,
        error: StageError,
    ) -> Outcome {
        if token.is_cancelled() && !matches!(error, StageError::Fatal(_)) {
            return Outcome::Cancelled;
        }

        match error {
            StageError::Fatal(message) => {
                self.fail_run(token, terminal_cancel, message).await;
                Outcome::Cancelled
            }
            StageError::Cancelled => Outcome::Cancelled,
            other => {
                tracing::warn!(%id, %name, error = %other, "asset failed");
                let _ = self
                    .emit_run(
                        token,
                        Event::AssetFailed {
                            id,
                            name,
                            error: other.to_string(),
                        },
                    )
                    .await;
                Outcome::Failed
            }
        }
    }

    async fn fail_run(
        &self,
        token: &CancellationToken,
        terminal_cancel: &CancellationToken,
        message: String,
    ) {
        if token.is_cancelled() {
            return;
        }

        tracing::error!(error = %message, "run stopped");
        token.cancel();
        let _ = self
            .emit_terminal(terminal_cancel, Event::Fatal { error: message })
            .await;
    }

    async fn stage(&self, token: &CancellationToken, id: &str, stage: Stage) -> bool {
        self.emit_run(
            token,
            Event::AssetStage {
                id: id.to_string(),
                stage,
            },
        )
        .await
    }

    async fn wait_until_resumed(
        &self,
        token: &CancellationToken,
        pause_rx: &mut watch::Receiver<bool>,
    ) -> Result<(), ()> {
        while *pause_rx.borrow() {
            tokio::select! {
                _ = token.cancelled() => return Err(()),
                result = pause_rx.changed() => {
                    if result.is_err() {
                        return Err(());
                    }
                }
            }
        }

        Ok(())
    }

    async fn emit_terminal(&self, cancel: &CancellationToken, event: Event) -> bool {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => false,
            result = self.events.send(event) => result.is_ok(),
        }
    }

    async fn emit_run(&self, token: &CancellationToken, event: Event) -> bool {
        if token.is_cancelled() {
            return false;
        }

        tokio::select! {
            biased;
            _ = token.cancelled() => false,
            result = self.events.send(event) => result.is_ok(),
        }
    }
}
