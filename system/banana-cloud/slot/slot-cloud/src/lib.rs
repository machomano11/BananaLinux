//! Two-way sync of a card's `Saves` and `States`, and optionally its
//! `Games` and `Labels`, with a remote folder.
//!
//! A manifest remembers each file as it was after the last sync, on both
//! sides, so a run can tell which side changed:
//!
//! - changed on one side only: copied to the other;
//! - changed on both, or found on both with no history: if the bytes differ,
//!   the newer copy wins and the other is kept on the card as
//!   `<name>.conflict-<when>`, which is never synced;
//! - gone on one side and unchanged on the other: deleted there too, except
//!   under `Saves`, `Games` and `Labels`, where nothing is ever deleted and
//!   the missing copy comes back instead;
//! - a game found on both sides with no history and the same size is taken
//!   to be the same game, rather than fetched just to compare;
//! - files written in the last few seconds wait for the next run;
//! - files of the game being played ("held") are only ever uploaded: nothing
//!   overwrites or deletes them under a running game.
//!
//! Files stream to and from disk, so a CD image costs no more memory than a
//! save. Uploads go to a hidden name first and are renamed once complete, so
//! a cut-off upload never looks like a finished file on the other side.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub mod webdav;

pub use webdav::WebDav;

/// Whether `rel` is one of the files a game keeps: its save
/// (`Saves/<platform>/<stem>.<ext>`) and its states
/// (`States/<platform>/<core>/<stem>/…`).
pub fn belongs_to(rel: &str, platform_dir: &str, stem: &str) -> bool {
    let parts: Vec<&str> = rel.split('/').collect();
    match parts.as_slice() {
        ["Saves", p, file] => {
            *p == platform_dir
                && file
                    .strip_prefix(stem)
                    .is_some_and(|rest| rest.starts_with('.'))
        }
        ["States", p, _core, s, _, ..] => *p == platform_dir && *s == stem,
        _ => false,
    }
}

/// The game files: saves and states, small and changing often.
pub const PROGRESS: [&str; 2] = ["Saves", "States"];
/// The ROMs.
pub const GAMES: &str = "Games";
/// The cartridge and disc art, and the per-game cart choices beside it.
pub const ART: &str = "Labels";
/// Every card folder that can sync, relative to the root.
pub const FOLDERS: [&str; 4] = ["Saves", "States", GAMES, ART];
/// Folders where a deletion is never passed on.
const KEEP: [&str; 3] = ["Saves", GAMES, ART];
const PART: &str = ".cloud-part";
/// Left free on the card after a download, for saves and states.
const ROOM: u64 = 64 << 20;
const SETTLE: Duration = Duration::from_secs(3);
const MANIFEST_HEADER: &str = "slot-cloud 1";
const CONFLICT: &str = ".conflict-";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Error {
    pub message: String,
    /// The server couldn't be reached at all (no Wi-Fi yet, say), as opposed
    /// to it answering with a refusal.
    pub offline: bool,
    /// The card has no room for a file: that one is skipped, the run goes on.
    no_room: bool,
}

impl Error {
    pub fn new(message: impl Into<String>) -> Error {
        Error {
            message: message.into(),
            offline: false,
            no_room: false,
        }
    }

    pub fn offline(message: impl Into<String>) -> Error {
        Error {
            offline: true,
            ..Error::new(message)
        }
    }

    fn no_room(rel: &str) -> Error {
        Error {
            no_room: true,
            ..Error::new(format!("no room on the card for {rel}"))
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Error {}

/// One entry of a remote folder.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Listed {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    /// Changes whenever the contents do (the ETag, or size and date).
    pub tag: String,
    /// Seconds since 1970.
    pub modified: Option<u64>,
}

pub trait Remote {
    /// A folder's entries, or `None` if it doesn't exist.
    fn list(&mut self, dir: &str) -> Result<Option<Vec<Listed>>, Error>;
    fn stat(&mut self, rel: &str) -> Result<Option<Listed>, Error>;
    /// Streams the file into `out`.
    fn get(&mut self, rel: &str, out: &mut dyn Write) -> Result<(), Error>;
    /// Stores the `len` bytes `body` gives.
    fn put(&mut self, rel: &str, body: &mut dyn Read, len: u64) -> Result<(), Error>;
    /// Renames a file, replacing any already at `to`.
    fn rename(&mut self, from: &str, to: &str) -> Result<(), Error>;
    fn mkdir(&mut self, rel: &str) -> Result<(), Error>;
    fn delete(&mut self, rel: &str) -> Result<(), Error>;
}

/// Where the server's files live on a device. Slot's cards keep them under
/// the same names ([`Card`]); a system with its own folders (BananaOS: one
/// flat `saves` folder, `roms/<system>`) maps them.
pub trait Layout {
    /// The device's copy of the server's `rel`, or `None` when this device
    /// doesn't keep that file: it is then left alone on both sides.
    fn local(&self, rel: &str) -> Option<PathBuf>;
    /// Calls `found` with every file under `folder` (one of [`FOLDERS`]) on
    /// the device: its name on the server and its path here. Hidden files and
    /// conflict copies ([`skipped`]) are left out.
    fn walk(&self, folder: &str, found: &mut dyn FnMut(String, PathBuf));
    /// Where free space is measured before a download.
    fn root(&self) -> &Path;
}

/// A Slot card: the server's names under the card's root.
pub struct Card<'a>(pub &'a Path);

impl Layout for Card<'_> {
    fn local(&self, rel: &str) -> Option<PathBuf> {
        Some(self.0.join(rel))
    }

    fn walk(&self, folder: &str, found: &mut dyn FnMut(String, PathBuf)) {
        let mut todo = vec![folder.to_string()];
        while let Some(dir) = todo.pop() {
            let Ok(entries) = std::fs::read_dir(self.0.join(&dir)) else {
                continue;
            };
            for e in entries.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                if skipped(&name) {
                    continue;
                }
                let rel = format!("{dir}/{name}");
                match e.file_type() {
                    Ok(t) if t.is_dir() => todo.push(rel),
                    Ok(_) => found(rel, e.path()),
                    Err(_) => {}
                }
            }
        }
    }

    fn root(&self) -> &Path {
        self.0
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Report {
    pub uploaded: usize,
    pub downloaded: usize,
    pub deleted: usize,
    /// Files where both sides changed; the loser was kept as a conflict copy.
    pub conflicts: Vec<String>,
    /// What came down from the server, relative to the root.
    pub received: Vec<String>,
    /// What the server has that the card had no room for; tried again next run.
    pub no_room: Vec<String>,
    /// Files that couldn't be moved, with why; tried again next run.
    pub failed: Vec<(String, String)>,
    /// The run stopped early because it was asked to.
    pub cancelled: bool,
}

impl Report {
    pub fn changed(&self) -> bool {
        self.uploaded + self.downloaded + self.deleted > 0 || !self.conflicts.is_empty()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Seen {
    local: u64,
    size: u64,
    tag: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Local {
    mtime_ns: u64,
    size: u64,
}

/// Syncs all of `root`'s [`FOLDERS`] with `remote`, keeping what it learns in
/// `manifest`. Stops soon after `cancel` is set, mid-file if need be.
pub fn sync(
    root: &Path,
    manifest: &Path,
    remote: &mut dyn Remote,
    cancel: &AtomicBool,
) -> Result<Report, Error> {
    sync_holding(root, manifest, remote, &FOLDERS, cancel, &|_| false)
}

/// [`sync`] of just `folders` (some of [`FOLDERS`]), with the files for which
/// `held` is true (a running game's) only uploaded when they changed here and
/// not on the server. What the manifest knows of other folders is kept.
pub fn sync_holding(
    root: &Path,
    manifest: &Path,
    remote: &mut dyn Remote,
    folders: &[&str],
    cancel: &AtomicBool,
    held: &dyn Fn(&str) -> bool,
) -> Result<Report, Error> {
    sync_layout(&Card(root), manifest, remote, folders, cancel, held)
}

/// [`sync_holding`] for a device whose files are where `layout` says.
pub fn sync_layout(
    layout: &dyn Layout,
    manifest: &Path,
    remote: &mut dyn Remote,
    folders: &[&str],
    cancel: &AtomicBool,
    held: &dyn Fn(&str) -> bool,
) -> Result<Report, Error> {
    let mut seen = read_manifest(manifest);
    let mut run = Run {
        layout,
        remote,
        cancel,
        dirs: BTreeSet::new(),
        report: Report::default(),
        now: SystemTime::now(),
    };
    let result = run.all(&mut seen, folders, held);
    let saved = write_manifest(manifest, &seen);
    result?;
    saved.map_err(|e| Error::new(format!("saving {}: {e}", manifest.display())))?;
    Ok(run.report)
}

struct Run<'a> {
    layout: &'a dyn Layout,
    remote: &'a mut dyn Remote,
    cancel: &'a AtomicBool,
    /// Remote folders known to exist.
    dirs: BTreeSet<String>,
    report: Report,
    now: SystemTime,
}

impl Run<'_> {
    fn all(
        &mut self,
        seen: &mut BTreeMap<String, Seen>,
        folders: &[&str],
        held: &dyn Fn(&str) -> bool,
    ) -> Result<(), Error> {
        let mut local = BTreeMap::new();
        let mut settling = BTreeSet::new();
        for folder in folders {
            walk_local(self.layout, folder, self.now, &mut local, &mut settling);
        }
        // The folder named in the settings may not exist yet.
        if self.remote.list("")?.is_none() {
            self.remote.mkdir("")?;
        }
        let mut remote = BTreeMap::new();
        for folder in folders {
            self.walk_remote(folder, &mut remote)?;
        }
        // What this device doesn't keep is left alone on both sides.
        remote.retain(|rel, _| self.layout.local(rel).is_some());
        let ours = |rel: &String| folders.iter().any(|f| rel.starts_with(&format!("{f}/")));
        let names: BTreeSet<String> = local
            .keys()
            .chain(remote.keys())
            .chain(
                seen.keys()
                    .filter(|k| ours(k) && self.layout.local(k).is_some()),
            )
            .cloned()
            .collect();
        // Saves and states first, so a big batch of games never holds them up;
        // then the rest smallest first, so one huge disc image can't hold up
        // every game after it.
        let progress = |rel: &String| PROGRESS.iter().any(|f| rel.starts_with(&format!("{f}/")));
        let (first, mut rest): (Vec<String>, Vec<String>) = names.into_iter().partition(progress);
        let size = |rel: &String| {
            let here = local.get(rel).map_or(0, |l| l.size);
            let there = remote.get(rel).map_or(0, |r: &Listed| r.size);
            here.max(there)
        };
        rest.sort_by_cached_key(|rel| (size(rel), rel.clone()));
        for rel in first.into_iter().chain(rest) {
            if settling.contains(&rel) {
                continue;
            }
            if self.cancel.load(Ordering::Relaxed) {
                self.report.cancelled = true;
                return Ok(());
            }
            let l = local.get(&rel).copied();
            let r = remote.get(&rel);
            let s = seen.get(&rel);
            let result = if held(&rel) {
                match l.filter(|&l| only_sent(rel.as_str(), l, r, s)) {
                    Some(l) => self.upload(&rel, l).map(Some),
                    None => continue,
                }
            } else {
                self.one(&rel, l, r, s)
            };
            match result {
                Ok(Some(s)) => seen.insert(rel, s),
                Ok(None) => seen.remove(&rel),
                // Left as it was, to try again once there is room.
                Err(e) if e.no_room => {
                    eprintln!("slot: cloud: {e}");
                    self.report.no_room.push(rel);
                    continue;
                }
                // Stopped mid-file: what was half done is thrown away.
                Err(_) if self.cancel.load(Ordering::Relaxed) => {
                    self.report.cancelled = true;
                    return Ok(());
                }
                // One file going wrong (too big for the server, say) doesn't
                // stop the rest, as long as the server still answers.
                Err(e) => {
                    if let Err(down) = self.remote.stat("") {
                        return Err(if e.offline { e } else { down });
                    }
                    eprintln!("slot: cloud: skipped {rel}: {e}");
                    self.report.failed.push((rel, e.message));
                    continue;
                }
            };
        }
        Ok(())
    }

    /// Brings one file in line; returns what to remember about it.
    fn one(
        &mut self,
        rel: &str,
        l: Option<Local>,
        r: Option<&Listed>,
        s: Option<&Seen>,
    ) -> Result<Option<Seen>, Error> {
        let keep = KEEP.iter().any(|k| rel.starts_with(&format!("{k}/")));
        let local_changed = |l: Local, s: &Seen| l.mtime_ns != s.local || l.size != s.size;
        Ok(match (l, r, s) {
            (Some(l), Some(r), Some(s)) => match (local_changed(l, s), r.tag != s.tag) {
                (false, false) => Some(s.clone()),
                (true, false) => Some(self.upload(rel, l)?),
                (false, true) => Some(self.download(rel, r)?),
                (true, true) => Some(self.both(rel, l, r)?),
            },
            (Some(l), Some(r), None) if rel.starts_with("Games/") && l.size == r.size => {
                Some(Seen {
                    local: l.mtime_ns,
                    size: l.size,
                    tag: r.tag.clone(),
                })
            }
            (Some(l), Some(r), None) => Some(self.both(rel, l, r)?),
            (Some(l), None, Some(s)) if !keep && !local_changed(l, s) => {
                self.remove_local(rel)?;
                None
            }
            (Some(l), None, _) => Some(self.upload(rel, l)?),
            (None, Some(r), Some(s)) if !keep && r.tag == s.tag => {
                self.remote.delete(rel)?;
                self.report.deleted += 1;
                None
            }
            (None, Some(r), _) => Some(self.download(rel, r)?),
            (None, None, _) => None,
        })
    }

    fn upload(&mut self, rel: &str, l: Local) -> Result<Seen, Error> {
        self.make_parents(rel)?;
        let part = part_of(rel);
        self.send(&part, rel)?;
        if let Err(e) = self.remote.rename(&part, rel) {
            // A server without MOVE: store it under its own name instead.
            let _ = self.remote.delete(&part);
            if e.offline {
                return Err(e);
            }
            eprintln!("slot: cloud: renaming {part}: {e}; sending it again directly");
            self.send(rel, rel)?;
        }
        let tag = self
            .remote
            .stat(rel)?
            .map(|r| r.tag)
            .ok_or_else(|| Error::new(format!("{rel} vanished right after uploading")))?;
        self.report.uploaded += 1;
        eprintln!("slot: cloud: uploaded {rel}");
        Ok(Seen {
            local: l.mtime_ns,
            size: l.size,
            tag,
        })
    }

    /// Streams the card's `rel` to `to` on the server.
    fn send(&mut self, to: &str, rel: &str) -> Result<(), Error> {
        let file = std::fs::File::open(self.path(rel)?)
            .map_err(|e| Error::new(format!("reading {rel}: {e}")))?;
        let len = file
            .metadata()
            .map_err(|e| Error::new(format!("reading {rel}: {e}")))?
            .len();
        let mut body = Watched {
            inner: file.take(len),
            cancel: self.cancel,
        };
        self.remote.put(to, &mut body, len)
    }

    /// Streams the server's `rel` into a hidden file beside the card's copy,
    /// flushed to the card. The caller renames or removes it.
    fn fetch(&mut self, rel: &str, size: u64) -> Result<PathBuf, Error> {
        if free_bytes(self.layout.root()).is_some_and(|free| free < size.saturating_add(ROOM)) {
            return Err(Error::no_room(rel));
        }
        let tmp = tmp_of(&self.path(rel)?);
        let written = (|| {
            if let Some(dir) = tmp.parent() {
                std::fs::create_dir_all(dir)
                    .map_err(|e| Error::new(format!("writing {rel}: {e}")))?;
            }
            let file = std::fs::File::create(&tmp)
                .map_err(|e| Error::new(format!("writing {rel}: {e}")))?;
            let mut out = Watched {
                inner: std::io::BufWriter::with_capacity(1 << 16, file),
                cancel: self.cancel,
            };
            self.remote.get(rel, &mut out)?;
            let file = out
                .inner
                .into_inner()
                .map_err(|e| Error::new(format!("writing {rel}: {}", e.error())))?;
            file.sync_all()
                .map_err(|e| Error::new(format!("writing {rel}: {e}")))
        })();
        if let Err(e) = written {
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
        Ok(tmp)
    }

    fn download(&mut self, rel: &str, r: &Listed) -> Result<Seen, Error> {
        let tmp = self.fetch(rel, r.size)?;
        let seen = self.adopt(rel, &tmp, &r.tag)?;
        self.report.downloaded += 1;
        self.report.received.push(rel.to_string());
        eprintln!("slot: cloud: downloaded {rel}");
        Ok(seen)
    }

    /// Both sides have the file and at least one changed it.
    fn both(&mut self, rel: &str, l: Local, r: &Listed) -> Result<Seen, Error> {
        let path = self.path(rel)?;
        let tmp = self.fetch(rel, r.size)?;
        if same_bytes(&path, &tmp).map_err(|e| Error::new(format!("reading {rel}: {e}")))? {
            let _ = std::fs::remove_file(&tmp);
            return Ok(Seen {
                local: l.mtime_ns,
                size: l.size,
                tag: r.tag.clone(),
            });
        }
        let conflict = format!("{rel}{CONFLICT}{}", stamp(self.now));
        let conflict_path = conflict_of(&path, self.now);
        let local_secs = l.mtime_ns / 1_000_000_000;
        let seen = if r.modified.unwrap_or(0) > local_secs {
            eprintln!("slot: cloud: {rel} changed on both sides, keeping the server's newer copy");
            if let Err(e) = std::fs::rename(&path, &conflict_path) {
                let _ = std::fs::remove_file(&tmp);
                return Err(Error::new(format!("keeping {conflict}: {e}")));
            }
            let seen = self.adopt(rel, &tmp, &r.tag)?;
            self.report.downloaded += 1;
            self.report.received.push(rel.to_string());
            seen
        } else {
            eprintln!("slot: cloud: {rel} changed on both sides, keeping this card's newer copy");
            std::fs::rename(&tmp, &conflict_path)
                .map_err(|e| Error::new(format!("keeping {conflict}: {e}")))?;
            self.upload(rel, l)?
        };
        self.report.conflicts.push(rel.to_string());
        Ok(seen)
    }

    /// Puts a fetched file in place as the card's `rel`.
    fn adopt(&mut self, rel: &str, tmp: &Path, tag: &str) -> Result<Seen, Error> {
        let path = self.path(rel)?;
        if let Err(e) = std::fs::rename(tmp, &path) {
            let _ = std::fs::remove_file(tmp);
            return Err(Error::new(format!("writing {rel}: {e}")));
        }
        if let Some(d) = path.parent().and_then(|d| std::fs::File::open(d).ok()) {
            let _ = d.sync_all();
        }
        let l = local_of(&path)
            .ok_or_else(|| Error::new(format!("{rel} vanished right after writing")))?;
        Ok(Seen {
            local: l.mtime_ns,
            size: l.size,
            tag: tag.to_string(),
        })
    }

    fn remove_local(&mut self, rel: &str) -> Result<(), Error> {
        match std::fs::remove_file(self.path(rel)?) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(Error::new(format!("deleting {rel}: {e}"))),
        }
        eprintln!("slot: cloud: deleted {rel}, as the server did");
        self.report.deleted += 1;
        Ok(())
    }

    /// The device's copy of `rel`; only names the layout keeps get this far.
    fn path(&self, rel: &str) -> Result<PathBuf, Error> {
        self.layout
            .local(rel)
            .ok_or_else(|| Error::new(format!("{rel} has no place on this device")))
    }

    fn make_parents(&mut self, rel: &str) -> Result<(), Error> {
        let parts: Vec<&str> = rel.split('/').collect();
        for i in 1..parts.len() {
            let dir = parts[..i].join("/");
            if self.dirs.insert(dir.clone()) {
                self.remote.mkdir(&dir)?;
            }
        }
        Ok(())
    }

    fn walk_remote(
        &mut self,
        folder: &str,
        out: &mut BTreeMap<String, Listed>,
    ) -> Result<(), Error> {
        let mut todo = vec![folder.to_string()];
        while let Some(dir) = todo.pop() {
            let Some(entries) = self.remote.list(&dir)? else {
                continue;
            };
            self.dirs.insert(dir.clone());
            for e in entries {
                if skipped(&e.name) {
                    continue;
                }
                let rel = format!("{dir}/{}", e.name);
                match e.is_dir {
                    true => todo.push(rel),
                    false => {
                        out.insert(rel, e);
                    }
                }
            }
        }
        Ok(())
    }
}

/// Whether a held file is one that may simply be sent: it changed here, and
/// the server's copy is the one last seen (or there is none and never was).
fn only_sent(rel: &str, l: Local, r: Option<&Listed>, s: Option<&Seen>) -> bool {
    let keep = KEEP.iter().any(|k| rel.starts_with(&format!("{k}/")));
    match (r, s) {
        (None, None) => true,
        (None, Some(s)) => keep || l.mtime_ns != s.local || l.size != s.size,
        (Some(r), Some(s)) => r.tag == s.tag && (l.mtime_ns != s.local || l.size != s.size),
        (Some(_), None) => false,
    }
}

/// Reads or writes through `inner` until `cancel` is set, then fails.
struct Watched<'a, T> {
    inner: T,
    cancel: &'a AtomicBool,
}

impl<T> Watched<'_, T> {
    fn check(&self) -> std::io::Result<()> {
        match self.cancel.load(Ordering::Relaxed) {
            true => Err(std::io::Error::other("cancelled")),
            false => Ok(()),
        }
    }
}

impl<T: Read> Read for Watched<'_, T> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.check()?;
        self.inner.read(buf)
    }
}

impl<T: Write> Write for Watched<'_, T> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.check()?;
        self.inner.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

/// `dir/.name.cloud-part`: where an upload goes until it is complete.
fn part_of(rel: &str) -> String {
    match rel.rsplit_once('/') {
        Some((dir, name)) => format!("{dir}/.{name}{PART}"),
        None => format!(".{rel}{PART}"),
    }
}

fn tmp_of(path: &Path) -> PathBuf {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    path.with_file_name(format!(".{name}.cloud.tmp"))
}

/// Free bytes on the card, when the system says.
fn free_bytes(root: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(root.as_os_str().as_bytes()).ok()?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: a valid C string and a statvfs to fill in.
    if unsafe { libc::statvfs(path.as_ptr(), &mut st) } != 0 {
        return None;
    }
    Some(st.f_bavail as u64 * st.f_frsize as u64)
}

fn same_bytes(a: &Path, b: &Path) -> std::io::Result<bool> {
    let (fa, fb) = (std::fs::File::open(a)?, std::fs::File::open(b)?);
    if fa.metadata()?.len() != fb.metadata()?.len() {
        return Ok(false);
    }
    let mut ra = std::io::BufReader::with_capacity(1 << 16, fa);
    let mut rb = std::io::BufReader::with_capacity(1 << 16, fb);
    let (mut ba, mut bb) = (vec![0; 1 << 16], vec![0; 1 << 16]);
    loop {
        let n = ra.read(&mut ba)?;
        if n == 0 {
            return Ok(true);
        }
        rb.read_exact(&mut bb[..n])?;
        if ba[..n] != bb[..n] {
            return Ok(false);
        }
    }
}

/// Hidden files (half-written temporaries among them) and conflict copies.
pub fn skipped(name: &str) -> bool {
    name.is_empty() || name.starts_with('.') || name.contains(CONFLICT)
}

fn walk_local(
    layout: &dyn Layout,
    folder: &str,
    now: SystemTime,
    out: &mut BTreeMap<String, Local>,
    settling: &mut BTreeSet<String>,
) {
    layout.walk(folder, &mut |rel, path| {
        let Ok(meta) = std::fs::metadata(&path) else {
            return;
        };
        if !meta.is_file() {
            return;
        }
        let modified = meta.modified().unwrap_or(UNIX_EPOCH);
        if now.duration_since(modified).is_ok_and(|age| age < SETTLE) {
            settling.insert(rel.clone());
        }
        out.insert(
            rel,
            Local {
                mtime_ns: ns(modified),
                size: meta.len(),
            },
        );
    });
}

/// `<path>.conflict-<when>`, beside the device's copy.
fn conflict_of(path: &Path, now: SystemTime) -> PathBuf {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    path.with_file_name(format!("{name}{CONFLICT}{}", stamp(now)))
}

fn local_of(path: &Path) -> Option<Local> {
    let meta = std::fs::metadata(path).ok()?;
    Some(Local {
        mtime_ns: ns(meta.modified().unwrap_or(UNIX_EPOCH)),
        size: meta.len(),
    })
}

fn ns(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

fn atomic_write(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir)?;
    let tmp = tmp_of(path);
    let mut f = std::fs::File::create(&tmp)?;
    let written = f.write_all(bytes).and_then(|_| f.sync_all());
    drop(f);
    if let Err(e) = written.and_then(|_| std::fs::rename(&tmp, path)) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    if let Ok(d) = std::fs::File::open(dir) {
        let _ = d.sync_all();
    }
    Ok(())
}

fn read_manifest(path: &Path) -> BTreeMap<String, Seen> {
    let mut out = BTreeMap::new();
    let Ok(text) = std::fs::read_to_string(path) else {
        return out;
    };
    let mut lines = text.lines();
    if lines.next() != Some(MANIFEST_HEADER) {
        return out;
    }
    for line in lines {
        let mut f = line.splitn(4, '\t');
        let (Some(local), Some(size), Some(tag), Some(rel)) =
            (f.next(), f.next(), f.next(), f.next())
        else {
            continue;
        };
        let (Ok(local), Ok(size)) = (local.parse(), size.parse()) else {
            continue;
        };
        out.insert(
            rel.to_string(),
            Seen {
                local,
                size,
                tag: tag.to_string(),
            },
        );
    }
    out
}

fn write_manifest(path: &Path, seen: &BTreeMap<String, Seen>) -> std::io::Result<()> {
    let mut text = String::from(MANIFEST_HEADER);
    text.push('\n');
    for (rel, s) in seen {
        let tag = s.tag.replace(['\t', '\n'], " ");
        text.push_str(&format!("{}\t{}\t{tag}\t{rel}\n", s.local, s.size));
    }
    atomic_write(path, text.as_bytes())
}

/// `20261014-100300` in UTC.
fn stamp(t: SystemTime) -> String {
    let secs = t
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (y, m, d) = civil_from_days((secs / 86_400) as i64);
    let s = secs % 86_400;
    format!(
        "{y:04}{m:02}{d:02}-{:02}{:02}{:02}",
        s / 3600,
        s / 60 % 60,
        s % 60
    )
}

/// Days since 1970-01-01 for a proleptic Gregorian date.
pub(crate) fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

/// What a card's `Config/cloudsync.ini` asks for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Account {
    pub url: String,
    pub username: Option<String>,
    pub password: Option<String>,
    /// `roms=0` leaves `Games` out.
    pub games: bool,
    /// `art=0` leaves `Labels` out.
    pub art: bool,
}

impl Account {
    /// `url` is required; `enabled=0` turns sync off.
    pub fn from_ini(values: &HashMap<String, String>) -> Option<Account> {
        let get = |k: &str| values.get(k).map(|v| v.trim()).filter(|v| !v.is_empty());
        let off = |k: &str| matches!(get(k), Some("0" | "false" | "off" | "no"));
        if off("enabled") {
            return None;
        }
        let url = get("url")?;
        if !(url.starts_with("https://") || url.starts_with("http://")) {
            eprintln!("slot: cloud: url must start with https:// or http://");
            return None;
        }
        Some(Account {
            url: url.to_string(),
            username: get("username").map(str::to_string),
            password: get("password").map(str::to_string),
            games: !off("roms"),
            art: !off("art"),
        })
    }

    /// The folders a run syncs; the library is left for when no game runs.
    pub fn folders(&self, playing: bool) -> Vec<&'static str> {
        let mut out = PROGRESS.to_vec();
        if !playing {
            out.extend(self.games.then_some(GAMES));
            out.extend(self.art.then_some(ART));
        }
        out
    }

    pub fn remote(&self) -> WebDav {
        WebDav::new(
            &self.url,
            self.username.as_deref(),
            self.password.as_deref(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_round_trip() {
        for days in [-1, 0, 59, 365, 11_016, 20_740, 30_000] {
            let (y, m, d) = civil_from_days(days);
            assert_eq!(days_from_civil(y, m, d), days);
        }
        assert_eq!(days_from_civil(2026, 10, 14), 20_740);
        assert_eq!(
            stamp(UNIX_EPOCH + Duration::from_secs(20_740 * 86_400 + 36_180)),
            "20261014-100300"
        );
    }

    #[test]
    fn roms_and_art_can_be_left_out_and_wait_while_playing() {
        let ini = |text: &str| {
            text.lines()
                .filter_map(|l| l.split_once('='))
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect::<HashMap<_, _>>()
        };
        let all = Account::from_ini(&ini("url=https://h/dav")).unwrap();
        assert_eq!(all.folders(false), FOLDERS);
        assert_eq!(all.folders(true), PROGRESS);
        let saves = Account::from_ini(&ini("url=https://h/dav\nroms=0\nart=off")).unwrap();
        assert_eq!(saves.folders(false), PROGRESS);
        let art = Account::from_ini(&ini("url=https://h/dav\nroms=no")).unwrap();
        assert_eq!(art.folders(false), ["Saves", "States", ART]);
    }

    #[test]
    fn the_manifest_survives_a_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("m");
        let mut seen = BTreeMap::new();
        seen.insert(
            "Saves/GBA/a b.sav".to_string(),
            Seen {
                local: 12,
                size: 3,
                tag: "\"x\ty\"".into(),
            },
        );
        write_manifest(&path, &seen).unwrap();
        let back = read_manifest(&path);
        assert_eq!(back["Saves/GBA/a b.sav"].tag, "\"x y\"");
        assert_eq!(back["Saves/GBA/a b.sav"].local, 12);
    }
}
