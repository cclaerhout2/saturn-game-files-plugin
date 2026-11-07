#!/usr/bin/env python3
"""Build a test SD card for the Saturn game files plugin, with CHD images
written here (uncompressed CHD v5, the layout chdman uses for CDs: 8 frames
of 2352 + 96 subcode bytes per hunk, tracks padded to 4 frames, CD audio
big-endian), and the files each disc should come out as.

  make_card.py OUT_DIR    -> OUT_DIR/pocket (the card), OUT_DIR/expected.json
"""
import hashlib
import json
import os
import random
import struct
import sys

SECTOR = 2352
UNIT = 2448
FRAMES_PER_HUNK = 8


def msf(frames):
    return '%02d:%02d:%02d' % (frames // 4500, frames // 75 % 60, frames % 75)


def data_sector(lba, rng, first=False):
    """A raw MODE1 sector: sync, header, user data (the Saturn ID first)."""
    sync = b'\x00' + b'\xff' * 10 + b'\x00'
    a = lba + 150
    bcd = lambda v: (v // 10) * 16 + v % 10
    header = bytes([bcd(a // 4500), bcd(a // 75 % 60), bcd(a % 75), 1])
    body = bytearray(rng.randbytes(SECTOR - 16))
    if first:
        body[:16] = b'SEGA SEGASATURN '
    return sync + header + bytes(body)


def make_disc(rng, tracks):
    """tracks: [(kind, frames)] -> [(kind, bytes)] (audio little-endian, as in a .bin)."""
    out, lba = [], 0
    for kind, frames in tracks:
        if kind == 'AUDIO':
            data = rng.randbytes(frames * SECTOR)
        else:
            data = b''.join(data_sector(lba + i, rng, first=(lba + i == 0)) for i in range(frames))
        out.append((kind, data))
        lba += frames
    return out


def write_chd(path, tracks, pregaps):
    """tracks: [(kind, bin bytes)], pregaps: [(frames, stored)] -> uncompressed CHD v5."""
    frames_data = bytearray()
    meta = []
    for n, ((kind, data), (pregap, stored)) in enumerate(zip(tracks, pregaps), 1):
        frames = len(data) // SECTOR
        for i in range(frames):
            f = data[i * SECTOR:(i + 1) * SECTOR]
            if kind == 'AUDIO':
                f = bytes(b for pair in zip(f[1::2], f[0::2]) for b in pair)
            frames_data += f + bytes(96)
        pad = (4 - frames % 4) % 4
        frames_data += bytes(UNIT * pad)
        ctype = 'AUDIO' if kind == 'AUDIO' else ('MODE1_RAW' if kind.startswith('MODE1') else 'MODE2_RAW')
        text = 'TRACK:%d TYPE:%s SUBTYPE:NONE FRAMES:%d PREGAP:%d PGTYPE:%s PGSUB:NONE POSTGAP:0' % (
            n, ctype, frames, pregap, ('V' + ctype) if stored and pregap else 'MODE1' if pregap else 'MODE1')
        meta.append(text.encode() + b'\0')
    hunk_bytes = UNIT * FRAMES_PER_HUNK
    while len(frames_data) % hunk_bytes:
        frames_data += bytes(UNIT)
    hunks = len(frames_data) // hunk_bytes
    header_len = 124
    map_offset = header_len
    map_len = 4 * hunks
    meta_offset = map_offset + map_len
    meta_blob = bytearray()
    for k, m in enumerate(meta):
        start = meta_offset + len(meta_blob)
        nxt = start + 16 + len(m) if k + 1 < len(meta) else 0
        meta_blob += b'CHT2' + bytes([1]) + len(m).to_bytes(3, 'big') + struct.pack('>Q', nxt) + m
    data_offset = meta_offset + len(meta_blob)
    data_offset += (-data_offset) % hunk_bytes
    first_hunk = data_offset // hunk_bytes
    hmap = b''.join(struct.pack('>I', first_hunk + i) for i in range(hunks))
    raw_sha = hashlib.sha1(frames_data).digest()
    header = b'MComprHD' + struct.pack('>II', header_len, 5) + bytes(16) + struct.pack(
        '>QQQII', len(frames_data), map_offset, meta_offset, hunk_bytes, UNIT) + raw_sha + raw_sha + bytes(20)
    assert len(header) == header_len
    with open(path, 'wb') as f:
        f.write(header + hmap + meta_blob)
        f.write(bytes(data_offset - f.tell()))
        f.write(frames_data)


def chdman_cue(name, tracks, pregaps):
    """What chdman extractcd writes for this disc."""
    lines, written = ['FILE "%s.bin" BINARY' % name], 0
    for n, ((kind, data), (pregap, stored)) in enumerate(zip(tracks, pregaps), 1):
        lines.append('  TRACK %02d %s' % (n, 'AUDIO' if kind == 'AUDIO' else kind))
        if pregap and not stored:
            lines += ['    PREGAP %s' % msf(pregap), '    INDEX 01 %s' % msf(written)]
        elif pregap:
            lines += ['    INDEX 00 %s' % msf(written), '    INDEX 01 %s' % msf(written + pregap)]
        else:
            lines.append('    INDEX 01 %s' % msf(written))
        written += len(data) // SECTOR
    return '\n'.join(lines) + '\n'


def main():
    out = sys.argv[1]
    card = os.path.join(out, 'pocket')
    common = os.path.join(card, 'Assets/saturn/common')
    core = os.path.join(card, 'Cores/akoaysigod.Saturn')
    os.makedirs(common, exist_ok=True)
    os.makedirs(core, exist_ok=True)
    os.makedirs(os.path.join(out, 'host'), exist_ok=True)
    rng = random.Random(7)
    json.dump({'core': {'metadata': {'platform_ids': ['saturn'], 'shortname': 'Saturn'}}}, open(os.path.join(core, 'core.json'), 'w'))
    # The core's data.json as packaged (a guess at its order: BIOS, cartridge
    # ROM, the two saves, then the disc): 38 slots, of which the core sees 32.
    slots = [
        {'name': 'BIOS', 'id': 0, 'filename': 'boot.rom', 'parameters': '0x0', 'size_exact': 524288},
        {'name': 'Cartridge', 'id': 1, 'parameters': '0x1', 'extensions': ['bin']},
        {'name': 'Save', 'id': 2, 'parameters': '0xA4', 'extensions': ['sav']},
        {'name': 'Cartridge save', 'id': 3, 'parameters': '0xA4', 'extensions': ['sav']},
        {'name': 'Disc', 'id': 14, 'parameters': '0x109', 'extensions': ['cue']},
        {'name': 'Audio', 'id': 15, 'parameters': '0x109', 'extensions': ['bin']},
    ] + [{'name': 'Track %d' % (k + 1), 'id': 16 + k, 'parameters': '0x109', 'extensions': ['bin']} for k in range(32)]
    json.dump({'data': {'magic': 'APF_VER_1', 'data_slots': slots}}, open(os.path.join(core, 'data.json'), 'w'), indent=2)
    expected = {}

    def chd(folder, name, layout, pregaps):
        d = os.path.join(common, folder)
        os.makedirs(d, exist_ok=True)
        tracks = make_disc(rng, layout)
        write_chd(os.path.join(d, name + '.chd'), tracks, pregaps)
        expected[folder + '/' + name] = {
            'cue': chdman_cue(name, tracks, pregaps),
            'bin_sha1': hashlib.sha1(b''.join(t[1] for t in tracks)).hexdigest(),
            'flat': True,
        }

    chd('Game A (USA)', 'Game A (USA)', [('MODE1/2352', 300), ('AUDIO', 151), ('AUDIO', 77)], [(0, False), (150, True), (150, True)])
    chd('Game B (Japan)', 'Game B (Japan)', [('MODE1/2352', 97), ('AUDIO', 40)], [(0, False), (150, False)])
    chd('Saga (USA) (Disc 1)', 'Saga (USA) (Disc 1)', [('MODE1/2352', 50)], [(0, False)])
    chd('Saga (USA) (Disc 2)', 'Saga (USA) (Disc 2)', [('MODE1/2352', 60)], [(0, False)])
    # A per-track Redump set, played as it is.
    d = os.path.join(common, 'Redump X (Europe)')
    os.makedirs(d)
    tracks = make_disc(rng, [('MODE1/2352', 120), ('AUDIO', 200)])
    cue = []
    for n, (kind, data) in enumerate(tracks, 1):
        fn = 'Redump X (Europe) (Track %d).bin' % n
        open(os.path.join(d, fn), 'wb').write(data)
        cue += ['FILE "%s" BINARY' % fn, '  TRACK %02d %s' % (n, kind)]
        cue += ['    INDEX 01 00:00:00'] if n == 1 else ['    INDEX 00 00:00:00', '    INDEX 01 00:02:00']
    open(os.path.join(d, 'Redump X (Europe).cue'), 'w', newline='\r\n').write('\n'.join(cue) + '\n')
    # 30 files: more than the 26 slots the core sees: joined.
    d = os.path.join(common, 'Many Tracks (USA)')
    os.makedirs(d)
    tracks = make_disc(rng, [('MODE1/2352', 30)] + [('AUDIO', 20 + k) for k in range(29)])
    cue = []
    for n, (kind, data) in enumerate(tracks, 1):
        fn = 'Many Tracks (USA) (Track %02d).bin' % n
        open(os.path.join(d, fn), 'wb').write(data)
        cue += ['FILE "%s" BINARY' % fn, '  TRACK %02d %s' % (n, kind)]
        cue += ['    INDEX 01 00:00:00'] if n == 1 else ['    INDEX 00 00:00:00', '    INDEX 01 00:02:00']
    open(os.path.join(d, 'Many Tracks (USA).cue'), 'w').write('\n'.join(cue) + '\n')
    expected['Many Tracks (USA)/Many Tracks (USA)'] = {'bin_sha1': hashlib.sha1(b''.join(t[1] for t in tracks)).hexdigest()}
    # A lone .bin (one data track, no cue).
    d = os.path.join(common, 'Lone')
    os.makedirs(d)
    open(os.path.join(d, 'Lone Game (USA).bin'), 'wb').write(make_disc(rng, [('MODE1/2352', 10)])[0][1])
    # A cue in lower case (the core can't read it), an .iso, a PlayStation disc.
    d = os.path.join(common, 'Lower')
    os.makedirs(d)
    open(os.path.join(d, 'Lower.bin'), 'wb').write(make_disc(rng, [('MODE1/2352', 10)])[0][1])
    open(os.path.join(d, 'Lower.cue'), 'w').write('file "Lower.bin" binary\n  track 01 mode1/2352\n    index 01 00:00:00\n')
    os.makedirs(os.path.join(common, 'Iso'))
    open(os.path.join(common, 'Iso/Old Rip.iso'), 'wb').write(bytes(2048 * 4))
    d = os.path.join(common, 'PSX')
    os.makedirs(d)
    psx = bytearray(SECTOR * 6)
    psx[4 * SECTOR + 24:4 * SECTOR + 60] = b'Licensed by Sony Computer Entertain'.ljust(36)
    open(os.path.join(d, 'Crash.bin'), 'wb').write(bytes(psx))
    open(os.path.join(d, 'Crash.cue'), 'w').write('FILE "Crash.bin" BINARY\n  TRACK 01 MODE2/2352\n    INDEX 01 00:00:00\n')
    # A disc converted by version 0.1.0/0.1.1 (into <name>/<name>.cue, the .chd
    # replaced): moved up next to where the .chd was.
    d = os.path.join(common, 'Old Game - Disc 1', 'Old Game (Europe)')
    os.makedirs(d)
    tracks = make_disc(rng, [('MODE1/2352', 25)])
    open(os.path.join(d, 'Old Game (Europe).bin'), 'wb').write(tracks[0][1])
    open(os.path.join(d, 'Old Game (Europe).cue'), 'w').write(chdman_cue('Old Game (Europe)', tracks, [(0, False)]))
    # Two CHDs in one folder: both converted next to them.
    for n in (1, 2):
        name = 'Duo (Japan) (Disc %d)' % n
        layout = [('MODE1/2352', 30 + n)]
        tracks = make_disc(rng, layout)
        write_chd(os.path.join(common, 'Duo', name + '.chd'), tracks, [(0, False)]) if os.path.isdir(os.path.join(common, 'Duo')) else (os.makedirs(os.path.join(common, 'Duo')), write_chd(os.path.join(common, 'Duo', name + '.chd'), tracks, [(0, False)]))
        expected['Duo/' + name] = {'cue': chdman_cue(name, tracks, [(0, False)]), 'bin_sha1': hashlib.sha1(tracks[0][1]).hexdigest(), 'flat': True}
    # A per-disc save from before (Saga disc 1), to start the shared save from.
    saves = os.path.join(card, 'Saves/saturn/akoaysigod.Saturn')
    os.makedirs(saves)
    open(os.path.join(saves, 'Saga (USA) (Disc 1).sav'), 'wb').write(b'\xffB' * 32768)
    # The BIOS, under another name.
    open(os.path.join(common, 'sega_101.bin'), 'wb').write(rng.randbytes(524288))
    json.dump(expected, open(os.path.join(out, 'expected.json'), 'w'), indent=1)


if __name__ == '__main__':
    main()
