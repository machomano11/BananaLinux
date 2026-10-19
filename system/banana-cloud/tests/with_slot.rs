//! A Slot card and a BananaOS card meeting through a real WebDAV server: set BANANA_WEBDAV_URL (an empty folder),
//! and BANANA_WEBDAV_USER and BANANA_WEBDAV_PASSWORD when it needs them, then
//! `cargo test --test with_slot -- --ignored`.

use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, SystemTime};

use banana_cloud::layout::Banana;
use slot_cloud::{sync_holding, sync_layout, WebDav, FOLDERS, GAMES, PROGRESS};

fn dav() -> WebDav {
    let url = std::env::var("BANANA_WEBDAV_URL").expect("BANANA_WEBDAV_URL");
    let user = std::env::var("BANANA_WEBDAV_USER").ok();
    let pw = std::env::var("BANANA_WEBDAV_PASSWORD").ok();
    WebDav::new(&url, user.as_deref(), pw.as_deref())
}

/// A file written a minute ago (sync leaves files written in the last few seconds for the next run).
fn aged(path: &Path, bytes: &[u8], ago: u64) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, bytes).unwrap();
    let f = std::fs::File::options().write(true).open(path).unwrap();
    f.set_modified(SystemTime::now() - Duration::from_secs(ago)).unwrap();
}

#[test]
#[ignore = "needs a WebDAV server"]
fn a_save_goes_from_slot_to_bananaos_and_back() {
    let no = AtomicBool::new(false);
    let slot = tempfile::tempdir().unwrap();
    let banana = tempfile::tempdir().unwrap();
    let s = slot.path();
    let b = banana.path();
    // the Brick plays Zelda and has Tetris; the RG DS has Zelda and a DS game
    aged(&s.join("Games/GBA/Zelda.gba"), b"zelda rom", 600);
    aged(&s.join("Saves/GBA/Zelda.sav"), b"zelda save from the brick", 600);
    aged(&s.join("Games/GB/Tetris.gb"), b"tetris rom", 600);
    aged(&s.join("States/GBA/gpsp/Zelda/0.state"), b"slot's own state", 600);
    aged(&b.join("roms/gba/Zelda.gba"), b"zelda rom", 600);
    aged(&b.join("roms/nds/Mario Kart DS.nds"), b"mario kart rom", 600);
    aged(&b.join("saves/nds/Mario Kart DS.sav"), b"mario kart save", 600);
    aged(&b.join("states/Zelda.state1"), b"retroarch state", 600);
    let systems = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../rootfs/etc/banana/systems.conf"
    ))
    .unwrap();
    let sm = s.join("Config/cloudsync.manifest");
    let bm = b.join("banana/cloudsync.manifest");

    let r = sync_holding(s, &sm, &mut dav(), &FOLDERS, &no, &|_| false).unwrap();
    assert_eq!(r.uploaded, 4);

    // the RG DS: Zelda's save comes down as RetroArch's, the DS save and the state go up; the games, then Tetris too
    let r = sync_layout(&Banana::new(b, &systems), &bm, &mut dav(), &PROGRESS, &no, &|_| false).unwrap();
    assert_eq!(
        std::fs::read(b.join("saves/Zelda.srm")).unwrap(),
        b"zelda save from the brick"
    );
    assert_eq!((r.uploaded, r.downloaded), (2, 1));
    assert!(!b.join("states/0.state").exists(), "Slot's states stay on Slot");
    let mut games = PROGRESS.to_vec();
    games.push(GAMES);
    sync_layout(&Banana::new(b, &systems), &bm, &mut dav(), &games, &no, &|_| false).unwrap();
    assert_eq!(std::fs::read(b.join("roms/gb/Tetris.gb")).unwrap(), b"tetris rom");

    // played on the RG DS: the Brick gets the new save under its own name
    aged(&b.join("saves/Zelda.srm"), b"zelda save from the rg ds", 30);
    let r = sync_layout(&Banana::new(b, &systems), &bm, &mut dav(), &PROGRESS, &no, &|_| false).unwrap();
    assert_eq!((r.uploaded, r.downloaded, r.conflicts.len()), (1, 0, 0));
    let r = sync_holding(s, &sm, &mut dav(), &FOLDERS, &no, &|_| false).unwrap();
    assert_eq!(
        std::fs::read(s.join("Saves/GBA/Zelda.sav")).unwrap(),
        b"zelda save from the rg ds"
    );
    assert!(r.conflicts.is_empty());
    assert!(
        s.join("States/GBA/retroarch/Zelda/Zelda.state1").exists(),
        "RetroArch's states have a core folder of their own on Slot"
    );
}
