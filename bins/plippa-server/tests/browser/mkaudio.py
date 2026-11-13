"""Audio fixtures for addmedia_flow.mjs: real (silent) WAV files, some with RIFF INFO tags.

usage: mkaudio.py <dir>
  mix1h.wav            1 hour (8 kHz, 8 bit -> 28.8 MB), tagged artist "DJ Test" (so the import is a
                       release; an untagged file would become a looseTrack)
  tracks/NN Name.wav   8 one-second tracks tagged artist "The Band", album "Debut"
  notes.txt, fake.mp3  not audio (text / html with an .mp3 name)
  big.wav              ~700 MB of silence for the "kill the browser mid-upload" check
  art/loose/back.png, wide.png   PNG cover files (wide.png is 500x250, not square)
  art/loose/gif.gif, junk.bmp    image types the portal refuses (only JPG, WebP, PNG are art)

The tagged MP3/FLAC/M4A fixtures with embedded art (addmedia_art.mjs) are written by
`cargo run -p plippa-server --example mkaudio_art -- <dir>` (lofty; no Python tag library needed).
"""
import os, struct, sys, zlib

def chunk(cid, data):
    if len(data) % 2:
        data += b"\0"
    return cid + struct.pack("<I", len(data)) + data

def wav(secs, rate=8000, info=None, tag=128):
    data = bytes([tag]) * int(rate * secs)
    fmt = struct.pack("<HHIIHH", 1, 1, rate, rate, 1, 8)
    body = b"WAVE" + chunk(b"fmt ", fmt)
    if info:
        body += chunk(b"LIST", b"INFO" + b"".join(chunk(k.encode(), v.encode() + b"\0") for k, v in info.items()))
    body += chunk(b"data", data)
    return b"RIFF" + struct.pack("<I", len(body)) + body

out = sys.argv[1]
os.makedirs(os.path.join(out, "tracks"), exist_ok=True)
open(os.path.join(out, "mix1h.wav"), "wb").write(wav(3600, info={"IART": "DJ Test"}))
names = ["Alpha", "Bravo", "Charlie", "Delta", "Echo", "Foxtrot", "Golf", "Hotel"]
for i, n in enumerate(names, 1):
    info = {"IART": "The Band", "IPRD": "Debut", "INAM": n}
    open(os.path.join(out, "tracks", f"{i:02d} {n}.wav"), "wb").write(wav(1, info=info, tag=100 + i))
open(os.path.join(out, "notes.txt"), "w").write("just text\n")
open(os.path.join(out, "fake.mp3"), "w").write("<html><script>alert(1)</script> not audio at all, long enough to sniff</html>")
with open(os.path.join(out, "big.wav"), "wb") as f:
    n = 700 * 1024 * 1024
    f.write(b"RIFF" + struct.pack("<I", 36 + n) + b"WAVE" + chunk(b"fmt ", struct.pack("<HHIIHH", 1, 1, 8000, 8000, 1, 8)) + b"data" + struct.pack("<I", n))
    f.truncate(f.tell() + n)

# --- ART_07: loose image files for the cover-art section (same pseudo-random 9x8 grey cells as
# the Rust fixtures, so every size of one seed is one picture) ---
def blocks_png(seed, w, h):
    cell, s = [], (seed * 2654435761 + 12345) & 0xFFFFFFFF
    for _ in range(72):
        s = (s * 1664525 + 1013904223) & 0xFFFFFFFF
        cell.append(s >> 24)
    rows = b""
    for y in range(h):
        rows += b"\x00" + bytes(cell[(y * 8 // h) * 9 + x * 9 // w] for x in range(w))
    def png_chunk(t, d):
        c = struct.pack(">I", len(d)) + t + d
        return c + struct.pack(">I", zlib.crc32(t + d) & 0xFFFFFFFF)
    return b"\x89PNG\r\n\x1a\n" + png_chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 0, 0, 0, 0)) + png_chunk(b"IDAT", zlib.compress(rows, 9)) + png_chunk(b"IEND", b"")

loose = os.path.join(out, "art", "loose")
os.makedirs(loose, exist_ok=True)
open(os.path.join(loose, "back.png"), "wb").write(blocks_png(8, 400, 400))
open(os.path.join(loose, "wide.png"), "wb").write(blocks_png(9, 500, 250))
open(os.path.join(loose, "gif.gif"), "wb").write(b"GIF89a\x01\x00\x01\x00\x80\x00\x00\x00\x00\x00\xff\xff\xff!\xf9\x04\x01\x00\x00\x00\x00,\x00\x00\x00\x00\x01\x00\x01\x00\x00\x02\x02D\x01\x00;")
open(os.path.join(loose, "junk.bmp"), "wb").write(b"BM" + bytes(60))
