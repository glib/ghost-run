"""Ghost Run's bridge to DaVinci Resolve.

Ghost Run runs this with the Python inside Resolve's container (davincibox),
sends one JSON request on stdin and reads one JSON reply from stdout. It uses
Resolve's external scripting API, which needs Resolve Studio running with
Preferences > System > General > External scripting using set to Local.

Requests:
  {"command": "status"}
  {"command": "add_overlays", "items": [
      {"clip": path, "proxy": path or null, "overlay": path,
       "first_frame": n, "frames": n}, ...]}
    A clip whose timed part isn't on the timeline is added after the
    timeline's last item first (on V1, with its audio, proxy linked).
  {"command": "new_project", "name": text, "fps": "29.97", "width": n, "height": n,
   "runs": [{"clip": path, "proxy": path or null, "overlay": path or null,
             "first_frame": n, "frames": n}, ...]}
  {"command": "read_timeline", "project": name or null, "timeline": name or null}
    The clips on a timeline of the open project (the current one if no name):
    {"project", "timeline", "items": [{"path", "track", "record",
    "first_frame", "frames"}, ...]} in timeline order. Refused when another
    project is open than the one named, rather than switching projects.
  {"command": "list_media", "whole_project": bool}
    The files the open project uses, each once: on the open timeline (all
    its video and audio tracks), or anywhere in the media pool.
    {"project", "timeline", "files": [{"path", "frames", "timecode",
    "online"}, ...]}; "online" says whether the file can be opened now.
  {"command": "relink", "project": name, "files": [{"path": old path,
   "new": path, "proxy": path or null}, ...]}
    Points each media pool item using an old path at its new file
    (ReplaceClip), links its proxy, and saves the project. Refused when
    another project is open than the one named.

Replies are {"ok": true, ...} or {"ok": false, "error": code, "message": text}.

Only the standard library is used, so it runs on whatever Python the
container has. The placement maths are plain functions, tested without
Resolve (tests/test_resolve_bridge.py).
"""

import json
import os
import sys

BIN_NAME = "Ghost Run overlays"
CLIPS_BIN = "Ghost Run clips"


class BridgeError(Exception):
    def __init__(self, code, message):
        Exception.__init__(self, message)
        self.code = code
        self.message = message


def connect():
    """Resolve's scripting object, or a BridgeError saying why not."""
    api = os.environ.get("RESOLVE_SCRIPT_API", "/opt/resolve/Developer/Scripting")
    lib = os.environ.get("RESOLVE_SCRIPT_LIB", "/opt/resolve/libs/Fusion/fusionscript.so")
    os.environ["RESOLVE_SCRIPT_API"] = api
    os.environ["RESOLVE_SCRIPT_LIB"] = lib
    modules = os.path.join(api, "Modules")
    if modules not in sys.path:
        sys.path.append(modules)
    try:
        import DaVinciResolveScript as dvr  # noqa: N813 (Resolve's name)
    except ImportError as e:
        raise BridgeError(
            "not_installed",
            "Resolve's scripting module wasn't found (%s). Is Resolve installed in this container?" % e,
        )
    resolve = dvr.scriptapp("Resolve")
    if resolve is None:
        raise BridgeError(
            "not_running",
            "Resolve didn't answer. Start Resolve Studio, and set Preferences > System > General > "
            "External scripting using to Local (the free Resolve has no external scripting).",
        )
    return resolve


def current_project(resolve):
    project = resolve.GetProjectManager().GetCurrentProject()
    if project is None:
        raise BridgeError("no_project", "No project is open in Resolve.")
    return project


def status(resolve):
    pm = resolve.GetProjectManager()
    project = pm.GetCurrentProject() if pm else None
    timeline = project.GetCurrentTimeline() if project else None
    return {
        "ok": True,
        "product": resolve.GetProductName(),
        "version": resolve.GetVersionString(),
        "project": project.GetName() if project else None,
        "timeline": timeline.GetName() if timeline else None,
    }


def same_file(a, b):
    """Whether two paths name the same file, as Resolve and Ghost Run may spell
    them differently (/home vs /var/home on Fedora Atomic hosts)."""
    if not a or not b:
        return False
    if a == b:
        return True
    try:
        return os.path.realpath(a) == os.path.realpath(b) or os.path.samefile(a, b)
    except OSError:
        return False


def placements(clip_uses, first_frame, frames):
    """Where an overlay goes over each use of its clip on the timeline.

    clip_uses: (track, start, end, left_offset) per timeline item showing the
      clip: its video track, first timeline frame, the frame after its last,
      and the clip frame it starts at.
    first_frame, frames: the clip frame the overlay's first frame goes over,
      and how many frames the overlay has.

    Returns, per use the overlay overlaps: the track above it, the timeline
    frame to put it at, and the overlay frames to use (inclusive), trimmed to
    the part of the clip that is on the timeline.
    """
    out = []
    for track, start, end, left in clip_uses:
        shown_from, shown_to = left, left + (end - start)  # clip frames, end exclusive
        lo = max(first_frame, shown_from)
        hi = min(first_frame + frames, shown_to)
        if hi <= lo:
            continue
        out.append({
            "track": track + 1,
            "record_frame": start + (lo - shown_from),
            "start_frame": lo - first_frame,
            "end_frame": hi - first_frame - 1,
        })
    return out


def free_track(occupied, wanted, record_frame, length):
    """The first video track from `wanted` up with nothing in
    [record_frame, record_frame + length). occupied: track -> [(start, end)]."""
    track = wanted
    while any(s < record_frame + length and record_frame < e for s, e in occupied.get(track, [])):
        track += 1
    return track


def timeline_items(timeline):
    """(track, item) for every video item on the timeline."""
    out = []
    for track in range(1, int(timeline.GetTrackCount("video")) + 1):
        for item in timeline.GetItemListInTrack("video", track) or []:
            out.append((track, item))
    return out


def item_path(item):
    mpi = item.GetMediaPoolItem()
    return mpi.GetClipProperty("File Path") if mpi else None


def overlay_bin(media_pool, name=BIN_NAME):
    root = media_pool.GetRootFolder()
    for folder in root.GetSubFolderList() or []:
        if folder.GetName() == name:
            return folder
    return media_pool.AddSubFolder(root, name)


def import_overlay(media_pool, folder, path):
    """The media pool item for `path`: the one already in the bin, or a new import."""
    for clip in folder.GetClipList() or []:
        if same_file(clip.GetClipProperty("File Path"), path):
            return clip
    previous = media_pool.GetCurrentFolder()
    media_pool.SetCurrentFolder(folder)
    try:
        imported = media_pool.ImportMedia([path]) or []
    finally:
        if previous is not None:
            media_pool.SetCurrentFolder(previous)
    if not imported:
        raise BridgeError("import_failed", "Resolve couldn't import %s" % path)
    return imported[0]


def find_in_pool(folder, path):
    """The media pool item for `path` anywhere under `folder`, or None."""
    for clip in folder.GetClipList() or []:
        if same_file(clip.GetClipProperty("File Path"), path):
            return clip
    for sub in folder.GetSubFolderList() or []:
        found = find_in_pool(sub, path)
        if found is not None:
            return found
    return None


def pool_clip(media_pool, clips_bin, path, proxy):
    """The media pool item for a run's clip: one already in the project, or a
    new import into the clips bin with its proxy linked. Returns (item, note)."""
    clip = find_in_pool(media_pool.GetRootFolder(), path)
    if clip is not None:
        return clip, None
    clip = import_overlay(media_pool, clips_bin, path)
    if proxy and os.path.isfile(proxy) and not clip.LinkProxyMedia(proxy):
        return clip, "Resolve didn't take its proxy"
    return clip, None


def timeline_end(timeline, items):
    """The frame after the last video item, or the timeline's start if empty."""
    ends = [item.GetEnd() for _, item in items]
    return max(ends) if ends else int(timeline.GetStartFrame())


def add_overlays(resolve, items):
    project = current_project(resolve)
    timeline = project.GetCurrentTimeline()
    if timeline is None:
        raise BridgeError("no_timeline", "No timeline is open in Resolve's current project.")
    media_pool = project.GetMediaPool()
    folder = overlay_bin(media_pool)
    clips_bin = None
    results = []
    for request in items:
        result = {"clip": request["clip"], "overlay": request["overlay"], "placed": 0,
                  "already": 0, "clip_added": False, "proxy_note": None, "note": None}
        results.append(result)
        if not os.path.isfile(request["overlay"]):
            result["note"] = "the overlay file is missing"
            continue
        existing = timeline_items(timeline)
        uses = []
        for track, item in existing:
            if same_file(item_path(item), request["clip"]):
                uses.append((track, item.GetStart(), item.GetEnd(), item.GetLeftOffset()))
        spots = placements(uses, request["first_frame"], request["frames"])
        if not spots:
            # The timed part of the clip isn't on the timeline: add it at the end.
            if not os.path.isfile(request["clip"]):
                result["note"] = "its clip is missing"
                continue
            if clips_bin is None:
                clips_bin = overlay_bin(media_pool, CLIPS_BIN)
            clip, note = pool_clip(media_pool, clips_bin, request["clip"], request.get("proxy"))
            first, frames = int(request["first_frame"]), int(request["frames"])
            record = timeline_end(timeline, existing)
            if not media_pool.AppendToTimeline([{
                "mediaPoolItem": clip,
                "startFrame": first,
                "endFrame": first + frames - 1,
                "recordFrame": record,
                "trackIndex": 1,
            }]):
                result["note"] = "Resolve refused to add its clip to the timeline"
                continue
            result["clip_added"] = True
            result["proxy_note"] = note
            existing = timeline_items(timeline)
            spots = [{"track": 2, "record_frame": record, "start_frame": 0,
                      "end_frame": frames - 1}]
        overlay = None
        for spot in spots:
            length = spot["end_frame"] - spot["start_frame"] + 1
            occupied = {}
            already = False
            for track, item in existing:
                start, end = item.GetStart(), item.GetEnd()
                occupied.setdefault(track, []).append((start, end))
                if (same_file(item_path(item), request["overlay"])
                        and start == spot["record_frame"]):
                    already = True
            if already:
                result["already"] += 1
                continue
            track = free_track(occupied, spot["track"], spot["record_frame"], length)
            while int(timeline.GetTrackCount("video")) < track:
                timeline.AddTrack("video")
            if overlay is None:
                overlay = import_overlay(media_pool, folder, request["overlay"])
            added = media_pool.AppendToTimeline([{
                "mediaPoolItem": overlay,
                "startFrame": spot["start_frame"],
                "endFrame": spot["end_frame"],
                "trackIndex": track,
                "recordFrame": spot["record_frame"],
                "mediaType": 1,
            }])
            if not added:
                result["note"] = "Resolve refused to add it to track %d" % track
                continue
            result["placed"] += 1
            existing = timeline_items(timeline)
    return {"ok": True, "project": project.GetName(), "timeline": timeline.GetName(),
            "results": results}


def unused_name(taken, name):
    """`name`, or `name 2`, `name 3`... if Resolve has a project called that."""
    if name not in taken:
        return name
    n = 2
    while "%s %d" % (name, n) in taken:
        n += 1
    return "%s %d" % (name, n)


def overlay_span(first, frames, overlay_first, overlay_frames):
    """Where an overlay goes over a clip used from clip frame `first` for
    `frames` frames, when the overlay starts at clip frame `overlay_first` and
    lasts `overlay_frames`: (offset from the item's start, first overlay frame,
    length), or None if they don't overlap."""
    start = max(first, overlay_first)
    end = min(first + frames, overlay_first + overlay_frames)
    if end <= start:
        return None
    return start - first, start - overlay_first, end - start


def new_project(resolve, request):
    """A new project with the runs (or clips) back to back: each one's clip on
    V1 (with its audio), the part from `first_frame` for `frames`, and its
    overlay, if it has one, above it on V2 where it falls in that part
    (`overlay_first_frame`/`overlay_frames`; by default the overlay starts
    with the part and is as long); its `caption`, if it has one, on V3 from
    the part's start for `caption_frames`; proxies linked."""
    runs = request.get("runs", [])
    if not runs:
        raise BridgeError("bad_request", "no runs to put in the project")
    pm = resolve.GetProjectManager()
    taken = set(pm.GetProjectListInCurrentFolder() or [])
    name = unused_name(taken, request.get("name") or "Ghost Run")
    project = pm.CreateProject(name)
    if not project:
        raise BridgeError("create_failed", "Resolve couldn't create a project called %s" % name)
    # The timeline's rate can only be set before any media is in the project.
    project.SetSetting("timelineFrameRate", str(request.get("fps", "29.97")))
    if request.get("width") and request.get("height"):
        project.SetSetting("timelineResolutionWidth", str(request["width"]))
        project.SetSetting("timelineResolutionHeight", str(request["height"]))
    media_pool = project.GetMediaPool()
    clips_bin = overlay_bin(media_pool, CLIPS_BIN)
    overlays_bin = overlay_bin(media_pool)
    timeline = media_pool.CreateEmptyTimeline(name)
    if not timeline:
        raise BridgeError("create_failed", "Resolve couldn't create the timeline")
    project.SetCurrentTimeline(timeline)
    tracks = 3 if any(run.get("caption") for run in runs) else 2
    while int(timeline.GetTrackCount("video")) < tracks:
        timeline.AddTrack("video")

    notes = []
    placed = 0
    record = int(timeline.GetStartFrame())
    for run in runs:
        clip_name = os.path.basename(run["clip"])
        if not os.path.isfile(run["clip"]):
            notes.append("%s: the clip is missing" % clip_name)
            continue
        clip, note = pool_clip(media_pool, clips_bin, run["clip"], run.get("proxy"))
        if note:
            notes.append("%s: %s" % (clip_name, note))
        first, frames = int(run["first_frame"]), int(run["frames"])
        added = media_pool.AppendToTimeline([{
            "mediaPoolItem": clip,
            "startFrame": first,
            "endFrame": first + frames - 1,
            "recordFrame": record,
            "trackIndex": 1,
        }])
        if not added:
            notes.append("%s: Resolve refused to add the clip" % clip_name)
            continue
        overlay = run.get("overlay")
        span = overlay_span(first, frames, int(run.get("overlay_first_frame", first)),
                            int(run.get("overlay_frames", frames)))
        if overlay and os.path.isfile(overlay) and span is None:
            notes.append("%s: its timed part isn't in the part used" % clip_name)
        elif overlay and os.path.isfile(overlay):
            offset, start, length = span
            item = import_overlay(media_pool, overlays_bin, overlay)
            if not media_pool.AppendToTimeline([{
                "mediaPoolItem": item,
                "startFrame": start,
                "endFrame": start + length - 1,
                "recordFrame": record + offset,
                "trackIndex": 2,
                "mediaType": 1,
            }]):
                notes.append("%s: Resolve refused to add its overlay" % clip_name)
        elif overlay:
            notes.append("%s: its overlay file is missing" % clip_name)
        caption = run.get("caption")
        if caption and os.path.isfile(caption):
            length = min(int(run.get("caption_frames", frames)), frames)
            item = import_overlay(media_pool, overlays_bin, caption)
            if not media_pool.AppendToTimeline([{
                "mediaPoolItem": item,
                "startFrame": 0,
                "endFrame": length - 1,
                "recordFrame": record,
                "trackIndex": 3,
                "mediaType": 1,
            }]):
                notes.append("%s: Resolve refused to add its caption" % clip_name)
        elif caption:
            notes.append("%s: its caption file is missing" % clip_name)
        placed += 1
        record += frames
    pm.SaveProject()
    return {"ok": True, "project": name, "timeline": timeline.GetName(), "placed": placed,
            "notes": notes}


def find_timeline(project, name):
    """The project's timeline called `name`, or its current one."""
    if not name:
        return project.GetCurrentTimeline()
    for index in range(1, int(project.GetTimelineCount() or 0) + 1):
        timeline = project.GetTimelineByIndex(index)
        if timeline is not None and timeline.GetName() == name:
            return timeline
    return None


def read_timeline(resolve, request):
    project = current_project(resolve)
    wanted = request.get("project")
    if wanted and project.GetName() != wanted:
        raise BridgeError(
            "other_project",
            "Open the project %s in Resolve first (%s is open)." % (wanted, project.GetName()),
        )
    timeline = find_timeline(project, request.get("timeline"))
    if timeline is None:
        if request.get("timeline"):
            raise BridgeError(
                "no_timeline",
                "%s has no timeline called %s any more." % (project.GetName(), request["timeline"]),
            )
        raise BridgeError("no_timeline", "No timeline is open in Resolve.")
    items = []
    for track, item in timeline_items(timeline):
        path = item_path(item)
        if not path:
            continue
        start, end = int(item.GetStart()), int(item.GetEnd())
        items.append({
            "path": path,
            "track": track,
            "record": start,
            "first_frame": int(item.GetLeftOffset()),
            "frames": end - start,
        })
    items.sort(key=lambda i: (i["record"], i["track"]))
    return {"ok": True, "project": project.GetName(), "timeline": timeline.GetName(),
            "items": items}


def as_int(value):
    try:
        return int(str(value).strip())
    except (TypeError, ValueError):
        return None


def pool_items(folder):
    """Every media pool item under `folder`."""
    out = list(folder.GetClipList() or [])
    for sub in folder.GetSubFolderList() or []:
        out.extend(pool_items(sub))
    return out


def media_file(mpi):
    """What list_media says about a media pool item, or None if it isn't a
    file (a timeline, title or generator)."""
    path = mpi.GetClipProperty("File Path") if mpi else None
    if not path:
        return None
    return {
        "path": path,
        "frames": as_int(mpi.GetClipProperty("Frames")),
        "timecode": mpi.GetClipProperty("Start TC") or None,
        "online": os.path.isfile(path),
    }


def list_media(resolve, request):
    project = current_project(resolve)
    timeline = project.GetCurrentTimeline()
    if request.get("whole_project"):
        items = pool_items(project.GetMediaPool().GetRootFolder())
    else:
        if timeline is None:
            raise BridgeError("no_timeline", "No timeline is open in Resolve.")
        items = []
        for kind in ("video", "audio"):
            for track in range(1, int(timeline.GetTrackCount(kind)) + 1):
                for item in timeline.GetItemListInTrack(kind, track) or []:
                    items.append(item.GetMediaPoolItem())
    files, seen = [], set()
    for mpi in items:
        found = media_file(mpi)
        if found is None or found["path"] in seen:
            continue
        seen.add(found["path"])
        files.append(found)
    return {"ok": True, "project": project.GetName(),
            "timeline": timeline.GetName() if timeline else None, "files": files}


def relink(resolve, request):
    project = current_project(resolve)
    wanted = request.get("project")
    if wanted and project.GetName() != wanted:
        raise BridgeError(
            "other_project",
            "Open the project %s in Resolve first (%s is open)." % (wanted, project.GetName()),
        )
    by_old = {}
    for f in request.get("files", []):
        by_old.setdefault(f["path"], f)
    relinked, notes, seen = 0, [], set()
    for mpi in pool_items(project.GetMediaPool().GetRootFolder()):
        path = mpi.GetClipProperty("File Path")
        wanted_file = by_old.get(path)
        if wanted_file is None:
            continue
        seen.add(path)
        name = os.path.basename(path)
        new = wanted_file["new"]
        if not os.path.isfile(new):
            notes.append("%s: %s isn't there" % (name, new))
            continue
        if not mpi.ReplaceClip(new):
            notes.append("%s: Resolve wouldn't take %s" % (name, os.path.basename(new)))
            continue
        relinked += 1
        proxy = wanted_file.get("proxy")
        if proxy and os.path.isfile(proxy) and not mpi.LinkProxyMedia(proxy):
            notes.append("%s: Resolve didn't take its proxy" % name)
    for path in by_old:
        if path not in seen:
            notes.append("%s: not in the media pool" % os.path.basename(path))
    resolve.GetProjectManager().SaveProject()
    return {"ok": True, "project": project.GetName(), "relinked": relinked, "notes": notes}


COMMANDS = ("status", "add_overlays", "new_project", "read_timeline", "list_media", "relink")


def handle(request, resolve_factory=connect):
    command = request.get("command")
    if command not in COMMANDS:
        return {"ok": False, "error": "bad_request", "message": "unknown command %r" % command}
    try:
        resolve = resolve_factory()
        if command == "status":
            return status(resolve)
        if command == "new_project":
            return new_project(resolve, request)
        if command == "read_timeline":
            return read_timeline(resolve, request)
        if command == "list_media":
            return list_media(resolve, request)
        if command == "relink":
            return relink(resolve, request)
        return add_overlays(resolve, request.get("items", []))
    except BridgeError as e:
        return {"ok": False, "error": e.code, "message": e.message}


def main():
    try:
        request = json.loads(sys.stdin.read() or "{}")
    except ValueError as e:
        reply = {"ok": False, "error": "bad_request", "message": "not JSON: %s" % e}
    else:
        try:
            reply = handle(request)
        except Exception as e:  # Resolve's API raises odd things; report, don't crash.
            reply = {"ok": False, "error": "failed", "message": "%s: %s" % (type(e).__name__, e)}
    # Resolve's module may print to stdout; the reply is the last line.
    sys.stdout.write("\n" + json.dumps(reply) + "\n")
    sys.stdout.flush()


if __name__ == "__main__":
    main()
