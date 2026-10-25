//! Independent sync workers; UI saves never wait for an in-flight upload.
use crate::{
    config::Config,
    engine::{Engine, run_loop},
    profiles::{Profiles, separate_resources},
    remote::S3Remote,
    tray::{Control, SyncState},
};
use anyhow::{Context, Result};
use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::JoinHandle,
    time::Duration,
};

struct Worker {
    config: Config,
    enabled: bool,
    control: Arc<Control>,
    finished: Arc<AtomicBool>,
    task: JoinHandle<()>,
}

pub struct Manager {
    runtime: Arc<tokio::runtime::Runtime>,
    workers: HashMap<String, Worker>,
    retired: Vec<Worker>,
    retiring_bindings: Vec<(Config, Arc<AtomicBool>)>,
}

impl Manager {
    pub fn new() -> Result<Self> {
        Ok(Self {
            runtime: Arc::new(
                tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(4)
                    .enable_all()
                    .build()?,
            ),
            workers: HashMap::new(),
            retired: Vec::new(),
            retiring_bindings: Vec::new(),
        })
    }

    pub fn control(&self, id: &str) -> Option<Arc<Control>> {
        self.workers.get(id).map(|w| w.control.clone())
    }

    pub fn validate_resources(&self, profiles: &Profiles) -> Result<()> {
        for connection in profiles.connections.iter().filter(|c| c.enabled) {
            for (config, finished) in &self.retiring_bindings {
                if !finished.load(Ordering::Acquire)
                    && config.device_id != connection.config.device_id
                {
                    separate_resources(&connection.config, config).context("A previous connection is still finishing its current synchronization; wait before reusing its folders or drive letter")?;
                }
            }
            for worker in self.workers.values() {
                if !worker.finished.load(Ordering::Acquire)
                    && worker.config.device_id != connection.config.device_id
                {
                    separate_resources(&connection.config, &worker.config).context(
                        "Another running connection is still using these folders or drive letter",
                    )?;
                }
            }
        }
        Ok(())
    }

    pub fn apply(&mut self, profiles: &Profiles) -> Result<()> {
        self.retiring_bindings
            .retain(|(_, done)| !done.load(Ordering::Acquire));
        let mut pending = Vec::new();
        for worker in self.retired.drain(..) {
            if worker.task.is_finished() {
                let _ = worker.task.join();
            } else {
                pending.push(worker);
            }
        }
        self.retired = pending;
        let removed: Vec<_> = self
            .workers
            .keys()
            .filter(|id| {
                !profiles
                    .connections
                    .iter()
                    .any(|c| &c.config.device_id == *id)
            })
            .cloned()
            .collect();
        for id in removed {
            if let Some(worker) = self.workers.remove(&id) {
                worker.control.stopping.store(true, Ordering::Release);
                self.retiring_bindings
                    .push((worker.config.clone(), worker.finished.clone()));
                self.retired.push(worker);
            }
        }
        for (index, connection) in profiles.connections.iter().enumerate() {
            let id = &connection.config.device_id;
            if self.workers.get(id).is_some_and(|w| {
                w.config == connection.config
                    && w.enabled == connection.enabled
                    && (!w.finished.load(Ordering::Acquire) || !w.enabled)
            }) {
                continue;
            }
            let previous = self.workers.remove(id);
            if let Some(worker) = &previous {
                worker.control.stopping.store(true, Ordering::Release);
                self.retiring_bindings
                    .push((worker.config.clone(), worker.finished.clone()));
            }
            let control = Arc::new(Control::default());
            if let Some(previous) = &previous {
                control.paused.store(
                    previous.control.paused.load(Ordering::Acquire),
                    Ordering::Release,
                );
                control.set_status(
                    SyncState::Syncing,
                    "Applying settings after the current sync finishes…".into(),
                );
            }
            let worker_control = control.clone();
            let finished = Arc::new(AtomicBool::new(false));
            let worker_finished = finished.clone();
            let config = connection.config.clone();
            let enabled = connection.enabled;
            let validation = profiles
                .validate_connection(index)
                .map_err(|e| format!("{e:#}"));
            let runtime = self.runtime.clone();
            let task = std::thread::Builder::new()
                .name(format!("bucket-{}", id))
                .spawn(move || {
                    struct Finished(Arc<AtomicBool>);
                    impl Drop for Finished {
                        fn drop(&mut self) {
                            self.0.store(true, Ordering::Release);
                        }
                    }
                    let _finished = Finished(worker_finished);
                    let span = tracing::info_span!("connection", id = %config.device_id);
                    let _span = span.enter();
                    if let Some(previous) = previous {
                        let _ = previous.task.join();
                    }
                    if worker_control.stopping.load(Ordering::Acquire) {
                        return;
                    }
                    if !enabled {
                        worker_control.set_status(
                            SyncState::Paused,
                            "Disabled — enable in Connections".into(),
                        );
                        return;
                    }
                    let result = (|| -> Result<()> {
                        validation.map_err(anyhow::Error::msg)?;
                        let _enter = runtime.enter();
                        let remote = Arc::new(S3Remote::new(&config)?);
                        loop {
                            if worker_control.stopping.load(Ordering::Acquire) {
                                return Ok(());
                            }
                            worker_control.set_status(SyncState::Syncing, "Connecting…".into());
                            match runtime.block_on(remote.verify_conditionals()) {
                                Ok(()) => break,
                                Err(error) => {
                                    tracing::warn!(error = %format!("{error:#}"), "Connection unavailable");
                                    worker_control.set_status(
                                        SyncState::Error,
                                        format!("Connection unavailable: {error:#}"),
                                    );
                                }
                            }
                            for _ in 0..50 {
                                if worker_control.stopping.load(Ordering::Acquire) {
                                    return Ok(());
                                }
                                std::thread::sleep(Duration::from_millis(200));
                            }
                        }
                        if worker_control.stopping.load(Ordering::Acquire) {
                            return Ok(());
                        }
                        let engine = Engine::start(config, remote, runtime.handle().clone())?;
                        run_loop(engine, &runtime, &worker_control);
                        Ok(())
                    })();
                    if let Err(error) = result {
                        tracing::error!(%error, "Connection stopped");
                        worker_control.set_status(SyncState::Error, format!("Stopped: {error:#}"));
                    }
                })
                .context("Start connection worker")?;
            self.workers.insert(
                id.clone(),
                Worker {
                    config: connection.config.clone(),
                    enabled,
                    control,
                    finished,
                    task,
                },
            );
        }
        Ok(())
    }

    pub fn stop(&self) {
        for worker in self.workers.values().chain(self.retired.iter()) {
            worker.control.stopping.store(true, Ordering::Release);
        }
    }
}

impl Drop for Manager {
    fn drop(&mut self) {
        self.stop();
        for (_, worker) in self.workers.drain() {
            let _ = worker.task.join();
        }
        for worker in self.retired.drain(..) {
            let _ = worker.task.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profiles::Connection;
    use std::sync::mpsc;

    #[test]
    fn replacing_one_worker_waits_off_ui_thread_and_preserves_other_workers() {
        let mut manager = Manager::new().unwrap();
        let mut one = Connection::new("One".into());
        one.enabled = false;
        let mut two = Connection::new("Two".into());
        two.enabled = false;
        let profiles = Profiles {
            format_version: 1,
            connections: vec![one.clone(), two.clone()],
        };
        manager.apply(&profiles).unwrap();
        let two_control = manager.control(&two.config.device_id).unwrap();
        let original = manager.workers.remove(&one.config.device_id).unwrap();
        original.task.join().unwrap();
        let (release, wait) = mpsc::channel();
        let old_control = Arc::new(Control::default());
        old_control.paused.store(true, Ordering::Release);
        let finished = Arc::new(AtomicBool::new(false));
        let done = finished.clone();
        manager.workers.insert(
            one.config.device_id.clone(),
            Worker {
                config: one.config.clone(),
                enabled: false,
                control: old_control.clone(),
                finished,
                task: std::thread::spawn(move || {
                    let _ = wait.recv_timeout(Duration::from_secs(5));
                    done.store(true, Ordering::Release);
                }),
            },
        );
        let mut changed = profiles;
        changed.connections[0].config.cache_max_files += 1;
        manager.apply(&changed).unwrap();
        assert!(old_control.stopping.load(Ordering::Acquire));
        assert!(Arc::ptr_eq(
            &two_control,
            &manager.control(&two.config.device_id).unwrap()
        ));
        let replacement = manager.workers.get(&one.config.device_id).unwrap();
        assert!(!replacement.finished.load(Ordering::Acquire));
        assert!(replacement.control.paused.load(Ordering::Acquire));
        release.send(()).unwrap();
        // Manager::drop joins the replacement, which first joins the old worker.
        drop(manager);
    }

    #[test]
    fn disabled_and_invalid_profiles_are_isolated_and_can_be_removed() {
        let mut manager = Manager::new().unwrap();
        let mut profiles = Profiles::default();
        let mut disabled = Connection::new("Disabled".into());
        disabled.enabled = false;
        profiles.connections.push(disabled);
        manager.apply(&profiles).unwrap();
        assert_eq!(manager.workers.len(), 2);
        for worker in manager.workers.values() {
            for _ in 0..100 {
                if worker.finished.load(Ordering::Acquire) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            assert!(worker.finished.load(Ordering::Acquire));
        }
        assert_eq!(
            manager
                .control(&profiles.connections[0].config.device_id)
                .unwrap()
                .sync_state(),
            SyncState::Error
        );
        assert_eq!(
            manager
                .control(&profiles.connections[1].config.device_id)
                .unwrap()
                .sync_state(),
            SyncState::Paused
        );
        profiles.connections.clear();
        manager.apply(&profiles).unwrap();
        assert!(manager.workers.is_empty());
    }
}
