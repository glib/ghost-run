"""Ghost Run's bridge to DaVinci Resolve.

Ghost Run runs this with the Python inside Resolve's container (davincibox),
sends one JSON request on stdin and reads one JSON reply from stdout. It uses
Resolve's external scripting API, which needs Resolve Studio running with
Preferences > System > General > External scripting using set to Local.

Requests:
  {"command": "status"}
  {"command": "add_overlays", "items": [
      {"clip": path, "overlay": path, "first_frame": n, "frames": n}, ...]}
  {"command": "new_project", "name": text, "fps": "29.97", "width": n, "height": n,
   "runs": [{"clip": path, "proxy": path or null, "overlay": path or null,
             "first_frame": n, "frames": n}, ...]}

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


def add_overlays(resolve, items):
    project = current_project(resolve)
    timeline = project.GetCurrentTimeline()
    if timeline is None:
        raise BridgeError("no_timeline", "No timeline is open in Resolve's current project.")
    media_pool = project.GetMediaPool()
    folder = overlay_bin(media_pool)
    results = []
    for request in items:
        result = {"clip": request["clip"], "overlay": request["overlay"], "placed": 0,
                  "already": 0, "note": None}
        results.append(result)
        if not os.path.isfile(request["overlay"]):
            result["note"] = "the overlay file is missing"
            continue
        existing = timeline_items(timeline)
        uses = []
        for track, item in existing:
            if same_file(item_path(item), request["clip"]):
                uses.append((track, item.GetStart(), item.GetEnd(), item.GetLeftOffset()))
        if not uses:
            result["note"] = "its clip isn't on the current timeline"
            continue
        spots = placements(uses, request["first_frame"], request["frames"])
        if not spots:
            result["note"] = "the timed part of the clip isn't on the timeline"
            continue
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


def new_project(resolve, request):
    """A new project with the runs back to back: each run's clip on V1 (with
    its audio), from a second before Start to the end of its overlay, and the
    overlay above it on V2; proxies linked."""
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
    while int(timeline.GetTrackCount("video")) < 2:
        timeline.AddTrack("video")

    notes = []
    placed = 0
    record = int(timeline.GetStartFrame())
    for run in runs:
        clip_name = os.path.basename(run["clip"])
        if not os.path.isfile(run["clip"]):
            notes.append("%s: the clip is missing" % clip_name)
            continue
        clip = import_overlay(media_pool, clips_bin, run["clip"])
        if run.get("proxy") and os.path.isfile(run["proxy"]):
            if not clip.LinkProxyMedia(run["proxy"]):
                notes.append("%s: Resolve didn't take its proxy" % clip_name)
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
        if overlay and os.path.isfile(overlay):
            item = import_overlay(media_pool, overlays_bin, overlay)
            if not media_pool.AppendToTimeline([{
                "mediaPoolItem": item,
                "startFrame": 0,
                "endFrame": frames - 1,
                "recordFrame": record,
                "trackIndex": 2,
                "mediaType": 1,
            }]):
                notes.append("%s: Resolve refused to add its overlay" % clip_name)
        elif overlay:
            notes.append("%s: its overlay file is missing" % clip_name)
        placed += 1
        record += frames
    pm.SaveProject()
    return {"ok": True, "project": name, "timeline": timeline.GetName(), "placed": placed,
            "notes": notes}


def handle(request, resolve_factory=connect):
    command = request.get("command")
    if command not in ("status", "add_overlays", "new_project"):
        return {"ok": False, "error": "bad_request", "message": "unknown command %r" % command}
    try:
        resolve = resolve_factory()
        if command == "status":
            return status(resolve)
        if command == "new_project":
            return new_project(resolve, request)
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
