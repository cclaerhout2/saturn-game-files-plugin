// SPDX-License-Identifier: MIT
// Game files for the Sega Saturn core (akoaysigod.Saturn) on an Analogue
// Pocket SD card. Adapted from the PlayStation game files plugin
// (GuardianSoul/openfpga-PlayStation-plugin).
//
// What the core reads (src/fw/cdb/cd_src.c in pocket_saturn_core): its
// data slots, loaded on demand, are the disc's .cue (slot 14) and the
// files the .cue names, in order (slots 16, 17, ...: one .bin for the
// whole disc, as chdman writes it, or one .bin per track, as Redump's
// sets come). Without a .cue, slot 16 alone is a one-track data disc. The
// core parses the .cue itself, with a small reader: cue_layout() below
// is a port of it, so a .cue it would not read is reported here instead
// of booting the wrong layout.
//
// CHD images are extracted next to them (chd_image.rs, as `chdman
// extractcd` writes them). Large writes are guarded (convert.rs): the
// space for a file plus RESERVE is claimed before it is written, a file
// only gets its name once it is written and read back, and originals are
// only removed after that.

use regex::Regex;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

mod chd_image;
pub mod bios;
pub mod convert;

pub const SECTOR: u64 = 2352;
/// Space a large write always leaves free on the SD card.
const RESERVE: u64 = 256 << 20;
/// A Saturn BIOS (boot.rom) is exactly this size; no disc image is.
pub const BIOS_BYTES: u64 = 512 << 10;
/// What every Saturn disc's first data sector holds after its 16-byte
/// sync and header (the IP.BIN's hardware ID).
const SATURN_ID: &[u8] = b"SEGA SEGASATURN ";

// The core's slots (cd_src.c) when its data.json can't be read.
const CUE_ID: i64 = 14;
const BIN_ID: i64 = 16;
/// cd_src.c reads only the first DT_ENTRIES entries of the data slot table.
const DT_ENTRIES: usize = 32;
/// cd_src.c MAXF: files of one .cue.
const MAXF: usize = 32;
/// cd_src.c MAXX: extents (runs of file data between holes).
const MAXX: usize = 40;
/// Files a disc can have when the core's data.json is not on the card: the
/// slots before the disc's in the table (BIOS, saves, cartridge) take some
/// of its 32 entries.
const UNKNOWN_MAX_FILES: usize = 25;

fn re(cell: &'static OnceLock<Regex>, pattern: &str) -> &'static Regex {
    cell.get_or_init(|| Regex::new(pattern).unwrap())
}
macro_rules! regex {
    ($pattern:expr) => {{
        static CELL: OnceLock<Regex> = OnceLock::new();
        re(&CELL, $pattern)
    }};
}
// Disc tags: redump's "(Disc N)", and "(CD 2)", "(Disc2)", "X - Disc 1",
// "X_CD2" at the end of the title (before any release tags).
fn disc_paren() -> &'static Regex { regex!(r"(?i)\s*\((?:Disc|CD)\s*(\d{1,2})\)") }
fn disc_end() -> &'static Regex { regex!(r"(?i)[\s_-]+(?:Disc|CD)[\s_]*(\d{1,2})((?:\s*\([^()]*\))*)\s*$") }

/// (disc number, stem without its disc tag), or None.
fn disc_tag(stem: &str) -> Option<(u64, String)> {
    if let Some(m) = disc_paren().captures(stem) {
        return Some((m[1].parse().ok()?, disc_paren().replace_all(stem, "").into_owned()));
    }
    let m = disc_end().captures(stem)?;
    Some((m[1].parse().ok()?, format!("{}{}", &stem[..m.get(0)?.start()], &m[2])))
}

/// Plugin settings (the host's config; tests on a computer use PLUGIN_<key>).
pub fn setting(key: &str) -> Option<String> {
    #[cfg(target_arch = "wasm32")]
    {
        extism_pdk::config::get(key).ok().flatten()
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        std::env::var(format!("PLUGIN_{key}")).ok()
    }
}

// ---- paths ------------------------------------------------------------------

pub(crate) fn name_of(path: &Path) -> String {
    path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

/// The name without its last suffix.
pub(crate) fn stem(name: &str) -> &str {
    match name.rfind('.') {
        Some(i) if i > 0 => &name[..i],
        _ => name,
    }
}

/// path below base, '/'-separated ('.' for base itself).
pub(crate) fn rel(path: &Path, base: &Path) -> String {
    let parts: Vec<String> = path
        .strip_prefix(base)
        .unwrap_or(path)
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    if parts.is_empty() { ".".into() } else { parts.join("/") }
}

fn segments(path: &Path) -> Vec<String> {
    path.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect()
}

fn is_file(path: &Path) -> bool { path.is_file() }

fn size(path: &Path) -> Option<u64> {
    fs::metadata(path).ok().filter(|m| m.is_file()).map(|m| m.len())
}

/// Files under folder whose name passes test, sorted, without macOS '._' files.
fn walk(folder: &Path, test: &dyn Fn(&str) -> bool) -> Vec<PathBuf> {
    fn visit(dir: &Path, test: &dyn Fn(&str) -> bool, found: &mut Vec<PathBuf>) {
        let Ok(entries) = fs::read_dir(dir) else { return };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = name_of(&path);
            match entry.file_type() {
                Ok(t) if t.is_dir() => visit(&path, test, found),
                Ok(t) if t.is_file() && test(&name) && !name.starts_with("._") => found.push(path),
                _ => {}
            }
        }
    }
    let mut found = vec![];
    visit(folder, test, &mut found);
    found.sort_by_key(|p| segments(p));
    found
}

/// Compared as bytes: a name with non-ASCII letters near its end has no
/// char boundary there, and slicing the str would stop the plugin.
fn has_ext(name: &str, ext: &str) -> bool {
    name.len() > ext.len() && name.as_bytes()[name.len() - ext.len()..].eq_ignore_ascii_case(ext.as_bytes())
}

/// A cue's FILE names are relative to its folder.
fn join(folder: &Path, name: &str) -> PathBuf {
    let mut path = folder.to_path_buf();
    for seg in name.split(['/', '\\']).filter(|s| !s.is_empty() && *s != ".") {
        path.push(seg);
    }
    path
}

// ---- guarded writes -------------------------------------------------------------

/// FNV-1a over everything written or read back.
struct Hashing<'a> {
    inner: &'a mut fs::File,
    hash: u64,
    len: u64,
}

impl Write for Hashing<'_> {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(data)?;
        self.hash = fnv(self.hash, &data[..n]);
        self.len += n as u64;
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> { self.inner.flush() }
}

fn fnv(mut hash: u64, data: &[u8]) -> u64 {
    for &b in data {
        hash = (hash ^ b as u64).wrapping_mul(0x100000001b3);
    }
    hash
}
const FNV: u64 = 0xcbf29ce484222325;

fn no_space(needed: u64) -> io::Error {
    io::Error::new(io::ErrorKind::StorageFull, format!("not enough free space (needs {} MB plus {} MB to spare)", mb(needed), mb(RESERVE)))
}

pub(crate) fn mb(bytes: u64) -> u64 { (bytes + (1 << 20) - 1) >> 20 }

/// Whether an error means the disk is (or would get) too full.
fn is_no_space(e: &io::Error) -> bool { e.kind() == io::ErrorKind::StorageFull }

/// Tests set test_card_bytes to make the SD card that small (hosts never
/// set it): a claim fails when the card's files, the claim and RESERVE
/// would not fit.
fn card_too_small_for(path: &Path, bytes: u64) -> bool {
    let card = Path::new(crate::CARD);
    if !path.starts_with(card) {
        return false;
    }
    let Some(limit) = setting("test_card_bytes").and_then(|v| v.parse::<u64>().ok()) else { return false };
    let used: u64 = walk(card, &|_| true).iter().filter_map(|p| size(p)).sum();
    used + bytes + RESERVE > limit
}

/// Claim bytes plus RESERVE for file (FAT and exFAT have no sparse files,
/// so this fails with nothing written when the card is too full).
fn claim(file: &fs::File, path: &Path, bytes: u64) -> io::Result<()> {
    if card_too_small_for(path, bytes) {
        return Err(no_space(bytes));
    }
    file.set_len(bytes + RESERVE).map_err(|_| no_space(bytes))
}

/// Write path via path.part, so a stopped run never leaves a short file
/// under the real name. With reserve, the part file first claims that many
/// bytes plus RESERVE. The file is synced and read back before it is renamed.
fn write_parts(path: &Path, reserve: Option<u64>, parts: &mut dyn FnMut(&mut dyn Write) -> io::Result<()>) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let partial = path.with_file_name(format!("{}.part", name_of(path)));
    let result = (|| {
        let mut file = fs::OpenOptions::new().read(true).write(true).create(true).truncate(true).open(&partial)?;
        if let Some(n) = reserve {
            claim(&file, &partial, n)?;
        }
        let mut out = Hashing { inner: &mut file, hash: FNV, len: 0 };
        parts(&mut out)?;
        out.flush()?;
        let (hash, len) = (out.hash, out.len);
        file.set_len(len)?;
        file.sync_all()?;
        file.seek(SeekFrom::Start(0))?;
        let (mut check, mut buffer) = (FNV, vec![0u8; 1 << 20]);
        loop {
            let n = file.read(&mut buffer)?;
            if n == 0 {
                break;
            }
            check = fnv(check, &buffer[..n]);
        }
        if check != hash {
            return Err(io::Error::other(format!("{}: read back differently from what was written", name_of(path))));
        }
        Ok(())
    })();
    match result {
        Ok(()) => fs::rename(&partial, path),
        Err(e) => {
            let _ = fs::remove_file(&partial);
            Err(e)
        }
    }
}

/// Remove what a run that was stopped part-way (closed, card pulled, power
/// lost) can leave behind: unfinished .part files under the plugin's folders
/// and the space check. Returns how many went.
pub fn clean_leftovers(card: &Path, platform: &str) -> usize {
    const OURS: [&str; 4] = [".bin.part", ".cue.part", ".json.part", ".sav.part"];
    let mut found = walk(&card.join("Assets").join(platform), &|n| OURS.iter().any(|e| n.ends_with(e)));
    found.extend(walk(&card.join("Saves").join(platform), &|n| OURS.iter().any(|e| n.ends_with(e))));
    let check = card.join(convert::SPACE_CHECK);
    if check.is_file() {
        found.push(check);
    }
    found.iter().filter(|f| fs::remove_file(f).is_ok()).count()
}

/// A small file. Left alone when it already holds the same data: each write
/// is synced and read back, which is slow on an SD card.
fn write_bytes(path: &Path, data: &[u8]) -> io::Result<()> {
    if fs::metadata(path).map_or(false, |m| m.is_file() && m.len() == data.len() as u64) && fs::read(path).map_or(false, |d| d == data) {
        return Ok(());
    }
    write_parts(path, None, &mut |out| out.write_all(data))
}

/// Copy count bytes of src (from offset) to out.
fn copy_range(src: &Path, offset: u64, count: u64, out: &mut dyn Write) -> io::Result<()> {
    let mut file = fs::File::open(src)?;
    file.seek(SeekFrom::Start(offset))?;
    let mut left = count;
    let mut buffer = vec![0u8; 1 << 22];
    while left > 0 {
        let want = left.min(buffer.len() as u64) as usize;
        let got = file.read(&mut buffer[..want])?;
        if got == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, format!("{}: short read", name_of(src))));
        }
        out.write_all(&buffer[..got])?;
        left -= got as u64;
    }
    Ok(())
}

/// MM:SS:FF as cue sheets write it.
fn msf_text(frames: u64) -> String {
    format!("{:02}:{:02}:{:02}", frames / (75 * 60), frames / 75 % 60, frames % 75)
}

// ---- cue sheets (to find the files) ------------------------------------------------

#[derive(Clone, Debug)]
struct Track {
    number: u64,
    kind: String,
}

type CueFiles = Vec<(String, Vec<Track>)>;

fn number(text: &str) -> Result<u64, String> {
    text.parse().map_err(|_| format!("bad number {text}"))
}

/// [(filename, [track])] in cue order, read leniently (any case, a FILE line
/// without its type): the files to put in the game file. Whether the core
/// reads the cue is cue_layout()'s question.
fn parse_cue_text(text: &str) -> Result<CueFiles, String> {
    let file_quoted = regex!(r#"(?i)^FILE\s+"(.+)""#);
    let file_plain = regex!(r"(?i)^FILE\s+(\S+)");
    let track = regex!(r"(?i)^TRACK\s+([0-9]+)\s+(\S+)");
    let index = regex!(r"(?i)^(INDEX|PREGAP|POSTGAP)\s");
    let mut files: CueFiles = vec![];
    for line in text.split(['\n', '\r']) {
        let words = line.trim().trim_start_matches('\u{feff}');
        if let Some(m) = file_quoted.captures(words).or_else(|| file_plain.captures(words)) {
            files.push((m[1].to_string(), vec![]));
        } else if let Some(m) = track.captures(words) {
            let Some(last) = files.last_mut() else { return Err("TRACK before FILE".into()) };
            last.1.push(Track { number: number(&m[1])?, kind: m[2].to_uppercase() });
        } else if let Some(m) = index.captures(words) {
            if files.last().map_or(true, |f| f.1.is_empty()) {
                return Err(format!("{} before TRACK", m[1].to_uppercase()));
            }
        }
    }
    Ok(files)
}

fn read_cue(path: &Path) -> Result<String, String> {
    let data = fs::read(path).map_err(|e| e.to_string())?;
    let data = data.strip_prefix(b"\xef\xbb\xbf").unwrap_or(&data);
    // Old Windows tools write names in Latin-1: read those bytes as Latin-1.
    Ok(std::str::from_utf8(data).map(String::from).unwrap_or_else(|_| data.iter().map(|&b| b as char).collect()))
}

fn parse_cue(path: &Path) -> Result<CueFiles, String> {
    let files = parse_cue_text(&read_cue(path)?)?;
    let names: Vec<String> = files.iter().map(|f| f.0.clone()).collect();
    Ok(files.into_iter().map(|(name, tracks)| (resolve_file(path, &name, &names), tracks)).collect())
}

fn track_file() -> &'static Regex { regex!(r"(?i)\(Track (\d+)\)") }

fn track_number(name: &str) -> Option<u64> { track_file().captures(name)?[1].parse().ok() }

/// The file in cue's folder that FILE name means: name itself when it is
/// there, else the one file that matches it ignoring case (or its folders),
/// else for a one-file cue "<cue name>.bin" (renamed along with its cue),
/// else the one .bin with the same "(Track n)" when the cue is the only one
/// in its folder and its .bin files are exactly the cue's tracks. Without
/// any of these, name (reported as missing). The core takes the files from
/// the game file's slots, in the cue's order, whatever their names.
fn resolve_file(cue: &Path, name: &str, names: &[String]) -> String {
    let folder = cue.parent().unwrap_or(Path::new(""));
    if is_file(&join(folder, name)) {
        return name.replace('\\', "/");
    }
    let mut files: Vec<String> = fs::read_dir(folder)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_type().map_or(false, |t| t.is_file()))
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| !n.starts_with("._"))
        .collect();
    files.sort();
    let one = |test: &dyn Fn(&str) -> bool| {
        let found: Vec<&String> = files.iter().filter(|f| test(f)).collect();
        (found.len() == 1).then(|| found[0].clone())
    };
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let lower = base.to_lowercase();
    let renamed = format!("{}.bin", stem(&name_of(cue)).to_lowercase());
    let track = track_number(base);
    let bins: Vec<&String> = files.iter().filter(|f| has_ext(f, ".bin")).collect();
    let mut numbers: Vec<Option<u64>> = names.iter().map(|n| track_number(n.rsplit(['/', '\\']).next().unwrap_or(n))).collect();
    let mut bin_numbers: Vec<Option<u64>> = bins.iter().map(|f| track_number(f)).collect();
    numbers.sort();
    bin_numbers.sort();
    let by_track = files.iter().filter(|f| has_ext(f, ".cue")).count() == 1
        && !numbers.contains(&None)
        && numbers.windows(2).all(|w| w[0] != w[1])
        && numbers == bin_numbers;
    one(&|f| f == base)
        .or_else(|| one(&|f| f.to_lowercase() == lower))
        .or_else(|| if names.len() == 1 { one(&|f| f.to_lowercase() == renamed) } else { None })
        .or_else(|| if track.is_some() && by_track { one(&|f| has_ext(f, ".bin") && track_number(f) == track) } else { None })
        .unwrap_or_else(|| name.replace('\\', "/"))
}

// ---- the core's own cue reader (cd_src.c cue_parse) ------------------------------------

/// The keywords as cd_src.c matches them: upper case, as a prefix, in this
/// order (PREGAP before PRE).
const KEYWORDS: [&[u8]; 12] =
    [b"FILE", b"TRACK", b"FLAGS", b"PREGAP", b"POSTGAP", b"INDEX", b"AUDIO", b"MODE1/2352", b"MODE2/2352", b"DCP", b"PRE", b"4CH"];
const K_FILE: usize = 0;
const K_TRACK: usize = 1;
const K_FLAGS: usize = 2;
const K_PREGAP: usize = 3;
const K_POSTGAP: usize = 4;
const K_INDEX: usize = 5;
const K_AUDIO: usize = 6;
const K_MODE1: usize = 7;
const K_MODE2: usize = 8;
const K_NONE: usize = 12;

/// What the core makes of a disc: its tracks and extents.
#[derive(Debug, PartialEq)]
pub struct Layout {
    pub tracks: usize,
    pub extents: usize,
    pub leadout: i64,
}

/// A port of cd_src.c's cue reader: the layout the core builds from this cue
/// with files of these many sectors, or why it falls back to its no-cue
/// convention (which plays a disc wrongly unless it is per-track Redump).
pub fn cue_layout(text: &[u8], fsec: &[u64]) -> Result<Layout, String> {
    fn sp(s: &[u8]) -> &[u8] {
        let n = s.iter().take_while(|&&c| c == b' ' || c == b'\t' || c & 0x80 != 0).count();
        &s[n..]
    }
    fn word(s: &mut &[u8]) -> usize {
        for (i, k) in KEYWORDS.iter().enumerate() {
            if s.starts_with(k) {
                *s = sp(&s[k.len()..]);
                return i;
            }
        }
        K_NONE
    }
    fn num(s: &mut &[u8]) -> i64 {
        let mut v = 0i64;
        while let Some(&c) = s.first().filter(|c| c.is_ascii_digit()) {
            v = v.saturating_mul(10).saturating_add((c - b'0') as i64);
            *s = &s[1..];
        }
        v
    }
    fn msf(mut s: &[u8]) -> i64 {
        let mut v = 0i64;
        for i in 0..3 {
            v = v.saturating_mul(if i == 2 { 75 } else { 60 }).saturating_add(num(&mut s));
            if !s.is_empty() {
                s = &s[1..];
            }
        }
        v
    }
    struct P {
        f: i64,
        t: i64,
        i1: bool,
        xo: bool,
        bad: Option<String>,
        base: i64,
        gap: i64,
        nx: usize,
        ntr: usize,
        x_lba: i64,
    }
    let nfiles = fsec.iter().take(MAXF).take_while(|&&s| s >= 1).count();
    let mut p = P { f: -1, t: 0, i1: false, xo: false, bad: None, base: 0, gap: 0, nx: 0, ntr: 0, x_lba: 0 };
    fn fail(p: &mut P, why: &str) {
        if p.bad.is_none() {
            p.bad = Some(why.to_string());
        }
    }
    fn x_close(p: &mut P, end: i64) {
        if !p.xo {
            return;
        }
        p.xo = false;
        if end > p.x_lba {
            p.nx += 1;
            if p.nx >= MAXX {
                fail(p, "more gaps between tracks than the core keeps (40 extents)");
            }
        }
    }
    let line = |p: &mut P, s: &[u8]| {
        let mut s = s;
        let k = word(&mut s);
        if k == K_FILE {
            if p.f >= 0 && (p.f as usize) < nfiles {
                let n = fsec[p.f as usize] as i64;
                x_close(p, p.base + n);
                p.base += n;
            }
            p.f += 1;
            if p.f as usize >= nfiles {
                fail(p, "more FILE lines than files");
            }
        } else if k == K_TRACK {
            let n = num(&mut s);
            if p.f < 0 {
                fail(p, "TRACK before FILE");
            } else if n != p.t + 1 {
                fail(p, &format!("TRACK {n:02} out of order (the core takes tracks 01, 02, ...)"));
            } else if p.t != 0 && !p.i1 {
                fail(p, &format!("track {} has no INDEX 01", p.t));
            } else if n > 99 {
                fail(p, "more than 99 tracks");
            }
            p.t = n;
            p.i1 = false;
            s = sp(s);
            let kind = word(&mut s);
            if kind != K_AUDIO && kind != K_MODE1 && kind != K_MODE2 {
                fail(p, &format!("track {n}: not a raw track (the core reads AUDIO, MODE1/2352 and MODE2/2352)"));
            }
        } else if k == K_PREGAP || k == K_POSTGAP {
            p.gap += msf(s);
        } else if k == K_INDEX {
            let i = num(&mut s);
            let q = msf(sp(s));
            if p.t == 0 || p.bad.is_some() {
                fail(p, "INDEX before TRACK");
                return;
            }
            if p.gap != 0 {
                x_close(p, p.base + q);
                p.base += p.gap;
                p.gap = 0;
            }
            if !p.xo {
                if p.nx >= MAXX {
                    fail(p, "more gaps between tracks than the core keeps (40 extents)");
                    return;
                }
                p.x_lba = p.base + q;
                p.xo = true;
            }
            if i == 1 {
                p.ntr = p.t as usize;
                p.i1 = true;
            }
        } else if k == K_FLAGS {
            // control bits only
        }
    };
    // Lines end at CR or LF; the core keeps 95 characters of each.
    let mut ln: Vec<u8> = Vec::with_capacity(96);
    for &c in text {
        if c == b'\n' || c == b'\r' {
            if !ln.is_empty() {
                let s = sp(&ln).to_vec();
                line(&mut p, &s);
            }
            ln.clear();
        } else if ln.len() < 95 {
            ln.push(c);
        }
    }
    if !ln.is_empty() {
        let s = sp(&ln).to_vec();
        line(&mut p, &s);
    }
    if p.bad.is_none() && (p.f < 0 || p.t == 0 || !p.i1) {
        p.bad = Some(if p.f < 0 { "no FILE line the core can read (keywords must be in upper case)".into() } else { "no track with an INDEX 01".into() });
    }
    if let Some(why) = p.bad {
        return Err(why);
    }
    let last = fsec[p.f as usize] as i64;
    let end = p.base + last;
    x_close(&mut p, end);
    if let Some(why) = p.bad {
        return Err(why);
    }
    Ok(Layout { tracks: p.ntr, extents: p.nx, leadout: p.base + last + p.gap })
}

/// Whether the file has a Saturn disc's ID at this offset's sector (a raw
/// 2352-byte sector: 16 bytes of sync and header, then the IP.BIN).
fn saturn_disc(path: &Path, sector: u64) -> bool {
    let Ok(mut file) = fs::File::open(path) else { return false };
    let mut id = [0u8; 16];
    file.seek(SeekFrom::Start(sector * SECTOR + 16)).is_ok() && file.read_exact(&mut id).is_ok() && id == SATURN_ID
}

/// A PlayStation disc (its license text "Licensed by Sony" in sector 4).
fn playstation_disc(path: &Path) -> bool {
    let Ok(mut file) = fs::File::open(path) else { return false };
    let mut text = [0u8; 80];
    file.seek(SeekFrom::Start(4 * SECTOR + 24)).is_ok() && file.read_exact(&mut text).is_ok() && text.windows(6).any(|w| w == b"Sony C")
}

// ---- the core's slots ---------------------------------------------------------------

/// The data slots a game file fills, from the core's data.json (or cd_src.c
/// when it isn't on the card).
#[derive(Clone, Debug)]
pub struct Slots {
    pub cue: i64,
    /// One per file of the cue, in order (only those the core sees).
    pub bins: Vec<i64>,
    /// The saves (internal backup RAM, then the backup cartridge): (id,
    /// filename suffix for a multi-disc game's shared save).
    pub saves: Vec<(i64, String)>,
    pub declared: bool,
}

pub(crate) fn read_json(path: &Path) -> Option<serde_json::Value> {
    serde_json::from_slice(&fs::read(path).ok()?).ok()
}

fn number_value(v: &serde_json::Value) -> Option<i64> {
    v.as_i64().or_else(|| {
        let s = v.as_str()?.trim();
        match s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
            Some(hex) => i64::from_str_radix(hex, 16).ok(),
            None => s.parse().ok(),
        }
    })
}

/// The core's data.json slots, in order: (id, extensions, parameters, filename).
pub(crate) fn declared_slots(card: &Path, core: &str) -> Option<Vec<(Option<i64>, Vec<String>, i64, Option<String>)>> {
    let data = read_json(&card.join("Cores").join(core).join("data.json"))?;
    let slots = data.get("data")?.get("data_slots")?.as_array()?;
    Some(
        slots
            .iter()
            .map(|s| {
                let id = s.get("id").and_then(number_value);
                let ext = s
                    .get("extensions")
                    .and_then(|v| v.as_array())
                    .map(|a| a.iter().filter_map(|e| e.as_str().map(|x| x.to_ascii_lowercase())).collect())
                    .unwrap_or_default();
                let parameters = s.get("parameters").and_then(number_value).unwrap_or(0);
                let filename = s.get("filename").and_then(|v| v.as_str()).map(String::from);
                (id, ext, parameters, filename)
            })
            .collect(),
    )
}

/// APF data slot parameters: bit 2, saved to the Saves folder.
const NONVOLATILE: i64 = 1 << 2;

pub fn slots(card: &Path, core: &str) -> Slots {
    let Some(declared) = declared_slots(card, core) else {
        return Slots { cue: CUE_ID, bins: (BIN_ID..BIN_ID + UNKNOWN_MAX_FILES as i64).collect(), saves: vec![], declared: false };
    };
    // The core sees the first DT_ENTRIES entries of the table (in data.json order).
    let seen = |i: usize| i < DT_ENTRIES;
    let cue = declared.iter().find(|s| s.0 == Some(CUE_ID) || s.1.iter().any(|e| e == "cue")).and_then(|s| s.0).unwrap_or(CUE_ID);
    let mut bins: Vec<i64> = declared
        .iter()
        .enumerate()
        .filter(|(i, s)| seen(*i) && s.0.is_some_and(|id| (BIN_ID..BIN_ID + MAXF as i64).contains(&id)))
        .filter_map(|(_, s)| s.0)
        .collect();
    bins.sort();
    // Only the run 16, 17, ... counts: the core stops at the first file it doesn't have.
    let run = bins.iter().enumerate().take_while(|(k, id)| **id == BIN_ID + *k as i64).count();
    bins.truncate(run);
    let mut saves = vec![];
    for (k, s) in declared.iter().filter(|s| s.2 & NONVOLATILE != 0).enumerate() {
        let Some(id) = s.0 else { continue };
        let ext = s.3.as_deref().and_then(|f| f.rsplit_once('.')).map(|(_, e)| e.to_string()).unwrap_or_else(|| "sav".into());
        let suffix = if k == 0 { format!(".{ext}") } else { format!(" (cartridge).{ext}") };
        saves.push((id, suffix));
    }
    Slots { cue, bins, saves, declared: true }
}

// ---- skip reasons ----------------------------------------------------------------

/// A skip reason as a plain sentence that ends with what to do about it.
pub fn explain(reason: &str) -> String {
    const DUMP: &str = "use the disc's .cue/.bin or .chd";
    if let Some(m) = regex!(r"^(.+): file missing$").captures(reason) {
        return format!("{} is missing: put it next to the .cue, with the name the .cue gives it", &m[1]);
    }
    if let Some(m) = regex!(r"^(.+): ECM-compressed$").captures(reason) {
        return format!("{} is ECM-compressed ({}.ecm): decompress it first, then Generate game files again", &m[1], &m[1]);
    }
    if let Some(m) = regex!(r"^(.+): not whole 2352-byte sectors$").captures(reason) {
        return format!("{} is not a raw CD image (its size is not whole 2352-byte sectors): {DUMP}", &m[1]);
    }
    if let Some(m) = regex!(r"^(.+): (WAVE|MP3|AIFF|OGG|FLAC) audio$").captures(reason) {
        return format!("{} is {} audio, which the core can't read: {DUMP} with BINARY tracks", &m[1], &m[2]);
    }
    if let Some(m) = regex!(r"^track (\d+): (.+) is not 2352-byte sectors$").captures(reason) {
        return format!("track {} is {}, which the core can't read: {DUMP} with 2352-byte sectors", &m[1], &m[2]);
    }
    if let Some(m) = regex!(r"^core cue reader: (.+)$").captures(reason) {
        return format!("the core can't read this .cue ({}): get the disc's original Redump .cue, or its .chd", &m[1]);
    }
    if regex!(r"^(no FILE entries|\S* ?before (TRACK|FILE))$").is_match(reason) {
        return format!("the .cue is damaged ({reason}): get the disc's real .cue");
    }
    if let Some(m) = regex!(r"^(\d+) files \(core takes (\d+)\)$").captures(reason) {
        return format!("{} files, but this core takes {} per disc: Generate game files again and choose to convert it (joins them into one .bin)", &m[1], &m[2]);
    }
    if let Some(what) = reason.strip_prefix("format: ") {
        return format!("it is {what}, which the core can't read: {DUMP}");
    }
    if let Some(why) = reason.strip_prefix("CHD: ") {
        return format!("this CHD can't be converted ({why}): {DUMP}");
    }
    match reason {
        "a track file whose .cue is missing" => "it is one track of a disc whose .cue is missing: put the disc's .cue next to it".into(),
        "not whole 2352-byte sectors" => format!("it is not a raw CD image (its size is not whole 2352-byte sectors): {DUMP}"),
        "CHD image not extracted" => "CHD image not converted: Generate game files again and choose to convert it".into(),
        "compressed archive" => "it is a compressed archive: extract it on a computer first, then Generate game files again".into(),
        "ECM-compressed image" => "it is ECM-compressed: decompress it to its .bin first, then Generate game files again".into(),
        _ => reason.to_string(),
    }
}

// ---- checks ------------------------------------------------------------------------

/// Why the core can't play this cue's disc as it is, or None. A disc with
/// more files than the core takes is "N files (core takes M)" (convert.rs
/// joins it).
fn disc_check(cue: &Path, files: &CueFiles, slots: &Slots) -> Option<String> {
    let folder = cue.parent()?;
    if files.is_empty() {
        return Some("no FILE entries".into());
    }
    let mut sectors = vec![];
    let text = read_cue(cue).unwrap_or_default();
    for (name, tracks) in files {
        let path = join(folder, name);
        let Some(bytes) = size(&path) else {
            if is_file(&path.with_file_name(format!("{}.ecm", name_of(&path)))) {
                return Some(format!("{name}: ECM-compressed"));
            }
            return Some(format!("{name}: file missing"));
        };
        if let Some(t) = tracks.iter().find(|t| !(t.kind == "AUDIO" || t.kind.ends_with("/2352"))) {
            return Some(format!("track {}: {} is not 2352-byte sectors", t.number, t.kind));
        }
        if bytes % SECTOR != 0 || bytes == 0 {
            return Some(format!("{name}: not whole 2352-byte sectors"));
        }
        sectors.push(bytes / SECTOR);
    }
    for kind in ["WAVE", "MP3", "AIFF", "OGG", "FLAC"] {
        let pattern = Regex::new(&format!(r#"(?im)^\s*FILE\s+("[^"]*"|\S+)\s+{kind}\s*$"#)).unwrap();
        if let Some(m) = pattern.captures(&text) {
            return Some(format!("{}: {kind} audio", m[1].trim_matches('"')));
        }
    }
    if files.len() > slots.bins.len() {
        return Some(format!("{} files (core takes {})", files.len(), slots.bins.len()));
    }
    let raw = fs::read(cue).unwrap_or_default();
    if let Err(why) = cue_layout(&raw, &sectors) {
        return Some(format!("core cue reader: {why}"));
    }
    None
}

// ---- .bin files without a cue -------------------------------------------------

/// Known Saturn BIOS names (a BIOS is not a disc).
fn bios_like(name: &str) -> bool {
    regex!(r"(?i)^(boot\.rom|sega_?10\d\.bin|sega1003\.bin|mpr-\d+\.(bin|ic\d+)|saturn.*bios.*|.*bios.*\.(bin|rom))$").is_match(name)
}

/// .bin files under common that no cue in their folder names and that have
/// no <name>.cue, with the reason one is skipped.
fn lone_bins(common: &Path) -> Vec<(PathBuf, Option<&'static str>)> {
    let mut found = vec![];
    for bin in walk(common, &|n| has_ext(n, ".bin")) {
        let folder = bin.parent().unwrap();
        let name = name_of(&bin);
        if is_file(&folder.join(format!("{}.cue", stem(&name)))) || size(&bin) == Some(BIOS_BYTES) || bios_like(&name) {
            continue;
        }
        let (mut named, mut broken) = (vec![], false);
        for entry in fs::read_dir(folder).into_iter().flatten().flatten() {
            let cue = entry.path();
            let cue_name = name_of(&cue);
            if !has_ext(&cue_name, ".cue") || cue_name.starts_with("._") || !is_file(&cue) {
                continue;
            }
            match parse_cue(&cue) {
                Ok(files) => named.extend(files.iter().map(|(f, _)| join(folder, f))),
                Err(_) => broken = true,
            }
        }
        if broken || named.contains(&bin) {
            continue;
        }
        let reason = if track_file().is_match(stem(&name)) {
            Some("a track file whose .cue is missing")
        } else if size(&bin).unwrap_or(0) % SECTOR != 0 || size(&bin) == Some(0) {
            Some("not whole 2352-byte sectors")
        } else {
            None
        };
        found.push((bin, reason));
    }
    found
}

// ---- other image formats -------------------------------------------------------

/// Disc image files the core can't read, and why.
const OTHER_IMAGES: &[(&str, &str)] = &[
    (".7z", "compressed archive"),
    (".zip", "compressed archive"),
    (".rar", "compressed archive"),
    (".ecm", "ECM-compressed image"),
    (".iso", "format: an .iso (2048-byte sectors, no CD music)"),
    (".ccd", "format: a CloneCD image (.ccd/.img/.sub)"),
    (".mds", "format: an Alcohol image (.mds/.mdf)"),
    (".nrg", "format: a Nero image"),
    (".cdi", "format: a DiscJuggler image"),
    (".gdi", "format: a Dreamcast GD-ROM image"),
    (".m3u", "format: a playlist (the core has no disc swap: each disc gets its own game file)"),
];

fn other_image(name: &str) -> Option<(&'static str, &'static str)> {
    OTHER_IMAGES.iter().copied().find(|(ext, _)| has_ext(name, ext))
}

/// Files under common in a format the core can't read that no cue in their
/// folder names, with the reason.
fn other_images(common: &Path) -> Vec<(PathBuf, &'static str)> {
    let mut found = vec![];
    for path in walk(common, &|n| other_image(n).is_some()) {
        let folder = path.parent().unwrap();
        let name = name_of(&path);
        let (ext, reason) = other_image(&name).unwrap();
        let mut named = vec![];
        for entry in fs::read_dir(folder).into_iter().flatten().flatten() {
            let cue = entry.path();
            let cue_name = name_of(&cue);
            if !has_ext(&cue_name, ".cue") || cue_name.starts_with("._") || !is_file(&cue) {
                continue;
            }
            if let Ok(files) = parse_cue(&cue) {
                named.extend(files.iter().map(|(f, _)| join(folder, f)));
            }
        }
        let bare = folder.join(&name[..name.len() - ext.len()]);
        if named.contains(&path) || named.contains(&bare) {
            continue;
        }
        found.push((path, reason));
    }
    found
}

// ---- CHD images ------------------------------------------------------------------

/// Where a CHD (or a joined disc) is converted to. A CHD's .cue and .bin go
/// next to it (<folder>/<name>.cue), in the game's own folder, unless that
/// would clash with other files there (a <name>.bin of its own, or a .cue
/// that is no CHD's): then into <folder>/<name>/<name>.cue, as a joined disc
/// always does (its .cue would take the original's name). A copy already
/// in either place is the converted one.
pub(crate) fn converted_cue(image: &Path) -> PathBuf {
    let base = stem(&name_of(image)).to_string();
    let folder = image.parent().unwrap();
    let nested = folder.join(&base).join(format!("{base}.cue"));
    if !has_ext(&name_of(image), ".chd") || is_file(&nested) {
        return nested;
    }
    let flat = folder.join(format!("{base}.cue"));
    if is_file(&flat) {
        return flat;
    }
    let names: Vec<String> = fs::read_dir(folder)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_type().map_or(false, |t| t.is_file()))
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| !n.starts_with("._"))
        .collect();
    let chds: Vec<String> = names.iter().filter(|n| has_ext(n, ".chd")).map(|n| stem(n).to_lowercase()).collect();
    let clash = names.iter().any(|n| {
        n.eq_ignore_ascii_case(&format!("{base}.bin")) || (has_ext(n, ".cue") && !chds.contains(&stem(n).to_lowercase()))
    });
    if clash { nested } else { flat }
}

/// Move discs converted into <folder>/<name>/ by earlier versions up into
/// <folder>, where this version puts them: only a folder holding nothing
/// but <name>.cue and <name>.bin, with no file of those names in <folder>.
/// A rename on the same card: nothing is copied. Returns how many moved.
pub fn flatten_converted(card: &Path, platform: &str, log: &mut dyn FnMut(&str)) -> usize {
    let common = card.join("Assets").join(platform).join("common");
    let mut moved = 0;
    for cue in walk(&common, &|n| has_ext(n, ".cue")) {
        let inner = cue.parent().unwrap();
        let base = stem(&name_of(&cue)).to_string();
        if name_of(inner) != base || inner == common {
            continue;
        }
        let outer = inner.parent().unwrap();
        if outer == common {
            continue; // a game in its own folder, as it should be
        }
        // Only what an earlier version made: <outer> holds nothing but this
        // folder (and the .chd it came from), and the .cue is the plugin's
        // (one FILE "<name>.bin" BINARY first, its own indentation).
        let others = fs::read_dir(outer).into_iter().flatten().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).any(|n| {
            !n.starts_with("._") && n != base && !n.eq_ignore_ascii_case(&format!("{base}.chd")) && !n.eq_ignore_ascii_case(&format!("{base}.bin"))
        });
        let ours = fs::read_to_string(&cue).is_ok_and(|t| {
            t.starts_with(&format!("FILE \"{base}.bin\" BINARY\n  TRACK 01 ")) && t.matches("FILE ").count() == 1
        });
        if others || !ours {
            continue;
        }
        let bin = inner.join(format!("{base}.bin"));
        let entries: Vec<String> = fs::read_dir(inner).into_iter().flatten().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        let free = |n: &str| fs::read_dir(outer).into_iter().flatten().flatten().all(|e| !e.file_name().to_string_lossy().eq_ignore_ascii_case(n));
        let both = entries.len() == 2 && is_file(&bin) && free(&format!("{base}.bin"));
        // An earlier move stopped after the .bin: only the .cue is left.
        let cue_left = entries.len() == 1 && is_file(&outer.join(format!("{base}.bin")));
        if !(both || cue_left) || !free(&format!("{base}.cue")) {
            continue;
        }
        // The .bin first, then the .cue (see cue_left).
        let up = |p: &Path| fs::rename(p, outer.join(name_of(p)));
        if (cue_left || up(&bin).is_ok()) && up(&cue).is_ok() {
            let _ = fs::remove_dir(inner);
            log(&format!("moved {} up into {}", rel(&cue, &common), rel(outer, &common)));
            moved += 1;
        }
    }
    moved
}

/// Report .chd files under common that have no converted copy next to them.
fn report_chds(common: &Path, log: &mut dyn FnMut(&str)) -> usize {
    let mut skipped = 0;
    for image in walk(common, &|n| has_ext(n, ".chd")) {
        if is_file(&converted_cue(&image)) {
            continue;
        }
        let why = match chd_image::check(&image) {
            Ok(_) => explain("CHD image not extracted"),
            Err(e) => explain(&format!("CHD: {e}")),
        };
        log(&format!("skip {}: {why}", rel(&image, common)));
        skipped += 1;
    }
    skipped
}

// ---- game files ---------------------------------------------------------------

/// 'Game (USA) (Disc 2)' -> 'Game (USA)'; None for single-disc games.
fn shared_name(stem: &str) -> Option<String> {
    disc_tag(stem).map(|(_, name)| regex!(r"\s+").replace_all(name.trim(), " ").into_owned())
}

/// JSON string as Python's json.dumps writes it: non-ASCII as \uXXXX.
fn json_string(text: &str) -> String {
    let mut out = String::from("\"");
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\x08' => out.push_str("\\b"),
            '\x0c' => out.push_str("\\f"),
            c if (c as u32) < 0x20 || (c as u32) > 0x7e => {
                let mut units = [0u16; 2];
                for unit in c.encode_utf16(&mut units) {
                    out.push_str(&format!("\\u{unit:04x}"));
                }
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

pub(crate) fn instance_json(data_path: &str, slots: &[(i64, String)], writes: &[(u32, u32)]) -> String {
    let slots: Vec<String> = slots
        .iter()
        .map(|(id, filename)| format!("      {{\n        \"id\": {id},\n        \"filename\": {}\n      }}", json_string(filename)))
        .collect();
    let writes = if writes.is_empty() {
        String::new()
    } else {
        let w: Vec<String> = writes
            .iter()
            .map(|(a, d)| format!("      {{\n        \"address\": \"0x{a:08X}\",\n        \"data\": \"0x{d:08X}\"\n      }}"))
            .collect();
        format!(",\n    \"memory_writes\": [\n{}\n    ]", w.join(",\n"))
    };
    format!(
        "{{\n  \"instance\": {{\n    \"magic\": \"APF_VER_1\",\n    \"data_path\": {},\n    \"data_slots\": [\n{}\n    ]{writes}\n  }}\n}}\n",
        json_string(data_path),
        slots.join(",\n")
    )
}

// ---- per-game settings (pk_settings.sv in the core) ----------------------------------

/// The core's registers for an instance JSON's memory_writes, used while
/// Core Settings > Region and Cartridge are on Auto (their default): MiSTer's
/// main program and Saturn.sv take these from the disc's header, which the
/// Pocket doesn't read.
const AREA_AUTO: u32 = 0x8000_0020;
const CART_AUTO: u32 = 0x8000_0024;
const DEZAEMON2: u32 = 0x8000_0028;
/// MiSTer's RAM cartridge database (Saturn_MiSTer releases/saturn_ramcart.txt).
const RAMCART_DB: &str = include_str!("instances/saturn_ramcart.txt");

/// The disc's IP.BIN (its first 256 bytes) from the first data sector of
/// the file, or None when it is not a Saturn disc.
fn boot_header(path: &Path) -> Option<[u8; 256]> {
    let mut file = fs::File::open(path).ok()?;
    let mut header = [0u8; 256];
    file.seek(SeekFrom::Start(16)).ok()?;
    file.read_exact(&mut header).ok()?;
    header.starts_with(SATURN_ID).then_some(header)
}

/// The memory writes for a disc (from its first file), and what they say
/// for the report: the area (Saturn.sv: the first area symbol at 0x40), the
/// RAM cartridge MiSTer's database names for its product number (at 0x20),
/// and the Dezaemon 2 patch.
fn game_settings(first: &Path) -> (Vec<(u32, u32)>, String) {
    let Some(h) = boot_header(first) else { return (vec![], String::new()) };
    let mut writes = vec![];
    let mut notes = vec![];
    let area = match h[0x40] {
        b'J' => Some((0x1, "Japan")),
        b'T' => Some((0x2, "Asia NTSC")),
        b'U' => Some((0x4, "USA")),
        b'B' => Some((0x5, "Brazil")),
        b'K' => Some((0x6, "Korea")),
        b'A' => Some((0xA, "Asia PAL")),
        b'E' => Some((0xC, "Europe PAL")),
        b'L' => Some((0xD, "Latin America PAL")),
        _ => None,
    };
    if let Some((code, name)) = area {
        writes.push((AREA_AUTO, code));
        notes.push(format!("region {name}"));
    }
    // The product number as MiSTer's saturn_ramcart_make_uuid() reads it.
    let raw = &h[0x20..0x2A];
    let len = raw.iter().rposition(|&c| c != b' ' && c != 0).map_or(0, |i| i + 1);
    let id: String = raw[..len].iter().map(|&c| (c as char).to_ascii_uppercase()).collect();
    if len > 0 && !id.contains(|c: char| c.is_whitespace() || c == '\0') {
        let cart = RAMCART_DB.lines().find_map(|line| {
            let line = line.trim_start_matches('\u{feff}').split(['#', ';']).next()?.trim();
            let mut words = line.split_whitespace();
            let (uuid, kind) = (words.next()?, words.next()?);
            (words.next().is_none() && uuid.eq_ignore_ascii_case(&id)).then(|| kind.to_ascii_uppercase())
        });
        match cart.as_deref() {
            Some("1M") => {
                writes.push((CART_AUTO, 2));
                notes.push("1 MB RAM cartridge".into());
            }
            Some("4M") => {
                writes.push((CART_AUTO, 3));
                notes.push("4 MB RAM cartridge".into());
            }
            _ => {}
        }
    }
    if &h[0x20..0x28] == b"T-16804G" {
        writes.push((DEZAEMON2, 1));
        notes.push("Dezaemon 2 patch".into());
    }
    (writes, notes.iter().map(|n| format!(", {n}")).collect())
}

/// A Saturn core on the card.
pub struct Core {
    pub id: String,
    pub platform: String,
    /// Whether it reads its discs as this plugin's game files describe them.
    pub supported: bool,
}

/// Cores on the card that run Saturn games, sorted by folder name.
pub fn find_cores(card: &Path) -> Vec<Core> {
    let mut found = vec![];
    let Ok(entries) = fs::read_dir(card.join("Cores")) else { return found };
    for entry in entries.flatten() {
        if !entry.file_type().map_or(false, |t| t.is_dir()) {
            continue;
        }
        let id = entry.file_name().to_string_lossy().into_owned();
        let platform = read_json(&entry.path().join("core.json"))
            .and_then(|v| v.get("core")?.get("metadata")?.get("platform_ids")?.get(0)?.as_str().map(String::from));
        let ours = id.eq_ignore_ascii_case(crate::OURS);
        let platform = match platform {
            Some(p) if p == "saturn" || p == "ss" || p == "segasaturn" => p,
            None if ours => "saturn".to_string(),
            _ => continue,
        };
        // This core's game files: a .cue in slot 14 and its files from 16.
        let declared: Vec<i64> = declared_slots(card, &id).into_iter().flatten().filter_map(|s| s.0).collect();
        let supported = ours || (declared.contains(&CUE_ID) && declared.contains(&BIN_ID));
        found.push(Core { id, platform, supported });
    }
    found.sort_by(|a, b| a.id.cmp(&b.id));
    found
}

/// Where else on the card discs sit when Assets/<platform>/common has none
/// (games copied for another core's platform folder), or None.
pub fn games_elsewhere(card: &Path, platform: &str) -> Option<String> {
    let is_disc = |n: &str| (has_ext(n, ".cue") || has_ext(n, ".chd")) && !n.starts_with("._");
    if !walk(&card.join("Assets").join(platform).join("common"), &is_disc).is_empty() {
        return None;
    }
    let mut others: Vec<String> = ["saturn", "ss", "segasaturn"]
        .iter()
        .filter(|p| **p != platform && !walk(&card.join("Assets").join(p).join("common"), &is_disc).is_empty())
        .map(|p| format!("Assets/{p}/common"))
        .collect();
    others.sort();
    others.first().cloned()
}

/// Start Saves/<platform>/common/<name> from the newest per-disc save. Never overwrites.
fn seed_shared_save(card: &Path, platform: &str, core: &str, stems: &[String], name: &str, suffix: &str) -> Option<String> {
    let shared = card.join("Saves").join(platform).join("common").join(name);
    if shared.exists() {
        return None;
    }
    let ext = suffix.rsplit('.').next().unwrap_or("sav");
    let mut newest: Option<(PathBuf, std::time::SystemTime)> = None;
    for s in stems {
        for file in [
            card.join("Saves").join(platform).join(core).join(format!("{s}.{ext}")),
            card.join("Saves").join(platform).join("common").join(format!("{s}.{ext}")),
        ] {
            let Ok(meta) = fs::metadata(&file) else { continue };
            if !meta.is_file() || suffix.contains("(cartridge)") {
                continue;
            }
            let modified = meta.modified().unwrap_or(std::time::UNIX_EPOCH);
            if newest.as_ref().map_or(true, |n| modified > n.1) {
                newest = Some((file, modified));
            }
        }
    }
    let (newest, _) = newest?;
    let data = fs::read(&newest).ok()?;
    write_bytes(&shared, &data).ok()?;
    Some(name_of(&newest))
}

pub struct Summary {
    pub written: usize,
    pub shared: usize,
}

/// One disc ready for a game file.
struct Disc {
    /// The game file's name (the cue's or the lone .bin's).
    name: String,
    folder: PathBuf,
    /// (slot id, file below folder).
    slots: Vec<(i64, String)>,
    /// What the report says about it.
    what: String,
    /// Per-game settings (memory_writes).
    writes: Vec<(u32, u32)>,
}

/// Write Assets/<platform>/<core>/<game>.json for every playable disc under
/// Assets/<platform>/common. log(line) gets the report.
pub fn make_instances(card: &Path, core: &str, platform: &str, log: &mut dyn FnMut(&str)) -> Result<Summary, String> {
    let slots = slots(card, core);
    let common = card.join("Assets").join(platform).join("common");
    let out = card.join("Assets").join(platform).join(core);
    if !common.is_dir() {
        return Err(format!(
            "there is no Assets/{platform}/common folder on this SD card. Make it, put each game in its own folder there, then Generate game files again"
        ));
    }
    if !slots.declared {
        log(&format!(
            "warning: Cores/{core}/data.json is missing or unreadable: using the core's slots from its source (cue in slot {CUE_ID}, files from slot {BIN_ID}), at most {UNKNOWN_MAX_FILES} files per disc, and no shared saves for multi-disc games"
        ));
    }
    let (mut written, mut skipped) = (0usize, report_chds(&common, log));
    for (path, reason) in other_images(&common) {
        log(&format!("skip {}: {}", rel(&path, &common), explain(reason)));
        skipped += 1;
    }
    let mut discs: Vec<Disc> = vec![];
    for (bin, reason) in lone_bins(&common) {
        let shown = rel(&bin, &common);
        if let Some(reason) = reason {
            log(&format!("skip {shown}: {}", explain(reason)));
            skipped += 1;
            continue;
        }
        let name = name_of(&bin);
        if !saturn_disc(&bin, 0) {
            log(&format!("skip {shown}: it has no .cue and doesn't start with a Saturn disc's data track: put the disc's .cue next to it"));
            skipped += 1;
            continue;
        }
        let (writes, notes) = game_settings(&bin);
        discs.push(Disc {
            name: stem(&name).to_string(),
            folder: bin.parent().unwrap().to_path_buf(),
            slots: vec![(slots.bins[0], name)],
            what: format!("1 track, no .cue{notes}"),
            writes,
        });
    }
    for cue in walk(&common, &|n| has_ext(n, ".cue")) {
        let folder = cue.parent().unwrap().to_path_buf();
        let cue_name = name_of(&cue);
        let cue_stem = stem(&cue_name).to_string();
        let shown = rel(&cue, &common);
        let (files, reason) = match parse_cue(&cue) {
            Ok(files) => {
                let reason = disc_check(&cue, &files, &slots);
                (files, reason)
            }
            Err(e) => (vec![], Some(e)),
        };
        if let Some(reason) = reason {
            // A disc converted next to it (<name>/<name>.cue) stands in for it.
            if is_file(&converted_cue(&cue)) {
                continue;
            }
            log(&format!("skip {shown}: {}", explain(&reason)));
            skipped += 1;
            continue;
        }
        let first = join(&folder, &files[0].0);
        if playstation_disc(&first) {
            log(&format!("skip {shown}: it is a PlayStation disc, not a Saturn one: move it to the PlayStation core's folder"));
            skipped += 1;
            continue;
        }
        if !saturn_disc(&first, 0) {
            log(&format!("warning: {shown}: no Saturn disc ID (\"SEGA SEGASATURN\") in its first sector; the game file is made, but check that it is a Saturn disc"));
        }
        let tracks: usize = files.iter().map(|f| f.1.len()).sum();
        let (writes, notes) = game_settings(&first);
        let mut disc_slots = vec![(slots.cue, cue_name.clone())];
        disc_slots.extend(files.iter().enumerate().map(|(k, (n, _))| (slots.bins[k], n.clone())));
        discs.push(Disc {
            name: cue_stem,
            folder,
            slots: disc_slots,
            what: format!("{tracks} track{}, {} file{}{notes}", if tracks == 1 { "" } else { "s" }, files.len(), if files.len() == 1 { "" } else { "s" }),
            writes,
        });
    }
    // Multi-disc games share one save (the core has no disc swap: each disc
    // has its own game file, and disc 2 must see what disc 1 saved).
    let mut groups: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for d in &discs {
        if let Some(game) = shared_name(&d.name) {
            groups.entry(game).or_default().insert(d.name.clone());
        }
    }
    groups.retain(|_, members| members.len() > 1);
    let mut names = BTreeSet::new();
    for d in &discs {
        if !names.insert(d.name.clone()) {
            log(&format!("warning: two discs are named {}: only the last one found has a game file; rename one of them", d.name));
        }
        let data_path = rel(&d.folder, &common);
        let mut prefix = if data_path != "." { format!("{data_path}/") } else { String::new() };
        let mut entries = d.slots.clone();
        let mut shared_note = String::new();
        if let Some(game) = shared_name(&d.name).filter(|g| groups.contains_key(g) && !slots.saves.is_empty()) {
            // The shared saves are in Saves/<platform>/common; data_path would
            // put them below the disc's folder, so it goes into the names.
            for e in &mut entries {
                e.1 = format!("{prefix}{}", e.1);
            }
            for (id, suffix) in &slots.saves {
                entries.push((*id, format!("{game}{suffix}")));
            }
            prefix = String::new();
            shared_note = format!(", shared save {game}{}", slots.saves[0].1);
        }
        let file = out.join(format!("{}.json", d.name));
        write_bytes(&file, instance_json(&prefix, &entries, &d.writes).as_bytes()).map_err(|e| format!("{}.json: {e}", d.name))?;
        log(&format!("wrote {}.json ({}{shared_note})", d.name, d.what));
        written += 1;
    }
    let mut shared = 0;
    if !slots.saves.is_empty() {
        for (game, members) in &groups {
            shared += 1;
            let stems: Vec<String> = members.iter().cloned().collect();
            for (_, suffix) in &slots.saves {
                let name = format!("{game}{suffix}");
                if let Some(from) = seed_shared_save(card, platform, core, &stems, &name, suffix) {
                    log(&format!("shared save {name} starts from {from}"));
                }
            }
        }
    } else if !groups.is_empty() {
        log(&format!(
            "warning: {} multi-disc game(s) found, but the core's data.json declares no save slots: each disc keeps its own save",
            groups.len()
        ));
    }
    let shown_out = rel(&out, card);
    log(&format!(
        "{written} game files in {shown_out}, {skipped} skipped{}",
        if shared == 0 { String::new() } else { format!(", {shared} multi-disc games share a save") }
    ));
    match (written, skipped) {
        (0, 0) => log(&format!(
            "No games found in Assets/{platform}/common: put each disc's .chd, or its .cue and .bin files, in its own folder there, then Generate game files again."
        )),
        (0, _) => log("No game files made: fix what the skip lines above say, then Generate game files again."),
        _ => {}
    }
    Ok(Summary { written, shared })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout(text: &str, fsec: &[u64]) -> Result<Layout, String> { cue_layout(text.as_bytes(), fsec) }

    #[test]
    fn merged_chdman_cue() {
        let cue = "FILE \"x.bin\" BINARY\n  TRACK 01 MODE1/2352\n    INDEX 01 00:00:00\n  TRACK 02 AUDIO\n    PREGAP 00:02:00\n    INDEX 01 00:10:00\n";
        let l = layout(cue, &[2000]).unwrap();
        assert_eq!(l.tracks, 2);
        assert_eq!(l.extents, 2); // the pregap is a hole
        assert_eq!(l.leadout, 2000 + 150);
    }

    #[test]
    fn redump_per_track() {
        let cue = "FILE \"a (Track 1).bin\" BINARY\r\n  TRACK 01 MODE1/2352\r\n    INDEX 01 00:00:00\r\nFILE \"a (Track 2).bin\" BINARY\r\n  TRACK 02 AUDIO\r\n    INDEX 00 00:00:00\r\n    INDEX 01 00:02:00\r\n";
        let l = layout(cue, &[1000, 500]).unwrap();
        assert_eq!((l.tracks, l.extents, l.leadout), (2, 2, 1500)); // one extent per file
    }

    #[test]
    fn lower_case_is_not_read() {
        assert!(layout("file \"x.bin\" binary\n track 01 mode1/2352\n index 01 00:00:00\n", &[10]).is_err());
    }

    #[test]
    fn iso_track_is_not_read() {
        assert!(layout("FILE \"x.iso\" BINARY\n TRACK 01 MODE1/2048\n INDEX 01 00:00:00\n", &[10]).is_err());
    }

    #[test]
    fn too_many_files() {
        assert!(layout("FILE \"a\" BINARY\nTRACK 01 MODE1/2352\nINDEX 01 00:00:00\nFILE \"b\" BINARY\nTRACK 02 AUDIO\nINDEX 01 00:00:00\n", &[10]).is_err());
    }

    #[test]
    fn disc_tags() {
        assert_eq!(shared_name("Panzer Dragoon Saga (USA) (Disc 2)").as_deref(), Some("Panzer Dragoon Saga (USA)"));
        assert_eq!(shared_name("Riven - Disc 3").as_deref(), Some("Riven"));
        assert_eq!(shared_name("Virtua Fighter 2 (USA)"), None);
    }
}
