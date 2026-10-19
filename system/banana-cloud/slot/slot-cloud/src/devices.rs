//! Which devices share a space on the server.
//!
//! Every device checks in at `Devices/<id>.ini` at the top of the server
//! folder, saying its name and the space it syncs with:
//!
//! ```text
//! name=TrimUI Brick
//! space=
//! seen=1792450000
//! ```
//!
//! An empty `space` is the top of the folder itself, where every device
//! starts and where `Saves`, `States`, `Games` and `Labels` lived before
//! devices could be picked. Any other space is `Spaces/<name>`, and has the
//! same four folders. Devices in the same space share everything in it;
//! a device that stops sharing moves to a space of its own and takes its
//! files with it on its next run. Nothing is ever deleted from a space left
//! behind.

use crate::{Error, Remote};

pub const DIR: &str = "Devices";
pub const SPACES: &str = "Spaces";

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Device {
    /// Letters, digits and `-`: the file name under [`DIR`].
    pub id: String,
    /// What the device is, for people.
    pub name: String,
    /// Where on the server it syncs: empty, or `Spaces/<name>`.
    pub space: String,
    /// When it last checked in, in seconds since 1970.
    pub seen: u64,
}

impl Device {
    pub fn to_ini(&self) -> String {
        format!(
            "name={}\nspace={}\nseen={}\n",
            self.name.replace('\n', " "),
            self.space,
            self.seen
        )
    }

    pub fn from_ini(id: &str, text: &str) -> Device {
        let mut d = Device {
            id: id.to_string(),
            ..Device::default()
        };
        for line in text.lines() {
            let Some((k, v)) = line.split_once('=') else {
                continue;
            };
            let v = v.trim();
            match k.trim() {
                "name" => d.name = v.to_string(),
                "space" => d.space = clean_space(v),
                "seen" => d.seen = v.parse().unwrap_or(0),
                _ => {}
            }
        }
        if d.name.is_empty() {
            d.name = d.id.clone();
        }
        d
    }

    /// A space of this device's own, other than `not`.
    pub fn own_space(&self, not: &str) -> String {
        let first = format!("{SPACES}/{}", self.id);
        match first == not {
            true => format!("{first}-{}", self.seen),
            false => first,
        }
    }
}

/// A space as written by anyone: `Spaces/<name>` with a safe name, or the top.
pub fn clean_space(s: &str) -> String {
    match s
        .trim()
        .trim_matches('/')
        .strip_prefix(&format!("{SPACES}/"))
    {
        Some(name) if id_ok(name) => format!("{SPACES}/{name}"),
        _ => String::new(),
    }
}

pub fn id_ok(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Writes `me` to the server's list of devices and reads the whole list,
/// `me` included. `remote` is the top of the server folder.
pub fn check_in(remote: &mut dyn Remote, me: &Device) -> Result<Vec<Device>, Error> {
    if remote.list("")?.is_none() {
        remote.mkdir("")?;
    }
    let mut entries = remote.list(DIR)?;
    if entries.is_none() {
        remote.mkdir(DIR)?;
    }
    if !me.space.is_empty() && remote.list(&me.space)?.is_none() {
        if remote.list(SPACES)?.is_none() {
            remote.mkdir(SPACES)?;
        }
        remote.mkdir(&me.space)?;
    }
    let body = me.to_ini();
    remote.put(
        &format!("{DIR}/{}.ini", me.id),
        &mut body.as_bytes(),
        body.len() as u64,
    )?;
    let mut out = vec![me.clone()];
    for e in entries.take().unwrap_or_default() {
        let Some(id) = e.name.strip_suffix(".ini").filter(|id| id_ok(id)) else {
            continue;
        };
        if e.is_dir || id == me.id || e.size > 4096 {
            continue;
        }
        let mut bytes = Vec::new();
        remote.get(&format!("{DIR}/{}", e.name), &mut bytes)?;
        out.push(Device::from_ini(id, &String::from_utf8_lossy(&bytes)));
    }
    Ok(out)
}
