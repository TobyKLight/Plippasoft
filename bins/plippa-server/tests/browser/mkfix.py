import json, os, sys, base64, shutil, zlib, struct
root=sys.argv[1]
shutil.rmtree(root, ignore_errors=True)
def w(rel, data):
    p=os.path.join(root,rel); os.makedirs(os.path.dirname(p),exist_ok=True)
    open(p,'wb').write(data if isinstance(data,bytes) else data.encode())
def nd(rel, obj): w(rel, json.dumps(obj,indent=1))
png=base64.b64decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==")
def solid(width, height, rgb):
    # ART_05: real PNGs of the given shape (the 1x1 above is too small to show cropping).
    row=b"\x00"+bytes(rgb)*width
    def chunk(t, d):
        c=struct.pack(">I", len(d))+t+d
        return c+struct.pack(">I", zlib.crc32(t+d)&0xffffffff)
    return b"\x89PNG\r\n\x1a\n"+chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0))+chunk(b"IDAT", zlib.compress(row*height, 9))+chunk(b"IEND", b"")
w("Music/Band/Album/cover.png", png)
for i,t in enumerate(["One","Two","Three"],1):
    w(f"Music/Band/Album/0{i} - {t}.mp3", b"")
nd("Music/Band/Album/_Album.noudata",{"noudataVersion":"0.4.0","noudataType":"release","noubinKey":"Album.noubin.AAA111","noubinKeyNormalised":"album.noubin.aaa111","playableItemTitle":"The Album","playableItemProfile": "music", "releaseData":{"credits":{"primaryArtists":[{"primaryArtistName":"Band"}]},"coverImage":{"images":[{"imageLocalFilename":"cover.png"}]}},"mediaList":[{"mediaTitle":t,"localFilePaths":[f"0{i} - {t}.mp3"]} for i,t in enumerate(["One","Two","Three"],1)]})
# hostile album
w("Music/Evil/Hostile/a.mp3", b"")
nd("Music/Evil/Hostile/_Hostile.noudata",{"noudataVersion":"0.4.0","noudataType":"release","playableItemTitle":"<img src=x onerror=alert(1)>","playableItemProfile": "music", "releaseData":{"credits":{"primaryArtists":[{"primaryArtistName":"<script>alert(2)</script>"}]}},"mediaList":[{"mediaTitle":"<b onmouseover=alert(3)>bold</b>","localFilePaths":["a.mp3"]}]})
# mixtape
w("Mixes/Japanese/mix.mp3", b"")
nd("Mixes/Japanese/_Mix.noudata",{"noudataVersion":"0.4.0","noudataType":"release","playableItemTitle":"Rocksteady Set","playableItemProfile": "mix", "releaseData":{"credits":{"primaryArtists":[{"primaryArtistName":"Japanese Rude Sounds"}]}},"mediaList":[{"mediaTitle":"Rocksteady Set","localFilePaths":["mix.mp3"],"cueLists":[{"cueListType":"nav","cueListTitle":"Chapters","cues":[{"startTime":"0","cueTextSpans":[{"text":"Warm up"}]},{"startTime":"10:00","cueTextSpans":[{"text":"Peak time <i>x</i>"}]},{"startTime":"45:00","cueTextSpans":[{"text":"Cool down"}]}]}]}]})
# audiobook
for i in (1,2): w(f"Audiobooks/Tolkien/Hobbit/hobbit-{i}.m4b", b"")
nd("Audiobooks/Tolkien/Hobbit/_Hobbit.noudata",{"noudataVersion":"0.4.0","noudataType":"release","playableItemTitle":"The Hobbit","playableItemProfile": "audiobook", "releaseData":{"credits":{"primaryArtists":[{"primaryArtistName":"J.R.R. Tolkien"}]}},"mediaList":[{"mediaTitle":"Part 1","localFilePaths":["hobbit-1.m4b"]},{"mediaTitle":"Part 2","localFilePaths":["hobbit-2.m4b"]}]})
# podcast
w("Podcasts/Show/ep1.mp3", b"")
nd("Podcasts/Show/_Show.noudata",{"noudataVersion":"0.4.0","noudataType":"release","playableItemTitle":"The Show","playableItemProfile": "podcast", "releaseData":{},"mediaList":[{"mediaTitle":"Episode 1","localFilePaths":["ep1.mp3"]}]})
# orphan media file (no .noudata) + a real looseTrack
w("Loose/orphan song.mp3", b"")
w("Loose/tagless demo.mp3", b"")
nd("Loose/tagless demo.noudata",{"noudataVersion":"0.4.0","noudataType":"looseTrack","playableItemTitle":"Tagless Demo","playableItemProfile":"music","mediaList":[{"localFilePaths":["tagless demo.mp3"]}]})
# playlists
nd("Playlists/_Rainy.noudata",{"noudataVersion":"0.4.0","noudataType":"userPlaylist","playableItemTitle":"Rainy","localPlaybackData":{"userNotes":{"createdByUser":"Plippa"}},"mediaList":[{"localFilePaths":["../Music/Band/Album/01 - One.mp3"]},{"localFilePaths":["../Music/Band/Album/02 - Two.mp3"]},{"localFilePaths":["../Music/Band/Album/03 - Three.mp3"]}]})
nd("Playlists/_Sunny.noudata",{"noudataVersion":"0.4.0","noudataType":"userPlaylist","playableItemTitle":"Sunny","localPlaybackData":{"userNotes":{"createdByUser":"Plippa"}},"mediaList":[{"localFilePaths":["../Mixes/Japanese/mix.mp3"]}]})
# ART_05 cover-art fixtures, only with `mkfix.py <dir> --covers` (the other browser scripts count
# albums, cards and rows, so the default library stays as it was): an album with 3 images (a 400x200,
# a 600x600 and a 2000x1000; one with hostile alt text), an album with 1 image, an album without,
# a playlist with art and a loose track with art.
if "--covers" in sys.argv[2:]:
    w("Music/Trip/Triptych/wide.png", solid(400,200,(40,200,40)))
    w("Music/Trip/Triptych/big.png", solid(2000,1000,(40,40,200)))
    w("Music/Trip/Triptych/t1.mp3", b"")
    nd("Music/Trip/Triptych/_Triptych.noudata",{"noudataVersion":"0.4.0","noudataType":"release","playableItemTitle":"Triptych","playableItemProfile":"music","releaseData":{"credits":{"primaryArtists":[{"primaryArtistName":"Trip"}]},"coverImage":{"images":[{"imageLocalFilename":"wide.png","altText":"Wide"},{"imageLocalFilename":"front.png","altText":"<img src=x onerror=alert(1)>"},{"imageLocalFilename":"big.png","altText":"Big"}]}},"mediaList":[{"mediaTitle":"Only","localFilePaths":["t1.mp3"]}]})
    w("Music/Solo/Single/cover.png", solid(300,300,(230,160,30)))
    w("Music/Solo/Single/s1.mp3", b"")
    nd("Music/Solo/Single/_Single.noudata",{"noudataVersion":"0.4.0","noudataType":"release","playableItemTitle":"Single Cover","playableItemProfile":"music","releaseData":{"credits":{"primaryArtists":[{"primaryArtistName":"Solo"}]},"coverImage":{"images":[{"imageLocalFilename":"cover.png"}]}},"mediaList":[{"mediaTitle":"S1","localFilePaths":["s1.mp3"]}]})
    w("Music/Bare/Plain/b1.mp3", b"")
    nd("Music/Bare/Plain/_Plain.noudata",{"noudataVersion":"0.4.0","noudataType":"release","playableItemTitle":"Bare Album","playableItemProfile":"music","releaseData":{"credits":{"primaryArtists":[{"primaryArtistName":"Bare"}]}},"mediaList":[{"mediaTitle":"B1","localFilePaths":["b1.mp3"]}]})
    w("Playlists/rainy-art.png", solid(256,256,(120,60,200)))
    w("Loose/demo-art.png", solid(500,250,(20,180,180)))
    d=json.load(open(os.path.join(root,"Playlists/_Rainy.noudata")))
    d["localPlaybackData"]["localCoverImage"]={"images":[{"imageLocalFilename":"rainy-art.png"}]}
    nd("Playlists/_Rainy.noudata", d)
    d=json.load(open(os.path.join(root,"Loose/tagless demo.noudata")))
    d["mediaList"][0]["itemCoverImage"]={"images":[{"imageLocalFilename":"demo-art.png"}]}
    nd("Loose/tagless demo.noudata", d)
if "--covers" in sys.argv[2:]:
    w("Music/Trip/Triptych/front.png", solid(600,600,(200,40,40)))
    w("Music/Trip/Triptych/wide.png", solid(400,200,(40,200,40)))
    w("Music/Trip/Triptych/big.png", solid(2000,1000,(40,40,200)))
    w("Music/Trip/Triptych/t1.mp3", b"")
    nd("Music/Trip/Triptych/_Triptych.noudata",{"noudataVersion":"0.4.0","noudataType":"release","playableItemTitle":"Triptych","playableItemProfile":"music","releaseData":{"credits":{"primaryArtists":[{"primaryArtistName":"Trip"}]},"coverImage":{"images":[{"imageLocalFilename":"wide.png","altText":"Wide"},{"imageLocalFilename":"front.png","altText":"<img src=x onerror=alert(1)>"},{"imageLocalFilename":"big.png","altText":"Big"}]}},"mediaList":[{"mediaTitle":"Only","localFilePaths":["t1.mp3"]}]})
    w("Music/Solo/Single/cover.png", solid(300,300,(230,160,30)))
    w("Music/Solo/Single/s1.mp3", b"")
    nd("Music/Solo/Single/_Single.noudata",{"noudataVersion":"0.4.0","noudataType":"release","playableItemTitle":"Single Cover","playableItemProfile":"music","releaseData":{"credits":{"primaryArtists":[{"primaryArtistName":"Solo"}]},"coverImage":{"images":[{"imageLocalFilename":"cover.png"}]}},"mediaList":[{"mediaTitle":"S1","localFilePaths":["s1.mp3"]}]})
    w("Music/Bare/Plain/b1.mp3", b"")
    nd("Music/Bare/Plain/_Plain.noudata",{"noudataVersion":"0.4.0","noudataType":"release","playableItemTitle":"Bare Album","playableItemProfile":"music","releaseData":{"credits":{"primaryArtists":[{"primaryArtistName":"Bare"}]}},"mediaList":[{"mediaTitle":"B1","localFilePaths":["b1.mp3"]}]})
    w("Playlists/rainy-art.png", solid(256,256,(120,60,200)))
    w("Loose/demo-art.png", solid(500,250,(20,180,180)))
    d=json.load(open(os.path.join(root,"Playlists/_Rainy.noudata")))
    d["localPlaybackData"]["localCoverImage"]={"images":[{"imageLocalFilename":"rainy-art.png"}]}
    nd("Playlists/_Rainy.noudata", d)
    d=json.load(open(os.path.join(root,"Loose/tagless demo.noudata")))
    d["mediaList"][0]["itemCoverImage"]={"images":[{"imageLocalFilename":"demo-art.png"}]}
    nd("Loose/tagless demo.noudata", d)
