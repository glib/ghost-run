#!/usr/bin/env python3
"""Check whether DaVinci Resolve imports an edit written as a timeline file.

Ghost Run is meant to write the first edit of a video as a timeline file that
Resolve opens (docs/decisions/0004-first-edit-and-thumbnails.md). Resolve's
scripting API can't add transitions, so the dissolves and the closing fade
have to come in through the file. This script makes a few short labelled test
clips and the same edit written two ways, FCPXML 1.9 and OpenTimelineIO, so
you can import each one into Resolve and see what survives.

    python3 scripts/edit_check.py [OUT_DIR] [--intro PATH] [--intro-cut FRAME]

It needs ffmpeg (with drawtext for the labels). The clips are 1080p 29.97,
about 250 MB in all, and take a few minutes to make. With --intro, the real
intro animation (ProRes 4444 with alpha, 29.97) is used instead of the generated one, and
--intro-cut is the frame of it that lands on the cut into the talking intro
(default 94: the middle of the Work From Loam hold, frames 78-111).

In Resolve: File > Import > Timeline..., pick edit-check.fcpxml, then again
with edit-check.otio. The list of things to look at is printed at the end.
Stdlib only; nothing here is Ghost Run's real edit writer.
"""

import argparse
import json
import os
import shutil
import subprocess
import sys
import xml.etree.ElementTree as ET
from pathlib import Path

FPS_NUM, FPS_DEN = 30000, 1001
OTIO_RATE = FPS_NUM / FPS_DEN
TIMELINE_START = 108000  # 01:00:00:00 non-drop at 29.97
RUN_TC = 1080000  # 10:00:00:00 non-drop: a clip with a timecode, like camera footage
SIZE = (1920, 1080)


def fcp_time(frames):
    """A frame count as an FCPXML rational time at 29.97."""
    return "0s" if frames == 0 else "%d/%ds" % (frames * FPS_DEN, FPS_NUM)


def tc_string(frames):
    f = frames % 30
    s = frames // 30
    return "%02d:%02d:%02d:%02d" % (s // 3600, s // 60 % 60, s % 60, f)


class Asset:
    def __init__(self, key, name, path, frames, tc=0, audio=True):
        self.key, self.name, self.path = key, name, Path(path).resolve()
        self.frames, self.tc, self.audio = frames, tc, audio

    @property
    def url(self):
        return self.path.as_uri()


class Edit:
    """V1 is a spine of clips, transitions (centred on the cut before the
    next clip) and gaps; V2 items are placed at record frames."""

    def __init__(self):
        self.spine = []  # ("clip", asset, src_in, length) | ("transition", length) | ("gap", length)
        self.above = []  # (asset, src_in, length, record)

    def clip(self, asset, src_in, length):
        self.spine.append(("clip", asset, src_in, length))

    def transition(self, length):
        self.spine.append(("transition", length))

    def gap(self, length):
        self.spine.append(("gap", length))

    def records(self):
        """Each spine item with its record frame (from 0)."""
        out, at = [], 0
        for item in self.spine:
            if item[0] == "transition":
                out.append((item, at - item[1] // 2))
                continue
            out.append((item, at))
            at += item[-1]
        return out

    def length(self):
        return sum(i[-1] for i in self.spine if i[0] != "transition")

    def record_of(self, asset):
        for item, at in self.records():
            if item[0] == "clip" and item[1] is asset:
                return at
        raise KeyError(asset.name)

    def parent_at(self, record):
        """The V1 clip a V2 item starting at `record` connects to."""
        for i, (item, at) in enumerate(self.records()):
            if item[0] == "clip" and at <= record < at + item[3]:
                return i
        raise ValueError("nothing on V1 at frame %d" % record)


def fcpxml(edit, assets, name):
    root = ET.Element("fcpxml", version="1.9")
    res = ET.SubElement(root, "resources")
    ET.SubElement(res, "format", id="r0", name="FFVideoFormat1080p2997",
                  frameDuration="%d/%ds" % (FPS_DEN, FPS_NUM),
                  width=str(SIZE[0]), height=str(SIZE[1]), colorSpace="1-1-1 (Rec. 709)")
    ids = {}
    for n, a in enumerate(assets, 1):
        ids[a.key] = "r%d" % n
        attrs = dict(id=ids[a.key], name=a.name, uid=a.key, start=fcp_time(a.tc),
                     duration=fcp_time(a.frames), hasVideo="1", format="r0",
                     videoSources="1")
        if a.audio:
            attrs.update(hasAudio="1", audioSources="1", audioChannels="2", audioRate="48000")
        el = ET.SubElement(res, "asset", **attrs)
        ET.SubElement(el, "media-rep", kind="original-media", src=a.url)
    ET.SubElement(res, "effect", id="rv", name="Cross Dissolve",
                  uid="FxPlug:4731E73A-8DAC-4113-9A30-AE85B1761265")
    ET.SubElement(res, "effect", id="ra", name="Audio Crossfade", uid="FFAudioTransition")

    lib = ET.SubElement(root, "library")
    event = ET.SubElement(lib, "event", name=name)
    project = ET.SubElement(event, "project", name=name)
    seq = ET.SubElement(project, "sequence", format="r0", duration=fcp_time(edit.length()),
                        tcStart=fcp_time(TIMELINE_START), tcFormat="NDF",
                        audioLayout="stereo", audioRate="48k")
    spine = ET.SubElement(seq, "spine")
    placed = []
    for item, at in edit.records():
        offset = fcp_time(TIMELINE_START + at)
        if item[0] == "clip":
            _, a, src_in, length = item
            el = ET.SubElement(spine, "asset-clip", ref=ids[a.key], offset=offset, name=a.name,
                               start=fcp_time(a.tc + src_in), duration=fcp_time(length),
                               format="r0", tcFormat="NDF")
            if a.audio:
                el.set("audioRole", "dialogue")
        elif item[0] == "transition":
            el = ET.SubElement(spine, "transition", name="Cross Dissolve", offset=offset,
                               duration=fcp_time(item[1]))
            ET.SubElement(el, "filter-video", ref="rv", name="Cross Dissolve")
            ET.SubElement(el, "filter-audio", ref="ra", name="Audio Crossfade")
        else:
            el = ET.SubElement(spine, "gap", name="Gap", offset=offset, start=fcp_time(0),
                               duration=fcp_time(item[1]))
        placed.append((el, item, at))
    # V2: a connected clip hangs off the V1 clip it starts over, timed in that
    # clip's own (source) time.
    for a, src_in, length, record in edit.above:
        el, item, at = placed[edit.parent_at(record)]
        parent_src = item[1].tc + item[2]
        ET.SubElement(el, "asset-clip", ref=ids[a.key], lane="1",
                      offset=fcp_time(parent_src + record - at), name=a.name,
                      start=fcp_time(a.tc + src_in), duration=fcp_time(length),
                      format="r0", tcFormat="NDF")
    ET.indent(root)
    return '<?xml version="1.0" encoding="UTF-8"?>\n<!DOCTYPE fcpxml>\n' + \
        ET.tostring(root, encoding="unicode") + "\n"


def otio(edit, name):
    def rt(v):
        return {"OTIO_SCHEMA": "RationalTime.1", "rate": OTIO_RATE, "value": float(v)}

    def tr(start, dur):
        return {"OTIO_SCHEMA": "TimeRange.1", "start_time": rt(start), "duration": rt(dur)}

    def base(schema, nm, **kw):
        d = {"OTIO_SCHEMA": schema, "name": nm, "metadata": {}}
        d.update(kw)
        return d

    def clip(a, src_in, length):
        return base("Clip.1", a.name, source_range=tr(a.tc + src_in, length), effects=[],
                    markers=[], media_reference=base("ExternalReference.1", "",
                                                     target_url=a.url,
                                                     available_range=tr(a.tc, a.frames)))

    def gap(length):
        return base("Gap.1", "", source_range=tr(0, length), effects=[], markers=[])

    def transition(length):
        half = length // 2
        return base("Transition.1", "Cross Dissolve", transition_type="SMPTE_Dissolve",
                    in_offset=rt(half), out_offset=rt(length - half))

    def track(nm, kind, children):
        return base("Track.1", nm, kind=kind, children=children, source_range=None,
                    effects=[], markers=[])

    def v1(audio):
        out = []
        for item in edit.spine:
            if item[0] == "clip":
                out.append(clip(*item[1:]) if (item[1].audio or not audio) else gap(item[3]))
            elif item[0] == "transition":
                out.append(transition(item[1]))
            else:
                out.append(gap(item[1]))
        return out

    v2, at = [], 0
    for a, src_in, length, record in sorted(edit.above, key=lambda x: x[3]):
        if record > at:
            v2.append(gap(record - at))
        v2.append(clip(a, src_in, length))
        at = record + length
    tracks = [track("V1", "Video", v1(False)), track("V2", "Video", v2),
              track("A1", "Audio", v1(True))]
    tl = base("Timeline.1", name, global_start_time=rt(TIMELINE_START),
              tracks=base("Stack.1", "tracks", children=tracks, source_range=None,
                          effects=[], markers=[]))
    return json.dumps(tl, indent=2) + "\n"


def ffmpeg(*args):
    subprocess.run(["ffmpeg", "-hide_banner", "-loglevel", "error", "-y", *args], check=True)


def find_font():
    if not shutil.which("fc-match"):
        return None
    out = subprocess.run(["fc-match", "-f", "%{file}", "sans:bold"], capture_output=True, text=True)
    return out.stdout.strip() or None


def has_drawtext():
    out = subprocess.run(["ffmpeg", "-hide_banner", "-filters"], capture_output=True, text=True)
    return " drawtext " in out.stdout


def text(font, label, y, tc=None, color="white", x=140):
    if not font:
        return []
    common = "fontfile='%s':fontcolor=%s:fontsize=64:x=%d:y=%d" % (font, color, x, y)
    if tc is not None:
        return ["drawtext=%s:timecode='%s':rate=%d/%d" % (common, tc_string(tc).replace(":", r"\:"),
                                                          FPS_NUM, FPS_DEN)]
    return ["drawtext=%s:text='%s'" % (common, label)]


def make_clip(path, frames, hue, tone, label, font, tc=0):
    vf = ["hue=h=%d" % hue] + text(font, label, 100) + text(font, None, 200, tc)
    ffmpeg("-f", "lavfi", "-i", "testsrc2=s=%dx%d:r=%d/%d" % (*SIZE, FPS_NUM, FPS_DEN),
           "-f", "lavfi", "-i", "sine=f=%d:sample_rate=48000" % tone,
           "-vf", ",".join(vf), "-frames:v", str(frames), "-timecode", tc_string(tc),
           "-c:v", "prores_ks", "-profile:v", "0", "-c:a", "pcm_s16le", "-ac", "2",
           "-shortest", str(path))


def alpha_src(frames):
    return "color=black@0.0:s=%dx%d:r=%d/%d,format=yuva444p" % (*SIZE, FPS_NUM, FPS_DEN)


def make_overlay(path, frames, font, tc):
    """A timer-overlay stand-in: an opaque panel over transparency that shows
    its own timecode next to the run clip's, so the two must read the same."""
    vf = [alpha_src(frames),
          "drawbox=x=1000:y=80:w=640:h=200:color=0x1d1b18@1:t=fill:replace=1"]
    vf += text(font, "OVERLAY", 100, None, "0xe6e1d6", 1040)
    vf += text(font, None, 200, tc, "0xe6e1d6", 1040)
    ffmpeg("-f", "lavfi", "-i", ",".join(vf),
           "-frames:v", str(frames), "-timecode", tc_string(tc), "-c:v", "prores_ks",
           "-profile:v", "4444", "-pix_fmt", "yuva444p10le", str(path))


def make_intro(path, frames=198, settle=78, leave=111):
    """An intro-animation stand-in: a bar slides in until `settle`, holds,
    slides out from `leave`; transparent around it, no audio."""
    w = 800
    x0 = ("if(lt(N,{s}),-{w}+{d}*N/{s},if(lt(N,{l}),{c},{c}+{d}*(N-{l})/{r}))"
          .format(s=settle, l=leave, w=w, c=560, d=560 + w, r=frames - leave))
    alpha = "255*between(X,{x},{x}+{w})*between(Y,390,690)".format(x=x0, w=w)
    ffmpeg("-f", "lavfi", "-i", alpha_src(frames),
           "-vf", "geq=lum=200:cb=128:cr=128:a='%s'" % alpha,
           "-frames:v", str(frames), "-c:v", "prores_ks", "-profile:v", "4444",
           "-pix_fmt", "yuva444p10le", str(path))


def probe_frames(path):
    out = subprocess.run(["ffprobe", "-v", "error", "-select_streams", "v:0", "-count_packets",
                          "-show_entries", "stream=nb_read_packets", "-of", "csv=p=0", str(path)],
                         capture_output=True, text=True, check=True)
    return int(out.stdout.strip())


def build(out, intro_path=None, intro_cut=94, make_media=True):
    """Make the media (unless told not to) and return (edit, assets)."""
    out.mkdir(parents=True, exist_ok=True)
    font = find_font() if make_media and has_drawtext() else None
    clips = [("s1", "S1 stinger", 120, 0, 330), ("s2", "S2 stinger", 120, 60, 440),
             ("s3", "S3 stinger", 120, 120, 550), ("talk", "Talking intro", 300, 180, 220),
             ("wrap", "Wrap-up", 300, 300, 260)]
    assets = {}
    for key, label, frames, hue, tone in clips:
        a = Asset(key, label, out / ("%s.mov" % key), frames)
        if make_media:
            make_clip(a.path, frames, hue, tone, label, font)
        assets[key] = a
    run = Asset("run", "Run", out / "run.mov", 750, RUN_TC)
    overlay = Asset("overlay", "Run overlay", out / "run.overlay.mov", 750, RUN_TC, audio=False)
    if make_media:
        make_clip(run.path, run.frames, 240, 300, "RUN", font, RUN_TC)
        make_overlay(overlay.path, overlay.frames, font, RUN_TC)
    if intro_path:
        intro = Asset("intro", Path(intro_path).name, intro_path,
                      probe_frames(intro_path) if make_media else 198, audio=False)
    else:
        intro = Asset("intro", "Intro animation", out / "intro.mov", 198, audio=False)
        if make_media:
            make_intro(intro.path)
    assets.update(run=run, overlay=overlay, intro=intro)

    e = Edit()
    for key in ("s1", "s2", "s3"):
        e.clip(assets[key], 30, 45)  # 1.5 s each, hard cuts
    e.clip(assets["talk"], 30, 210)
    e.transition(16)  # ~0.5 s dissolve into the run
    e.clip(run, 60, 630)  # 2 s "before Start" to 1 s "after Finish"
    e.clip(assets["wrap"], 30, 210)
    e.transition(30)  # 1 s fade to black: a dissolve into the gap after it
    e.gap(30)
    cut = e.record_of(assets["talk"])
    e.above.append((intro, 0, intro.frames, cut - intro_cut))
    e.above.append((overlay, 60, 630, e.record_of(run)))
    return e, list(assets.values())


CHECKS = """
Import each file with File > Import > Timeline... (FCPXML, then OTIO). For each, note:
 1. Media: every clip links (nothing offline, no relink prompt).
 2. Timeline: 29.97, starts at 01:00:00:00, 40.5 s long.
 3. V1 order: S1, S2, S3 (1.5 s each, hard cuts), Talking intro, Run, Wrap-up.
 4. Intro animation on V2 over the stingers, transparent around the bar: the bar
    slides in over the stingers, is settled at the cut into Talking intro, and
    slides out over it.
 5. Talking intro -> Run: a ~0.5 s cross dissolve, and an audio crossfade.
 6. Run overlay on V2 over the run: the panel's timecode matches the run
    clip's burned-in timecode (10:00:02:00 at the run's first frame).
 7. End: Wrap-up fades to black over the last second.
 8. Audio on A1 from each clip; the intro and overlay bring no audio.
"""


def main(argv=None):
    p = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    p.add_argument("out", nargs="?", default="ghost-run-edit-check")
    p.add_argument("--intro", help="the real intro animation (ProRes 4444 with alpha, 29.97)")
    p.add_argument("--intro-cut", type=int, default=94,
                   help="frame of the intro that lands on the cut into the talking intro")
    args = p.parse_args(argv)
    if not shutil.which("ffmpeg"):
        sys.exit("ffmpeg isn't on PATH")
    out = Path(args.out)
    edit, assets = build(out, args.intro, args.intro_cut)
    (out / "edit-check.fcpxml").write_text(fcpxml(edit, assets, "Ghost Run edit check"))
    (out / "edit-check.otio").write_text(otio(edit, "Ghost Run edit check"))
    print("Wrote %s and %s" % (out / "edit-check.fcpxml", out / "edit-check.otio"))
    print(CHECKS)


if __name__ == "__main__":
    main()
