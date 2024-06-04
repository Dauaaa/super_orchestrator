use std::{
    collections::{btree_map::Entry, BTreeMap, BTreeSet},
    mem,
    net::IpAddr,
    sync::atomic::Ordering,
    time::Duration,
};

use stacked_errors::{Error, Result, StackableErr};
use tokio::time::{sleep, Instant};
use tracing::{info, warn};
use uuid::Uuid;

use crate::{
    acquire_file_path,
    docker::{Container, Dockerfile},
    docker_helpers::wait_get_ip_addr,
    Command, CommandResult, CommandRunner, FileOptions, CTRLC_ISSUED,
};

#[derive(Debug, Default)]
#[allow(clippy::large_enum_variant)]
enum RunState {
    #[default]
    PreActive,
    Active(CommandRunner),
    PostActive(CommandResult),
}

#[derive(Debug)]
struct ContainerState {
    container: Container,
    run_state: RunState,
    // NOTE: logically, only the `Active` state should have actual containers that should be
    // removed before program exit, but in the run function there is a loop that first creates all
    // containers before starting them. If an error occurs in between, the created containers with
    // associated IDs need to be removed. The drop and terminate functions only deals with this
    // variable and assume that panicking is happening or the state is cleaned up before giving
    // back to a user.
    active_container_id: Option<String>,
    // this variable should be per-ContainerState and not on a higher or lower level
    already_tried_drop: bool,
}

impl Drop for ContainerState {
    fn drop(&mut self) {
        if self.already_tried_drop {
            // avoid recursive panics if something goes wrong in the `Command`
            return
        }
        self.already_tried_drop = true;
        if let Some(id) = self.active_container_id.take() {
            let _ = std::process::Command::new("docker")
                .arg("rm")
                .arg("-f")
                .arg(id)
                .output();
        }
    }
}

impl ContainerState {
    pub async fn terminate(&mut self) {
        if let Some(id) = self.active_container_id.take() {
            let _ = Command::new("docker rm -f")
                .arg(id)
                .run_to_completion()
                .await;
        }
        let state = mem::take(&mut self.run_state);
        match state {
            RunState::PreActive => (),
            RunState::Active(mut runner) => {
                let _ = runner.terminate().await;
                if let Some(result) = runner.take_command_result() {
                    self.run_state = RunState::PostActive(result);
                }
            }
            RunState::PostActive(_) => (),
        }
    }

    pub fn new(container: Container) -> Self {
        Self {
            container,
            run_state: RunState::PreActive,
            active_container_id: None,
            already_tried_drop: false,
        }
    }

    pub fn container(&self) -> &Container {
        &self.container
    }

    pub fn container_mut(&mut self) -> &mut Container {
        &mut self.container
    }

    pub fn is_active(&self) -> bool {
        matches!(self.run_state, RunState::Active(_))
    }
}

///
/// # Note
///
/// If a CTRL-C/sigterm signal is sent while containers are running, and
/// [ctrlc_init](crate::ctrlc_init) or some other handler has not been set up,
/// the containers may continue to run in the background and will have to be
/// manually stopped. If the handlers are set, then one of the runners will
/// trigger an error or a check for `CTRLC_ISSUED` will terminate all.
#[derive(Debug)]
pub struct ContainerNetwork {
    uuid: Uuid,
    network_name: String,
    /// FIXME in init args
    pub network_args: Vec<String>,
    set: BTreeMap<String, ContainerState>,
    dockerfile_write_dir: Option<String>,
    log_dir: String,
    network_active: bool,
}

impl Drop for ContainerNetwork {
    fn drop(&mut self) {
        // here we are only concerned with logically active containers in a non
        // panicking situation, the `Drop` impl on each `ContainerState` handles the
        // rest if necessary
        for state in self.set.values() {
            // we purposely order in this way to avoid calling `panicking` in the
            // normal case
            if state.is_active() && (!std::thread::panicking()) {
                warn!(
                    "A `ContainerNetwork` was dropped without all active containers being \
                     properly terminated"
                )
            }
        }
    }
}

impl ContainerNetwork {
    /// Creates a new `ContainerNetwork`.
    ///
    /// This function generates a UUID used for enabling multiple
    /// `ContainerNetwork`s with the same names and ids to run simultaneously.
    /// The uuid is appended to network names, container names, and hostnames.
    /// Arguments involving container names automatically append the uuid.
    ///
    /// `network_name` sets the name of the docker network that containers will
    /// be attached to, `containers` is the set of containers that can be
    /// referred to later by name, `dockerfile_write_dir` is the directory in
    /// which "__tmp.dockerfile" can be written if `Dockerfile::Contents` is
    /// used, `is_not_internal` turns off `--internal`, and `log_dir` is where
    /// ".log" log files will be written.
    ///
    /// Note: if `Dockerfile::Contents` is used, and if it uses resources like
    /// `COPY --from [resource]`, then the resource needs to be in
    /// `dockerfile_write_dir` because of restrictions that Docker makes.
    ///
    /// The standard layout is to have a "./logs" directory for the log files,
    /// "./dockerfiles" for the write directory, and
    /// "./dockerfiles/dockerfile_resources" for resources used by the
    /// dockerfiles.
    ///
    /// # Errors
    ///
    /// Can return an error if there are containers with duplicate names, or a
    /// container is built with `Dockerfile::Content` but no
    /// `dockerfile_write_dir` is specified.
    pub fn new(network_name: &str, dockerfile_write_dir: Option<&str>, log_dir: &str) -> Self {
        Self {
            uuid: Uuid::new_v4(),
            network_name: network_name.to_owned(),
            network_args: vec![],
            set: BTreeMap::new(),
            dockerfile_write_dir: dockerfile_write_dir.map(|s| s.to_owned()),
            log_dir: log_dir.to_owned(),
            network_active: false,
        }
    }

    /// Adds arguments to be passed to `docker network create` (which will be
    /// run once any container is started)
    pub fn add_network_args<I, S>(&mut self, network_args: I) -> &mut Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        self.network_args
            .extend(network_args.into_iter().map(|s| s.as_ref().to_owned()));
        self
    }

    /// Returns the common UUID
    pub fn uuid(&self) -> Uuid {
        self.uuid
    }

    /// Returns the common UUID as a string
    pub fn uuid_as_string(&self) -> String {
        self.uuid.to_string()
    }

    /// Returns the full network name
    pub fn network_name(&self) -> &str {
        &self.network_name
    }

    /// Adds the container to the inactive set
    pub fn add_container(&mut self, container: Container) -> Result<&mut Self> {
        if self.dockerfile_write_dir.is_none()
            && matches!(container.dockerfile, Dockerfile::Contents(_))
        {
            return Err(Error::from_kind_locationless(
                "ContainerNetwork::new() a container is built with `Dockerfile::Contents`, but \
                 `dockerfile_write_dir` is unset",
            ))
        }
        match self.set.entry(container.name.clone()) {
            Entry::Vacant(v) => {
                v.insert(ContainerState::new(container));
            }
            Entry::Occupied(_) => {
                return Err(Error::from_kind_locationless(format!(
                    "ContainerNetwork::new() two containers were supplied with the same name \
                     \"{}\"",
                    container.name
                )))
            }
        }
        Ok(self)
    }

    /// Adds the volumes to every container currently in the network
    pub fn add_common_volumes<I, K, V>(&mut self, volumes: I) -> &mut Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<str>,
        V: AsRef<str>,
    {
        let volumes: Vec<(String, String)> = volumes
            .into_iter()
            .map(|x| (x.0.as_ref().to_string(), x.1.as_ref().to_string()))
            .collect();
        for state in self.set.values_mut() {
            state
                .container_mut()
                .volumes
                .extend(volumes.iter().cloned());
        }
        self
    }

    /// Adds the arguments to every container currently in the network
    pub fn add_common_entrypoint_args<I, S>(&mut self, args: I) -> &mut Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let args: Vec<String> = args.into_iter().map(|s| s.as_ref().to_string()).collect();
        for state in self.set.values_mut() {
            state
                .container_mut()
                .entrypoint_args
                .extend(args.iter().cloned())
        }
        self
    }

    /// Get a map of active container names to ids
    pub fn get_active_container_ids(&self) -> BTreeMap<String, String> {
        let mut v = BTreeMap::new();
        for (name, state) in &self.set {
            if state.is_active() {
                v.insert(name.to_string(), state.active_container_id.clone().unwrap());
            }
        }
        v
    }

    /// Get the names of all active containers
    pub fn active_names(&self) -> Vec<String> {
        let mut v = vec![];
        for (name, state) in &self.set {
            if state.is_active() {
                v.push(name.to_string());
            }
        }
        v
    }

    /// Get the names of all inactive containers (both containers that have not
    /// been run before, and containers that were terminated)
    pub fn inactive_names(&self) -> Vec<String> {
        let mut v = vec![];
        for (name, state) in &self.set {
            if !state.is_active() {
                v.push(name.to_string());
            }
        }
        v
    }

    /// Force removes any active containers found with the given names
    pub async fn terminate<I, S>(&mut self, names: I)
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        for name in names {
            let name = name.as_ref();
            if let Some(state) = self.set.get_mut(name) {
                state.terminate().await;
            }
        }
    }

    /// Force removes all active containers, but does not remove the docker
    /// network
    pub async fn terminate_containers(&mut self) {
        for state in self.set.values_mut() {
            state.terminate().await;
        }
    }

    /// Force removes all active containers and removes the network
    pub async fn terminate_all(&mut self) {
        self.terminate_containers().await;
        if self.network_active {
            let _ = Command::new("docker network rm")
                .arg(self.network_name())
                .run_to_completion()
                .await;
            self.network_active = false;
        }
    }

    /// Runs only the given `names`. This prechecks as much as it can before
    /// creating any containers. If an error happens in the middle of creating
    /// and starting the containers, any of the `names` that had been created
    /// are terminated before the function returns.
    pub async fn run<I, S>(&mut self, names: I, debug: bool) -> Result<()>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        // avoid polymorphizing such a large function
        self.run_internal(
            &names
                .into_iter()
                .map(|s| s.as_ref().to_owned())
                .collect::<Vec<String>>(),
            debug,
        )
        .await
    }

    async fn run_internal(&mut self, names: &[String], debug: bool) -> Result<()> {
        if debug {
            info!(
                "`ContainerNetwork::run(debug: true, ..)` with UUID {}",
                self.uuid_as_string()
            )
        }
        // relatively cheap preverification should be done first to prevent much more
        // expensive later undos
        let mut set = BTreeSet::new();
        for name in names {
            if set.contains(name) {
                return Err(Error::from_kind_locationless(format!(
                    "ContainerNetwork::run -> two containers were supplied with the same name \
                     \"{name}\""
                )))
            }
            if let Some(state) = self.set.get(name) {
                if state.is_active() {
                    return Err(Error::from_kind_locationless(format!(
                        "ContainerNetwork::run -> name \"{name}\" is already an active container"
                    )))
                }
            } else {
                return Err(Error::from_kind_locationless(format!(
                    "ContainerNetwork::run -> argument name \"{name}\" is not contained in the \
                     network"
                )))
            }
            set.insert(name.to_string());
        }

        let mut get_dockerfile_write_dir = false;
        for name in names {
            match self.set[name].container().dockerfile {
                Dockerfile::NameTag(_) => {}
                Dockerfile::Path(ref path) => {
                    acquire_file_path(path).await.stack_err_locationless(|| {
                        "ContainerNetwork::run -> could not acquire the path in a \
                         `Dockerfile::Path`"
                    })?;
                }
                Dockerfile::Contents(_) => get_dockerfile_write_dir = true,
            }
        }
        let mut dockerfile_write_file = None;
        if get_dockerfile_write_dir {
            let file_options = FileOptions::write2(
                self.dockerfile_write_dir.as_ref().unwrap(),
                "__tmp.dockerfile",
            );
            let path = file_options.preacquire().await.stack_err_locationless(|| {
                "ContainerNetwork::run -> could not acquire the `dockerfile_write_dir`"
            })?;
            dockerfile_write_file = Some(path.to_str().unwrap().to_owned());
        }

        let log_file = FileOptions::write2(
            &self.log_dir,
            format!("container_network_{}.log", self.network_name()),
        );
        log_file.preacquire().await.stack_err_locationless(|| {
            "ContainerNetwork::run -> could not acquire logs directory"
        })?;

        if !self.network_active {
            // remove old network if it exists (there is no option to ignore nonexistent
            // networks, drop exit status errors and let the creation command handle any
            // higher order errors)
            /*let _ = Command::new("docker network rm", &[&self.network_name_with_uuid()])
            .debug(false)
            .stdout_log(&debug_log)
            .stderr_log(&debug_log)
            .run_to_completion()
            .await;*/
            let comres = Command::new("docker network create --internal")
                .args(self.network_args.iter())
                .arg(self.network_name())
                .log(Some(&log_file))
                .run_to_completion()
                .await
                .stack_err_locationless(|| {
                    "ContainerNetwork::run -> when running network creation command"
                })?;
            // TODO we can get the network id
            comres
                .assert_success()
                .stack_err_locationless(|| "ContainerNetwork::run -> failed to create network")?;
            self.network_active = true;
        }

        // run all the creation first so that everything is pulled and prepared
        let network_name = &self.network_name;
        for (i, name) in names.iter().enumerate() {
            let state = self.set.get_mut(name).unwrap();

            match state
                .container()
                .create(network_name, &dockerfile_write_file, debug, Some(&log_file))
                .await
                .stack_err_locationless(|| {
                    format!("ContainerNetwork::run when creating the container for name \"{name}\"")
                }) {
                Ok(docker_id) => {
                    state.active_container_id = Some(docker_id);
                }
                Err(e) => {
                    // need to fix all the containers in the intermediate state
                    for name in &names[..i] {
                        self.set.get_mut(name).unwrap().terminate().await;
                    }
                    return Err(e)
                }
            }
        }

        // start containers
        for name in names {
            let state = self.set.get_mut(name).unwrap();
            let stdout_log = state.container.stdout_log.clone().unwrap_or_else(|| {
                FileOptions::write2(&self.log_dir, format!("container_{}_stdout.log", name))
            });
            let stderr_log = state.container.stderr_log.clone().unwrap_or_else(|| {
                FileOptions::write2(&self.log_dir, format!("container_{}_stderr.log", name))
            });
            match state
                .container()
                .start(
                    state.active_container_id.as_ref().unwrap(),
                    Some(&stdout_log),
                    Some(&stderr_log),
                )
                .await
                .stack_err_locationless(|| {
                    format!("ContainerNetwork::run when starting the container for name \"{name}\"")
                }) {
                Ok(runner) => {
                    state.run_state = RunState::Active(runner);
                }
                Err(e) => {
                    for name in names.iter() {
                        self.set.get_mut(name).unwrap().terminate().await;
                    }
                    return Err(e)
                }
            }
        }

        Ok(())
    }

    pub async fn run_all(&mut self, debug: bool) -> Result<()> {
        let names = self.inactive_names();
        let mut v: Vec<&str> = vec![];
        for name in &names {
            v.push(name);
        }
        self.run(&v, debug)
            .await
            .stack_err_locationless(|| "ContainerNetwork::run_all")
    }

    /// Looks through the results and includes the last "Error: Error { stack:
    /// [" or " panicked at " parts of stdouts. Omits stacks that have
    /// "ProbablyNotRootCauseError".
    fn error_compilation(&mut self) -> Result<()> {
        let not_root_cause = "ProbablyNotRootCauseError";
        let error_stack = "Error { stack: [";
        let panicked_at = " panicked at ";
        let mut res = Error::empty();
        for (name, state) in self.set.iter() {
            // TODO not sure if we should have a generation counter to track different sets
            // of `wait_*` failures, for now we will just always use all unsuccessful
            // `PostActive` containers
            if let RunState::PostActive(ref comres) = state.run_state {
                if !comres.successful() {
                    let mut encountered = false;
                    let stdout = comres.stdout_as_utf8_lossy();
                    if let Some(start) = stdout.rfind(error_stack) {
                        if !stdout.contains(not_root_cause) {
                            encountered = true;
                            res = res.add_kind_locationless(format!(
                                "Error stack from container \"{name}\":\n{}\n",
                                &stdout[start..]
                            ));
                        }
                    }

                    if let Some(i) = stdout.rfind(panicked_at) {
                        if let Some(i) = stdout[0..i].rfind("thread") {
                            encountered = true;
                            res = res.add_kind_locationless(format!(
                                "Panic message from container \"{name}\":\n{}\n",
                                &stdout[i..]
                            ));
                        }
                    }

                    if (!encountered) && (!comres.successful_or_terminated()) {
                        res = res.add_kind_locationless(format!(
                            "Error: Container \"{name}\" was unsuccessful but does not seem to \
                             have an error stack or panic message\n"
                        ));
                    }
                }
            }
        }
        Err(res)
    }

    /// If `terminate_on_failure`, then if there is a timeout or any
    /// container from `names` has an error, then the whole network will be
    /// terminated.
    ///
    /// Note that if a CTRL-C/sigterm signal is sent and
    /// [ctrlc_init](crate::ctrlc_init) has been run, then either terminating
    /// runners or an internal [CTRLC_ISSUED] check will trigger
    /// [terminate_all](ContainerNetwork::terminate_all). Otherwise,
    /// containers may continue to run in the background.
    ///
    /// If called with `Duration::ZERO`, this will always complete successfully
    /// if all containers were terminated before this call.
    pub async fn wait_with_timeout(
        &mut self,
        names: &mut Vec<String>,
        terminate_on_failure: bool,
        duration: Duration,
    ) -> Result<()> {
        for name in names.iter() {
            if let Some(state) = self.set.get(name) {
                if !state.is_active() {
                    return Err(Error::from(format!(
                        "ContainerNetwork::wait_with_timeout -> name \"{name}\" is already \
                         inactive"
                    )));
                }
            } else {
                return Err(Error::from(format!(
                    "ContainerNetwork::wait_with_timeout -> name \"{name}\" not found in the \
                     network"
                )));
            }
        }
        let start = Instant::now();
        let mut skip_fail = true;
        // we will check in a loop so that if a container has failed in the meantime, we
        // terminate all
        let mut i = 0;
        loop {
            if CTRLC_ISSUED.load(Ordering::SeqCst) {
                // most of the time, a terminating runner will cause a stop before this, but
                // still check
                self.terminate_all().await;
                return Err(Error::from_kind_locationless(
                    "ContainerNetwork::wait_with_timeout terminating because of `CTRLC_ISSUED`",
                ))
            }
            if names.is_empty() {
                break
            }
            if i >= names.len() {
                i = 0;
                let current = Instant::now();
                let elapsed = current.saturating_duration_since(start);
                if elapsed > duration {
                    if skip_fail {
                        // give one extra round, this is strong enough for the `Duration::ZERO`
                        // guarantee
                        skip_fail = false;
                    } else {
                        if terminate_on_failure {
                            // we put in some extra delay so that the log file writers have some
                            // extra time to finish
                            sleep(Duration::from_millis(300)).await;
                            self.terminate_all().await;
                        }
                        return Err(Error::timeout().add_kind_locationless(format!(
                            "ContainerNetwork::wait_with_timeout timeout waiting for container \
                             names {names:?} to complete"
                        )))
                    }
                } else {
                    sleep(Duration::from_millis(256)).await;
                }
            }

            let name = &names[i];
            let state = self.set.get_mut(name).unwrap();
            if let RunState::Active(ref mut runner) = state.run_state {
                match runner.wait_with_timeout(Duration::ZERO).await {
                    Ok(()) => {
                        let comres = runner.take_command_result().unwrap();
                        let err = !comres.successful();
                        if terminate_on_failure && err {
                            // give some time for other containers to react, they will be sending
                            // ProbablyNotRootCause errors and other things
                            sleep(Duration::from_millis(300)).await;
                            self.terminate_all().await;
                            return self.error_compilation().stack_err_locationless(|| {
                                "ContainerNetwork::wait_with_timeout error compilation (check logs \
                                 for more):\n"
                            })
                        }
                        state.run_state = RunState::PostActive(comres);
                        names.remove(i);
                    }
                    Err(e) => {
                        if !e.is_timeout() {
                            let _ = runner.terminate().await;
                            if terminate_on_failure {
                                // give some time like in the earlier case
                                sleep(Duration::from_millis(300)).await;
                                self.terminate_all().await;
                            }
                            return self.error_compilation().stack_err_locationless(|| {
                                "ContainerNetwork::wait_with_timeout error compilation (check logs \
                                 for more):\n"
                            })
                        }
                        i += 1;
                    }
                }
            }
        }
        Ok(())
    }

    /// Runs [ContainerNetwork::wait_with_timeout] on all active containers.
    pub async fn wait_with_timeout_all(
        &mut self,
        terminate_on_failure: bool,
        duration: Duration,
    ) -> Result<()> {
        let mut names = self.active_names();
        self.wait_with_timeout(&mut names, terminate_on_failure, duration)
            .await
    }

    /// Gets the IP address of an active container. There is a delay between a
    /// container starting and an IP address being assigned, which is why this
    /// has a retry mechanism.
    pub async fn wait_get_ip_addr(
        &self,
        num_retries: u64,
        delay: Duration,
        name: &str,
    ) -> Result<IpAddr> {
        let state = self.set.get(name).stack_err_locationless(|| {
            format!(
                "ContainerNetwork::get_ip_addr(num_retries: {num_retries}, delay: {delay:?}, \
                 name: {name}) -> could not find name in container network"
            )
        })?;
        let id = state
            .active_container_id
            .as_ref()
            .stack_err_locationless(|| {
                format!(
                    "ContainerNetwork::get_ip_addr(num_retries: {num_retries}, delay: {delay:?}, \
                     name: {name}) -> found container, but it was not active"
                )
            })?;
        let ip = wait_get_ip_addr(num_retries, delay, id)
            .await
            .stack_err_locationless(|| {
                format!(
                    "ContainerNetwork::get_ip_addr(num_retries: {num_retries}, delay: {delay:?}, \
                     name: {name})"
                )
            })?;
        Ok(ip)
    }
}
