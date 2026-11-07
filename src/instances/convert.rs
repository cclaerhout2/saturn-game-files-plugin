// SPDX-License-Identifier: MIT
// Discs the Saturn core cannot play as they are: CHD images (extracted to
// one .bin and its .cue) and discs with more files than the core has slots
// for (joined into one .bin with a .cue that names it). Both go into
// <folder>/<name>/<name>.cue next to the original.
//
// A full SD card corrupts easily, so every large file claims its space
// plus RESERVE before it is written (write_parts), and originals are only
// removed once the new files are written and read back. Keep needs room
// for the new files next to the originals. Replace removes the originals;
// when the card has no room for both, the disc is converted in the
// plugin's folder on the computer first, then the originals are removed
// and the new files copied over. If that copy fails, the converted files
// stay on the computer.

use super::{
    chd_image, claim, converted_cue, copy_range, has_ext, is_file, is_no_space, mb, msf_text, name_of, no_space, parse_cue, read_cue,
    rel, size, slots, stem, walk, write_bytes, write_parts, SECTOR,
};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

const NO_SPACE: &str = "not enough free space";
/// The plugin's own folder on the computer, under the names hosts give it.
const TEMP_ROOTS: [&str; 2] = ["host", "computer"];
const TEMP_FOLDER: &str = "Saturn conversions";
pub(crate) const SPACE_CHECK: &str = ".saturn-space-check";

#[derive(Clone, Copy, PartialEq)]
pub enum Mode {
    Keep,
    Replace,
}

enum Kind {
    /// Extract into dest.
    Chd { chd: PathBuf },
    /// Join a disc's files (sources, in cue order) into one .bin in dest.
    Join { cue: PathBuf, sources: Vec<PathBuf> },
}

pub struct Job {
    kind: Kind,
    /// Where the new files go: <folder>/<name>/.
    dest: PathBuf,
    /// The original, below common.
    shown: String,
    /// Bytes the new files take, and bytes Replace removes.
    pub bytes: u64,
    pub freed: u64,
}

/// io errors as report text; running out of space always starts with NO_SPACE.
pub(super) fn reason(e: io::Error) -> String {
    if is_no_space(&e) && !e.to_string().starts_with(NO_SPACE) {
        format!("{NO_SPACE} ({e})")
    } else {
        e.to_string()
    }
}

/// Discs under Assets/<platform>/common that need converting for this core.
pub fn plan(card: &Path, core: &str, platform: &str) -> Vec<Job> {
    let common = card.join("Assets").join(platform).join("common");
    let max_files = slots(card, core).bins.len();
    let mut jobs = vec![];
    let dest_of = |image: &Path| converted_cue(image).parent().unwrap().to_path_buf();
    for chd in walk(&common, &|n| has_ext(n, ".chd")) {
        if is_file(&converted_cue(&chd)) {
            continue;
        }
        let Ok(bytes) = chd_image::check(&chd) else { continue }; // the run reports why
        jobs.push(Job { shown: rel(&chd, &common), bytes, freed: size(&chd).unwrap_or(0), dest: dest_of(&chd), kind: Kind::Chd { chd } });
    }
    for cue in walk(&common, &|n| has_ext(n, ".cue")) {
        if is_file(&converted_cue(&cue)) {
            continue;
        }
        let Ok(files) = parse_cue(&cue) else { continue };
        if files.len() <= max_files {
            continue;
        }
        let folder = cue.parent().unwrap();
        let sources: Vec<PathBuf> = files.iter().map(|(n, _)| super::join(folder, n)).collect();
        let sizes: Option<Vec<u64>> = sources.iter().map(|s| size(s).filter(|b| b % SECTOR == 0 && *b > 0)).collect();
        let Some(sizes) = sizes else { continue }; // missing or damaged files: the run reports them
        if joined_cue(&cue, &sizes, "x.bin").is_err() {
            continue;
        }
        let bytes: u64 = sizes.iter().sum();
        jobs.push(Job { shown: rel(&cue, &common), bytes, freed: bytes, dest: dest_of(&cue), kind: Kind::Join { cue, sources } });
    }
    jobs
}

impl Job {
    /// The original, below Assets/<platform>/common.
    pub fn shown(&self) -> &str { &self.shown }
}

/// What Replace adds to the card in the end (negative: frees).
pub fn replace_net(jobs: &[Job]) -> i64 {
    jobs.iter().map(|j| j.bytes as i64 - j.freed as i64).sum()
}

/// The cue of a disc whose files are joined into image: every FILE after the
/// first goes, and each INDEX moves by the sectors of the files before it.
/// TRACK, FLAGS, PREGAP and POSTGAP stay; keywords in upper case, as the
/// core reads them.
fn joined_cue(cue: &Path, sizes: &[u64], image: &str) -> Result<String, String> {
    let text = read_cue(cue)?;
    let file = regex::Regex::new(r"(?i)^FILE\s").unwrap();
    let track = regex::Regex::new(r"(?i)^TRACK\s+([0-9]+)\s+(\S+)").unwrap();
    let index = regex::Regex::new(r"(?i)^INDEX\s+([0-9]+)\s+([0-9]+):([0-9]+):([0-9]+)").unwrap();
    let gap = regex::Regex::new(r"(?i)^(PREGAP|POSTGAP)\s+([0-9]+:[0-9]+:[0-9]+)").unwrap();
    let flags = regex::Regex::new(r"(?i)^FLAGS\s+(.*)$").unwrap();
    let (mut lines, mut f, mut offset) = (vec![], -1i64, 0u64);
    for line in text.split(['\n', '\r']) {
        let words = line.trim().trim_start_matches('\u{feff}');
        if file.is_match(words) {
            if f >= 0 {
                offset += sizes.get(f as usize).ok_or("more FILE lines than files")? / SECTOR;
            } else {
                lines.push(format!("FILE \"{image}\" BINARY"));
            }
            f += 1;
        } else if let Some(m) = track.captures(words) {
            lines.push(format!("  TRACK {:02} {}", m[1].parse::<u64>().map_err(|e| e.to_string())?, m[2].to_uppercase()));
        } else if let Some(m) = index.captures(words) {
            let n = |i: usize| m[i].parse::<u64>().map_err(|e| e.to_string());
            let frames = (n(2)? * 60 + n(3)?) * 75 + n(4)?;
            lines.push(format!("    INDEX {:02} {}", n(1)?, msf_text(offset + frames)));
        } else if let Some(m) = gap.captures(words) {
            lines.push(format!("    {} {}", m[1].to_uppercase(), &m[2]));
        } else if let Some(m) = flags.captures(words) {
            lines.push(format!("    FLAGS {}", m[1].to_uppercase()));
        }
    }
    if f + 1 != sizes.len() as i64 {
        return Err("FILE lines and files differ".into());
    }
    Ok(lines.join("\n") + "\n")
}

fn temp_root() -> Option<PathBuf> {
    TEMP_ROOTS.iter().map(PathBuf::from).find(|p| p.is_dir())
}

/// Whether bytes plus RESERVE fit on the card now (claimed, then let go).
fn has_room(card: &Path, bytes: u64) -> bool {
    let check = card.join(SPACE_CHECK);
    let ok = fs::File::create(&check).and_then(|f| claim(&f, &check, bytes)).is_ok();
    let _ = fs::remove_file(&check);
    ok
}

/// Write the job's new files into dest; returns the new cue.
fn produce(job: &Job, dest: &Path, log: &mut dyn FnMut(&str)) -> Result<PathBuf, String> {
    match &job.kind {
        Kind::Chd { chd } => {
            log(&format!("  extracting {} ({} MB)", name_of(chd), mb(job.bytes)));
            chd_image::extract(chd, dest)
        }
        Kind::Join { cue, sources } => {
            let base = stem(&name_of(cue)).to_string();
            let image = format!("{base}.bin");
            log(&format!("  joining {} files into {image} ({} MB)", sources.len(), mb(job.bytes)));
            let sizes: Vec<u64> = sources.iter().map(|s| size(s).unwrap_or(0)).collect();
            let text = joined_cue(cue, &sizes, &image)?;
            write_parts(&dest.join(&image), Some(job.bytes), &mut |w| {
                for (path, bytes) in sources.iter().zip(&sizes) {
                    copy_range(path, 0, *bytes, w)?;
                }
                Ok(())
            })
            .map_err(reason)?;
            let out = dest.join(format!("{base}.cue"));
            write_bytes(&out, text.as_bytes()).map_err(reason)?;
            Ok(out)
        }
    }
}

/// The job's originals, removed by Replace.
fn originals(job: &Job) -> Vec<PathBuf> {
    match &job.kind {
        Kind::Chd { chd } => vec![chd.clone()],
        Kind::Join { cue, sources } => sources.iter().cloned().chain([cue.clone()]).collect(),
    }
}

/// Remove the job's originals (its new files are in place on the card).
fn replace_originals(job: &Job, log: &mut dyn FnMut(&str)) -> Result<(), String> {
    for path in originals(job) {
        if path.exists() {
            fs::remove_file(&path).map_err(|e| format!("{}: {e}", name_of(&path)))?;
        }
    }
    log(&format!("  removed the original {}", job.shown));
    Ok(())
}

/// The journal of a conversion made on the computer, in its work folder
/// next to files/. It is written once the new files are complete, before
/// anything on the card changes, so a run that stops part-way (closed, card
/// pulled, power lost) is finished by the next one (resume). Paths are below
/// the card.
#[derive(serde::Serialize, serde::Deserialize)]
struct Journal {
    /// The original, as reported.
    shown: String,
    /// Where files/ goes on the card.
    dest: String,
    /// Originals to remove first (the room the copy needs).
    remove: Vec<String>,
}

const JOURNAL: &str = "journal.json";

/// Tests stop the plugin at a named step (setting test_stop_at) to check
/// that the next run finishes the job.
fn test_stop(step: &str) {
    if super::setting("test_stop_at").as_deref() == Some(step) {
        panic!("stopped at {step} (test)");
    }
}

/// Convert in the computer's folder, then swap the originals on the card for
/// the new files.
fn through_computer(card: &Path, job: &Job, temp: &Path, log: &mut dyn FnMut(&str)) -> Result<PathBuf, String> {
    let work = temp.join(TEMP_FOLDER).join(stem(&name_of(Path::new(&job.shown))));
    if work.join(JOURNAL).exists() {
        // A ready conversion that resume could not finish: never overwrite it.
        return Err(format!("an earlier conversion is still waiting to be copied back, in the plugin's folder under \"{TEMP_FOLDER}/{}\"", name_of(&work)));
    }
    let _ = fs::remove_dir_all(&work); // unfinished: its originals are still on the card
    log("  the SD card is short of space: converting on the computer first");
    let files_dir = work.join("files");
    let made = produce(job, &files_dir, log).inspect_err(|_| {
        let _ = fs::remove_dir_all(&work);
    })?;
    // After the originals go, the new files must fit with RESERVE to spare.
    let extra = job.bytes.saturating_sub(job.freed);
    if !has_room(card, extra) {
        let _ = fs::remove_dir_all(&work);
        return Err(no_space(extra).to_string());
    }
    let journal = Journal {
        shown: job.shown.clone(),
        dest: rel(&job.dest, card),
        remove: originals(job).iter().map(|p| rel(p, card)).collect(),
    };
    let text = serde_json::to_string_pretty(&journal).map_err(|e| e.to_string())?;
    write_bytes(&work.join(JOURNAL), text.as_bytes()).map_err(reason)?;
    test_stop("journal");
    finish(card, &work, &journal, log)?;
    Ok(card.join(&journal.dest).join(rel(&made, &files_dir)))
}

/// Carry out a journal: remove the originals, copy files/ to the card
/// (each file written, synced and read back), then drop the work folder.
/// Every step can be repeated, so a stopped run is simply finished again.
fn finish(card: &Path, work: &Path, journal: &Journal, log: &mut dyn FnMut(&str)) -> Result<(), String> {
    let files_dir = work.join("files");
    for path in &journal.remove {
        let path = card.join(path);
        if path.exists() {
            fs::remove_file(&path).map_err(|e| format!("{}: {e}", name_of(&path)))?;
        }
        test_stop("remove");
    }
    let dest = card.join(&journal.dest);
    for file in walk(&files_dir, &|_| true) {
        let target = dest.join(rel(&file, &files_dir));
        let bytes = size(&file).unwrap_or(0);
        if same_file(&file, &target) {
            continue; // copied before the run stopped
        }
        write_parts(&target, Some(bytes), &mut |w| copy_range(&file, 0, bytes, w)).map_err(|e| {
            format!(
                "copying back to the SD card failed ({}); the original is removed and the converted files are kept on \
                 the computer, in the plugin's folder under \"{TEMP_FOLDER}/{}\". The next run copies them back.",
                reason(e),
                name_of(work)
            )
        })?;
        test_stop("copied");
    }
    let _ = fs::remove_dir_all(work);
    log(&format!("  removed the original {}", journal.shown));
    Ok(())
}

/// Whether two files hold the same bytes.
fn same_file(a: &Path, b: &Path) -> bool {
    use std::io::Read;
    if size(a).is_none() || size(a) != size(b) {
        return false;
    }
    let (Ok(mut a), Ok(mut b)) = (fs::File::open(a), fs::File::open(b)) else { return false };
    fn fill(f: &mut fs::File, buf: &mut [u8]) -> io::Result<usize> {
        let mut n = 0;
        while n < buf.len() {
            match f.read(&mut buf[n..])? {
                0 => break,
                k => n += k,
            }
        }
        Ok(n)
    }
    let (mut x, mut y) = (vec![0u8; 1 << 20], vec![0u8; 1 << 20]);
    loop {
        let (Ok(n), Ok(m)) = (fill(&mut a, &mut x), fill(&mut b, &mut y)) else { return false };
        if n != m || x[..n] != y[..m] {
            return false;
        }
        if n == 0 {
            return true;
        }
    }
}

/// Finish conversions that a stopped run left in the computer's folder (see
/// Journal); folders without a journal never touched the card and go. Runs
/// before anything else.
pub fn resume(card: &Path, log: &mut dyn FnMut(&str)) {
    let Some(temp) = temp_root() else { return };
    let Ok(entries) = fs::read_dir(temp.join(TEMP_FOLDER)) else { return };
    for work in entries.flatten().map(|e| e.path()).filter(|p| p.is_dir()) {
        let Ok(text) = fs::read_to_string(work.join(JOURNAL)) else {
            if work.join("files").is_dir() {
                let _ = fs::remove_dir_all(&work); // stopped while converting; the original is on the card
            }
            continue;
        };
        let Ok(journal) = serde_json::from_str::<Journal>(&text) else {
            log(&format!("could not read {TEMP_FOLDER}/{}/{JOURNAL}; its files are left there", name_of(&work)));
            continue;
        };
        log(&format!("Finishing the conversion of {} that an earlier run left part-way", journal.shown));
        let files_dir = work.join("files");
        let dest = card.join(&journal.dest);
        let new: Vec<PathBuf> = walk(&files_dir, &|_| true);
        let needed: u64 = new
            .iter()
            .map(|f| size(f).unwrap_or(0))
            .sum::<u64>()
            .saturating_sub(new.iter().map(|f| size(&dest.join(rel(f, &files_dir))).unwrap_or(0)).sum())
            .saturating_sub(journal.remove.iter().map(|p| size(&card.join(p)).unwrap_or(0)).sum());
        if !has_room(card, needed) {
            log(&format!("  {}; the files stay in the plugin's folder under \"{TEMP_FOLDER}/{}\"", no_space(needed), name_of(&work)));
            continue;
        }
        if let Err(e) = finish(card, &work, &journal, log) {
            log(&format!("  {e}"));
        }
    }
}

fn run_job(card: &Path, job: &Job, mode: Mode, temp: Option<&Path>, log: &mut dyn FnMut(&str)) -> Result<PathBuf, String> {
    let existed = job.dest.exists();
    match produce(job, &job.dest, log) {
        Ok(made) => {
            if mode == Mode::Replace {
                replace_originals(job, log)?;
            }
            Ok(made)
        }
        Err(e) => {
            if !existed {
                let _ = fs::remove_dir_all(&job.dest);
            }
            match (e.starts_with(NO_SPACE), mode, temp) {
                (true, Mode::Replace, Some(temp)) => through_computer(card, job, temp, log),
                (true, Mode::Keep, _) => Err(format!("{e}; converting with \"replace the originals\" needs less")),
                _ => Err(e),
            }
        }
    }
}

/// Convert the planned discs.
pub fn run(card: &Path, platform: &str, jobs: &[Job], mode: Mode, log: &mut dyn FnMut(&str)) -> (usize, usize) {
    let common = card.join("Assets").join(platform).join("common");
    let temp = temp_root();
    let (mut done, mut failed) = (0, 0);
    for job in jobs {
        match run_job(card, job, mode, temp.as_deref(), log) {
            Ok(made) => {
                log(&format!("converted {} -> {}", job.shown, rel(&made, &common)));
                done += 1;
            }
            Err(e) => {
                log(&format!("skip {}: {e}", job.shown));
                failed += 1;
            }
        }
    }
    log(&format!("{done} discs converted, {failed} failed"));
    (done, failed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn join_moves_indexes() {
        let dir = std::env::temp_dir().join("saturn-join-test");
        let _ = fs::create_dir_all(&dir);
        let cue = dir.join("g.cue");
        fs::write(
            &cue,
            "REM x\nFILE \"g (Track 1).bin\" BINARY\n  TRACK 01 MODE1/2352\n    INDEX 01 00:00:00\nFILE \"g (Track 2).bin\" BINARY\n  TRACK 02 AUDIO\n    FLAGS dcp\n    INDEX 00 00:00:00\n    INDEX 01 00:02:00\n",
        )
        .unwrap();
        let text = joined_cue(&cue, &[1000 * SECTOR, 500 * SECTOR], "g.bin").unwrap();
        assert_eq!(
            text,
            "FILE \"g.bin\" BINARY\n  TRACK 01 MODE1/2352\n    INDEX 01 00:00:00\n  TRACK 02 AUDIO\n    FLAGS DCP\n    INDEX 00 00:13:25\n    INDEX 01 00:15:25\n"
        );
        let layout = super::super::cue_layout(text.as_bytes(), &[1500]).unwrap();
        assert_eq!((layout.tracks, layout.leadout), (2, 1500));
    }
}
