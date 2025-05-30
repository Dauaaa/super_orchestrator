use std::{
    any::type_name,
    collections::HashSet,
    ffi::OsString,
    future::Future,
    path::{Path, PathBuf},
    str::FromStr,
    sync::{Arc, Mutex},
    time::Duration,
};

pub(crate) use color_cycle::next_terminal_color;
use stacked_errors::{bail_locationless, Result, StackableErr, TimeoutError};
use tokio::{
    fs::{read_dir, remove_file, File},
    io::AsyncWriteExt,
    time::sleep,
};
use tracing::warn;

use crate::{acquire_dir_path, Command};

/// A convenience wrapper around the functionality of [tokio::signal::ctrl_c]
pub struct CtrlCTask {
    cancel: tokio::task::AbortHandle,
    complete: Arc<Mutex<bool>>,
}

impl Drop for CtrlCTask {
    fn drop(&mut self) {
        self.cancel.abort();
    }
}

impl CtrlCTask {
    /// This spawns a task that sets a `Arc<Mutex<bool>>` to true when
    /// `tokio::signal::ctrl_c().await` completes. This task is cancelled when
    /// the struct is dropped.
    pub fn spawn() -> Self {
        let complete = Arc::new(Mutex::new(false));
        let complete1 = complete.clone();
        let handle = tokio::task::spawn(async move {
            // do not panic and do nothing on errors
            let res = tokio::signal::ctrl_c().await;
            match res {
                Ok(()) => {
                    *complete1.lock().unwrap() = true;
                }
                Err(e) => warn!(
                    "super_orchestrator CtrlCTask got an error from ctrl_c, doing nothing: {e:?}"
                ),
            }
        });
        CtrlCTask {
            cancel: handle.abort_handle(),
            complete,
        }
    }

    /// If the `ctrl_c` has been triggered
    pub fn is_complete(&self) -> bool {
        *self.complete.lock().unwrap()
    }
}

pub fn random_name(name: impl std::fmt::Display) -> String {
    // lazy programming at its finest
    format!("{name}-{}", &uuid::Uuid::new_v4().to_string()[..6])
}

/// Takes the hash of the type name of `T` and returns it. Has the
/// potential to change between compiler versions.
pub fn type_hash<T: ?Sized>() -> [u8; 16] {
    // we can't make this `const` currently because of `type_name`, however it
    // should compile down to the result in practice, at least on release mode

    // TODO `type_name` should be const soon
    use sha3::{Digest, Sha3_256};
    let name = type_name::<T>();
    let mut hasher = Sha3_256::new();
    hasher.update(name.as_bytes());
    let tmp: [u8; 32] = hasher.finalize().into();
    let mut res = [0u8; 16];
    res.copy_from_slice(&tmp[0..16]);
    res
}

/// Equivalent to calling
/// `Command::new(program_with_args[0]).args(program_with_args[1..])
/// .debug(true).run_to_completion().await?.assert_success()?;` and
/// returning the stdout as a `String`.
///
/// Returns an error if `program_with_args` is empty, there was a
/// `run_to_completion` error, the command return status was unsuccessful, or
/// the stdout was not utf-8.
pub async fn sh<I, S>(program_with_args: I) -> Result<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut command = None;
    for (i, part) in program_with_args.into_iter().enumerate() {
        if i == 0 {
            command = Some(Command::new(part.as_ref()));
        } else {
            command = Some(command.unwrap().arg(part.as_ref()));
        }
    }
    let comres = command
        .stack_err_locationless("super_orchestrator::sh was called with an empty iterator")?
        .debug(true)
        .run_to_completion()
        .await?;
    comres.assert_success()?;
    comres
        .stdout_as_utf8()
        .map(|s| s.to_owned())
        .stack_err_locationless("super_orchestrator::sh -> `Command` output was not UTF-8")
}

/// [sh] but without debug mode
pub async fn sh_no_debug<I, S>(program_with_args: I) -> Result<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut command = None;
    for (i, part) in program_with_args.into_iter().enumerate() {
        if i == 0 {
            command = Some(Command::new(part.as_ref()));
        } else {
            command = Some(command.unwrap().arg(part.as_ref()));
        }
    }
    let comres = command
        .stack_err_locationless("sh_no_debug was called with an empty iterator")?
        .run_to_completion()
        .await?;
    comres.assert_success()?;
    comres
        .stdout_as_utf8()
        .map(|s| s.to_owned())
        .stack_err_locationless("sh_no_debug -> `Command` output was not UTF-8")
}

/// Repeatedly polls `f` until it returns an `Ok` which is returned, or
/// `num_retries` is reached in which a timeout error is returned.
///
/// # Example
///
/// This is the definition of `wait_for_ok_lookup_host`
/// ```
/// use std::{net::SocketAddr, time::Duration};
///
/// use stacked_errors::{Error, Result, StackableErr};
/// use super_orchestrator::wait_for_ok;
/// use tokio::net::lookup_host;
///
/// pub async fn wait_for_ok_lookup_host(
///     num_retries: u64,
///     delay: Duration,
///     host: &str,
/// ) -> Result<SocketAddr> {
///     async fn f(host: &str) -> Result<SocketAddr> {
///         match lookup_host(host).await {
///             Ok(mut addrs) => {
///                 if let Some(addr) = addrs.next() {
///                     Ok(addr)
///                 } else {
///                     Err(Error::from_err("empty addrs"))
///                 }
///             }
///             Err(e) => Err(Error::from_err(e))
///                 .stack_err(format!("wait_for_ok_lookup_host(.., host: {host})")),
///         }
///     }
///     wait_for_ok(num_retries, delay, || f(host)).await
/// }
/// ```
pub async fn wait_for_ok<F: FnMut() -> Fut, Fut: Future<Output = Result<T>>, T>(
    num_retries: u64,
    delay: Duration,
    mut f: F,
) -> Result<T> {
    let mut i = num_retries;
    loop {
        match f().await {
            Ok(o) => return Ok(o),
            Err(e) => {
                if i == 0 {
                    return Err(e.add_err_locationless(TimeoutError {})).stack_err_locationless(
                        format!(
                            "wait_for_ok(num_retries: {num_retries}, delay: {delay:?}) timeout, \
                             last error stack was:"
                        ),
                    );
                }
                i -= 1;
            }
        }
        // for `num_retries` we have the check afterwards so that 0 retries can still
        // pass
        sleep(delay).await;
    }
}

/// This function makes sure changes are flushed and `sync_all` is called to
/// make sure the file has actually been completely written to the filesystem
/// and closed before the end of this function.
pub async fn close_file(mut file: File) -> Result<()> {
    file.flush().await.stack()?;
    file.sync_all().await.stack()?;
    Ok(())
}

#[allow(clippy::too_long_first_doc_paragraph)]
/// This is a guarded kind of removal that only removes all files in a directory
/// that match an element of `ends_with`. If the element starts with ".",
/// extensions are matched against, otherwise whole file names are matched
/// against. Only whole extension components are matched against.
///
/// # Example
///
/// ```no_run
/// use stacked_errors::{ensure, Result};
/// use super_orchestrator::{acquire_file_path, remove_files_in_dir, FileOptions};
/// async fn ex() -> Result<()> {
///     // note: in regular use you would use `.await.stack()?` on the ends
///     // to tell what lines are failing
///
///     // create some empty example files
///     FileOptions::write_str("./logs/binary", "").await?;
///     FileOptions::write_str("./logs/ex0.log", "").await?;
///     FileOptions::write_str("./logs/ex1.log", "").await?;
///     FileOptions::write_str("./logs/ex2.tar.gz", "").await?;
///     FileOptions::write_str("./logs/tar.gz", "").await?;
///
///     remove_files_in_dir("./logs", &["r.gz", ".r.gz"]).await?;
///     // check that files "ex2.tar.gz" and "tar.gz" were not removed
///     // even though "r.gz" is in their string suffixes, because it
///     // only matches against complete extension components.
///     acquire_file_path("./logs/ex2.tar.gz").await?;
///     acquire_file_path("./logs/tar.gz").await?;
///
///     remove_files_in_dir("./logs", &["binary", ".log"]).await?;
///     // check that only the "binary" and all ".log" files were removed
///     ensure!(acquire_file_path("./logs/binary").await.is_err());
///     ensure!(acquire_file_path("./logs/ex0.log").await.is_err());
///     ensure!(acquire_file_path("./logs/ex1.log").await.is_err());
///     acquire_file_path("./logs/ex2.tar.gz").await?;
///     acquire_file_path("./logs/tar.gz").await?;
///
///     remove_files_in_dir("./logs", &[".gz"]).await?;
///     // any thing ending with ".gz" should be gone
///     ensure!(acquire_file_path("./logs/ex2.tar.gz").await.is_err());
///     ensure!(acquire_file_path("./logs/tar.gz").await.is_err());
///
///     // recreate some files
///     FileOptions::write_str("./logs/ex2.tar.gz", "").await?;
///     FileOptions::write_str("./logs/ex3.tar.gz.other", "").await?;
///     FileOptions::write_str("./logs/tar.gz", "").await?;
///
///     remove_files_in_dir("./logs", &["tar.gz"]).await?;
///     // only the file is matched because the element did not begin with a "."
///     acquire_file_path("./logs/ex2.tar.gz").await?;
///     acquire_file_path("./logs/ex3.tar.gz.other").await?;
///     ensure!(acquire_file_path("./logs/tar.gz").await.is_err());
///
///     FileOptions::write_str("./logs/tar.gz", "").await?;
///
///     remove_files_in_dir("./logs", &[".tar.gz"]).await?;
///     // only a strict extension suffix is matched
///     ensure!(acquire_file_path("./logs/ex2.tar.gz").await.is_err());
///     acquire_file_path("./logs/ex3.tar.gz.other").await?;
///     acquire_file_path("./logs/tar.gz").await?;
///
///     FileOptions::write_str("./logs/ex2.tar.gz", "").await?;
///
///     remove_files_in_dir("./logs", &[".gz", ".other"]).await?;
///     ensure!(acquire_file_path("./logs/ex2.tar.gz").await.is_err());
///     ensure!(acquire_file_path("./logs/ex3.tar.gz.other").await.is_err());
///     ensure!(acquire_file_path("./logs/tar.gz").await.is_err());
///
///     Ok(())
/// }
/// ```
///
/// # Errors
///
/// - If any `ends_with` element has more than one component (e.x. if there are
///   any '/' or '\\')
///
/// - If `acquire_dir_path(dir)` fails
pub async fn remove_files_in_dir<I, S>(dir: impl AsRef<Path>, ends_with: I) -> Result<()>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut file_name_set: HashSet<OsString> = HashSet::new();
    let mut extension_set: HashSet<OsString> = HashSet::new();
    let ends_with: Vec<String> = ends_with
        .into_iter()
        .map(|s| s.as_ref().to_string())
        .collect();
    for (i, s) in ends_with.iter().enumerate() {
        let mut s = s.as_str();
        if s.is_empty() {
            bail_locationless!(
                "remove_files_in_dir(dir: {:?}, ends_with: {:?}) -> `ends_with` element {} is \
                 empty",
                dir.as_ref(),
                ends_with,
                i
            )
        }
        let is_extension = s.starts_with('.');
        if is_extension {
            s = &s[1..];
        }
        let path = PathBuf::from(s);
        let mut iter = path.components();
        let component = iter.next().stack_err_with_locationless(|| {
            format!(
                "remove_files_in_dir(dir: {:?}, ends_with: {:?}) -> `ends_with` element {} has no \
                 component",
                dir.as_ref(),
                ends_with,
                i
            )
        })?;
        if iter.next().is_some() {
            bail_locationless!(
                "remove_files_in_dir(dir: {:?}, ends_with: {:?}) -> `ends_with` element {} has \
                 more than one component",
                dir.as_ref(),
                ends_with,
                i
            )
        }
        if is_extension {
            extension_set.insert(component.as_os_str().to_owned());
        } else {
            file_name_set.insert(component.as_os_str().to_owned());
        }
    }

    let dir_path_buf = acquire_dir_path(dir.as_ref())
        .await
        .stack_err_with_locationless(|| {
            format!(
                "remove_files_in_dir(dir: {:?}, ends_with: {:?})",
                dir.as_ref(),
                ends_with
            )
        })?;
    // only in cases where we are certain that there can be no error will we
    // `unwrap`, we return this fallibly for other cases
    let unexpected_error = "remove_files_in_dir -> unexpected filesystem error";
    // TODO should we be doing folder locking or something?
    let mut iter = read_dir(dir_path_buf.clone())
        .await
        .stack_err(unexpected_error)?;
    loop {
        let entry = iter.next_entry().await.stack_err(unexpected_error)?;
        if let Some(entry) = entry {
            let file_type = entry.file_type().await.stack_err(unexpected_error)?;
            if file_type.is_file() {
                let file_only_path = PathBuf::from(entry.file_name());
                // check against the whole file name
                let mut rm_file = file_name_set.contains(file_only_path.as_os_str());
                if !rm_file {
                    // now check against suffixes
                    // the way we do this is check with every possible extension suffix
                    let mut subtracting = file_only_path.clone();
                    let mut suffix = OsString::new();
                    while let Some(extension) = subtracting.extension() {
                        let mut tmp = extension.to_owned();
                        tmp.push(&suffix);
                        suffix = tmp;

                        if extension_set.contains(&suffix) {
                            rm_file = true;
                            break;
                        }

                        // remove very last extension as we add on extensions fo `suffix
                        subtracting = PathBuf::from(subtracting.file_stem().unwrap().to_owned());

                        // prepare "." prefix
                        let mut tmp = OsString::from_str(".").unwrap();
                        tmp.push(&suffix);
                        suffix = tmp;
                    }
                }
                if rm_file {
                    let mut combined = dir_path_buf.clone();
                    combined.push(file_only_path);
                    remove_file(combined).await.stack_err(unexpected_error)?;
                }
            }
        } else {
            break;
        }
    }
    Ok(())
}

mod color_cycle {
    use std::sync::atomic::AtomicUsize;

    use owo_colors::{AnsiColors, AnsiColors::*};

    const COLOR_CYCLE: [AnsiColors; 8] = [
        White,
        Yellow,
        Green,
        Cyan,
        BrightBlack,
        Blue,
        BrightCyan,
        BrightGreen,
    ];

    static COLOR_NUM: AtomicUsize = AtomicUsize::new(0);

    pub(crate) fn next_terminal_color() -> AnsiColors {
        let inx = COLOR_NUM.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        COLOR_CYCLE[inx % COLOR_CYCLE.len()]
    }
}
