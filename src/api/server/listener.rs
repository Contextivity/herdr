use super::*;
use std::sync::atomic::AtomicUsize;

// Subscriptions and waits retain workers, so admission bounds concurrent
// connections rather than queuing requests behind potentially long streams.
const MAX_CONNECTION_WORKERS: usize = 128;

#[derive(Clone)]
struct ListenerConfig {
    api_tx: ApiRequestSender,
    event_hub: EventHub,
    capabilities: Option<ServerCapabilities>,
    server_stop: Option<Arc<AtomicBool>>,
}

enum ListenerFailure {
    Accept(io::Error),
    Worker(io::Error),
}

struct BoundListener {
    thread: std::thread::JoinHandle<Result<(), ListenerFailure>>,
    identity: SocketFileIdentity,
}

pub struct ServerHandle {
    listener: Option<BoundListener>,
    path: PathBuf,
    identity: Option<SocketFileIdentity>,
    running: Arc<AtomicBool>,
    workers: Arc<AtomicUsize>,
    config: ListenerConfig,
}

impl Drop for ServerHandle {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Release);
        let cleanup = (|| {
            let admission = crate::ipc::public_endpoints::Admission::try_lock(&self.path)?;
            match &self.identity {
                Some(identity) => admission.cleanup(&self.path, identity),
                None => Ok(()),
            }
        })();
        if let Err(err) = cleanup {
            warn!(path = %self.path.display(), err = %err, "failed to remove api socket on shutdown");
        }
    }
}

impl ServerHandle {
    #[cfg(unix)]
    pub(crate) fn remove_socket_file_if_owned(
        &mut self,
        admission: &crate::ipc::public_endpoints::Admission,
    ) -> io::Result<()> {
        match self.identity.take() {
            Some(identity) => admission.unlink_owned(&self.path, &identity),
            None => Ok(()),
        }
    }

    /// The caller excludes shutdown and live handoff. A worker admission
    /// failure or panic is fatal; an accept failure may repair its owned path.
    pub(crate) fn recover_if_finished(&mut self) -> io::Result<()> {
        if self
            .config
            .server_stop
            .as_ref()
            .is_some_and(|stop| stop.load(Ordering::Acquire))
            || !self
                .listener
                .as_ref()
                .is_some_and(|listener| listener.thread.is_finished())
        {
            return Ok(());
        }
        let admission = crate::ipc::public_endpoints::Admission::lock(&self.path)?;
        let Some(listener) = self.listener.take() else {
            return Ok(());
        };
        match listener.thread.join() {
            Ok(Err(ListenerFailure::Accept(err))) => {
                error!(event = "api.listener.failed", path = %self.path.display(), err = %err, "api listener stopped; attempting owned recovery");
            }
            Ok(Err(ListenerFailure::Worker(err))) => return Err(err),
            Ok(Ok(())) => return Err(io::Error::other("api listener exited unexpectedly")),
            Err(_) => return Err(io::Error::other("api listener panicked")),
        }
        // Retire this generation's cleanup authority before unlinking. A
        // competitor can win the vacant name and even reuse the old inode.
        let identity = self
            .identity
            .take()
            .ok_or_else(|| io::Error::other("api socket ownership unavailable"))?;
        if socket_file_identity(&self.path)? != identity {
            return Err(io::Error::new(
                io::ErrorKind::AddrInUse,
                "api listener lost socket pathname ownership; refusing recovery",
            ));
        }
        admission.unlink_owned(&self.path, &identity)?;
        // Direct binding never reclaims a competitor's path. Updating only
        // the listener generation preserves established connections and avoids
        // running the old handle's cleanup against the replacement socket.
        let replacement = bind_listener(
            &self.path,
            &self.config,
            &self.running,
            &self.workers,
            &admission,
        )?;
        self.identity = Some(replacement.identity.clone());
        self.listener = Some(replacement);
        info!(event = "api.listener.recovered", path = %self.path.display(), "api listener recovered without replacing session runtime");
        Ok(())
    }
}

pub(super) fn start_on_path(
    path: PathBuf,
    api_tx: ApiRequestSender,
    event_hub: EventHub,
    capabilities: Option<ServerCapabilities>,
    server_stop: Option<Arc<AtomicBool>>,
    admission: &crate::ipc::public_endpoints::Admission,
) -> io::Result<ServerHandle> {
    let config = ListenerConfig {
        api_tx,
        event_hub,
        capabilities,
        server_stop,
    };
    let running = Arc::new(AtomicBool::new(true));
    let workers = Arc::new(AtomicUsize::new(0));
    let listener = bind_listener(&path, &config, &running, &workers, admission)?;
    let identity = listener.identity.clone();
    Ok(ServerHandle {
        listener: Some(listener),
        path,
        identity: Some(identity),
        running,
        workers,
        config,
    })
}

struct WorkerPermit(Arc<AtomicUsize>);

impl WorkerPermit {
    fn acquire(workers: &Arc<AtomicUsize>) -> Option<Self> {
        workers
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                (count < MAX_CONNECTION_WORKERS).then_some(count + 1)
            })
            .ok()
            .map(|_| Self(workers.clone()))
    }
}

impl Drop for WorkerPermit {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Release);
    }
}

fn bind_listener(
    path: &Path,
    config: &ListenerConfig,
    running: &Arc<AtomicBool>,
    workers: &Arc<AtomicUsize>,
    admission: &crate::ipc::public_endpoints::Admission,
) -> io::Result<BoundListener> {
    let listener = bind_local_listener(path)?;
    let identity = socket_file_identity(path)?;
    let listener_running = running.clone();
    let listener_config = config.clone();
    let workers = workers.clone();
    restrict_socket_permissions(path)?;
    admission.publish(path, &identity)?;
    let result = {
        std::thread::Builder::new().name("herdr-api-listener".into()).spawn(move || {
            for stream in listener.incoming() {
                let stream = match stream {
                    Ok(stream) => stream,
                    Err(err) if matches!(err.kind(), io::ErrorKind::Interrupted | io::ErrorKind::ConnectionAborted) => continue,
                    Err(err) => return Err(ListenerFailure::Accept(err)),
                };
                if !listener_running.load(Ordering::Acquire) {
                    return Ok(());
                }
                let Some(permit) = WorkerPermit::acquire(&workers) else {
                    warn!(event = "api.connection.capacity", limit = MAX_CONNECTION_WORKERS, "api connection capacity reached; connection closed");
                    continue;
                };
                let config = listener_config.clone();
                let connection_running = listener_running.clone();
                if let Err(err) = std::thread::Builder::new().name("herdr-api-connection".into()).spawn(move || {
                    let _permit = permit;
                    if let Err(err) = handle_connection_with_stop(stream, &config.api_tx, &config.event_hub, &connection_running, config.capabilities, config.server_stop.as_ref()) {
                        warn!(err = %err, "api connection failed");
                    }
                }) {
                    error!(event = "api.connection.spawn_failed", err = %err, "failed to start api connection worker; stopping server");
                    return Err(ListenerFailure::Worker(err));
                }
            }
            Err(ListenerFailure::Accept(io::Error::other("api listener ended unexpectedly")))
        })
    };
    match result {
        Ok(thread) => {
            info!(path = %path.display(), "api server listening");
            Ok(BoundListener { thread, identity })
        }
        Err(err) => {
            let _ = admission.cleanup(path, &identity);
            Err(err)
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    // Neither a Rust panic nor an early clean return can be injected through
    // the socket API. Exercise the real thread-join boundary without exposing
    // a production fault switch. Worker failure is covered end to end.
    #[test]
    fn unexpected_api_listener_exit_is_fatal_unless_stopping() {
        for panic in [false, true] {
            let path =
                std::env::temp_dir().join(format!("hapi-exit-{}-{panic}.sock", std::process::id()));
            let _socket = bind_local_listener(&path).unwrap();
            let identity = socket_file_identity(&path).unwrap();
            let (api_tx, _rx) = tokio::sync::mpsc::unbounded_channel();
            let stop = Arc::new(AtomicBool::new(true));
            let thread = std::thread::spawn(move || {
                assert!(!panic, "injected listener panic");
                Ok(())
            });
            while !thread.is_finished() {
                std::thread::yield_now();
            }
            let mut handle = ServerHandle {
                listener: Some(BoundListener {
                    thread,
                    identity: identity.clone(),
                }),
                path: path.clone(),
                identity: Some(identity.clone()),
                running: Arc::new(AtomicBool::new(true)),
                workers: Arc::new(AtomicUsize::new(0)),
                config: ListenerConfig {
                    api_tx,
                    event_hub: EventHub::default(),
                    capabilities: None,
                    server_stop: Some(stop.clone()),
                },
            };
            handle.recover_if_finished().unwrap();
            assert!(
                handle.listener.is_some(),
                "explicit stop must skip recovery"
            );
            stop.store(false, Ordering::Release);
            let error = handle.recover_if_finished().unwrap_err();
            assert!(error.to_string().contains(if panic {
                "panicked"
            } else {
                "exited unexpectedly"
            }));
            assert_eq!(
                socket_file_identity(&path).unwrap(),
                identity,
                "fatal join must not rebind"
            );
            drop(handle);
            // This private thread fixture has no published owner lease. The
            // real shutdown generation contract is covered end to end.
            fs::remove_file(&path).unwrap();
        }
    }
}
