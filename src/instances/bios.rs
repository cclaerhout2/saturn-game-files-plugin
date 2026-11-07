// SPDX-License-Identifier: MIT
// The BIOS check: whether the Saturn core will find its BIOS, boot.rom in
// Assets/<platform>/common (tools/package.py in pocket_saturn_core; the
// name and folder come from the core's data.json when it is on the card),
// exactly 512 KB.

use super::{has_ext, read_json, rel, BIOS_BYTES};
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

const BIOS_NAME: &str = "boot.rom";
/// Entries looked at per folder when searching for a BIOS elsewhere.
const MAX_LISTED: usize = 2000;
/// APF data slot parameters: bit 1, the file is in the core's own Assets folder.
const CORE_SPECIFIC: i64 = 1 << 1;

enum Bios {
    Usable,
    WrongSize(u64),
    /// A PlayStation BIOS ("System ROM Version" at 0x7FF32), a common mix-up.
    PlayStation,
    Blank,
}

fn examine(path: &Path) -> Bios {
    let Ok(mut file) = fs::File::open(path) else { return Bios::Blank };
    let bytes = file.metadata().map(|m| m.len()).unwrap_or(0);
    if bytes != BIOS_BYTES {
        return Bios::WrongSize(bytes);
    }
    let mut data = vec![0u8; BIOS_BYTES as usize];
    if file.seek(SeekFrom::Start(0)).is_err() || file.read_exact(&mut data).is_err() {
        return Bios::Blank;
    }
    if data[0x7ff32..].starts_with(b"System ROM Version") {
        return Bios::PlayStation;
    }
    if data.iter().all(|&b| b == data[0]) {
        return Bios::Blank;
    }
    Bios::Usable
}

/// (file name, folder below Assets/<platform>) of the core's BIOS slot.
fn bios_slot(card: &Path, core: &str) -> (String, String) {
    let slot = read_json(&card.join("Cores").join(core).join("data.json"))
        .and_then(|d| d.get("data")?.get("data_slots")?.as_array().cloned())
        .unwrap_or_default()
        .into_iter()
        .find(|s| {
            s.get("size_exact").and_then(|v| v.as_u64()) == Some(BIOS_BYTES)
                || s.get("filename").and_then(|v| v.as_str()).is_some_and(|f| f.eq_ignore_ascii_case(BIOS_NAME))
        });
    let name = slot.as_ref().and_then(|s| s.get("filename")?.as_str().map(String::from)).unwrap_or_else(|| BIOS_NAME.into());
    let parameters = slot.as_ref().and_then(|s| s.get("parameters")).map_or(0, |v| {
        v.as_i64().or_else(|| v.as_str().and_then(|t| i64::from_str_radix(t.trim_start_matches("0x"), 16).ok())).unwrap_or(0)
    });
    (name, if parameters & CORE_SPECIFIC != 0 { core.to_string() } else { "common".to_string() })
}

/// The file in folder named name, ignoring case (the Pocket's FAT/exFAT does).
fn named(folder: &Path, name: &str) -> Option<PathBuf> {
    fs::read_dir(folder).ok()?.flatten().map(|e| e.path()).find(|p| p.is_file() && super::name_of(p).eq_ignore_ascii_case(name))
}

/// 512 KB files elsewhere that could be the BIOS: in the platform's common
/// and core folders under another name, or in another Saturn platform
/// folder (MiSTer and other cores' names). Only these folders, and at most
/// MAX_LISTED entries each.
fn strays(card: &Path, platform: &str, core: &str, name: &str, place: &Path) -> Vec<PathBuf> {
    let assets = card.join("Assets");
    let mut folders = vec![assets.join(platform).join("common"), assets.join(platform).join(core)];
    for other in ["saturn", "ss", "segasaturn"].iter().filter(|p| **p != platform) {
        folders.push(assets.join(other).join("common"));
    }
    let mut found = vec![];
    for folder in folders {
        for entry in fs::read_dir(&folder).into_iter().flatten().flatten().take(MAX_LISTED) {
            let path = entry.path();
            let file = super::name_of(&path);
            let candidate = (has_ext(&file, ".bin") || has_ext(&file, ".rom") || !file.contains('.')) && !file.starts_with("._") && path.is_file();
            let in_place = folder == place && file.eq_ignore_ascii_case(name);
            if candidate && !in_place && matches!(examine(&path), Bios::Usable) {
                found.push(path);
            }
        }
    }
    found.sort();
    found
}

/// The BIOS report: one line for the BIOS file, then what to do when it is not usable.
pub fn check_bios(card: &Path, core: &str, platform: &str) -> Vec<String> {
    let (name, folder) = bios_slot(card, core);
    let place = card.join("Assets").join(platform).join(&folder);
    let shown_place = format!("Assets/{platform}/{folder}");
    let mut lines = vec![];
    if let Some(path) = named(&place, &name) {
        let shown = super::name_of(&path);
        match examine(&path) {
            Bios::Usable => {
                lines.push(format!("BIOS {shown_place}/{shown}: 512 KB, OK"));
                return lines;
            }
            Bios::WrongSize(bytes) => lines.push(format!(
                "BIOS {shown} is {} KB, but a Saturn BIOS is exactly 512 KB: replace it with a full dump of your console's BIOS",
                (bytes + 1023) / 1024
            )),
            Bios::PlayStation => lines.push(format!("BIOS {shown} is a PlayStation BIOS, not a Saturn one: replace it with your Saturn's BIOS")),
            Bios::Blank => lines.push(format!("BIOS {shown} is blank (every byte the same): replace it with a dump of your Saturn's BIOS")),
        }
    }
    let found = strays(card, platform, core, &name, &place);
    for path in found.iter().take(3) {
        lines.push(format!(
            "{} is 512 KB and may be a Saturn BIOS, but the core doesn't look for it there: copy it to {shown_place}/ as {name}",
            rel(path, card)
        ));
    }
    if found.is_empty() {
        lines.push(format!("No BIOS found: the core can't start games without one. Copy your Saturn BIOS (512 KB) to {shown_place}/ as {name}"));
    }
    lines
}
