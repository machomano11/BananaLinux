//! banana-cloud: BananaOS's cloud sync. Keeps the card's saves and states, and with `all` its games, in step with a
//! WebDAV folder, with Slot's own sync (slot/, imported from github.com/machomano11/slot), so the RG DS shares one
//! folder with Slot on the TrimUI Brick and the RG SP. Where each file is on the card: layout.rs.
//!
//!   banana-cloud            the saves and the states (banana-ui runs it at the start, after each game, every 10 minutes)
//!   banana-cloud all        the games too (Settings > Network > Cloud sync > Sync now)
//!
//! The account is banana/cloudsync.ini on the card, the same file as Slot's Config/cloudsync.ini: `url=` (the
//! folder), `username=`, `password=`, and `enabled=0` to stop, `roms=0` to leave the games out. Without it nothing
//! happens.
//!
//! This handheld, as on Slot (slot-cloud's devices.rs), is in banana/cloudsync-device.ini: `id=` (made up the first
//! time) and `systems=`, the systems it syncs as the server names them (`GBA,GB,NDS`; Settings > Network > Cloud
//! systems); without it, every system it has, and empty, none. A system left out is left alone, on the card and on
//! the server. Every run checks in at `Devices/<id>.ini` on the server with its name and those systems. What the last
//! run saw: banana/cloudsync.manifest.
//!
//! A game deleted from the server isn't deleted here, nor sent back, until someone says which: a run of the games
//! lists such games in banana/cloudsync-gone.txt (their server paths, `Games/GBA/x.gba`, one a line), banana-ui asks
//! (Settings > Network > Cloud sync), and writes the answers in banana/cloudsync-answers.txt (`delete <path>` or
//! `keep <path>`, a line each), which the next run of the games carries out and then removes. A game kept is kept on
//! this handheld only: it isn't sent back.
//!
//! How it went is one line in /run/banana/cloud.status: "running <done> <total> <what>|<file>" while it runs, then
//! "done <sent> <received> <conflicts> <skipped>", "offline <why>", "error <why>" or "off" (no account). SIGTERM
//! stops it between files (a game is starting; the running one's pid is in /run/banana/cloud.lock). One run at a
//! time: a second one while one runs says so and leaves. BANANA_CARD and BANANA_RUN put /data and /run/banana
//! elsewhere (the tests).

use banana_cloud::layout;

use std::collections::{BTreeMap, HashMap};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use slot_cloud::devices::{self, Device};
use slot_cloud::{Account, Answer, Report, Tally};

static CANCEL: AtomicBool = AtomicBool::new(false);

extern "C" fn stop(_: libc::c_int) {
    CANCEL.store(true, Ordering::Relaxed);
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let all = args.first().is_some_and(|a| a == "all");
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
    status.set("running 0 0 Comparing|");

    let (mut me, picked) = this_device(&data);
    me.seen = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    if let Err(e) = devices::check_in(&mut account.remote(), &me) {
        // the sync says so in a moment if the server is out of reach
        eprintln!("banana-cloud: checking in: {e}");
    }

    let systems = std::fs::read_to_string(data.join("banana/systems.conf"))
        .or_else(|_| std::fs::read_to_string("/etc/banana/systems.conf"))
        .unwrap_or_default();
    let mut card = layout::Banana::new(&data, &systems);
    if let Some(picked) = &picked {
        card = card.only(picked);
    }
    let mut folders = slot_cloud::PROGRESS.to_vec();
    let games = all && account.games;
    if games {
        folders.push(slot_cloud::GAMES);
    }
    let answers_file = data.join("banana/cloudsync-answers.txt");
    let answers = read_answers(&answers_file);
    eprintln!(
        "banana-cloud: syncing {} of {} with {}",
        folders.join(", "),
        picked.as_ref().map_or("every system".into(), |p| p.join(", ")),
        account.url
    );
    let mut remote = account.remote();
    let manifest = data.join("banana/cloudsync.manifest");
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
        let r = slot_cloud::sync_asking(
            &card,
            &manifest,
            &mut remote,
            &folders,
            &CANCEL,
            &|_| false,
            &tally,
            &answers,
        );
        done.store(true, Ordering::Relaxed);
        r
    });
    match result {
        Ok(r) => {
            log(&r);
            // what waits for an answer, after a run that looked at the games all the way through (the answers it was
            // given are carried out by then)
            if games && !r.cancelled {
                write_lines(&data.join("banana/cloudsync-gone.txt"), &r.gone);
                let _ = std::fs::remove_file(&answers_file);
            }
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

/// This handheld as banana/cloudsync-device.ini has it, given an id the first time ("rgds-" and four hex digits),
/// and the systems picked to sync (`None`: all of them; an empty `systems=`, none).
fn this_device(data: &Path) -> (Device, Option<Vec<String>>) {
    let saved = read_ini(&data.join(DEVICE_FILE));
    let mut me = Device {
        id: saved
            .get("id")
            .filter(|id| devices::id_ok(id))
            .cloned()
            .unwrap_or_default(),
        name: model(),
        systems: Vec::new(),
        seen: 0,
    };
    let picked = saved.get("systems").map(|s| systems_of(s));
    // on the server, as Slot's devices say it: the systems it syncs
    me.systems = picked
        .clone()
        .unwrap_or_else(|| layout::SYSTEMS.iter().map(|(_, p)| p.to_string()).collect());
    if me.id.is_empty() {
        me.id = format!("rgds-{:04x}", random16());
        save_device(data, &me.id, picked.as_deref());
    }
    (me, picked)
}

/// `GBA, gb,NDS` as the server's names: GBA, GB, NDS.
fn systems_of(list: &str) -> Vec<String> {
    devices::systems(&list.to_ascii_uppercase())
}

const DEVICE_FILE: &str = "banana/cloudsync-device.ini";

fn save_device(data: &Path, id: &str, picked: Option<&[String]>) {
    let mut text = format!("id={id}\n");
    if let Some(p) = picked {
        text.push_str(&format!("systems={}\n", p.join(",")));
    }
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
    for rel in &r.gone {
        eprintln!(
            "banana-cloud: {rel} was deleted from the server: waiting for an answer (Settings > Network > Cloud sync)"
        );
    }
}

/// banana/cloudsync-answers.txt: `delete <server path>` or `keep <server path>`, a line each.
fn read_answers(path: &Path) -> BTreeMap<String, Answer> {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    text.lines()
        .filter_map(|l| l.trim_end().split_once(' '))
        .filter_map(|(a, rel)| match a {
            "delete" => Some((rel.to_string(), Answer::Delete)),
            "keep" => Some((rel.to_string(), Answer::Keep)),
            _ => None,
        })
        .collect()
}

/// The lines in a file, written beside and renamed over; none: no file.
fn write_lines(path: &Path, lines: &[String]) {
    if lines.is_empty() {
        let _ = std::fs::remove_file(path);
        return;
    }
    let tmp = path.with_extension("tmp");
    let written = std::fs::write(&tmp, lines.iter().map(|l| format!("{l}\n")).collect::<String>());
    if written.and_then(|_| std::fs::rename(&tmp, path)).is_err() {
        let _ = std::fs::remove_file(&tmp);
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
