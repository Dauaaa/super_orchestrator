use std::{collections::HashMap, net::IpAddr};

use crate::{api_docker::SuperDockerfile, cli_docker::Dockerfile};

/// Wrapper struct for a simple image
#[derive(Debug, Clone)]
pub struct SuperImage(String);

impl SuperImage {
    /// Should use a prebuilt image
    pub fn new(image_id: String) -> Self {
        Self(image_id)
    }

    pub fn into_inner(self) -> String {
        self.0
    }

    /// Get the ID of the image as a `&str`
    pub fn get_image_id(&self) -> &str {
        &self.0
    }

    /// Uses `SuperDockerfile::new(Dockerfile::name_tag(self.get_image_id()),
    /// None)`
    pub fn to_docker_file(&self) -> SuperDockerfile {
        SuperDockerfile::new(Dockerfile::name_tag(self.get_image_id()), None)
    }
}

/// When using the docker entrypoint strategy, this specifies what domain the
/// binaries are under (which affects how the binary can be built and where it
/// ends up in the target folder).
#[derive(Debug, Clone, Copy)]
pub enum BootstrapOptions {
    /// If this is a normal binary
    Bin,
    /// If this is under the `example/` folder
    Example,
    /// If this is a test
    Test,
    /// If this is a benchmark
    Bench,
}

impl BootstrapOptions {
    pub fn to_flag(self) -> &'static str {
        match self {
            BootstrapOptions::Bin => "--bin",
            BootstrapOptions::Example => "--example",
            BootstrapOptions::Test => "--test",
            BootstrapOptions::Bench => "--bench",
        }
    }

    pub fn to_path_str(self) -> Option<&'static str> {
        match self {
            BootstrapOptions::Bin => None,
            BootstrapOptions::Example => Some("examples"),
            BootstrapOptions::Test => Some("tests"),
            BootstrapOptions::Bench => Some("benches"),
        }
    }
}

/// Define port mapping like for the argument `-p
/// <host_ip>:<host_port>:<container_port>/<protocol>`.
///
/// Usually, this shouldn't be used for integration testing because all
/// containers in the same network should already be accessible (and container
/// names are usually their hostname which can be used directly in URLs, see the
/// examples).
#[derive(Debug, Clone)]
pub struct PortBind {
    container_port: u16,
    host_port: Option<u16>,
    host_ip: Option<IpAddr>,
    protocol: PortBindProtocol,
}

/// Protocol of the container port binding
#[derive(Debug, Clone, Copy)]
pub enum PortBindProtocol {
    Tcp,
    Udp,
}

impl std::fmt::Display for PortBindProtocol {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}",
            if matches!(self, Self::Tcp) {
                "tcp"
            } else {
                "udp"
            }
        )
    }
}

impl PortBind {
    /// Results in the port mapping `<port>:<port>`, which is equivalent to
    /// `0.0.0.0:<port>:<port>/tcp`
    pub fn new(port: u16) -> Self {
        Self {
            container_port: port,
            host_port: Some(port),
            host_ip: None,
            protocol: PortBindProtocol::Tcp,
        }
    }

    /// Sets a different `host_port` in `<host_port>:<container_port>`
    pub fn with_host_port(mut self, host_port: u16) -> Self {
        self.host_port = Some(host_port);
        self
    }

    /// Sets a different host IP in `<host_ip>:<host_port>:<container_port>`
    pub fn with_host_ip(mut self, host_ip: IpAddr) -> Self {
        self.host_ip = Some(host_ip);
        self
    }

    /// Sets the protocol
    pub fn with_protocol(mut self, protocol: PortBindProtocol) -> Self {
        self.protocol = protocol;
        self
    }
}

impl From<u16> for PortBind {
    /// Calls `Self::new(port)`
    fn from(port: u16) -> Self {
        Self::new(port)
    }
}

#[allow(clippy::type_complexity)] // internal only
pub(crate) fn port_bindings_to_bollard_args(
    pbs: &[PortBind],
) -> (
    Option<HashMap<String, HashMap<(), ()>>>,
    Option<HashMap<String, Option<Vec<bollard::secret::PortBinding>>>>,
) {
    let mut port_map =
        HashMap::<String, Option<Vec<bollard::secret::PortBinding>>>::with_capacity(pbs.len());
    let mut open_ports = HashMap::<String, HashMap<(), ()>>::with_capacity(pbs.len());

    for pb in pbs {
        let key = format!("{}/{}", pb.container_port, pb.protocol);
        let entry = port_map.entry(key.clone()).or_insert_with(|| Some(vec![]));
        if let Some(entry) = entry {
            // always matches, just more ergonomic
            entry.push(bollard::secret::PortBinding {
                host_port: pb
                    .host_port
                    .or(Some(pb.container_port))
                    .as_ref()
                    .map(ToString::to_string),
                host_ip: pb.host_ip.as_ref().map(ToString::to_string),
            });
        }
        open_ports.entry(key).or_default();
    }

    (Some(open_ports), Some(port_map))
}
