//! Models and the Piper binary, downloaded once into the cache directory
//! (`~/.cache/meerora/voice/`). Every pinned file is checked against its
//! size and SHA-256 before it takes its place, so a file in the cache is
//! always complete.

use std::fs::File;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use sha2::{Digest, Sha256};

/// Free space a download leaves at least, on top of the files themselves.
pub const SPACE_MARGIN: u64 = 1 << 30;

/// One file to download.
#[derive(Clone, Debug, PartialEq)]
pub struct Asset {
    /// Shown in the progress, for example "whisper base model".
    pub label: String,
    pub url: String,
    /// Expected SHA-256 in hex, when the source publishes one or it is pinned here.
    pub sha256: Option<String>,
    /// Expected size in bytes, when known.
    pub size: Option<u64>,
    /// Where it lands, relative to the cache root. For an archive, the folder it unpacks into.
    pub path: PathBuf,
    pub kind: Kind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Stored as it is.
    File,
    /// A `.tar.gz` unpacked into `path`. The archive itself is not kept.
    TarGz,
}

/// Marks a folder that was unpacked completely.
const UNPACKED: &str = ".meerora-unpacked";

/// Progress of a download, for a progress bar.
#[derive(Clone, Debug, PartialEq)]
pub struct Progress {
    pub label: String,
    /// Bytes of this file so far.
    pub done: u64,
    pub total: Option<u64>,
    /// Which file of how many, counting from 1.
    pub index: usize,
    pub count: usize,
}

impl Progress {
    /// 0 to 1 over all files, counting finished ones as whole.
    pub fn fraction(&self) -> f32 {
        let file = self.total.filter(|t| *t > 0).map_or(0.0, |t| self.done as f32 / t as f32);
        ((self.index.saturating_sub(1)) as f32 + file.min(1.0)) / self.count.max(1) as f32
    }
}

#[derive(Debug, thiserror::Error)]
pub enum FetchError {
    #[error("{0}")]
    Io(#[from] io::Error),
    #[error("downloading {label} failed: {message}")]
    Http { label: String, message: String },
    #[error("{label} has the wrong checksum (expected {expected}, got {got}), so it was not kept")]
    Checksum { label: String, expected: String, got: String },
    #[error("{label} has {got} bytes instead of {expected}, so it was not kept")]
    Size { label: String, expected: u64, got: u64 },
    #[error("not enough disk space: the voice models need {} MB and {} MB more stay free, but only {} MB are free", .needed >> 20, SPACE_MARGIN >> 20, .free >> 20)]
    Space { needed: u64, free: u64 },
    #[error("unpacking {label} failed: {message}")]
    Unpack { label: String, message: String },
    #[error("the download was cancelled")]
    Cancelled,
}

/// Where downloads come from. The real one is [`Http`], tests use a fake.
pub trait Source: Send + Sync {
    /// Opens `url` for reading and returns its length when the server tells it.
    fn open(&self, url: &str) -> io::Result<(Option<u64>, Box<dyn Read + Send>)>;
}

/// HTTPS downloads with ureq, following redirects.
pub struct Http;

impl Source for Http {
    fn open(&self, url: &str) -> io::Result<(Option<u64>, Box<dyn Read + Send>)> {
        let response = ureq::get(url).header("User-Agent", "meerora-voice").call().map_err(io::Error::other)?;
        let length = response.headers().get("content-length").and_then(|v| v.to_str().ok()).and_then(|v| v.parse().ok());
        let reader = response.into_body().into_reader();
        Ok((length, Box::new(reader)))
    }
}

/// The cache directory and what is in it.
#[derive(Clone, Debug)]
pub struct Cache {
    root: PathBuf,
}

impl Cache {
    pub fn new(root: impl Into<PathBuf>) -> Cache {
        Cache { root: root.into() }
    }

    /// `$MEERORA_VOICE_CACHE`, else `$XDG_CACHE_HOME/meerora/voice`, else `~/.cache/meerora/voice`.
    pub fn default_root() -> Option<PathBuf> {
        if let Some(dir) = std::env::var_os("MEERORA_VOICE_CACHE") {
            return Some(dir.into());
        }
        std::env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .or_else(|| std::env::var_os("HOME").map(|home| Path::new(&home).join(".cache")))
            .map(|dir| dir.join("meerora").join("voice"))
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Where an asset is (or will be) in the cache.
    pub fn path(&self, asset: &Asset) -> PathBuf {
        self.root.join(&asset.path)
    }

    /// True when the asset is complete in the cache. Files are only renamed
    /// into place after their checksum matched, so the size is enough here.
    pub fn is_ready(&self, asset: &Asset) -> bool {
        let path = self.path(asset);
        match asset.kind {
            Kind::File => std::fs::metadata(&path).is_ok_and(|m| m.is_file() && asset.size.is_none_or(|s| s == m.len())),
            Kind::TarGz => path.join(UNPACKED).is_file(),
        }
    }

    /// The assets that still need a download.
    pub fn missing(&self, assets: &[Asset]) -> Vec<Asset> {
        assets.iter().filter(|a| !self.is_ready(a)).cloned().collect()
    }

    /// Downloads every missing asset, one after the other. Checks the free
    /// space first. `progress` is called often, `cancel` stops between chunks.
    pub fn fetch(
        &self,
        assets: &[Asset],
        source: &dyn Source,
        progress: &mut dyn FnMut(&Progress),
        cancel: &AtomicBool,
    ) -> Result<(), FetchError> {
        let missing = self.missing(assets);
        if missing.is_empty() {
            return Ok(());
        }
        std::fs::create_dir_all(&self.root)?;
        // An archive needs room for itself and its unpacked files.
        let needed: u64 = missing.iter().map(|a| a.size.unwrap_or(0) * if a.kind == Kind::TarGz { 3 } else { 1 }).sum();
        check_space(needed, free_bytes(&self.root)?)?;
        let count = missing.len();
        for (i, asset) in missing.iter().enumerate() {
            self.fetch_one(
                asset,
                source,
                &mut |done, total| progress(&Progress { label: asset.label.clone(), done, total, index: i + 1, count }),
                cancel,
            )?;
        }
        Ok(())
    }

    fn fetch_one(
        &self,
        asset: &Asset,
        source: &dyn Source,
        progress: &mut dyn FnMut(u64, Option<u64>),
        cancel: &AtomicBool,
    ) -> Result<(), FetchError> {
        let dest = self.path(asset);
        if let Some(dir) = dest.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let part = part_path(&dest);
        let result = download(asset, source, &part, progress, cancel);
        if let Err(e) = result {
            let _ = std::fs::remove_file(&part);
            return Err(e);
        }
        match asset.kind {
            Kind::File => std::fs::rename(&part, &dest)?,
            Kind::TarGz => {
                let unpacked = unpack(&part, &dest).map_err(|e| FetchError::Unpack { label: asset.label.clone(), message: e.to_string() });
                let _ = std::fs::remove_file(&part);
                unpacked?;
            }
        }
        tracing::info!("downloaded {} to {}", asset.label, dest.display());
        Ok(())
    }
}

/// `file.ext` becomes `file.ext.part`.
fn part_path(dest: &Path) -> PathBuf {
    let mut name = dest.file_name().unwrap_or_default().to_os_string();
    name.push(".part");
    dest.with_file_name(name)
}

/// Streams `asset` into `part` while hashing it, then checks size and checksum.
fn download(
    asset: &Asset,
    source: &dyn Source,
    part: &Path,
    progress: &mut dyn FnMut(u64, Option<u64>),
    cancel: &AtomicBool,
) -> Result<(), FetchError> {
    let http = |e: io::Error| FetchError::Http { label: asset.label.clone(), message: e.to_string() };
    let (length, mut reader) = source.open(&asset.url).map_err(http)?;
    let total = asset.size.or(length);
    let mut file = File::create(part)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    let mut done = 0u64;
    let mut last_report = 0u64;
    progress(0, total);
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(FetchError::Cancelled);
        }
        let n = match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(http(e)),
        };
        file.write_all(&buf[..n])?;
        hasher.update(&buf[..n]);
        done += n as u64;
        if done - last_report >= 256 * 1024 {
            last_report = done;
            progress(done, total);
        }
        if let Some(expected) = asset.size
            && done > expected
        {
            return Err(FetchError::Size { label: asset.label.clone(), expected, got: done });
        }
    }
    file.sync_all()?;
    progress(done, total);
    if let Some(expected) = asset.size
        && done != expected
    {
        return Err(FetchError::Size { label: asset.label.clone(), expected, got: done });
    }
    let got = hex(&hasher.finalize());
    if let Some(expected) = &asset.sha256
        && !expected.eq_ignore_ascii_case(&got)
    {
        return Err(FetchError::Checksum { label: asset.label.clone(), expected: expected.clone(), got });
    }
    if asset.sha256.is_none() {
        tracing::info!("{} has no pinned checksum, its SHA-256 is {got}", asset.label);
    }
    Ok(())
}

/// Unpacks a `.tar.gz` into `dest` through a temporary folder, so `dest`
/// appears complete or not at all. The tar crate refuses entries that would
/// land outside the folder.
fn unpack(archive: &Path, dest: &Path) -> io::Result<()> {
    let mut name = dest.file_name().unwrap_or_default().to_os_string();
    name.push(".unpacking");
    let tmp = dest.with_file_name(name);
    if tmp.exists() {
        std::fs::remove_dir_all(&tmp)?;
    }
    std::fs::create_dir_all(&tmp)?;
    let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(File::open(archive)?));
    tar.set_preserve_permissions(true);
    tar.set_overwrite(true);
    if let Err(e) = tar.unpack(&tmp) {
        let _ = std::fs::remove_dir_all(&tmp);
        return Err(e);
    }
    File::create(tmp.join(UNPACKED))?;
    if dest.exists() {
        std::fs::remove_dir_all(dest)?;
    }
    std::fs::rename(&tmp, dest)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// SHA-256 of a file in hex.
pub fn sha256_file(path: &Path) -> io::Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    io::copy(&mut file, &mut hasher)?;
    Ok(hex(&hasher.finalize()))
}

/// Refuses a download that would leave less than [`SPACE_MARGIN`] free.
pub fn check_space(needed: u64, free: u64) -> Result<(), FetchError> {
    if needed.saturating_add(SPACE_MARGIN) > free { Err(FetchError::Space { needed, free }) } else { Ok(()) }
}

/// Free bytes for an unprivileged user on the file system of `path` or of
/// its nearest existing parent.
pub fn free_bytes(path: &Path) -> io::Result<u64> {
    let mut dir = path;
    while !dir.exists() {
        dir = dir.parent().ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no existing parent"))?;
    }
    let c = std::ffi::CString::new(dir.as_os_str().as_encoded_bytes()).map_err(io::Error::other)?;
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: `c` is a valid C string and `stat` a writable statvfs.
    if unsafe { libc::statvfs(c.as_ptr(), &mut stat) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(stat.f_bavail as u64 * stat.f_frsize as u64)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Mutex;

    use super::*;

    /// Serves files from memory and counts the requests.
    #[derive(Default)]
    struct Fake {
        files: HashMap<String, Vec<u8>>,
        opened: Mutex<Vec<String>>,
    }

    impl Source for Fake {
        fn open(&self, url: &str) -> io::Result<(Option<u64>, Box<dyn Read + Send>)> {
            self.opened.lock().unwrap().push(url.to_string());
            let data = self.files.get(url).cloned().ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "404"))?;
            Ok((Some(data.len() as u64), Box::new(io::Cursor::new(data))))
        }
    }

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> TempDir {
            let dir = std::env::temp_dir().join(format!("meerora-voice-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            TempDir(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn sha(data: &[u8]) -> String {
        hex(&Sha256::digest(data))
    }

    fn asset(url: &str, data: &[u8], path: &str) -> Asset {
        Asset {
            label: path.into(),
            url: url.into(),
            sha256: Some(sha(data)),
            size: Some(data.len() as u64),
            path: path.into(),
            kind: Kind::File,
        }
    }

    fn fetch(cache: &Cache, assets: &[Asset], source: &Fake) -> (Result<(), FetchError>, Vec<Progress>) {
        let mut seen = Vec::new();
        let result = cache.fetch(assets, source, &mut |p| seen.push(p.clone()), &AtomicBool::new(false));
        (result, seen)
    }

    #[test]
    fn downloads_once_and_then_uses_the_cache() {
        let dir = TempDir::new("cache");
        let data = vec![7u8; 700_000];
        let mut source = Fake::default();
        source.files.insert("https://x/model.bin".into(), data.clone());
        let cache = Cache::new(&dir.0);
        let assets = [asset("https://x/model.bin", &data, "models/model.bin")];
        assert!(!cache.is_ready(&assets[0]));
        let (result, progress) = fetch(&cache, &assets, &source);
        result.unwrap();
        assert_eq!(std::fs::read(dir.0.join("models/model.bin")).unwrap(), data);
        assert!(cache.is_ready(&assets[0]));
        assert!(!dir.0.join("models/model.bin.part").exists());
        // Progress grows to the full size.
        assert!(progress.len() >= 3, "{progress:?}");
        assert!(progress.windows(2).all(|w| w[0].done <= w[1].done));
        let last = progress.last().unwrap();
        assert_eq!((last.done, last.total, last.index, last.count), (700_000, Some(700_000), 1, 1));
        assert_eq!(last.fraction(), 1.0);
        // The second fetch finds everything in the cache.
        fetch(&cache, &assets, &source).0.unwrap();
        assert_eq!(source.opened.lock().unwrap().len(), 1);
    }

    #[test]
    fn a_wrong_checksum_keeps_nothing() {
        let dir = TempDir::new("checksum");
        let mut source = Fake::default();
        source.files.insert("https://x/a".into(), b"tampered".to_vec());
        let cache = Cache::new(&dir.0);
        let mut a = asset("https://x/a", b"original", "a.bin");
        a.size = None;
        let (result, _) = fetch(&cache, std::slice::from_ref(&a), &source);
        assert!(matches!(result, Err(FetchError::Checksum { .. })), "{result:?}");
        assert!(!dir.0.join("a.bin").exists());
        assert!(!dir.0.join("a.bin.part").exists());
        assert!(!cache.is_ready(&a));
    }

    #[test]
    fn a_wrong_size_keeps_nothing() {
        let dir = TempDir::new("size");
        let mut source = Fake::default();
        source.files.insert("https://x/a".into(), b"too long data".to_vec());
        let cache = Cache::new(&dir.0);
        let mut a = asset("https://x/a", b"short", "a.bin");
        a.sha256 = None;
        let (result, _) = fetch(&cache, &[a], &source);
        assert!(matches!(result, Err(FetchError::Size { expected: 5, .. })), "{result:?}");
        assert!(!dir.0.join("a.bin").exists());
    }

    #[test]
    fn a_file_of_the_wrong_size_in_the_cache_is_fetched_again() {
        let dir = TempDir::new("stale");
        let data = b"complete".to_vec();
        let mut source = Fake::default();
        source.files.insert("https://x/a".into(), data.clone());
        std::fs::write(dir.0.join("a.bin"), b"half").unwrap();
        let cache = Cache::new(&dir.0);
        let a = asset("https://x/a", &data, "a.bin");
        assert!(!cache.is_ready(&a));
        fetch(&cache, std::slice::from_ref(&a), &source).0.unwrap();
        assert_eq!(std::fs::read(dir.0.join("a.bin")).unwrap(), data);
    }

    #[test]
    fn unpinned_files_are_kept_without_a_check() {
        let dir = TempDir::new("unpinned");
        let mut source = Fake::default();
        source.files.insert("https://x/v.json".into(), b"{}".to_vec());
        let cache = Cache::new(&dir.0);
        let a = Asset {
            label: "voice".into(),
            url: "https://x/v.json".into(),
            sha256: None,
            size: None,
            path: "v.json".into(),
            kind: Kind::File,
        };
        fetch(&cache, std::slice::from_ref(&a), &source).0.unwrap();
        assert!(cache.is_ready(&a));
    }

    #[test]
    fn a_missing_url_is_an_http_error() {
        let dir = TempDir::new("404");
        let cache = Cache::new(&dir.0);
        let (result, _) = fetch(&cache, &[asset("https://x/none", b"x", "x")], &Fake::default());
        assert!(matches!(result, Err(FetchError::Http { .. })), "{result:?}");
    }

    #[test]
    fn cancel_stops_the_download() {
        let dir = TempDir::new("cancel");
        let mut source = Fake::default();
        source.files.insert("https://x/a".into(), vec![1u8; 10]);
        let cache = Cache::new(&dir.0);
        let result = cache.fetch(&[asset("https://x/a", &[1u8; 10], "a.bin")], &source, &mut |_| {}, &AtomicBool::new(true));
        assert!(matches!(result, Err(FetchError::Cancelled)));
        assert!(!dir.0.join("a.bin").exists());
    }

    #[test]
    fn archives_unpack_into_their_folder() {
        let dir = TempDir::new("tar");
        let mut builder = tar::Builder::new(flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast()));
        let mut header = tar::Header::new_gnu();
        header.set_size(5);
        header.set_mode(0o755);
        header.set_cksum();
        builder.append_data(&mut header, "piper/piper", &b"#!bin"[..]).unwrap();
        let archive = builder.into_inner().unwrap().finish().unwrap();
        let mut source = Fake::default();
        source.files.insert("https://x/piper.tar.gz".into(), archive.clone());
        let cache = Cache::new(&dir.0);
        let mut a = asset("https://x/piper.tar.gz", &archive, "piper");
        a.kind = Kind::TarGz;
        fetch(&cache, std::slice::from_ref(&a), &source).0.unwrap();
        assert!(cache.is_ready(&a));
        let bin = dir.0.join("piper/piper/piper");
        assert_eq!(std::fs::read(&bin).unwrap(), b"#!bin");
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&bin).unwrap().permissions().mode() & 0o111, 0o111);
        // Only the unpacked folder stays.
        let mut names: Vec<_> = std::fs::read_dir(&dir.0).unwrap().map(|e| e.unwrap().file_name()).collect();
        names.sort();
        assert_eq!(names, vec!["piper"]);
    }

    #[test]
    fn archives_cannot_write_outside_their_folder() {
        let dir = TempDir::new("traversal");
        let mut data = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut data);
            let mut header = tar::Header::new_gnu();
            let name = b"../escaped";
            header.as_old_mut().name[..name.len()].copy_from_slice(name);
            header.set_size(1);
            header.set_cksum();
            builder.append(&header, &b"x"[..]).unwrap();
            builder.finish().unwrap();
        }
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        gz.write_all(&data).unwrap();
        let archive = gz.finish().unwrap();
        let mut source = Fake::default();
        source.files.insert("https://x/evil.tar.gz".into(), archive.clone());
        let cache = Cache::new(dir.0.join("cache"));
        let mut a = asset("https://x/evil.tar.gz", &archive, "evil");
        a.kind = Kind::TarGz;
        let _ = fetch(&cache, std::slice::from_ref(&a), &source).0;
        assert!(!dir.0.join("escaped").exists());
        assert!(!dir.0.join("cache/escaped").exists());
    }

    #[test]
    fn space_check_keeps_a_margin() {
        assert!(check_space(100, SPACE_MARGIN + 100).is_ok());
        let err = check_space(300 << 20, SPACE_MARGIN + (200 << 20)).unwrap_err();
        assert!(err.to_string().contains("need 300 MB"), "{err}");
        assert!(free_bytes(&std::env::temp_dir().join("does/not/exist")).unwrap() > 0);
    }

    #[test]
    fn progress_fraction_counts_finished_files() {
        let p = Progress { label: "b".into(), done: 50, total: Some(100), index: 2, count: 4 };
        assert_eq!(p.fraction(), 0.375);
        let unknown = Progress { total: None, ..p };
        assert_eq!(unknown.fraction(), 0.25);
    }
}
