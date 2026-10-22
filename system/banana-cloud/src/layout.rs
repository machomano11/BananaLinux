//! Where the server's files are on a BananaOS card. The server keeps them as a Slot card does (`Saves/GBA/<game>.sav`,
//! `Games/GBA/...`), so the Brick, the RG SP and the RG DS share one folder; BananaOS keeps them its own way:
//!
//! | server                                   | card                                   |
//! |------------------------------------------|----------------------------------------|
//! | `Saves/<P>/<game>.sav`                   | `saves/<game>.srm` (RetroArch's)        |
//! | `Saves/NDS/<game>.sav`                   | `saves/nds/<game>.sav` (BananaDS's raw save, which `nds-saves` shares with DraStic) |
//! | `States/<P>/retroarch/<game>/<file>`     | `states/<file>` (RetroArch's `<game>.state`, `.state1`, `.state.auto`...) |
//! | `Games/<P>/<path>`                       | `roms/<system folder>/<path>`, not its `media` folder or `gamelist.xml` |
//!
//! `<P>` is Slot's name for the system (GBA, GB, GBC, NES, SNES, MD, PS; NDS and N64 are BananaOS's). Slot runs the
//! same libretro cores as BananaOS's RetroArch for those, so a save made on one is the save on the other. A save or a
//! state belongs to the system whose folder has the game: RetroArch keeps every system's saves in one folder, and
//! the game's folder is the only place that says which system a save is for. A save whose game isn't on the card is
//! left alone (on the card and on the server). RetroArch's states aren't Slot's, so they have a core folder of their
//! own (`retroarch`), which Slot never opens.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use slot_cloud::{skipped, Layout};

/// BananaOS's systems (systems.conf's ids) that sync, and their names on the server.
pub const SYSTEMS: [(&str, &str); 9] = [
    ("gba", "GBA"),
    ("gb", "GB"),
    ("gbc", "GBC"),
    ("nes", "NES"),
    ("snes", "SNES"),
    ("megadrive", "MD"),
    ("psx", "PS"),
    ("n64", "N64"),
    ("nds", "NDS"),
];

/// The DS's saves are BananaDS's, not RetroArch's.
const NDS: &str = "NDS";
/// RetroArch's states' core folder on the server.
const RETROARCH: &str = "retroarch";
/// What a game's folder keeps beside the games: the art and the scraped names (`banana-scrape`).
const NOT_GAMES: [&str; 2] = ["media", "gamelist.xml"];

pub struct Banana {
    data: PathBuf,
    /// The server's system name to the system's folder under `roms`.
    folders: BTreeMap<&'static str, String>,
    /// Each game's name (its file name without the extension) to the system it is in; the first system in
    /// [`SYSTEMS`] wins for a name in two folders.
    owner: BTreeMap<String, &'static str>,
    /// The systems picked to sync on this handheld (Settings > Network > Cloud systems); `None` for all of them.
    picked: Option<BTreeSet<String>>,
}

impl Banana {
    /// Reads the systems' folders from `systems` (systems.conf) and the games in them under `data`/roms.
    pub fn new(data: &Path, systems: &str) -> Banana {
        let conf = parse_systems(systems);
        let mut folders = BTreeMap::new();
        let mut owner = BTreeMap::new();
        for (id, server) in SYSTEMS {
            let Some(folder) = conf.get(id) else { continue };
            // an app that ships with the system (an absolute folder) has no games to sync
            if folder.starts_with('/') {
                continue;
            }
            for name in game_names(&data.join("roms").join(folder)) {
                owner.entry(name).or_insert(server);
            }
            folders.insert(server, folder.clone());
        }
        Banana {
            data: data.to_path_buf(),
            folders,
            owner,
            picked: None,
        }
    }

    /// Syncs only these systems (the server's names): the others' files are left alone, here and on the server.
    pub fn only(mut self, systems: &[String]) -> Banana {
        self.picked = Some(systems.iter().cloned().collect());
        self
    }

    fn picked(&self, server: &str) -> bool {
        self.picked.as_ref().is_none_or(|p| p.contains(server))
    }

    fn owns(&self, game: &str, server: &str) -> bool {
        self.owner.get(game).is_some_and(|s| *s == server)
    }

    fn saves(&self) -> PathBuf {
        self.data.join("saves")
    }

    fn states(&self) -> PathBuf {
        self.data.join("states")
    }
}

impl Layout for Banana {
    fn local(&self, rel: &str) -> Option<PathBuf> {
        let parts: Vec<&str> = rel.split('/').collect();
        if !self.picked(parts.get(1)?) {
            return None;
        }
        match parts.as_slice() {
            ["Saves", sys, file] => {
                let game = file.strip_suffix(".sav")?;
                if !self.owns(game, sys) {
                    return None;
                }
                Some(match *sys {
                    NDS => self.saves().join("nds").join(file),
                    _ => self.saves().join(format!("{game}.srm")),
                })
            }
            ["States", sys, RETROARCH, game, file] => {
                (*sys != NDS && self.owns(game, sys) && state_of(file) == Some(*game)).then(|| self.states().join(file))
            }
            ["Games", sys, path @ ..] if !path.is_empty() => {
                if NOT_GAMES.contains(&path[0]) || path.iter().any(|p| skipped(p)) {
                    return None;
                }
                let folder = self.folders.get(sys)?;
                Some(path.iter().fold(self.data.join("roms").join(folder), |p, c| p.join(c)))
            }
            _ => None,
        }
    }

    fn walk(&self, folder: &str, found: &mut dyn FnMut(String, PathBuf)) {
        self.walk_all(folder, &mut |rel: String, path| {
            if rel.split('/').nth(1).is_some_and(|sys| self.picked(sys)) {
                found(rel, path);
            }
        });
    }

    fn root(&self) -> &Path {
        &self.data
    }
}

impl Banana {
    /// Every file under `folder`, picked or not.
    fn walk_all(&self, folder: &str, found: &mut dyn FnMut(String, PathBuf)) {
        match folder {
            "Saves" => {
                for (name, path) in files(&self.saves()) {
                    let Some(game) = name.strip_suffix(".srm") else {
                        continue;
                    };
                    match self.owner.get(game) {
                        Some(&sys) if sys != NDS => found(format!("Saves/{sys}/{game}.sav"), path),
                        _ => {}
                    }
                }
                for (name, path) in files(&self.saves().join("nds")) {
                    match name.strip_suffix(".sav") {
                        Some(game) if self.owns(game, NDS) => found(format!("Saves/{NDS}/{name}"), path),
                        _ => {}
                    }
                }
            }
            "States" => {
                for (name, path) in files(&self.states()) {
                    let Some(game) = state_of(&name) else { continue };
                    match self.owner.get(game) {
                        Some(&sys) if sys != NDS => found(format!("States/{sys}/{RETROARCH}/{game}/{name}"), path),
                        _ => {}
                    }
                }
            }
            "Games" => {
                for (sys, dir) in &self.folders {
                    let root = self.data.join("roms").join(dir);
                    let mut todo = vec![(root, format!("Games/{sys}"), true)];
                    while let Some((dir, rel, top)) = todo.pop() {
                        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
                        for e in entries.flatten() {
                            let name = e.file_name().to_string_lossy().into_owned();
                            if skipped(&name) || (top && NOT_GAMES.contains(&name.as_str())) {
                                continue;
                            }
                            let child = format!("{rel}/{name}");
                            match e.file_type() {
                                Ok(t) if t.is_dir() => todo.push((e.path(), child, false)),
                                Ok(_) => found(child, e.path()),
                                Err(_) => {}
                            }
                        }
                    }
                }
            }
            // Slot's cartridge art: BananaOS keeps its art its own way
            _ => {}
        }
    }
}

/// The game a RetroArch state is of: `Zelda.state`, `Zelda.state3`, `Zelda.state.auto` are Zelda's.
fn state_of(file: &str) -> Option<&str> {
    let at = file.rfind(".state")?;
    let rest = &file[at + ".state".len()..];
    let ok = rest.is_empty() || rest == ".auto" || rest.bytes().all(|b| b.is_ascii_digit());
    (ok && at > 0).then(|| &file[..at])
}

/// A folder's files (not its folders), hidden ones and conflict copies left out.
fn files(dir: &Path) -> Vec<(String, PathBuf)> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
        .map(|e| (e.file_name().to_string_lossy().into_owned(), e.path()))
        .filter(|(name, _)| !skipped(name))
        .collect()
}

/// The names of the games in a system's folder and its subfolders (not `media`): file names without their extension.
/// A disc's other files (`.bin` beside a `.cue`) are names too, which only matters if a save is called the same.
fn game_names(dir: &Path) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let mut todo = vec![(dir.to_path_buf(), true)];
    while let Some((dir, top)) = todo.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if skipped(&name) || (top && NOT_GAMES.contains(&name.as_str())) {
                continue;
            }
            match e.file_type() {
                Ok(t) if t.is_dir() => todo.push((e.path(), false)),
                Ok(_) => {
                    let stem = name.rsplit_once('.').map_or(name.as_str(), |(s, _)| s);
                    out.insert(stem.to_string());
                }
                Err(_) => {}
            }
        }
    }
    out
}

/// systems.conf's systems: each `[id]` section's `folder` (the id when it has none).
fn parse_systems(text: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let mut id: Option<String> = None;
    for line in text.lines() {
        let line = line.trim();
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            out.insert(name.to_string(), name.to_string());
            id = Some(name.to_string());
        } else if let (Some(id), Some((k, v))) = (&id, line.split_once('=')) {
            if k.trim() == "folder" {
                out.insert(id.clone(), v.trim().to_string());
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const SYSTEMS_CONF: &str = "# Banana's systems\n[nds]\nname = Nintendo DS\n\n[gba]\next = gba zip\n\n[gb]\n[gbc]\n\
        [megadrive]\nfolder = megadrive\n[psx]\n[dodakvarter]\nfolder = /usr/share/banana/apps/dodakvarter\n";

    fn card() -> (tempfile::TempDir, Banana) {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        for f in [
            "roms/gba/Zelda.gba",
            "roms/gba/media/box/Zelda.png",
            "roms/gba/gamelist.xml",
            "roms/gb/Tetris.gb",
            "roms/gbc/Tetris.gbc",
            "roms/gbc/Hacks/Crystal Clear.gbc",
            "roms/nds/Mario Kart DS.nds",
            "roms/psx/FF7/FF7 (Disc 1).cue",
            "saves/Zelda.srm",
            "saves/Tetris.srm",
            "saves/Crystal Clear.srm",
            "saves/Gone.srm",
            "saves/.Zelda.srm.cloud.tmp",
            "saves/Zelda.srm.conflict-20261019-120000",
            "saves/nds/Mario Kart DS.sav",
            "saves/nds/Mario Kart DS.sav.bak",
            "states/Zelda.state",
            "states/Zelda.state2",
            "states/Zelda.state.auto",
            "states/Gone.state",
        ] {
            let p = d.join(f);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, f).unwrap();
        }
        let b = Banana::new(d, SYSTEMS_CONF);
        (dir, b)
    }

    fn walked(b: &Banana, folder: &str) -> Vec<String> {
        let mut out = Vec::new();
        b.walk(folder, &mut |rel, path| {
            assert_eq!(b.local(&rel), Some(path), "{rel} maps back to where it was found");
            out.push(rel);
        });
        out.sort();
        out
    }

    #[test]
    fn saves_go_to_the_system_whose_folder_has_the_game() {
        let (_d, b) = card();
        assert_eq!(
            walked(&b, "Saves"),
            [
                "Saves/GB/Tetris.sav",
                "Saves/GBA/Zelda.sav",
                "Saves/GBC/Crystal Clear.sav",
                "Saves/NDS/Mario Kart DS.sav",
            ]
        );
    }

    #[test]
    fn a_save_for_a_game_in_another_folder_or_not_here_is_left_alone() {
        let (d, b) = card();
        // Tetris is in gb (the first of the two): the GBC one is another card's
        assert_eq!(b.local("Saves/GBC/Tetris.sav"), None);
        assert_eq!(b.local("Saves/GBA/Metroid.sav"), None);
        assert_eq!(b.local("Saves/GBA/Zelda.srm"), None);
        assert_eq!(b.local("Saves/GBA/Zelda.sav"), Some(d.path().join("saves/Zelda.srm")));
    }

    #[test]
    fn states_have_a_core_folder_of_their_own() {
        let (_d, b) = card();
        assert_eq!(
            walked(&b, "States"),
            [
                "States/GBA/retroarch/Zelda/Zelda.state",
                "States/GBA/retroarch/Zelda/Zelda.state.auto",
                "States/GBA/retroarch/Zelda/Zelda.state2",
            ]
        );
        assert_eq!(b.local("States/GBA/retroarch/Zelda/Other.state"), None);
        assert_eq!(b.local("States/GBA/gpsp/Zelda/0.state"), None, "Slot's own states");
        assert!(slot_cloud::belongs_to(
            "States/GBA/retroarch/Zelda/Zelda.state",
            "GBA",
            "Zelda"
        ));
    }

    #[test]
    fn games_are_the_system_folders_without_their_art() {
        let (d, b) = card();
        assert_eq!(
            walked(&b, "Games"),
            [
                "Games/GB/Tetris.gb",
                "Games/GBA/Zelda.gba",
                "Games/GBC/Hacks/Crystal Clear.gbc",
                "Games/GBC/Tetris.gbc",
                "Games/NDS/Mario Kart DS.nds",
                "Games/PS/FF7/FF7 (Disc 1).cue",
            ]
        );
        assert_eq!(
            b.local("Games/MD/Sonic.md"),
            Some(d.path().join("roms/megadrive/Sonic.md"))
        );
        assert_eq!(b.local("Games/GBA/media/box/Zelda.png"), None);
        assert_eq!(
            b.local("Games/SMS/Alex Kidd.sms"),
            None,
            "a system BananaOS doesn't run"
        );
        assert_eq!(b.local("Games/N64/Mario.z64"), None, "n64 isn't in this systems.conf");
        assert_eq!(walked(&b, "Labels"), Vec::<String>::new());
    }

    #[test]
    fn state_names() {
        assert_eq!(state_of("Zelda.state"), Some("Zelda"));
        assert_eq!(state_of("Zelda.state12"), Some("Zelda"));
        assert_eq!(state_of("Zelda.state.auto"), Some("Zelda"));
        assert_eq!(state_of("A.state.of.mind.state1"), Some("A.state.of.mind"));
        assert_eq!(state_of("Zelda.state.png"), None);
        assert_eq!(state_of(".state"), None);
    }

    #[test]
    fn systems_not_picked_are_left_alone() {
        let (_d, b) = card();
        let b = b.only(&["GBA".to_string(), "NDS".to_string()]);
        assert_eq!(walked(&b, "Saves"), ["Saves/GBA/Zelda.sav", "Saves/NDS/Mario Kart DS.sav"]);
        assert_eq!(walked(&b, "Games"), ["Games/GBA/Zelda.gba", "Games/NDS/Mario Kart DS.nds"]);
        assert_eq!(b.local("Saves/GB/Tetris.sav"), None);
        assert_eq!(b.local("Games/PS/Crash.chd"), None);
    }
}
