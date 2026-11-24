use std::{
    env,
    fs::{self, DirBuilder, File, OpenOptions},
    io::ErrorKind,
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};

pub const SESSION_FILE: &str = "account.session";

pub fn directory() -> Result<PathBuf> {
    let path = match env::var_os("TELEGRAM_READER_DIR") {
        Some(path) => PathBuf::from(path),
        None => {
            let base = env::var_os("XDG_DATA_HOME")
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
                .or_else(|| {
                    env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share"))
                })
                .context("Set TELEGRAM_READER_DIR to a private directory.")?;
            base.join("telegram-reader-mcp")
        }
    };
    if !path.is_absolute() {
        bail!("TELEGRAM_READER_DIR must be absolute.");
    }
    let mut options = DirBuilder::new();
    options.recursive(true).mode(0o700);
    options.create(&path)?;
    let meta = fs::symlink_metadata(&path)?;
    if !meta.is_dir() || !private(&meta) {
        bail!("The state directory must be a real directory owned by you with mode 0700.");
    }
    Ok(path)
}

pub fn private_file(path: &Path, create: bool) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(create)
        .create(create)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .map_err(|error| {
            if error.kind() == ErrorKind::NotFound && !create {
                anyhow::anyhow!(
                    "Missing local state file. Run login before enabling Telegram access."
                )
            } else {
                error.into()
            }
        })?;
    let meta = file.metadata()?;
    if !meta.is_file() || meta.nlink() != 1 || !private(&meta) {
        bail!("State files must be private regular files owned by you (mode 0600).");
    }
    Ok(file)
}

fn private(meta: &fs::Metadata) -> bool {
    // geteuid takes no arguments and reads process credentials.
    let owner = unsafe { libc::geteuid() };
    meta.uid() == owner && meta.mode() & 0o077 == 0
}
