//! banana-cloud: BananaOS's cloud sync. Keeps the card's saves and states, and with `all` its games, in step with a
//! WebDAV folder, with Slot's own sync (slot/, imported from github.com/machomano11/slot), so the RG DS shares one
//! folder with Slot on the TrimUI Brick and the RG SP. Where each file is on the card: layout.rs.
//!
//!   banana-cloud            the saves and the states (banana-ui runs it at the start, after each game, every 10 minutes)
//!   banana-cloud all        the games too (Settings > Network > Cloud sync > Sync now)
//!   banana-cloud share <id> start sharing with that device, or stop if this one already does; then the saves and
//!                           the states in the new space (Settings > Network > Cloud devices)
//!
//! The account is banana/cloudsync.ini on the card, the same file as Slot's Config/cloudsync.ini: `url=` (the
//! folder), `username=`, `password=`, and `enabled=0` to stop, `roms=0` to leave the games out. Without it nothing
//! happens.
//!
//! Devices, as on Slot (slot-cloud's devices.rs): every run checks in at `Devices/<id>.ini` on the server with this
//! handheld's name and the space it syncs in (the top of the folder, or `Spaces/<name>`); devices in one space share
//! everything in it. This one's id and space are in banana/cloudsync-device.ini, what the last run in each space saw in
//! banana/cloudsync.manifest (the top) or banana/cloudsync-<name>.manifest. The devices on the server, this one first,
//! go to /run/banana/cloud.devices, a line each: "<id>\t<name>\t<space>\t<last seen>".
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
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use slot_cloud::devices::{self, Device};
use slot_cloud::{Account, Report, Tally};

static CANCEL: AtomicBool = AtomicBool::new(false);

extern "C" fn stop(_: libc::c_int) {
    CANCEL.store(true, Ordering::Relaxed);
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let all = args.first().is_some_and(|a| a == "all");
    let share = match args.as_slice() {
        [c, id] if c == "share" => Some(id.clone()),
        _ => None,
    };
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
    status.set(&format!(
        "running 0 0 {}|",
        if share.is_some() { "Moving" } else { "Comparing" }
    ));

    // check in, and move to another space first when asked
    let mut me = this_device(&data);
    me.seen = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let mut top = account.remote();
    let checked = devices::check_in(&mut top, &me);
    let known = match (&checked, &share) {
        (Ok(list), _) => list.clone(),
        (Err(e), Some(_)) => return failed(&status, e),
        (Err(e), None) => {
            // the sync says so in a moment if the server is out of reach; the list is only for the settings
            eprintln!("banana-cloud: checking in: {e}");
            Vec::new()
        }
    };
    if let Some(id) = share {
        let Some(other) = known.iter().find(|d| d.id == id) else {
            status.set(&format!("error no device {id} on the server"));
            return;
        };
        let to = match other.space == me.space {
            true => me.own_space(&me.space),
            false => other.space.clone(),
        };
        eprintln!(
            "banana-cloud: {} {}: now syncing in {to:?}",
            if other.space == me.space {
                "stopped sharing with"
            } else {
                "sharing with"
            },
            other.name
        );
        me.space = devices::clean_space(&to);
        save_device(&data, &me);
        if let Err(e) = devices::check_in(&mut top, &me) {
            return failed(&status, &e);
        }
    }
    let known: Vec<Device> = known
        .into_iter()
        .map(|d| if d.id == me.id { me.clone() } else { d })
        .collect();
    write_devices(&run.join("cloud.devices"), &me, &known);

    let systems = std::fs::read_to_string(data.join("banana/systems.conf"))
        .or_else(|_| std::fs::read_to_string("/etc/banana/systems.conf"))
        .unwrap_or_default();
    let card = layout::Banana::new(&data, &systems);
    let mut folders = slot_cloud::PROGRESS.to_vec();
    if all && account.games {
        folders.push(slot_cloud::GAMES);
    }
    eprintln!(
        "banana-cloud: syncing {} with {} in {:?}",
        folders.join(", "),
        account.url,
        me.space
    );
    let mut remote = account.remote_in(&me.space);
    let manifest = data.join(manifest_for(&me.space));
    let tally = Tally::default();
    let done = AtomicBool::new(false);
    let result = std::thread::scope(|s| {
        // the progress, twice a second, for the settings screen
        s.spawn(|| {
            let mut last = String::new();
            while !done.load(Ordering::Relaxed) {
                let m = tally.read();
                let what = match (m.files, m.name.is_empty(), m.sending) {
                    (0, _, _) => "Comparing",
                    (_, true, _) => "Syncing",
                    (_, false, true) => "Sending",
                    (_, false, false) => "Receiving",
                };
                let line = format!("running {} {} {what}|{}", m.moved, m.files, m.name);
                if line != last {
                    status.set(&line);
                    last = line;
                }
                std::thread::sleep(Duration::from_millis(500));
            }
        });
        let r = slot_cloud::sync_layout(&card, &manifest, &mut remote, &folders, &CANCEL, &|_| false, &tally);
        done.store(true, Ordering::Relaxed);
        r
    });
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
        Err(e) => failed(&status, &e),
    }
}

fn failed(status: &Status, e: &slot_cloud::Error) {
    eprintln!("banana-cloud: {}{e}", if e.offline { "offline: " } else { "" });
    status.set(&format!("{} {e}", if e.offline { "offline" } else { "error" }));
}

/// This handheld as banana/cloudsync-device.ini has it, given an id the first time ("rgds-" and four hex digits).
fn this_device(data: &Path) -> Device {
    let file = data.join(DEVICE_FILE);
    let saved = read_ini(&file);
    let mut me = Device {
        id: saved
            .get("id")
            .filter(|id| devices::id_ok(id))
            .cloned()
            .unwrap_or_default(),
        name: model(),
        space: devices::clean_space(saved.get("space").map_or("", |s| s.as_str())),
        seen: 0,
    };
    if me.id.is_empty() {
        me.id = format!("rgds-{:04x}", random16());
        save_device(data, &me);
    }
    me
}

const DEVICE_FILE: &str = "banana/cloudsync-device.ini";

fn save_device(data: &Path, me: &Device) {
    let text = format!("id={}\nspace={}\n", me.id, me.space);
    let path = data.join(DEVICE_FILE);
    let tmp = path.with_extension("tmp");
    if let Err(e) = std::fs::write(&tmp, text).and_then(|_| std::fs::rename(&tmp, &path)) {
        eprintln!("banana-cloud: saving {}: {e}", path.display());
    }
}

/// The handheld's name for the other devices: the device tree's ("Anbernic RG DS Plus"), else the RG DS's.
fn model() -> String {
    let m = std::fs::read_to_string("/proc/device-tree/model").unwrap_or_default();
    let m = m.trim_matches(|c: char| c == '\0' || c.is_whitespace());
    match m.contains("RG DS") || m.contains("RGDS") {
        true => m.to_string(),
        false => "Anbernic RG DS".into(),
    }
}

fn random16() -> u16 {
    use std::io::Read;
    let mut b = [0u8; 2];
    match std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut b)) {
        Ok(()) => u16::from_le_bytes(b),
        Err(_) => SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.subsec_nanos() as u16),
    }
}

/// What the last run in a space saw: banana/cloudsync.manifest for the top, banana/cloudsync-<name>.manifest else.
fn manifest_for(space: &str) -> String {
    match space.rsplit('/').next().filter(|_| !space.is_empty()) {
        Some(name) => format!("banana/cloudsync-{name}.manifest"),
        None => "banana/cloudsync.manifest".into(),
    }
}

/// The devices on the server for the settings screen, this one first.
fn write_devices(path: &Path, me: &Device, all: &[Device]) {
    let line = |d: &Device| format!("{}\t{}\t{}\t{}\n", d.id, d.name.replace('\t', " "), d.space, d.seen);
    let mut text = line(me);
    for d in all.iter().filter(|d| d.id != me.id) {
        text.push_str(&line(d));
    }
    if all.is_empty() {
        return; // (out of reach: the list from the last run stays)
    }
    let tmp = path.with_extension("tmp");
    if std::fs::write(&tmp, text)
        .and_then(|_| std::fs::rename(&tmp, path))
        .is_err()
    {
        let _ = std::fs::remove_file(&tmp);
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
