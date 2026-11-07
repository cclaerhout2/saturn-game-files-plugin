// Runs the plugin's steps on a folder with a native build (no Pocket Sync):
// cd to a folder holding the SD card as `pocket` (and the computer's
// folder as `host`), then: harness [keep|replace|skip].
use plugin::instances::{self, convert};
use std::path::Path;

fn main() {
    let mode = std::env::args().nth(1).unwrap_or_else(|| "keep".into());
    let card = Path::new("pocket");
    let mut log = |line: &str| println!("{line}");
    let cores = instances::find_cores(card);
    let core = cores.iter().find(|c| c.supported).expect("no supported core");
    let (core, platform) = (core.id.clone(), core.platform.clone());
    convert::resume(card, &mut log);
    let left = instances::clean_leftovers(card, &platform);
    if left > 0 {
        log(&format!("removed {left} leftovers"));
    }
    let jobs = convert::plan(card, &core, &platform);
    for j in &jobs {
        log(&format!("needs converting: {} ({} bytes)", j.shown(), j.bytes));
    }
    let mode = match mode.as_str() {
        "keep" => Some(convert::Mode::Keep),
        "replace" => Some(convert::Mode::Replace),
        _ => None,
    };
    if let Some(mode) = mode {
        convert::run(card, &platform, &jobs, mode, &mut log);
    }
    match instances::make_instances(card, &core, &platform, &mut log) {
        Ok(s) => log(&format!("[summary written={} shared={}]", s.written, s.shared)),
        Err(e) => log(&format!("Stopped: {e}")),
    }
    for line in instances::bios::check_bios(card, &core, &platform) {
        log(&line);
    }
}
