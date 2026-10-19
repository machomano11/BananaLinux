//! banana-cloud: BananaOS's cloud sync. Keeps the card's saves and states, and with `all` its games, in step with a
//! WebDAV folder, with Slot's own sync (slot/, imported from github.com/machomano11/slot), so the RG DS shares one
//! folder with Slot on the TrimUI Brick and the RG SP. Where each file is on the card: layout.rs.
//!
//!   banana-cloud          the saves and the states (banana-ui runs it at the start, after each game, every 10 minutes)
//!   banana-cloud all      the games too (Settings > Network > Cloud sync > Sync now)
//!
//! The account is banana/cloudsync.ini on the card, the same file as Slot's Config/cloudsync.ini: `url=` (the
//! folder), `username=`, `password=`, and `enabled=0` to stop, `roms=0` to leave the games out. Without it nothing
//! happens. What it learns is kept in banana/cloudsync.manifest.
//!
//! How it went is one line in /run/banana/cloud.status: "running <done> <total> <what>|<file>" while it runs, then
//! "done <sent> <received> <conflicts> <skipped>", "offline <why>", "error <why>" or "off" (no account). SIGTERM
//! stops it between files (a game is starting; the running one's pid is in /run/banana/cloud.lock). One run at a
//! time: a second one while one runs says so and leaves. BANANA_CARD and BANANA_RUN put /data and /run/banana
//! elsewhere (the tests).

use banana_cloud::layout;

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use slot_cloud::{Account, Report};

static CANCEL: AtomicBool = AtomicBool::new(false);

extern "C" fn stop(_: libc::c_int) {
    CANCEL.store(true, Ordering::Relaxed);
}

fn main() {
    let all = std::env::args().nth(1).is_some_and(|a| a == "all");
    let data = PathBuf::from(std::env::var_os("BANANA_CARD").unwrap_or_else(|| "/data".into()));
    let run = PathBuf::from(std::env::var_os("BANANA_RUN").unwrap_or_else(|| "/run/banana".into()));
    let _ = std::fs::create_dir_all(&run);
    let status = Status(run.join("cloud.status"));

    let Some(account) = Account::from_ini(&read_ini(&data.join("banana/cloudsync.ini"))) else {
        status.set("off");
        return;
    };
    let Some(_lock) = Lock::take(&run.join("cloud.lock")) else {
        eprintln!("banana-cloud: already running");
        return;
    };
    // SAFETY: the handler only stores to an atomic.
    unsafe {
        libc::signal(libc::SIGTERM, stop as *const () as libc::sighandler_t);
        libc::signal(libc::SIGINT, stop as *const () as libc::sighandler_t);
    }
    // a run of the games can take a long time: bananad doesn't sleep for idleness while it goes on
    let _awake = all.then(|| Awake::take(&run.join("awake.pid"))).flatten();

    let systems = std::fs::read_to_string(data.join("banana/systems.conf"))
        .or_else(|_| std::fs::read_to_string("/etc/banana/systems.conf"))
        .unwrap_or_default();
    let card = layout::Banana::new(&data, &systems);
    let mut folders = slot_cloud::PROGRESS.to_vec();
    if all && account.games {
        folders.push(slot_cloud::GAMES);
    }
    eprintln!("banana-cloud: syncing {} with {}", folders.join(", "), account.url);
    status.set(&format!("running 0 0 {}|", if all { "Games" } else { "Saves" }));
    let mut remote = account.remote();
    let result = slot_cloud::sync_layout(
        &card,
        &data.join("banana/cloudsync.manifest"),
        &mut remote,
        &folders,
        &CANCEL,
        &|_| false,
    );
    match result {
        Ok(r) => {
            log(&r);
            status.set(&format!(
                "done {} {} {} {}",
                r.uploaded,
                r.downloaded,
                r.conflicts.len(),
                r.failed.len() + r.no_room.len()
            ));
        }
        Err(e) if e.offline => {
            eprintln!("banana-cloud: offline: {e}");
            status.set(&format!("offline {e}"));
        }
        Err(e) => {
            eprintln!("banana-cloud: {e}");
            status.set(&format!("error {e}"));
        }
    }
}

fn log(r: &Report) {
    eprintln!(
        "banana-cloud: sent {}, received {}, deleted {}{}",
        r.uploaded,
        r.downloaded,
        r.deleted,
        if r.cancelled { ", stopped" } else { "" }
    );
    for c in &r.conflicts {
        eprintln!("banana-cloud: {c} changed here and on the server: the older copy is kept as a .conflict file");
    }
    for (rel, why) in &r.failed {
        eprintln!("banana-cloud: skipped {rel}: {why}");
    }
    for rel in &r.no_room {
        eprintln!("banana-cloud: no room on the card for {rel}");
    }
}

/// `key=value` lines, as Slot reads its cloudsync.ini (`#` and `;` comments, `[sections]` ignored).
fn read_ini(path: &Path) -> HashMap<String, String> {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with(['#', ';', '[']))
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .filter(|(k, _)| !k.is_empty())
        .collect()
}

/// The status line, written beside and renamed over, so banana-ui never reads half of one.
struct Status(PathBuf);

impl Status {
    fn set(&self, line: &str) {
        let line = line.replace('\n', " ");
        let tmp = self.0.with_extension("tmp");
        let written = std::fs::File::create(&tmp).and_then(|mut f| writeln!(f, "{line}"));
        if written.and_then(|_| std::fs::rename(&tmp, &self.0)).is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
    }
}

/// An flock()ed file holding our pid (banana-ui stops the run with it when a game starts): released when the run
/// ends, however it ends, and emptied when it ends normally.
struct Lock(std::fs::File);

impl Lock {
    fn take(path: &Path) -> Option<Lock> {
        use std::os::fd::AsRawFd;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(path)
            .ok()?;
        // SAFETY: a file descriptor this function owns.
        if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return None;
        }
        let _ = f.set_len(0).and_then(|_| writeln!(f, "{}", std::process::id()));
        Some(Lock(f))
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        let _ = self.0.set_len(0);
    }
}

/// Our pid in awake.pid (when no other job's is there, banana-scrape's), taken away again at the end.
struct Awake(PathBuf);

impl Awake {
    fn take(path: &Path) -> Option<Awake> {
        let other: i32 = std::fs::read_to_string(path)
            .ok()
            .and_then(|p| p.trim().parse().ok())
            .unwrap_or(0);
        // SAFETY: kill with signal 0 only asks whether the process is there.
        if other > 0 && unsafe { libc::kill(other, 0) } == 0 {
            return None;
        }
        std::fs::write(path, format!("{}\n", std::process::id())).ok()?;
        Some(Awake(path.to_path_buf()))
    }
}

impl Drop for Awake {
    fn drop(&mut self) {
        let ours = std::fs::read_to_string(&self.0).is_ok_and(|p| p.trim() == std::process::id().to_string());
        if ours {
            let _ = std::fs::remove_file(&self.0);
        }
    }
}
