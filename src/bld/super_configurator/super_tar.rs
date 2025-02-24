use stacked_errors::{Result, StackableErr};

pub struct SuperTarballWrapper {
    tar: tar::Builder<Vec<u8>>,
    paths: Vec<String>,
}

impl Default for SuperTarballWrapper {
    fn default() -> Self {
        Self {
            tar: tar::Builder::new(Vec::new()),
            paths: Vec::new(),
        }
    }
}

impl std::fmt::Debug for SuperTarballWrapper {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "SuperTarballWrapper {{ {} }}",
            self.paths.clone().join("\n")
        )
    }
}

impl SuperTarballWrapper {
    pub fn append_file(&mut self, path: String, file: &mut std::fs::File) -> Result<()> {
        self.paths.push(path.clone());
        self.tar.append_file(path, file).stack()
    }

    pub fn into_tarball(self) -> Result<Vec<u8>> {
        self.tar.into_inner().stack()
    }
}
