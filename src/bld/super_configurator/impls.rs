use std::{
    io::{Seek, Write},
    path::PathBuf,
    sync::Arc,
};

use bollard::image::BuildImageOptions;
use bollard_wrappers::SuperBuildImageOptionsWrapper;
use futures::TryStreamExt;
use stacked_errors::{Result, StackableErr};

use super::*;
use crate::docker_container::Dockerfile;

impl SuperDockerFile {
    pub fn new(base: Dockerfile) -> Self {
        Self {
            base,
            content_extend: Vec::new(),
            build_opts: SuperBuildImageOptionsWrapper::default(),
            tarball: Default::default(),
            build_path: None,
        }
    }

    /// The build path is the last argument in a docker build command.
    ///
    /// `docker build [OPTS] <build_path>`
    ///
    /// If you're copying relative files, they will be copied relative to
    /// the current build_path which resolves to cwd if not specified
    /// (absolute paths don't apply). So specify this before copying or defining
    /// entrypoint to have paths resolved according to build path.
    pub fn with_build_path(mut self, build_path: PathBuf) -> Self {
        self.build_path = Some(build_path);
        self
    }

    pub fn with_build_opts(mut self, build_opts: SuperBuildImageOptionsWrapper) -> Self {
        self.build_opts = build_opts;
        self
    }

    /// Add instructions to the underlying docker file
    pub fn appending_dockerfile_instructions(
        mut self,
        v: impl IntoIterator<Item = impl AsRef<str>>,
    ) -> Self {
        self.appending_dockerfile_lines_mut(v);
        self
    }

    pub fn appending_dockerfile_lines_mut(&mut self, v: impl IntoIterator<Item = impl AsRef<str>>) {
        for s in v {
            // Extra \n is ok! :)
            self.content_extend.push(b'\n');

            self.content_extend.extend(s.as_ref().as_bytes());
        }
    }

    /// Add a `COPY` instruction to docker file, when called this will copy the
    /// file into memory so as long as it returns Ok(_), TOCTOU won't be a
    /// problem.
    ///
    /// The argument receives an iterator with items as (from, to). If to is
    /// None, it'll be equivalent to (from, from).
    pub async fn copying_from_paths<'a>(
        mut self,
        v: impl IntoIterator<Item = (impl Into<String>, Option<impl Into<String>>)>,
    ) -> Result<Self> {
        let build_path = self.build_path.clone();

        tracing::debug!("Current tarball paths: {:?}", self.tarball);

        let this = Arc::new(std::sync::Mutex::new(self));
        let mut futs = v
            .into_iter()
            .map(|(from, to)| {
                let this = this.clone();
                let (from, to) = resolve_from_to(from, to, build_path.clone());

                tokio::task::spawn_blocking(move || {
                    let file = &mut std::fs::File::open(&from).stack()?;

                    let mut this_ref = this.lock().unwrap();

                    this_ref.appending_dockerfile_lines_mut([format!("COPY {from} {to}")]);
                    this_ref.tarball.append_file(from, file).stack()?;

                    Ok(()) as Result<_>
                })
            })
            .collect::<Vec<_>>();

        while !futs.is_empty() {
            let (res, _, rest) = futures::future::select_all(futs).await;
            res.stack()??;
            futs = rest;
        }

        self = Arc::try_unwrap(this).unwrap().into_inner().stack()?;

        tracing::debug!("New tarball paths: {:?}", self.tarball);

        Ok(self)
    }

    /// Add an `ENTRYPOINT` instruction and append its file to docker "build
    /// tarball".
    ///
    /// The entrypoint parameter is of the format (from, to).
    pub async fn with_entrypoint(
        mut self,
        entrypoint: (impl Into<String> + Clone, Option<impl Into<String> + Clone>),
        entrypoint_args: impl IntoIterator<Item = impl Into<String>>,
    ) -> Result<Self> {
        self = self
            .copying_from_paths([entrypoint.clone()])
            .await
            .stack()?;
        let (_, to) = resolve_from_to(entrypoint.0, entrypoint.1, self.build_path.clone());

        Ok(self.appending_dockerfile_instructions([format!(
            r#"ENTRYPOINT ["{to}", {}] "#,
            entrypoint_args
                .into_iter()
                .map(Into::into)
                .collect::<Vec<String>>()
                .join(" ")
        )]))
    }

    /// Creates a `HEALTHCHECK` instruction as specified in https://docs.docker.com/reference/dockerfile/#healthcheck
    ///
    /// Should only be useful for testing, since "production" health checks are
    /// usually defined in the provider's API.
    pub async fn with_healthcheck(
        mut self,
        opts: impl IntoIterator<Item = impl Into<String>>,
        command: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        self.content_extend.extend(
            format!(
                "HEALTHCHECK {} CMD {}",
                opts.into_iter()
                    .map(Into::into)
                    .collect::<Vec<String>>()
                    .join(" "),
                command
                    .into_iter()
                    .map(Into::into)
                    .collect::<Vec<String>>()
                    .join(" "),
            )
            .as_bytes(),
        );
        self
    }

    /// Make the current running binary the image's entrypoint, will call
    /// [SuperDockerFile::with_entrypoint]. If `to` is None, will create file as
    /// /tmp/bootstrapped-{uuid}
    ///
    /// This is useful for defining a complete test using a single rust file by
    /// traversing through different branches of the code using the
    /// entrypoint_args.
    pub async fn bootstrap(
        self,
        to: Option<String>,
        entrypoint_args: impl IntoIterator<Item = impl Into<String>>,
    ) -> Result<Self> {
        let bootstrap_path =
            to.unwrap_or_else(|| format!("/tmp/bootstrapped-{}", uuid::Uuid::new_v4()));

        self.with_entrypoint(
            (
                std::env::current_exe()
                    .stack()?
                    .as_os_str()
                    .to_str()
                    .unwrap(),
                Some(bootstrap_path),
            ),
            entrypoint_args,
        )
        .await
    }

    pub async fn build_image(self) -> Result<SuperImage> {
        let (build_opts, tarball) = super_docker_file_to_bollard_args(self).await.stack()?;

        let docker_instance = crate::bld::docker_socket::get_or_init_docker_instance()
            .await
            .stack()?;

        Ok(SuperImage(
            docker_instance
                .build_image(build_opts, None, tarball.map(Into::into))
                .inspect_ok(|msg| {
                    msg.stream
                        .as_ref()
                        .inspect(|x| tracing::debug!("{}", x.trim()));
                })
                .try_filter_map(|x| futures::future::ready(Ok(x.aux)))
                .try_collect::<Vec<_>>()
                .await
                .stack_err("try to build img")?
                .into_iter()
                .next()
                .and_then(|x| x.id)
                .stack_err("image built without id")?,
        ))
    }
}

fn resolve_from_to(
    from: impl Into<String>,
    to: Option<impl Into<String>>,
    build_path: Option<PathBuf>,
) -> (String, String) {
    let from: String = if let Some(ref build_path) = build_path {
        build_path
            .join(from.into() as String)
            .as_os_str()
            .to_str()
            .unwrap()
            .to_string()
    } else {
        from.into()
    };
    let to = to.map(|to| to.into()).unwrap_or_else(|| from.clone());

    (from, to)
}

async fn super_docker_file_to_bollard_args(
    mut sdf: SuperDockerFile,
) -> Result<(BuildImageOptions<String>, Option<Vec<u8>>)> {
    let docker_file = &mut create_docker_file_returning_file_handle(&sdf)
        .await
        .stack()?;

    // use random name for dockerfile to avoid collision with anything
    let docker_file_random_name = format!("/{}.dockerfile", uuid::Uuid::new_v4());

    sdf.tarball
        .append_file(docker_file_random_name.clone(), docker_file)
        .stack()?;

    let opts = BuildImageOptions {
        dockerfile: docker_file_random_name,
        t: sdf.build_opts.t,
        extrahosts: sdf.build_opts.extrahosts,
        q: sdf.build_opts.q,
        nocache: sdf.build_opts.nocache,
        cachefrom: sdf.build_opts.cachefrom,
        pull: sdf.build_opts.pull,
        rm: sdf.build_opts.rm,
        forcerm: sdf.build_opts.forcerm,
        memory: sdf.build_opts.memory,
        memswap: sdf.build_opts.memswap,
        cpushares: sdf.build_opts.cpushares,
        cpusetcpus: sdf.build_opts.cpusetcpus,
        cpuperiod: sdf.build_opts.cpuperiod,
        cpuquota: sdf.build_opts.cpuquota,
        buildargs: sdf.build_opts.buildargs,
        shmsize: sdf.build_opts.shmsize,
        squash: sdf.build_opts.squash,
        labels: sdf.build_opts.labels,
        networkmode: sdf.build_opts.networkmode,
        platform: sdf.build_opts.platform,
        target: sdf.build_opts.target,
        version: sdf.build_opts.version,
        ..Default::default()
    };

    let tarball = sdf.tarball.into_tarball().stack()?.into();

    Ok((opts, Some(tarball)))
}

async fn create_docker_file_returning_file_handle(sdf: &SuperDockerFile) -> Result<std::fs::File> {
    let mut temp_file_name = std::env::temp_dir();
    temp_file_name.push(uuid::Uuid::new_v4().to_string());

    let file_contents = match &sdf.base {
        Dockerfile::NameTag(nt) => Ok(format!("FROM {nt}").into_bytes()),
        Dockerfile::Path(path) => std::fs::read(path).stack(),
        Dockerfile::Contents(content) => Ok(content.clone().into_bytes()),
    }
    .map(|mut df| {
        df.extend_from_slice(&sdf.content_extend);
        df
    })
    .stack()?;

    tokio::task::spawn_blocking(move || {
        let mut temp_file = std::fs::File::options()
            .truncate(true)
            .create(true)
            .write(true)
            .read(true)
            .open(&temp_file_name)
            .stack()?;

        temp_file.write_all(&file_contents).stack()?;

        temp_file.seek(std::io::SeekFrom::Start(0)).stack()?;

        Ok(temp_file)
    })
    .await
    .stack()?
}

impl SuperImage {
    pub fn get_image_id(&self) -> &str {
        &self.0
    }
}
