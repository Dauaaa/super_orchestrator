use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use stacked_errors::{Result, StackableErr};
use tokio::{
    fs::{File, OpenOptions},
    io::{AsyncReadExt, AsyncWriteExt, BufReader},
};

use crate::{acquire_dir_path, acquire_file_path, close_file};

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct WriteOptions {
    /// creates file if nonexistent
    pub create: bool,
    /// append rather than truncate
    pub append: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum ReadOrWrite {
    Read,
    Write(WriteOptions),
}

/// A wrapper combining capabilities from `tokio::fs::{OpenOptions, File}` with
/// a lot of opinionated defaults and `close_file`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileOptions {
    /// What should be a path to a file
    pub path: PathBuf,
    /// `ReadOrWrite` options
    pub options: ReadOrWrite,
}

impl FileOptions {
    pub fn read(file_path: impl AsRef<Path>) -> Self {
        Self {
            path: file_path.as_ref().to_owned(),
            options: ReadOrWrite::Read,
        }
    }

    /// `FileOptions` for reading from `file_name` in `directory`
    pub fn read2(directory: impl AsRef<Path>, file_name: impl AsRef<Path>) -> Self {
        let mut path = directory.as_ref().to_owned();
        path.push(file_name.as_ref());
        Self {
            path,
            options: ReadOrWrite::Read,
        }
    }

    /// `FileOptions` for writing to `file_name`. Sets `create` to true and
    /// `append` to false by default.
    pub fn write(file_path: impl AsRef<Path>) -> Self {
        Self {
            path: file_path.as_ref().to_owned(),
            options: ReadOrWrite::Write(WriteOptions {
                create: true,
                append: false,
            }),
        }
    }

    /// `FileOptions` for writing to `file_name` in `directory`. Sets `create`
    /// to true and `append` to false by default.
    pub fn write2(directory: impl AsRef<Path>, file_name: impl AsRef<Path>) -> Self {
        let mut path = directory.as_ref().to_owned();
        path.push(file_name.as_ref());
        Self {
            path,
            options: ReadOrWrite::Write(WriteOptions {
                create: true,
                append: false,
            }),
        }
    }

    pub fn create(mut self, create: bool) -> Result<Self> {
        if let ReadOrWrite::Write(ref mut options) = self.options {
            options.create = create;
            Ok(self)
        } else {
            Err(Error::from(format!(
                "{self:?}.create() -> options are readonly"
            )))
        }
    }

    pub fn append(mut self, append: bool) -> Result<Self> {
        if let ReadOrWrite::Write(ref mut options) = self.options {
            options.append = append;
            Ok(self)
        } else {
            Err(Error::from(format!(
                "{self:?}.append() -> options are readonly"
            )))
        }
    }

    /// Checks only for existence of the directory and file (allowing the file
    /// to not exist if `create` is not true). Returns the combined path if
    /// `!create`, else returns the directory.
    pub async fn preacquire(&self) -> Result<PathBuf> {
        let dir = self
            .path
            .parent()
            .stack_err_locationless(|| "FileOptions::preacquire() -> empty path")?;
        let mut path = acquire_dir_path(dir).await.stack_err_locationless(|| {
            format!("{self:?}.preacquire() could not acquire directory")
        })?;
        // we do this always for normalization purposes
        let file_name = self.path.file_name().stack_err_locationless(|| {
            format!("{self:?}.precheck() could not acquire file name, was only a directory input?")
        })?;
        path.push(file_name);
        match self.options {
            ReadOrWrite::Read => (),
            ReadOrWrite::Write(WriteOptions { create, .. }) => {
                if create {
                    return Ok(path)
                }
            }
        }
        acquire_file_path(path).await.stack_err_locationless(|| {
            format!(
                "{self:?}.precheck() could not acquire path to combined directory and file name"
            )
        })
    }

    /// Acquires a `File`, first running [preacquire](FileOptions::preacquire)
    /// on `self` and then opening a file according to the `ReadOrWrite`
    /// options.
    pub async fn acquire_file(&self) -> Result<File> {
        let path = self
            .preacquire()
            .await
            .stack_err_locationless(|| "FileOptions::acquire_file()")?;
        Ok(match self.options {
            ReadOrWrite::Read => OpenOptions::new()
                .read(true)
                .open(path)
                .await
                .stack_err_locationless(|| format!("{self:?}.acquire_file()"))?,
            ReadOrWrite::Write(WriteOptions { create, append }) => {
                if create {
                    OpenOptions::new()
                        .write(true)
                        .create(true)
                        .truncate(!append)
                        .append(append)
                        .open(path)
                        .await
                        .stack_err_locationless(|| format!("{self:?}.acquire_file()"))?
                } else {
                    OpenOptions::new()
                        .write(true)
                        .truncate(!append)
                        .append(append)
                        .open(path)
                        .await
                        .stack_err_locationless(|| format!("{self:?}.acquire_file()"))?
                }
            }
        })
    }

    /// Reads a file at `file_path` to a string, returning an error if acquiring
    /// the file fails or if the data is not UTF-8
    pub async fn read_to_string(file_path: impl AsRef<Path>) -> Result<String> {
        let mut file = Self::read(file_path)
            .acquire_file()
            .await
            .stack_err_locationless(|| "FileOptions::read_to_string")?;
        let mut s = String::new();
        file.read_to_string(&mut s)
            .await
            .stack_err_locationless(|| "FileOptions::read_to_string")?;
        Ok(s)
    }

    /// Reads a file at `file_name` in `directory` to a string, returning an
    /// error if acquiring the file fails or if the data is not UTF-8
    pub async fn read2_to_string(
        directory: impl AsRef<Path>,
        file_name: impl AsRef<Path>,
    ) -> Result<String> {
        let mut file = Self::read2(directory, file_name)
            .acquire_file()
            .await
            .stack_err_locationless(|| "FileOptions::read2_to_string")?;
        let mut s = String::new();
        file.read_to_string(&mut s)
            .await
            .stack_err_locationless(|| "FileOptions::read2_to_string")?;
        Ok(s)
    }

    /// Writes `s` to a file at `file_path`, returning an error if acquiring the
    /// file fails or if there is some filesystem error. Uses the
    /// [FileOptions::write] defaults.
    pub async fn write_str(file_path: impl AsRef<Path>, s: &str) -> Result<()> {
        let mut file = Self::write(file_path)
            .acquire_file()
            .await
            .stack_err_locationless(|| "FileOptions::write_str")?;
        file.write_all(s.as_bytes())
            .await
            .stack_err_locationless(|| "FileOptions::write_str")?;
        close_file(file).await.stack_err_locationless(|| {
            "FileOptions::write_str -> unexpected error when closing file"
        })?;
        Ok(())
    }

    /// Writes `s` to `file_name` in `directory`, returning an error if
    /// acquiring the file fails or if there is some filesystem error. Uses the
    /// [FileOptions::write2] defaults.
    pub async fn write2_str(
        directory: impl AsRef<Path>,
        file_name: impl AsRef<Path>,
        s: &str,
    ) -> Result<()> {
        let mut file = Self::write2(directory, file_name)
            .acquire_file()
            .await
            .stack_err_locationless(|| "FileOptions::write2_str")?;
        file.write_all(s.as_bytes())
            .await
            .stack_err_locationless(|| "FileOptions::write2_str")?;
        close_file(file).await.stack_err_locationless(|| {
            "FileOptions::write2_str -> unexpected error when closing file"
        })?;
        Ok(())
    }

    /// Copies bytes from the source to destination files. Does not do any
    /// permissions copying unlike `tokio::fs::copy`.
    pub async fn copy(
        src_file_path: impl AsRef<Path>,
        dst_file_path: impl AsRef<Path>,
    ) -> Result<()> {
        let src_file_path = src_file_path.as_ref();
        let dst_file_path = dst_file_path.as_ref();
        let src = Self::read(src_file_path)
            .acquire_file()
            .await
            .stack_err_locationless(|| {
                format!(
                    "FileOptions::copy(src_file_path: {src_file_path:?}, dst_file_path: \
                     {dst_file_path:?}) when opening source"
                )
            })?;
        let mut dst = Self::write(dst_file_path)
            .acquire_file()
            .await
            .stack_err_locationless(|| {
                format!(
                    "FileOptions::copy(src_file_path: {src_file_path:?}, dst_file_path: \
                     {dst_file_path:?}) when opening destination"
                )
            })?;
        tokio::io::copy_buf(&mut BufReader::new(src), &mut dst)
            .await
            .stack_err_locationless(|| {
                format!(
                    "FileOptions::copy(src_file_path: {src_file_path:?}, dst_file_path: \
                     {dst_file_path:?}) when copying"
                )
            })?;
        Ok(())
    }
}
