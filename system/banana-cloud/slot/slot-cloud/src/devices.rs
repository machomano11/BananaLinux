//! The devices that sync with the server, and which systems each one syncs.
//!
//! Every device checks in at `Devices/<id>.ini` at the top of the server
//! folder, saying its name, the systems it syncs, and when it last did:
//!
//! ```text
//! name=TrimUI Brick
//! systems=GBA,GB,GBC
//! seen=1792450000
//! ```
//!
//! Systems are the platform folder names under `Saves`, `States`, `Games`
//! and `Labels`. A device syncs those four folders only for its systems,
//! and leaves everything else on the server alone. Every device shares the
//! one server folder.

use crate::{Error, Remote};

pub const DIR: &str = "Devices";

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Device {
    /// Letters, digits and `-`: the file name under [`DIR`].
    pub id: String,
    /// What the device is, for people.
    pub name: String,
    /// The systems it syncs.
    pub systems: Vec<String>,
    /// When it last checked in, in seconds since 1970.
    pub seen: u64,
}

impl Device {
    pub fn to_ini(&self) -> String {
        format!(
            "name={}\nsystems={}\nseen={}\n",
            self.name.replace('\n', " "),
            self.systems.join(","),
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
                "systems" => d.systems = systems(v),
                "seen" => d.seen = v.parse().unwrap_or(0),
                _ => {}
            }
        }
        if d.name.is_empty() {
            d.name = d.id.clone();
        }
        d
    }
}

/// A `systems=` value: names separated by commas, each a safe folder name.
pub fn systems(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|s| id_ok(s))
        .map(str::to_string)
        .collect()
}

pub fn id_ok(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Writes `me` to the server's list of devices and reads the whole list,
/// `me` first.
pub fn check_in(remote: &mut dyn Remote, me: &Device) -> Result<Vec<Device>, Error> {
    if remote.list("")?.is_none() {
        remote.mkdir("")?;
    }
    let mut entries = remote.list(DIR)?;
    if entries.is_none() {
        remote.mkdir(DIR)?;
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
