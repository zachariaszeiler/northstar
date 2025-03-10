use super::{
    cgroups,
    config::{Config, RepositoryType},
    console::Request,
    error::Error,
    fork::Forker,
    io,
    mount::MountControl,
    repository::{DirRepository, MemRepository, Npk},
    stats::ContainerStats,
    Container, ContainerEvent, Event, EventTx, ExitStatus, NotificationTx, Pid, RepositoryId,
};
use crate::{
    api::{self, model},
    common::{name::Name, non_nul_string::NonNulString, version::VersionReq},
    npk::manifest::{
        mount::{Mount, Resource},
        Autostart, Manifest,
    },
    runtime::{
        console::{Console, Peer},
        io::ContainerIo,
        ipc::owned_fd::OwnedFd,
        CGroupEvent, ENV_CONSOLE, ENV_CONTAINER, ENV_NAME, ENV_VERSION,
    },
};
use bytes::Bytes;
use futures::{
    future::{join_all, ready, Either},
    Future, FutureExt, Stream, StreamExt, TryFutureExt,
};
use humantime::format_duration;
use itertools::Itertools;
use log::{debug, error, info, warn};
use nix::sys::signal::Signal;
use std::{
    collections::{HashMap, HashSet},
    convert::TryFrom,
    fmt::Debug,
    iter::{once, FromIterator},
    os::unix::net::UnixStream as StdUnixStream,
    path::PathBuf,
    result,
    sync::Arc,
};
use tokio::{
    net::UnixStream,
    pin,
    sync::{mpsc, oneshot},
    task::{self, JoinHandle},
    time,
};
use tokio_util::sync::CancellationToken;

/// Repository
type Repository = Box<dyn super::repository::Repository + Send + Sync>;

#[derive(Debug)]
pub(super) struct State {
    config: Config,
    events_tx: EventTx,
    notification_tx: NotificationTx,
    mount_control: Arc<MountControl>,
    launcher: Forker,
    containers: HashMap<Container, ContainerState>,
    repositories: HashMap<RepositoryId, Repository>,
}

#[derive(Debug, Default)]
pub(super) struct ContainerState {
    /// Reference to the repository where the npk resides
    pub repository: RepositoryId,
    /// Mount point of the root fs
    pub root: Option<PathBuf>,
    /// Process information when started
    pub process: Option<ContainerContext>,
}

impl ContainerState {
    pub fn is_mounted(&self) -> bool {
        self.root.is_some()
    }
}

#[derive(Debug)]
pub(super) struct ContainerContext {
    pid: Pid,
    started: time::Instant,
    debug: super::debug::Debug,
    cgroups: cgroups::CGroups,
    stop: CancellationToken,
    log_task: Option<JoinHandle<std::io::Result<()>>>,
    /// Resources used by this container. This list differs from
    /// manifest because the manifest just containers version
    /// requirements and not concrete resources.
    resources: HashSet<Container>,
}

impl ContainerContext {
    async fn destroy(mut self) {
        // Stop console if there's any any
        self.stop.cancel();

        if let Some(log_task) = self.log_task.take() {
            // Wait for the pty to finish
            drop(log_task.await);
        }

        self.debug
            .destroy()
            .await
            .expect("failed to destroy debug utilities");

        self.cgroups.destroy().await;
    }
}

impl State {
    /// Create a new empty State instance
    pub(super) async fn new(
        config: Config,
        events_tx: EventTx,
        notification_tx: NotificationTx,
        forker: Forker,
    ) -> Result<State, Error> {
        let repositories = HashMap::new();
        let containers = HashMap::new();
        let mount_control = Arc::new(
            MountControl::new(
                config.device_mapper_device_timeout,
                config.loop_device_timeout,
            )
            .await
            .expect("failed to initialize mount control"),
        );

        let mut state = State {
            events_tx,
            notification_tx,
            repositories,
            containers,
            config,
            launcher: forker,
            mount_control,
        };

        // Initialize repositories. This populates self.containers and self.repositories
        let mount_repositories = state.initialize_repositories().await?;

        // Mount all containers if configured
        state.automount(&mount_repositories).await?;

        // Start containers flagged with autostart
        state.autostart().await?;

        Ok(state)
    }

    /// Iterate the list of repositories and initialize them
    async fn initialize_repositories(&mut self) -> Result<HashSet<RepositoryId>, Error> {
        // List of repositories to mount
        let mut mount_repositories = HashSet::with_capacity(self.config.repositories.len());

        // Build a map of repositories from the configuration
        for (id, repository) in &self.config.repositories {
            if repository.mount_on_start {
                mount_repositories.insert(id.clone());
            }

            let repository = match &repository.r#type {
                RepositoryType::Fs { dir } => {
                    let repository = DirRepository::new(dir, repository.key.as_deref()).await?;
                    Box::new(repository) as Repository
                }
                RepositoryType::Memory => {
                    let repository = MemRepository::new(repository.key.as_deref()).await?;
                    Box::new(repository) as Repository
                }
            };

            for npk in repository.containers() {
                let name = npk.manifest().name.clone();
                let version = npk.manifest().version.clone();
                let container = Container::new(name, version);

                if let Ok(state) = self.state(&container) {
                    warn!("Skipping duplicate container {} which is already loaded from repository {}", container, state.repository);
                } else {
                    self.containers.insert(
                        container,
                        ContainerState {
                            repository: id.clone(),
                            ..Default::default()
                        },
                    );
                }
            }
            self.repositories.insert(id.clone(), repository);
        }

        Ok(mount_repositories)
    }

    /// Try to mount all installed continers
    async fn automount(&mut self, repositories: &HashSet<RepositoryId>) -> Result<(), Error> {
        if repositories.is_empty() {
            return Ok(());
        }

        info!(
            "Trying to mount containers from repository {}",
            repositories.iter().join(", ")
        );
        // Collect all containers that match a repository in `repositories
        let containers = self
            .containers
            .iter()
            .filter(|(_, state)| repositories.contains(&state.repository))
            .map(|(container, _)| container.clone())
            .collect::<Vec<Container>>();

        if !containers.is_empty() {
            for result in self.mount_all(&containers).await {
                result?;
            }
        }

        Ok(())
    }

    async fn autostart(&mut self) -> Result<(), Error> {
        // List of containers from all repositories with the autostart flag set
        let mut autostarts = Vec::with_capacity(self.containers.len());
        // List of containers that need to be mounted
        let mut to_mount = Vec::with_capacity(self.containers.len());

        for (container, state) in self.containers.iter() {
            if let Some(autostart) = self
                .manifest(container)
                .expect("internal error")
                .autostart
                .as_ref()
            {
                autostarts.push((container.clone(), autostart.clone()));
                if !state.is_mounted() {
                    to_mount.push(container.clone())
                }
            }
        }

        // Add resources of containers that have the autostart flag set
        for (container, autostart) in &autostarts {
            let manifest = self.manifest(container)?;
            for mount in manifest.mounts.values() {
                if let Mount::Resource(Resource { name, version, .. }) = mount {
                    if let Some(resource) =
                        State::match_container(name, version, self.containers.keys())
                    {
                        to_mount.push(resource.clone());
                    } else {
                        let error = Error::StartContainerMissingResource(
                            container.clone(),
                            name.clone(),
                            version.to_string(),
                        );
                        Self::warn_autostart_failure(container, autostart, error)?
                    }
                }
            }
        }

        // Mount (parallel). Do not care about the result - this normally is fine. If not, the container will not start.
        if !to_mount.is_empty() {
            self.mount_all(&to_mount).await;

            for (container, autostart) in autostarts {
                info!("Autostarting {} ({:?})", container, autostart);
                if let Err(e) = self
                    .start(&container, &[], &HashMap::with_capacity(0))
                    .await
                {
                    Self::warn_autostart_failure(&container, &autostart, e)?
                }
            }
        }

        Ok(())
    }

    fn warn_autostart_failure(
        container: &Container,
        autostart: &Autostart,
        e: Error,
    ) -> Result<(), Error> {
        match autostart {
            Autostart::Relaxed => {
                warn!("Failed to autostart relaxed {}: {}", container, e);
                Ok(())
            }
            Autostart::Critical => {
                error!("Failed to autostart critical {}: {}", container, e);
                Err(e)
            }
        }
    }

    /// Create a future that mounts `container`
    fn mount(&self, container: &Container) -> impl Future<Output = Result<PathBuf, Error>> {
        // Find the repository that has the container
        let container_state = self.containers.get(container).expect("internal error");
        let repository = self
            .repositories
            .get(&container_state.repository)
            .expect("internal error");
        let key = repository.key().cloned();
        let npk = self.npk(container).expect("internal error");
        let root = self.config.run_dir.join(container.to_string());
        let mount_control = self.mount_control.clone();
        mount_control
            .mount(npk, &root, key.as_ref())
            .map_err(Error::Mount)
            .map(|_| Ok(root))
    }

    /// Create a future that umounts `container`. Return a futures that yield
    /// a busy error if the container is not mounted.
    fn umount(&self, container: &Container) -> impl Future<Output = Result<(), Error>> {
        // Check if this container is in used by other containers
        if let Some(user) = self
            .containers
            .iter()
            .filter_map(|(c, state)| state.process.as_ref().map(|process| (c, process)))
            .find(|(_, process)| process.resources.contains(container))
            .map(|(c, _)| c)
        {
            warn!(
                "Failed to umount {} because it is used by {}",
                container, user
            );
            return Either::Right(ready(Err(Error::UmountBusy(container.clone()))));
        }

        match self.state(container).and_then(|state| {
            state
                .root
                .as_ref()
                .ok_or_else(|| Error::UmountBusy(container.clone()))
        }) {
            Ok(root) => Either::Left(MountControl::umount(root).map_err(Error::Mount)),
            Err(e) => Either::Right(ready(Err(e))),
        }
    }

    /// Start a container
    /// `container`: Container to start
    /// `args_extra`: Optional command line arguments that overwrite the values from the manifest
    /// `env_extra`: Optional env variables that overwrite the values from the manifest
    pub(super) async fn start(
        &mut self,
        container: &Container,
        args_extra: &[NonNulString],
        env_extra: &HashMap<NonNulString, NonNulString>,
    ) -> Result<(), Error> {
        let start = time::Instant::now();
        info!("Trying to start {}", container);

        // Check if the container is already running
        let container_state = self.state(container)?;
        if container_state.process.is_some() {
            warn!("Application {} is already running", container);
            return Err(Error::StartContainerStarted(container.clone()));
        }

        // Check optional env variables for reserved ENV_NAME or ENV_VERSION key which cannot be overwritten
        if env_extra.keys().any(|k| {
            k.as_str() == ENV_NAME
                || k.as_str() == ENV_VERSION
                || k.as_str() == ENV_CONTAINER
                || k.as_str() == ENV_CONSOLE
        }) {
            return Err(Error::InvalidArguments(format!(
                "env contains reserved key {} or {} or {} or {}",
                ENV_NAME, ENV_VERSION, ENV_CONTAINER, ENV_CONSOLE
            )));
        }

        let manifest = self.manifest(container)?.clone();

        // Check if the container is not a resource
        let init = if let Some(ref init) = manifest.init {
            NonNulString::try_from(init.display().to_string())
                .map_err(|_| Error::InvalidArguments(init.display().to_string()))?
        } else {
            warn!("Container {} is a resource", container);
            return Err(Error::StartContainerResource(container.clone()));
        };

        // Containers that need to be mounted before container can be started
        let mut need_mount = HashSet::new();
        // Resources use by this container
        let mut resources = HashSet::new();

        // The container to be started
        if !container_state.is_mounted() {
            need_mount.insert(container.clone());
        }

        // Collect resources used by container
        let required_resources = manifest
            .mounts
            .values()
            .filter_map(|m| match m {
                Mount::Resource(resource) => Some(resource),
                _ => None,
            })
            .collect::<Vec<_>>();
        for resource in required_resources {
            let best_match =
                State::match_container(&resource.name, &resource.version, self.containers.keys())
                    .ok_or_else(|| {
                    Error::StartContainerMissingResource(
                        container.clone(),
                        resource.name.clone(),
                        resource.version.to_string(),
                    )
                })?;
            let state = self
                .state(best_match)
                .expect("Failed to determine resource container state");

            resources.insert(best_match.clone());

            if !state.is_mounted() {
                need_mount.insert(best_match.clone());
            }
        }

        // Mount containers
        if !need_mount.is_empty() {
            info!(
                "Mounting {} container(s) for the start of {}",
                need_mount.len(),
                container
            );
            for mount in self.mount_all(&Vec::from_iter(need_mount)).await {
                // Abort if at least one container failed to mount
                if let Err(e) = mount {
                    warn!("failed to mount: {}", e);
                    return Err(e);
                }
            }
        }

        // Spawn process
        info!("Creating {}", container);

        // Create a token to stop tasks spawned related to this container
        let stop = CancellationToken::new();

        // We send the fd to the forker so that it can pass it to the init
        let console_fd = if let Some(configuration) = manifest.console.clone() {
            let peer = Peer::Container(container.clone());
            let (runtime_stream, container_stream) =
                StdUnixStream::pair().expect("failed to create socketpair");
            let container_fd: OwnedFd = container_stream.into();

            let runtime = runtime_stream
                .set_nonblocking(true)
                .and_then(|_| UnixStream::from_std(runtime_stream))
                .expect("failed to set socket into nonblocking mode");

            let notifications = self.notification_tx.subscribe();
            let events_tx = self.events_tx.clone();
            let stop = stop.clone();
            let container = Some(container.clone());
            let connection = Console::connection(
                runtime,
                peer,
                stop,
                container,
                configuration,
                self.config.token_validity,
                events_tx,
                notifications,
                None,
            );

            // Start console task
            task::spawn(connection);

            Some(container_fd)
        } else {
            None
        };

        // Create container
        let config = &self.config;
        let containers = self.containers.iter().map(|(c, _)| c);
        let pid = self
            .launcher
            .create(config, &manifest, console_fd, containers)
            .await?;

        // Debug
        let debug = super::debug::Debug::new(&self.config, &manifest, pid).await?;

        // CGroups
        let cgroups = {
            let config = manifest.cgroups.clone().unwrap_or_default();
            let events_tx = self.events_tx.clone();

            // Creating a cgroup is a northstar internal thing. If it fails it's not recoverable.
            cgroups::CGroups::new(&self.config.cgroup, events_tx, container, &config, pid)
                .await
                .expect("failed to create cgroup")
        };

        // Open a file handle for stdin, stdout and stderr according to the manifest
        let ContainerIo { io, log_task } = io::open(container, &manifest.io)
            .await
            .expect("IO setup error");

        // Binary arguments
        let mut args = Vec::with_capacity(
            1 + if args_extra.is_empty() {
                manifest.args.len()
            } else {
                args_extra.len()
            },
        );
        args.push(init.clone());
        if !args_extra.is_empty() {
            args.extend(args_extra.iter().cloned());
        } else {
            args.extend(manifest.args.iter().cloned());
        };

        // Overwrite the env variables from the manifest if variables are provided
        // with the start command
        let env = if env_extra.is_empty() {
            &manifest.env
        } else {
            env_extra
        };

        let env = env
            .iter()
            .map(|(k, v)| format!("{}={}", k, v))
            .chain(once(format!("{}={}", ENV_CONTAINER, container)))
            .chain(once(format!("{}={}", ENV_NAME, container.name())))
            .chain(once(format!("{}={}", ENV_VERSION, container.version())))
            .map(|s| unsafe { NonNulString::from_string_unchecked(s) })
            .collect::<Vec<_>>();

        debug!("Container {} init is {:?}", container, init);
        debug!("Container {} argv is {}", container, args.iter().join(" "));
        debug!("Container {} env is {}", container, env.iter().join(", "));

        // Send exec request to launcher
        if let Err(e) = self
            .launcher
            .exec(container.clone(), init, args, env, io)
            .await
        {
            warn!("failed to exec {} ({}): {}", container, pid, e);

            stop.cancel();

            if let Some(log_task) = log_task {
                drop(log_task.await);
            }
            debug.destroy().await.expect("failed to destroy debug");
            cgroups.destroy().await;
            return Err(e);
        }

        // Get a mutable reference to the container state in order to update the process field
        let container_state = self.containers.get_mut(container).expect("Internal error");

        // Add process context to process
        let started = time::Instant::now();
        container_state.process = Some(ContainerContext {
            pid,
            started,
            debug,
            cgroups,
            stop,
            log_task,
            resources,
        });

        let duration = start.elapsed().as_secs_f32();
        info!("Started {} ({}) in {:.03}s", container, pid, duration);

        // Send container started event
        self.container_event(container, ContainerEvent::Started);

        Ok(())
    }

    /// Send signal `signal` to container if running
    pub(super) async fn kill(
        &mut self,
        container: &Container,
        signal: Signal,
    ) -> Result<(), Error> {
        let container_state = self.state_mut(container)?;

        match &mut container_state.process {
            Some(context) => {
                info!("Killing {} with {}", container, signal.as_str());
                let pid = context.pid;
                let process_group = nix::unistd::Pid::from_raw(-(pid as i32));
                match nix::sys::signal::kill(process_group, Some(signal)) {
                    Ok(_) => Ok(()),
                    Err(nix::Error::ESRCH) => {
                        debug!("Process {} already exited", pid);
                        Ok(())
                    }
                    Err(e) => unimplemented!("Kill error {}", e),
                }
            }
            None => Err(Error::StopContainerNotStarted(container.clone())),
        }
    }

    /// Shutdown the runtime: stop running applications and umount npks
    pub(super) async fn shutdown(
        mut self,
        event_rx: impl Stream<Item = Event>,
    ) -> Result<(), Error> {
        let started_containers = self
            .containers
            .iter()
            .filter_map(|(container, state)| state.process.as_ref().map(|_| container.clone()))
            .collect::<Vec<_>>();

        // Send a SIGKILL to each started container
        for container in &started_containers {
            self.kill(container, Signal::SIGKILL).await?;
        }

        // Wait until all processes are gone
        pin!(event_rx);
        while self
            .containers
            .values()
            .any(|state| state.process.is_some())
        {
            if let Some(Event::Container(container, event)) = event_rx.next().await {
                self.on_event(&container, &event, true).await?;
            }
        }

        // Try to umount mounted containers
        let to_umount = self
            .containers
            .iter()
            .filter(|(_, state)| state.is_mounted())
            .map(|(container, _)| container.clone())
            .collect::<Vec<_>>();
        self.umount_all(&to_umount).await;

        Ok(())
    }

    /// Install an NPK
    async fn install(
        &mut self,
        id: &str,
        rx: &mut mpsc::Receiver<Bytes>,
    ) -> Result<Container, Error> {
        // Find the repository
        let repository = self
            .repositories
            .get_mut(id)
            .ok_or_else(|| Error::InvalidRepository(id.to_string()))?;

        // Add the npk to the repository
        let container = repository.insert(rx).await?;

        // Check if container is already known and remove newly installed one if so
        let already_installed = self
            .state(&container)
            .ok()
            .map(|state| state.repository.clone());

        if let Some(current_repository) = already_installed {
            warn!(
                "Skipping duplicate container {} which is already in repository {}",
                container, current_repository
            );

            let repository = self
                .repositories
                .get_mut(id)
                .ok_or_else(|| Error::InvalidRepository(id.to_string()))?;
            repository.remove(&container).await?;
            return Err(Error::InstallDuplicate(container));
        }

        // Add the container to the state
        self.containers.insert(
            container.clone(),
            ContainerState {
                repository: id.into(),
                ..Default::default()
            },
        );
        info!("Successfully installed {}", container);

        self.container_event(&container, ContainerEvent::Installed);

        Ok(container)
    }

    /// Remove and umount a specific app
    async fn uninstall(&mut self, container: &Container) -> Result<(), Error> {
        info!("Trying to uninstall {}", container);

        let state = self.state(container)?;
        let repository = state.repository.clone();

        // Umount
        if state.is_mounted() {
            self.umount_all(&[container.clone()])
                .await
                .pop()
                .expect("internal error")?;
        }

        // Remove from repository
        debug!("Removing {} from {}", container, repository);
        self.repositories
            .get_mut(&repository)
            .expect("Internal error")
            .remove(container)
            .await?;

        self.containers.remove(container);
        info!("Successfully uninstalled {}", container);

        self.container_event(container, ContainerEvent::Uninstalled);

        Ok(())
    }

    /// Gather statistics for `container`
    async fn container_stats(
        &mut self,
        container: &Container,
    ) -> result::Result<ContainerStats, Error> {
        // Get container state or return if it's unknown
        let state = self.state(container)?;

        // Gather stats if the container is running
        if let Some(process) = state.process.as_ref() {
            info!("Collecting stats of {}", container);
            Ok(process.cgroups.stats())
        } else {
            Err(Error::ContainerNotStarted(container.clone()))
        }
    }

    /// Handle the exit of a container
    async fn on_exit(
        &mut self,
        container: &Container,
        exit_status: &ExitStatus,
        is_shutdown: bool,
    ) -> Result<(), Error> {
        let autostart = self
            .manifest(container)
            .ok()
            .and_then(|manfiest| manfiest.autostart.clone());

        if let Ok(state) = self.state_mut(container) {
            if let Some(process) = state.process.take() {
                let is_critical = autostart == Some(Autostart::Critical);
                let is_critical = is_critical && !is_shutdown;
                let duration = process.started.elapsed();
                if is_critical {
                    error!(
                        "Critical process {} exited after {} with status {}",
                        container,
                        format_duration(duration),
                        exit_status,
                    );
                } else {
                    info!(
                        "Process {} exited after {} with status {}",
                        container,
                        format_duration(duration),
                        exit_status,
                    );
                }

                process.destroy().await;

                self.container_event(container, ContainerEvent::Exit(exit_status.clone()));

                info!("Container {} exited with status {}", container, exit_status);

                // This is a critical flagged container that exited with a error exit code. That's not good...
                if !exit_status.success() && is_critical {
                    return Err(Error::CriticalContainer(
                        container.clone(),
                        exit_status.clone(),
                    ));
                }
            }
        }
        Ok(())
    }

    // Handle global events
    pub(super) async fn on_event(
        &mut self,
        container: &Container,
        event: &ContainerEvent,
        is_shutdown: bool,
    ) -> Result<(), Error> {
        match event {
            ContainerEvent::Started => (),
            ContainerEvent::Exit(exit_status) => {
                self.on_exit(container, exit_status, is_shutdown).await?;
            }
            ContainerEvent::Installed => (),
            ContainerEvent::Uninstalled => (),
            ContainerEvent::CGroup(CGroupEvent::Memory(_)) => {
                warn!("Process {} is out of memory", container);
            }
        }

        Ok(())
    }

    /// Process console events
    pub(super) async fn on_request(
        &mut self,
        request: Request,
        response: oneshot::Sender<model::Response>,
    ) -> Result<(), Error> {
        match request {
            Request::Request(ref request) => {
                let payload = match request {
                    model::Request::Containers => {
                        model::Response::Containers(self.list_containers())
                    }
                    model::Request::Install { .. } => unreachable!(),
                    model::Request::Mount(containers) => {
                        let result = self
                            .mount_all(containers)
                            .await
                            .drain(..)
                            .zip(containers)
                            .map(|(r, c)| match r {
                                Ok(r) => model::MountResult::Ok { container: r },
                                Err(e) => model::MountResult::Error {
                                    container: c.clone(),
                                    error: e.into(),
                                },
                            })
                            .collect();
                        model::Response::Mount(result)
                    }
                    model::Request::Umount(containers) => {
                        let result = self
                            .umount_all(containers)
                            .await
                            .drain(..)
                            .zip(containers)
                            .map(|(r, c)| match r {
                                Ok(r) => model::UmountResult::Ok { container: r },
                                Err(e) => model::UmountResult::Error {
                                    container: c.clone(),
                                    error: e.into(),
                                },
                            })
                            .collect();
                        model::Response::Umount(result)
                    }
                    model::Request::Repositories => {
                        let repositories = self.repositories.keys().cloned().collect();
                        model::Response::Repositories(repositories)
                    }
                    model::Request::Shutdown => {
                        self.events_tx
                            .send(Event::Shutdown)
                            .await
                            .expect("Internal channel error on main");
                        model::Response::Ok
                    }
                    model::Request::Start(container, args, env) => {
                        match self.start(container, args, env).await {
                            Ok(_) => model::Response::Ok,
                            Err(e) => {
                                warn!("failed to start {}: {}", container, e);
                                model::Response::Error(e.into())
                            }
                        }
                    }
                    model::Request::Kill(container, signal) => match Signal::try_from(*signal) {
                        Ok(signal) => match self.kill(container, signal).await {
                            Ok(_) => model::Response::Ok,
                            Err(e) => {
                                error!("failed to kill {} with {}: {}", container, signal, e);
                                model::Response::Error(e.into())
                            }
                        },
                        Err(e) => {
                            error!("failed to kill {} with {}: {}", container, signal, e);
                            model::Response::Error(model::Error::Unexpected {
                                module: "invalid signal".into(),
                                error: e.to_string(),
                            })
                        }
                    },
                    model::Request::Uninstall(container) => match self.uninstall(container).await {
                        Ok(_) => api::model::Response::Ok,
                        Err(e) => {
                            warn!("failed to uninstall {}: {}", container, e);
                            model::Response::Error(e.into())
                        }
                    },
                    model::Request::ContainerStats(container) => {
                        match self.container_stats(container).await {
                            Ok(stats) => {
                                api::model::Response::ContainerStats(container.clone(), stats)
                            }
                            Err(e) => {
                                warn!("failed to gather stats for {}: {}", container, e);
                                model::Response::Error(e.into())
                            }
                        }
                    }
                    model::Request::Ident => unreachable!(), // handled in module console
                    model::Request::TokenCreate(..) => unreachable!(), // handled in module console
                    model::Request::TokenVerify(..) => unreachable!(), // handled in module console
                };

                // A error on the response_tx means that the connection
                // was closed in the meantime. Ignore it.
                response.send(payload).ok();
            }
            Request::Install(repository, mut rx) => {
                let payload = match self.install(&repository, &mut rx).await {
                    Ok(container) => model::Response::Install(container),
                    Err(e) => model::Response::Error(e.into()),
                };

                // A error on the response_tx means that the connection
                // was closed in the meantime. Ignore it.
                response.send(payload).ok();
            }
        }
        Ok(())
    }

    /// Try to mount all containers in `containers` in parallel and return the results. The parallelism
    /// is archived by a dedicated thread pool that executes the blocking mount operations on n threads
    /// as configured in the runtime configuration.
    async fn mount_all(&mut self, containers: &[Container]) -> Vec<Result<Container, Error>> {
        let start = time::Instant::now();
        let mut mounts = Vec::with_capacity(containers.len());

        // Create mount futures
        for container in containers {
            match self.state(container) {
                // Containers cannot be mounted twice. If the container
                // is already mounted return an error for this entity.
                Ok(state) if state.is_mounted() => {
                    let error = Err(Error::MountBusy(container.clone()));
                    mounts.push(Either::Right(ready(error)));
                }
                Ok(_) => mounts.push(Either::Left(self.mount(container))),
                Err(_) => {
                    let error = Err(Error::InvalidContainer(container.clone()));
                    mounts.push(Either::Right(ready(error)));
                }
            }
        }

        // Mount and process results
        let mut result = Vec::with_capacity(containers.len());
        for (container, mount_result) in containers.iter().zip(join_all(mounts).await) {
            match mount_result {
                Ok(root) => {
                    let state = self.state_mut(container).expect("Internal error");
                    state.root = Some(root);
                    info!("Mounted {}", container);
                    result.push(Ok(container.clone()));
                }
                Err(e) => {
                    warn!("failed to mount {}: {}", container, e);
                    result.push(Err(e));
                }
            }
        }

        let duration = start.elapsed();
        if result.iter().any(|e| e.is_err()) {
            warn!("Mount operation failed after {}", format_duration(duration));
        } else {
            info!(
                "Successfully mounted {} container(s) in {}",
                result.len(),
                format_duration(duration)
            );
        }
        result
    }

    async fn umount_all(&mut self, containers: &[Container]) -> Vec<Result<Container, Error>> {
        let start = time::Instant::now();
        let mut mounts = Vec::with_capacity(containers.len());

        // Create mount futures
        'outer: for umount_container in containers {
            // Retrieve container state. If the container is unknown insert a
            // ready future with the corresponding error
            let (container_state, manifest) = if let Ok((state, manifest)) =
                self.state(umount_container).and_then(|state| {
                    self.manifest(umount_container)
                        .map(|manifest| (state, manifest))
                }) {
                (state, manifest)
            } else {
                let error = Err(Error::InvalidContainer(umount_container.clone()));
                mounts.push(Either::Right(ready(error)));
                continue;
            };

            // Check if container is mounted at all
            if !container_state.is_mounted() {
                let error = Err(Error::UmountBusy(umount_container.clone()));
                mounts.push(Either::Right(ready(error)));
                continue;
            }

            // Check if container is started
            if container_state.process.is_some() {
                let error = Err(Error::UmountBusy(umount_container.clone()));
                mounts.push(Either::Right(ready(error)));
                continue;
            }

            // If this container is a resource check all running containers if they
            // depend on `container`
            if manifest.init.is_none() {
                for (running_container, state) in &self.containers {
                    // A not started container cannot use `container`
                    if state.process.is_none() {
                        continue;
                    }

                    // Get manifest for container in question
                    let manifest = self.manifest(running_container).expect("Internal error");

                    // Resources cannot have resource dependencies
                    if manifest.init.is_none() {
                        continue;
                    }

                    for mount in &manifest.mounts {
                        if let Mount::Resource(Resource { name, version, .. }) = mount.1 {
                            if State::match_container(name, version, self.containers.keys())
                                .filter(|resource| &umount_container == resource)
                                .is_some()
                            {
                                warn!(
                                    "Resource container {} is used by {}",
                                    umount_container, running_container
                                );
                                let error = Err(Error::UmountBusy(running_container.clone()));
                                mounts.push(Either::Right(ready(error)));
                                continue 'outer;
                            }
                        }
                    }
                }
            }

            // Hm. Seems that it really needs to be umounted.
            mounts.push(Either::Left(self.umount(umount_container)));
        }

        debug_assert_eq!(mounts.len(), containers.len());

        // Umount and process umount results
        let mut result = Vec::with_capacity(containers.len());
        for (container, mount_result) in containers.iter().zip(join_all(mounts).await) {
            match mount_result {
                Ok(_) => {
                    let state = self.state_mut(container).expect("Internal error");
                    state.root = None;
                    info!("Umounted {}", container);
                    result.push(Ok(container.clone()));
                }
                Err(e) => {
                    warn!("failed to umount {}: {}", container, e);
                    result.push(Err(e));
                }
            }
        }

        let duration = start.elapsed();
        if result.iter().any(|e| e.is_err()) {
            warn!(
                "Umount operation failed after {}",
                format_duration(duration)
            );
        } else {
            info!(
                "Successfully umounted {} container(s) in {}",
                result.len(),
                format_duration(duration)
            );
        }
        result
    }

    /// Find a resource container that best matches the given version requirement.
    pub fn match_container<'a, I: Iterator<Item = &'a Container>>(
        name: &Name,
        version_req: &VersionReq,
        containers: I,
    ) -> Option<&'a Container> {
        containers
            .filter(|c| c.name() == name && version_req.matches(c.version()))
            .sorted_by(|c1, c2| c1.version().cmp(c2.version()))
            .next()
    }

    fn list_containers(&self) -> Vec<api::model::ContainerData> {
        let mut result = Vec::with_capacity(self.containers.len());

        for (container, state) in &self.containers {
            let manifest = self.manifest(container).expect("Internal error").clone();
            let process = state.process.as_ref().map(|context| api::model::Process {
                pid: context.pid,
                uptime: context.started.elapsed().as_nanos() as u64,
            });
            let repository = state.repository.clone();
            let mounted = state.is_mounted();
            let container = container.clone();
            let container_data = api::model::ContainerData {
                container,
                repository,
                manifest,
                process,
                mounted,
            };
            result.push(container_data);
        }

        result
    }

    /// Send a container event to all subscriber consoles
    fn container_event(&self, container: &Container, event: ContainerEvent) {
        // Do not fill the notification channel if there's nobody subscribed
        if self.notification_tx.receiver_count() > 0 {
            self.notification_tx.send((container.clone(), event)).ok();
        }
    }

    fn state(&self, container: &Container) -> Result<&ContainerState, Error> {
        self.containers
            .get(container)
            .ok_or_else(|| Error::InvalidContainer(container.clone()))
    }

    fn state_mut(&mut self, container: &Container) -> Result<&mut ContainerState, Error> {
        self.containers
            .get_mut(container)
            .ok_or_else(|| Error::InvalidContainer(container.clone()))
    }

    fn npk(&self, container: &Container) -> Result<&Npk, Error> {
        let state = self.state(container)?;
        let repository = self.repository(&state.repository)?;
        repository
            .get(container)
            .ok_or_else(|| Error::InvalidContainer(container.clone()))
    }

    fn manifest(&self, container: &Container) -> Result<&Manifest, Error> {
        let npk = self.npk(container)?;
        Ok(npk.manifest())
    }

    fn repository(&self, repository: &str) -> Result<&Repository, Error> {
        self.repositories
            .get(repository)
            .ok_or_else(|| Error::InvalidRepository(repository.into()))
    }
}

#[test]
#[allow(clippy::unwrap_used)]
fn find_newest_resource() {
    use std::str::FromStr;

    let old = Container::try_from("test:0.0.1").unwrap();
    let new = Container::try_from("test:0.0.2").unwrap();
    let other = Container::try_from("other:1.0.0").unwrap();
    let containers = [old, new.clone(), other];
    let resource = State::match_container(
        &Name::try_from("test").unwrap(),
        &VersionReq::from_str(">=0.0.2").unwrap(),
        &mut containers.iter(),
    );
    assert!(resource.is_some());
    assert_eq!(resource.unwrap(), &new);
}

#[test]
#[allow(clippy::unwrap_used)]
fn cannot_find_newer_resource() {
    use std::str::FromStr;

    let old = Container::try_from("test:0.0.1").unwrap();
    let new = Container::try_from("test:0.0.2").unwrap();
    let other = Container::try_from("other:1.0.0").unwrap();
    let containers = [old, new, other];
    let resource = State::match_container(
        &Name::try_from("test").unwrap(),
        &VersionReq::from_str(">=0.0.3").unwrap(),
        &mut containers.iter(),
    );
    assert!(resource.is_none());
}
