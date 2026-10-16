#!/bin/sh
# make-media.sh: synthetic test library, generated with the Jellyfin image's own ffmpeg
# (no network). media/: 2 films per backend (one shared by IMDb id).
# media-big/: Film 000-129 on A, Film 100-229 on B (30 shared), genres via NFO.
set -e
E=$(cd "$(dirname "$0")" && pwd)
[ -f "$E/media/clip.mkv" ] && [ -d "$E/media-big" ] && exit 0
mkdir -p "$E/media"
docker run --rm --network none --entrypoint /usr/lib/jellyfin-ffmpeg/ffmpeg -v "$E/media:/out" \
  "jellyfin/jellyfin:${JF_A:-12.2}" -loglevel error -y -f lavfi -i testsrc=size=64x64:rate=2 -t 1200 \
  -c:v libx264 -preset ultrafast -crf 51 -pix_fmt yuv420p /out/clip.mkv
python3 - "$E" <<'PY'
import os, shutil, sys
E = sys.argv[1]; clip = os.path.join(E, "media", "clip.mkv")
def film(root, side, name, nfo):
    d = os.path.join(E, root, side, "movies", name); os.makedirs(d, exist_ok=True)
    if not os.path.exists(os.path.join(d, name + ".mkv")):
        os.link(clip, os.path.join(d, name + ".mkv"))
    open(os.path.join(d, name + ".nfo"), "w").write('<?xml version="1.0"?><movie>' + nfo + "</movie>")
synced = "<title>Synced Film</title><year>2001</year><imdbid>tt0000001</imdbid><uniqueid type=\"imdb\" default=\"true\">tt0000001</uniqueid>"
film("media", "a", "Halloween (1978)", "<title>Halloween</title><year>1978</year>")
film("media", "a", "Synced Film (2001)", synced)
film("media", "b", "Halloween (2018)", "<title>Halloween</title><year>2018</year>")
film("media", "b", "Synced Film (2001)", synced)
for side, rng, genre in (("a", range(0, 130), "Drama"), ("b", range(100, 230), "Horror")):
    for i in rng:
        g = "Comedy" if i % 2 else genre
        film("media-big", side, f"Film {i:03d} ({1900 + i})",
             f"<title>Film {i:03d}</title><year>{1900 + i}</year><genre>{g}</genre>"
             f"<uniqueid type=\"imdb\" default=\"true\">tt{9000000 + i}</uniqueid>")
PY
echo "generated media/ and media-big/"
