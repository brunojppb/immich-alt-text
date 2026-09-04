//! Discovery, worker pool, and retries. Talks to the UI only through `Event`s.

use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{mpsc, oneshot, watch, Mutex};
use tokio::task::{JoinHandle, JoinSet};
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
    Config(#[from] crate::config::ConfigError),
    #[error(transparent)]
    Immich(#[from] ImmichError),
    #[error(transparent)]
    Llm(#[from] LlmError),
}

/// Handle to a running engine task.
pub struct EngineHandle {
    cmd_tx: mpsc::Sender<ControlMessage>,
    cancel: CancellationToken,
    force: CancellationToken,
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

    /// Cancels the engine, escalating after `grace`, and waits until every task stops.
    pub async fn shutdown(mut self, grace: Duration) {
        self.cancel.cancel();
        if tokio::time::timeout(grace, &mut self.task).await.is_err() {
            self.force.cancel();
            let _ = self.task.await;
        }
    }

    /// Stops before replacement without aborting an in-flight Immich write.
    pub async fn shutdown_for_replacement(self) {
        self.cancel.cancel();
        let _ = self.task.await;
    }
}

/// A fully validated engine that owns no running tasks until `start` is called.
pub struct PreparedEngine {
    engine: Arc<Engine>,
}

impl PreparedEngine {
    /// Starts the control task. It still waits for `Command::Start` before processing assets.
    pub fn start(self) -> EngineHandle {
        let (cmd_tx, cmd_rx) = mpsc::channel(16);
        let cancel = self.engine.cancel.clone();
        let force = self.engine.force.clone();
        let task = tokio::spawn(self.engine.control_loop(cmd_rx));
        EngineHandle {
            cmd_tx,
            cancel,
            force,
            task,
        }
    }
}

/// Starts the engine with production options.
pub fn spawn(config: Config, events: mpsc::Sender<Event>) -> Result<EngineHandle, EngineError> {
    Ok(prepare(config, events)?.start())
}

/// Validates and constructs an inert engine with production options.
pub fn prepare(config: Config, events: mpsc::Sender<Event>) -> Result<PreparedEngine, EngineError> {
    prepare_with(config, events, EngineOptions::default())
}

/// Starts the engine. It waits for `Command::Start` before doing any work.
pub fn spawn_with(
    config: Config,
    events: mpsc::Sender<Event>,
    options: EngineOptions,
) -> Result<EngineHandle, EngineError> {
    Ok(prepare_with(config, events, options)?.start())
}

/// Validates and constructs an inert engine with caller-supplied options.
pub fn prepare_with(
    config: Config,
    events: mpsc::Sender<Event>,
    options: EngineOptions,
) -> Result<PreparedEngine, EngineError> {
    config.validate()?;
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
    let cancel = CancellationToken::new();
    let force = CancellationToken::new();
    let handoff = Arc::new(Mutex::new(()));
    let engine = Arc::new(Engine {
        immich,
        llm,
        config,
        options,
        events,
        cancel,
        force,
        handoff: handoff.clone(),
    });
    Ok(PreparedEngine { engine })
}

struct Engine {
    immich: ImmichClient,
    llm: LlmClient,
    config: Config,
    options: EngineOptions,
    events: mpsc::Sender<Event>,
    cancel: CancellationToken,
    force: CancellationToken,
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
                            let active = run
                                .as_ref()
                                .is_some_and(|run| run.active.load(Ordering::Acquire));
                            let restartable = !active
                                || run
                                    .as_ref()
                                    .is_some_and(|run| run.token.is_cancelled());
                            if restartable {
                                if let Some(run) = run.take() {
                                    run.token.cancel();
                                    run.terminal_cancel.cancel();
                                    if active {
                                        let _ = run.task.await;
                                    } else {
                                        retired_runs.push(run.task);
                                    }
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
        let (asset_tx, asset_rx) = mpsc::channel::<Asset>(workers.saturating_mul(4));
        let asset_rx = Arc::new(Mutex::new(asset_rx));
        let done = Arc::new(AtomicU64::new(0));
        let failed = Arc::new(AtomicU64::new(0));

        let mut handles = JoinSet::new();
        for _ in 0..workers {
            handles.spawn(self.clone().worker(
                token.clone(),
                terminal_cancel.clone(),
                pause_rx.clone(),
                asset_rx.clone(),
                done.clone(),
                failed.clone(),
            ));
        }

        let discovery = self
            .clone()
            .discover(token.clone(), terminal_cancel, asset_tx);
        tokio::pin!(discovery);
        tokio::select! {
            biased;
            _ = self.force.cancelled() => {
                handles.abort_all();
                while handles.join_next().await.is_some() {}
                active.store(false, Ordering::Release);
                return;
            }
            _ = &mut discovery => {}
        }

        while !handles.is_empty() {
            tokio::select! {
                biased;
                _ = self.force.cancelled() => {
                    handles.abort_all();
                    while handles.join_next().await.is_some() {}
                    break;
                }
                _ = handles.join_next() => {}
            }
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

            scanned = scanned.saturating_add(page_data.items.len() as u64);
            let wanted: Vec<Asset> = page_data
                .items
                .into_iter()
                .filter(|asset| asset.needs_description())
                .collect();
            queued = queued.saturating_add(wanted.len() as u64);
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

            let event_permit = tokio::select! {
                biased;
                _ = token.cancelled() => return,
                permit = self.events.reserve() => match permit {
                    Ok(permit) => permit,
                    Err(_) => return,
                },
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
                event_permit.send(Event::AssetStarted {
                    id: asset.id.clone(),
                    name: asset.name.clone(),
                });
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
        let attempts = self.config.run.retries.saturating_add(1);
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
                    let multiplier = 2u32.saturating_pow(attempt.saturating_sub(1));
                    let delay = self.options.backoff_base.saturating_mul(multiplier);
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
                    attempt = attempt.saturating_add(1);
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

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicU64;
    use std::sync::Arc;
    use std::time::Duration;

    use tokio::sync::{mpsc, watch, Barrier, Mutex};
    use tokio_util::sync::CancellationToken;

    use super::prepare;
    use crate::config::{Config, ImmichConfig, LlmConfig, RunConfig, UiConfig};
    use crate::events::Event;
    use crate::immich::Asset;

    fn config() -> Config {
        Config {
            immich: ImmichConfig {
                url: "http://127.0.0.1:3001".into(),
                api_key: "key".into(),
                timeout_secs: 5,
            },
            llm: LlmConfig {
                base_url: "http://127.0.0.1:3002/v1".into(),
                api_key: String::new(),
                model: "vision".into(),
                max_tokens: 100,
                timeout_secs: 5,
                prompt: "describe".into(),
            },
            run: RunConfig {
                workers: 1,
                retries: 0,
                page_size: 10,
            },
            ui: UiConfig::default(),
        }
    }

    #[tokio::test]
    async fn cancellation_drops_a_non_terminal_event_blocked_on_a_full_channel() {
        let (events, mut rx) = mpsc::channel(1);
        events
            .send(Event::PageLoaded {
                scanned: 1,
                queued: 1,
            })
            .await
            .expect("event receiver is open");
        let engine = prepare(config(), events).expect("valid engine").engine;
        let token = CancellationToken::new();
        let gate = Arc::new(Barrier::new(2));
        let blocked_send = tokio::spawn({
            let engine = engine.clone();
            let token = token.clone();
            let gate = gate.clone();
            async move {
                gate.wait().await;
                engine
                    .emit_run(&token, Event::DiscoveryDone { total_queued: 1 })
                    .await
            }
        });

        gate.wait().await;
        token.cancel();
        assert_eq!(
            rx.recv().await,
            Some(Event::PageLoaded {
                scanned: 1,
                queued: 1
            })
        );
        assert!(!blocked_send.await.expect("send task did not panic"));
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn pause_handoff_is_acknowledged_when_capacity_one_events_are_saturated() {
        let (events, _rx) = mpsc::channel(1);
        events
            .send(Event::DiscoveryDone { total_queued: 0 })
            .await
            .expect("event receiver is open");
        let engine = prepare(config(), events).expect("valid engine").engine;
        let token = CancellationToken::new();
        let terminal_cancel = CancellationToken::new();
        let (pause_tx, pause_rx) = watch::channel(false);
        let (asset_tx, asset_rx) = mpsc::channel(1);
        asset_tx
            .send(Asset {
                id: "a1".into(),
                name: "IMG_1.HEIC".into(),
                description: None,
            })
            .await
            .expect("asset receiver is open");

        let worker = tokio::spawn(engine.clone().worker(
            token.clone(),
            terminal_cancel,
            pause_rx,
            Arc::new(Mutex::new(asset_rx)),
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicU64::new(0)),
        ));
        while asset_tx.capacity() == 0 {
            tokio::task::yield_now().await;
        }
        tokio::task::yield_now().await;

        pause_tx
            .send(true)
            .expect("worker still receives pause state");
        tokio::time::timeout(Duration::from_millis(100), async {
            let handoff = engine.handoff.lock().await;
            drop(handoff);
        })
        .await
        .expect("Pause acknowledgement blocked behind a full event channel");

        token.cancel();
        worker.await.expect("worker did not panic");
    }
}
