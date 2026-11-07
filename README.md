# Saturn game files plugin

A [Pocket plugin](https://github.com/openfpga-library/pocket-plugin) for
Pocket Sync and pupdate, for the Sega Saturn core
[akoaysigod/pocket_saturn_core](https://github.com/akoaysigod/pocket_saturn_core)
(`akoaysigod.Saturn`). Put `.chd` images on the SD card. The plugin turns them
into the files the core reads and writes a game file (`.json`) for every disc.

It is an adaptation of the
[PlayStation game files plugin](https://github.com/GuardianSoul/openfpga-PlayStation-plugin)
by GuardianSoul. The SD card safety (claimed space, verified writes,
resumable conversions) is the same. The disc layout follows the Saturn
core's own CD code (`src/fw/cdb/cd_src.c`).

## What the core reads

The core doesn't read CHD. It loads a disc from its data slots:

| slot | file |
|---|---|
| 14 | the disc's `.cue` |
| 16, 17, … | the files the `.cue` names, in order: one `.bin` for the whole disc (what `chdman extractcd` writes), or one `.bin` per track (Redump sets) |
| 16 alone, no cue | a one-track data disc |

The core reads the `.cue` itself with a small parser: keywords in upper case,
`AUDIO`, `MODE1/2352` and `MODE2/2352` tracks, `PREGAP`/`POSTGAP`, at most
99 tracks and 40 extents. Its data slot table holds only 32 entries, and the
BIOS, saves and cartridge slots come first. So a disc can have only as many
files as there are visible slots (26 with the layout above). The plugin works
the count out from the core's `data.json`.

## What the plugin does

- **CHD images.** Each `X.chd` is extracted into `X/X.cue` + `X/X.bin` next to
  it, byte for byte as `chdman extractcd` would write them (CD audio swapped
  to little-endian, pregaps as `INDEX 00` or `PREGAP`). You choose to keep the
  `.chd` or replace it.
- **Discs with too many files** (more tracks than the core has slots) are
  joined into one `.bin`, with a `.cue` whose indexes are moved to match.
- **Game files.** One `Assets/saturn/akoaysigod.Saturn/<disc>.json` per disc:
  the `.cue` in slot 14 and its files from slot 16. A `.bin` without a `.cue`
  gets slot 16 only.
- **Multi-disc games** ("… (Disc 1)", "… (Disc 2)"). The core can't swap
  discs, so each disc keeps its own game file. All discs of one game share
  one save (internal backup RAM and backup cartridge), in
  `Saves/saturn/common/<game>.sav`. Disc 2 sees what disc 1 saved. The
  shared save starts from the newest per-disc save already on the card.
- **Checks**, each with what to do about it:
  - a `.cue` the core's parser would reject (for example lower-case keywords, or `MODE1/2048` tracks);
  - missing or renamed track files;
  - files that aren't whole 2352-byte sectors;
  - `.iso`, `.ccd`, `.mds`, `.cdi`, `.nrg`, `.gdi`, ECM, zipped games, and WAVE/MP3 tracks;
  - CHDs made from an `.iso` (2048-byte sectors);
  - a PlayStation disc in the Saturn folder;
  - a disc with no `SEGA SEGASATURN` ID.
- **BIOS.** It checks `Assets/saturn/common/boot.rom`: exactly 512 KB, not
  blank, and not a PlayStation BIOS. If it isn't there, it looks for a 512 KB
  file under another name, such as `sega_101.bin`, and says to copy it as
  `boot.rom`.

## Use

Put your games in `Assets/saturn/common/`, each in its own folder. In Pocket
Sync or pupdate, add the plugin from this repository's link and put the SD
card in. Open *Saturn game files* and choose *Generate game files*. When
discs need converting, the plugin lists them and asks whether to keep the
originals, replace them, or skip them.

How it keeps a full card safe:

- Every large file first claims its size plus 256 MB on the card.
- A file gets its real name only after it is written, synced and read back.
- Originals are removed only after that.
- With *replace*, a disc that doesn't fit next to its original is converted
  on the computer first (`Saturn conversions/<game>`). Then the original is
  removed and the new files are copied over. If a run is stopped part-way,
  the next run finishes it.

Each run writes its full report to
`Assets/saturn/akoaysigod.Saturn/Game files report.txt`.

## Build and test

```
rustup target add wasm32-wasip1
scripts/build.sh               # target/wasm32-wasip1/release/plugin.wasm
```

`cargo test --lib --target x86_64-unknown-linux-gnu` runs the unit tests,
including a port of the core's cue parser. The `Test` workflow builds
`plugin.wasm`, runs it with `host/run_plugin.py` on a card made by
`tests/make_card.py`, and checks every file with `tests/verify.py`. The test
card has CHD images with stored and unstored pregaps, a multi-disc game, a
30-track disc, a lone `.bin`, and discs the core can't play. The workflow
runs once keeping the originals and once replacing them on a full card.
On a computer, without the wasm target:

```
cargo build --target x86_64-unknown-linux-gnu --example harness
python3 tests/make_card.py /tmp/t && cd /tmp/t && <repo>/target/x86_64-unknown-linux-gnu/debug/examples/harness keep
python3 <repo>/tests/verify.py /tmp/t keep
```

## Release

Pocket Sync and pupdate use the latest release, which must carry
`plugin.wasm` and `plugin.json`. Publishing a release runs
`.github/workflows/release.yml`, which builds `plugin.wasm` and attaches both
files.

## Notes

- The core's repository has no `dist/` folder, so its `data.json` isn't
  public. The plugin reads the slot ids from `Cores/akoaysigod.Saturn/data.json`
  on the card. Without that file it falls back to the ids in `cd_src.c`: cue
  14, files from 16, at most 25 files, and no shared saves.
- This plugin was written with an AI coding assistant (Anthropic Claude),
  from GuardianSoul's plugin. Review it with that in mind.

## License

MIT (see [LICENSE](LICENSE)). `plugin.wasm` also contains Rust crates, which
are listed with their license texts in
[THIRD_PARTY_LICENSES.md](THIRD_PARTY_LICENSES.md).
