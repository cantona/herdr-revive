use crate::model::{Config, MAX_BYTES, Snapshot};
use anyhow::{Context, Result, bail, ensure};
use fs2::FileExt;
use serde::{Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

pub fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

pub fn now_ms() -> Result<u64> {
    Ok(std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_millis()
        .try_into()?)
}

pub fn session_identity(socket: &Path) -> Result<(PathBuf, String)> {
    ensure!(socket.is_absolute(), "HERDR_SOCKET_PATH must be absolute");
    let name = socket.file_name().context("socket has no filename")?;
    let parent = socket
        .parent()
        .context("socket has no parent")?
        .canonicalize()
        .context("cannot resolve socket directory")?;
    let normalized = parent.join(name);
    let text = normalized
        .to_str()
        .context("non-UTF-8 socket paths are unsupported")?;
    Ok((
        normalized.clone(),
        // Keep the schema-1 namespace stable so a rename cannot lose boot claims.
        digest(format!("cantona.herde-revive\0session-v1\0{text}").as_bytes()),
    ))
}

pub fn private_dir(path: &Path) -> Result<()> {
    if !path.exists() {
        let parent = path.parent().context("state directory has no parent")?;
        if !parent.exists() {
            private_dir(parent)?;
        }
        let mut builder = fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        match builder.create(path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e.into()),
        }
        File::open(path)?.sync_all()?;
        File::open(parent)?.sync_all()?;
    }
    let meta = fs::symlink_metadata(path)?;
    ensure!(
        meta.is_dir() && !meta.file_type().is_symlink(),
        "state directory is not a real directory"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        ensure!(
            meta.uid() == nix::unistd::geteuid().as_raw(),
            "state directory has another owner"
        );
        if meta.permissions().mode() & 0o777 != 0o700 {
            fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
            File::open(path)?.sync_all()?;
        }
    }
    File::open(path.parent().context("state directory has no parent")?)?.sync_all()?;
    Ok(())
}

fn open_read(path: &Path) -> Result<File> {
    let mut opts = OpenOptions::new();
    opts.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK);
    }
    let file = opts.open(path).context("cannot open input file")?;
    ensure!(file.metadata()?.is_file(), "input is not a regular file");
    Ok(file)
}

pub fn read_bytes(path: &Path) -> Result<Vec<u8>> {
    let file = open_read(path)?;
    ensure!(
        file.metadata()?.len() <= MAX_BYTES as u64,
        "input exceeds size limit"
    );
    let mut bytes = Vec::with_capacity(8192);
    file.take(MAX_BYTES as u64 + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= MAX_BYTES, "input exceeds size limit");
    Ok(bytes)
}

pub fn read_json<T: DeserializeOwned>(path: &Path) -> Result<T> {
    serde_json::from_slice(&read_bytes(path)?)
        .map_err(|_| anyhow::anyhow!("invalid JSON or schema in state file"))
}

pub fn load_config(dir: &Path) -> Result<Config> {
    let path = dir.join("config.toml");
    let config = match fs::symlink_metadata(&path) {
        Ok(_) => {
            let bytes = read_bytes(&path)?;
            let text = std::str::from_utf8(&bytes).context("config must be UTF-8")?;
            toml::from_str(text)
                .map_err(|_| anyhow::anyhow!("invalid config.toml (unknown fields are rejected)"))?
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Config::default(),
        Err(e) => return Err(e.into()),
    };
    config.validate()?;
    Ok(config)
}

pub fn atomic_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(value)?;
    ensure!(bytes.len() <= MAX_BYTES, "output exceeds size limit");
    atomic_write(path, &bytes)
}

pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().context("output has no directory")?;
    if let Ok(meta) = fs::symlink_metadata(path) {
        ensure!(
            meta.is_file() && !meta.file_type().is_symlink(),
            "refusing to replace non-regular file"
        );
    }
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    temp.write_all(bytes)
        .context("temporary write failed; previous state preserved")?;
    temp.as_file()
        .sync_all()
        .context("temporary file sync failed; previous state preserved")?;
    temp.persist(path)
        .map_err(|e| e.error)
        .context("atomic replacement failed")?;
    File::open(parent)?
        .sync_all()
        .context("directory sync failed; replacement durability uncertain")?;
    Ok(())
}

pub struct Store {
    pub root: PathBuf,
    pub session: String,
    pub spaces: PathBuf,
}
pub struct Lock {
    _file: File,
}

impl Store {
    pub fn new(state: &Path, session: String) -> Result<Self> {
        private_dir(state)?;
        let root = state.join(&session);
        let spaces = state.join("spaces");
        private_dir(&spaces)?;
        private_dir(&root)?;
        for name in ["snapshots", "boots"] {
            private_dir(&root.join(name))?;
        }
        Ok(Self {
            root,
            session,
            spaces,
        })
    }
    pub fn try_lock(&self) -> Result<Option<Lock>> {
        self.lock_at(&self.root.join("operation.lock"))
    }
    pub fn try_space_lock(&self) -> Result<Option<Lock>> {
        self.lock_at(&self.spaces.join("library.lock"))
    }
    fn lock_at(&self, path: &Path) -> Result<Option<Lock>> {
        let mut opts = OpenOptions::new();
        opts.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600)
                .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK);
        }
        let file = opts.open(path)?;
        ensure!(file.metadata()?.is_file(), "lock is not a regular file");
        match file.try_lock_exclusive() {
            Ok(()) => Ok(Some(Lock { _file: file })),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
    pub fn snapshot_path(&self, name: Option<&str>) -> Result<PathBuf> {
        match name {
            Some(name) => {
                validate_name(name)?;
                Ok(self.spaces.join(format!("{name}.json")))
            }
            None => Ok(self.root.join("latest.json")),
        }
    }
    pub fn read_snapshot(&self, path: &Path) -> Result<Snapshot> {
        let snapshot: Snapshot = read_json(path)?;
        snapshot.validate()?;
        ensure!(
            snapshot.session == self.session,
            "cross-session snapshot refused; use explicit named-space import"
        );
        Ok(snapshot)
    }
    pub fn save(&self, snapshot: &Snapshot, name: Option<&str>, retention: usize) -> Result<()> {
        snapshot.validate()?;
        ensure!(
            name.is_some() || snapshot.session == self.session,
            "snapshot session mismatch"
        );
        let path = self.snapshot_path(name)?;
        if name.is_none() {
            let bytes = serde_json::to_vec(snapshot)?;
            let archive = self.root.join("snapshots").join(format!(
                "{:020}-{}.json",
                snapshot.created_ms,
                &digest(&bytes)[..16]
            ));
            atomic_json(&archive, snapshot)?;
        }
        atomic_json(&path, snapshot)?;
        if name.is_none() {
            let files = self.list("snapshots")?;
            let remove_count = files.len().saturating_sub(retention);
            for file in files.into_iter().take(remove_count) {
                fs::remove_file(file)?;
            }
            File::open(self.root.join("snapshots"))?.sync_all()?;
        }
        Ok(())
    }
    pub fn list(&self, subdir: &str) -> Result<Vec<PathBuf>> {
        ensure!(
            matches!(subdir, "spaces" | "snapshots" | "boots"),
            "invalid state category"
        );
        let mut paths = Vec::new();
        let directory = if subdir == "spaces" {
            self.spaces.clone()
        } else {
            self.root.join(subdir)
        };
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            if entry.path().extension().is_some_and(|ext| ext == "json") {
                ensure!(entry.file_type()?.is_file(), "non-regular state entry");
                paths.push(entry.path());
            }
        }
        paths.sort();
        Ok(paths)
    }
}

pub fn validate_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.len() > 64
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        bail!("space name must be 1..64 ASCII letters, digits, hyphens or underscores");
    }
    Ok(())
}
