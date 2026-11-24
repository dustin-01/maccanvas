use super::*;
use std::{
    fs::File,
    io::{Read, Seek, SeekFrom},
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
};

pub fn current_sid() -> Result<String> {
    Ok(unsafe { libc::geteuid() }.to_string())
}

pub fn reject_reparse(path: &Path) -> Result<()> {
    let stat = fs::symlink_metadata(path)?;
    if stat.file_type().is_symlink()
        || !(stat.is_dir() || stat.is_file() && stat.nlink() == 1)
        || stat.uid() != unsafe { libc::geteuid() }
    {
        return Err(Error::Audit(format!(
            "Unsafe audit path {}",
            path.display()
        )));
    }
    Ok(())
}

pub fn protect(path: &Path, _: &str, directory: bool) -> Result<()> {
    reject_reparse(path)?;
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let stat = file.metadata()?;
    if directory != stat.is_dir() || stat.uid() != unsafe { libc::geteuid() } {
        return Err(Error::Audit("Unexpected audit file type or owner".into()));
    }
    file.set_permissions(fs::Permissions::from_mode(if directory {
        0o700
    } else {
        0o600
    }))?;
    Ok(())
}

pub struct LockFile(File);
pub struct LockGuard<'a>(&'a File);
impl LockFile {
    pub fn open(directory: &Path, sid: &str) -> Result<Self> {
        let path = directory.join(".audit.lock");
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(&path)?;
        protect(&path, sid, false)?;
        Ok(Self(file))
    }
    pub fn acquire(&self) -> Result<LockGuard<'_>> {
        self.0.lock()?;
        Ok(LockGuard(&self.0))
    }
}
impl Drop for LockGuard<'_> {
    fn drop(&mut self) {
        if let Err(error) = self.0.unlock() {
            eprintln!("MacCanvas audit unlock failed: {error}");
        }
    }
}

pub fn resource(instance: &str) -> Value {
    let mut fields = json!({
        "service.name":"MacCanvas", "service.instance.id":instance, "service.version":crate::VERSION,
        "process.pid":std::process::id(), "process.parent_pid":unsafe {libc::getppid()},
        "process.runtime.name":"Rust", "process.runtime.version":crate::RUSTC,
        "user.id":unsafe {libc::geteuid()}.to_string(), "host.arch":std::env::consts::ARCH, "os.type":"macos",
        "build.time":crate::BUILD_UTC, "build.time.source":crate::BUILD_CLOCK,
        "build.target":crate::TARGET, "vcs.ref.head.revision":crate::REVISION
    });
    if let Ok(path) = std::env::current_exe() {
        fields["process.executable.path"] = json!(path);
        let hash = (|| -> std::io::Result<String> {
            let mut digest = Sha256::new();
            std::io::copy(&mut File::open(path)?, &mut digest)?;
            Ok(format!("{:x}", digest.finalize()))
        })();
        match hash {
            Ok(hash) => fields["process.executable.hash.sha256"] = json!(hash),
            Err(error) => fields["process.executable.hash.error"] = json!(error.to_string()),
        }
    }
    fields
}

pub fn incomplete_tail(path: &Path) -> Result<bool> {
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    file.seek(SeekFrom::End(-1))?;
    let mut byte = [0];
    file.read_exact(&mut byte)?;
    Ok(byte != [b'\n'])
}
