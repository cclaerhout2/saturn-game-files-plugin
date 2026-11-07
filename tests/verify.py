#!/usr/bin/env python3
"""Check a test card after the plugin ran (make_card.py made it): the
converted discs' files against what chdman writes, and the game files."""
import hashlib
import json
import os
import sys

out = sys.argv[1]
mode = sys.argv[2] if len(sys.argv) > 2 else 'keep'
card = os.path.join(out, 'pocket')
common = os.path.join(card, 'Assets/saturn/common')
games = os.path.join(card, 'Assets/saturn/akoaysigod.Saturn')
expected = json.load(open(os.path.join(out, 'expected.json')))
fails = []


def check(ok, what):
    print(('ok   ' if ok else 'FAIL ') + what)
    if not ok:
        fails.append(what)


for key, want in expected.items():
    folder, name = key.split('/')
    base = os.path.join(common, folder, name)
    cue, binf = base + '/' + name + '.cue', base + '/' + name + '.bin'
    check(os.path.isfile(cue) and os.path.isfile(binf), key + ': converted .cue and .bin')
    if 'cue' in want:
        check(open(cue).read() == want['cue'], key + ': .cue as chdman writes it')
    check(hashlib.sha1(open(binf, 'rb').read()).hexdigest() == want['bin_sha1'], key + ': .bin bytes')
    original = os.path.join(common, folder, name + ('.chd' if 'cue' in want else '.cue'))
    check(os.path.exists(original) == (mode == 'keep'), key + ': original ' + ('kept' if mode == 'keep' else 'removed'))


def game(name):
    return json.load(open(os.path.join(games, name + '.json')))['instance']


g = game('Game A (USA)')
check(g['data_path'] == 'Game A (USA)/Game A (USA)/' and g['data_slots'] == [
    {'id': 14, 'filename': 'Game A (USA).cue'}, {'id': 16, 'filename': 'Game A (USA).bin'}], 'Game A: game file')
g = game('Redump X (Europe)')
check([s['id'] for s in g['data_slots']] == [14, 16, 17] and g['data_slots'][2]['filename'] == 'Redump X (Europe) (Track 2).bin',
      'Redump X: per-track files as they are')
g = game('Lone Game (USA)')
check(g['data_slots'] == [{'id': 16, 'filename': 'Lone Game (USA).bin'}] and g['data_path'] == 'Lone/', 'lone .bin: slot 16 alone')
for n in (1, 2):
    g = game('Saga (USA) (Disc %d)' % n)
    check(g['data_path'] == '' and {'id': 2, 'filename': 'Saga (USA).sav'} in g['data_slots']
          and {'id': 3, 'filename': 'Saga (USA) (cartridge).sav'} in g['data_slots']
          and g['data_slots'][0]['filename'] == 'Saga (USA) (Disc %d)/Saga (USA) (Disc %d)/Saga (USA) (Disc %d).cue' % (n, n, n),
          'Saga disc %d: shared saves' % n)
check(open(os.path.join(card, 'Saves/saturn/common/Saga (USA).sav'), 'rb').read() == b'\xffB' * 32768, 'shared save seeded from disc 1')
g = game('Many Tracks (USA)')
check(len(g['data_slots']) == 2, 'Many Tracks: joined into one .bin')
for bad in ('Lower', 'Crash', 'Old Rip'):
    check(not os.path.exists(os.path.join(games, bad + '.json')), bad + ': no game file')
left = [os.path.join(r, f) for r, _, fs in os.walk(card) for f in fs if f.endswith('.part')]
check(not left, 'no .part files left')
print('%d failed' % len(fails))
sys.exit(1 if fails else 0)
