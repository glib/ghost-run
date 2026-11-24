"""Ghost Run's Resolve bridge (crates/app/src/resolve_bridge.py), against a
fake of the parts of Resolve's scripting API it uses."""

import importlib.util
import json
import os
import tempfile
import unittest
from pathlib import Path

BRIDGE = Path(__file__).resolve().parents[1] / "crates/app/src/resolve_bridge.py"
spec = importlib.util.spec_from_file_location("resolve_bridge", BRIDGE)
bridge = importlib.util.module_from_spec(spec)
spec.loader.exec_module(bridge)


class FakeMediaPoolItem:
    def __init__(self, path):
        self.path = path

    def GetClipProperty(self, name):
        return self.path if name == "File Path" else None


class FakeItem:
    def __init__(self, mpi, start, end, left):
        self.mpi, self.start, self.end, self.left = mpi, start, end, left

    def GetMediaPoolItem(self):
        return self.mpi

    def GetStart(self):
        return self.start

    def GetEnd(self):
        return self.end

    def GetLeftOffset(self):
        return self.left


class FakeTimeline:
    def __init__(self, tracks):
        self.tracks = tracks  # list of lists of FakeItem, track 1 first

    def GetName(self):
        return "Edit"

    def GetTrackCount(self, kind):
        return len(self.tracks)

    def GetItemListInTrack(self, kind, index):
        return list(self.tracks[index - 1])

    def AddTrack(self, kind):
        self.tracks.append([])
        return True


class FakeFolder:
    def __init__(self, name):
        self.name, self.clips, self.subfolders = name, [], []

    def GetName(self):
        return self.name

    def GetClipList(self):
        return list(self.clips)

    def GetSubFolderList(self):
        return list(self.subfolders)


class FakeMediaPool:
    def __init__(self, timeline):
        self.timeline = timeline
        self.root = FakeFolder("Master")
        self.current = self.root
        self.imports = 0

    def GetRootFolder(self):
        return self.root

    def AddSubFolder(self, parent, name):
        folder = FakeFolder(name)
        parent.subfolders.append(folder)
        return folder

    def GetCurrentFolder(self):
        return self.current

    def SetCurrentFolder(self, folder):
        self.current = folder
        return True

    def ImportMedia(self, paths):
        self.imports += 1
        items = [FakeMediaPoolItem(p) for p in paths]
        self.current.clips.extend(items)
        return items

    def AppendToTimeline(self, infos):
        for info in infos:
            length = info["endFrame"] - info["startFrame"] + 1
            item = FakeItem(info["mediaPoolItem"], info["recordFrame"],
                            info["recordFrame"] + length, info["startFrame"])
            self.timeline.tracks[info["trackIndex"] - 1].append(item)
        return [object()] * len(infos)


class FakeProject:
    def __init__(self, timeline):
        self.timeline = timeline
        self.pool = FakeMediaPool(timeline)

    def GetName(self):
        return "Race day"

    def GetCurrentTimeline(self):
        return self.timeline

    def GetMediaPool(self):
        return self.pool


class FakeResolve:
    def __init__(self, project):
        self.project = project

    def GetProjectManager(self):
        return self

    def GetCurrentProject(self):
        return self.project

    def GetProductName(self):
        return "DaVinci Resolve Studio"

    def GetVersionString(self):
        return "20.2.1"


class PlacementTests(unittest.TestCase):
    def test_overlay_goes_on_the_track_above_at_the_clips_frame(self):
        # Clip shown from its frame 100, at timeline frame 86400; overlay starts at clip frame 130.
        spots = bridge.placements([(1, 86400, 87000, 100)], 130, 300)
        self.assertEqual(spots, [{"track": 2, "record_frame": 86430,
                                  "start_frame": 0, "end_frame": 299}])

    def test_overlay_is_trimmed_to_the_part_of_the_clip_on_the_timeline(self):
        # The clip is cut from frame 200 to 350 (exclusive): overlay frames 70..219.
        spots = bridge.placements([(2, 1000, 1150, 200)], 130, 300)
        self.assertEqual(spots, [{"track": 3, "record_frame": 1000,
                                  "start_frame": 70, "end_frame": 219}])

    def test_uses_outside_the_overlay_are_skipped(self):
        self.assertEqual(bridge.placements([(1, 0, 50, 0)], 130, 300), [])

    def test_a_busy_track_moves_the_overlay_up(self):
        occupied = {2: [(0, 100)], 3: [(500, 600)]}
        self.assertEqual(bridge.free_track(occupied, 2, 50, 100), 3)
        self.assertEqual(bridge.free_track(occupied, 2, 100, 100), 2)
        self.assertEqual(bridge.free_track(occupied, 2, 50, 500), 4)


class AddOverlaysTests(unittest.TestCase):
    def setUp(self):
        self.dir = tempfile.TemporaryDirectory()
        self.clip = os.path.join(self.dir.name, "DJI_0001.MP4")
        self.overlay = os.path.join(self.dir.name, "DJI_0001.overlay.mov")
        for p in (self.clip, self.overlay):
            Path(p).write_bytes(b"x")
        clip_item = FakeItem(FakeMediaPoolItem(self.clip), 86400, 87400, 0)
        other = FakeItem(FakeMediaPoolItem("/v/other.mp4"), 87400, 88000, 0)
        self.timeline = FakeTimeline([[clip_item, other]])
        self.project = FakeProject(self.timeline)
        self.resolve = FakeResolve(self.project)

    def tearDown(self):
        self.dir.cleanup()

    def request(self, **overrides):
        item = {"clip": self.clip, "overlay": self.overlay, "first_frame": 30, "frames": 330}
        item.update(overrides)
        return {"command": "add_overlays", "items": [item]}

    def test_places_the_overlay_once_and_imports_it_into_a_bin(self):
        reply = bridge.handle(self.request(), lambda: self.resolve)
        self.assertTrue(reply["ok"], reply)
        self.assertEqual(reply["results"][0]["placed"], 1)
        self.assertEqual(len(self.timeline.tracks), 2)
        placed = self.timeline.tracks[1][0]
        self.assertEqual((placed.start, placed.end), (86430, 86760))
        self.assertEqual(self.project.pool.root.subfolders[0].name, "Ghost Run overlays")

        # Again: nothing new, and the overlay isn't imported twice.
        reply = bridge.handle(self.request(), lambda: self.resolve)
        self.assertEqual(reply["results"][0]["already"], 1)
        self.assertEqual(reply["results"][0]["placed"], 0)
        self.assertEqual(len(self.timeline.tracks[1]), 1)
        self.assertEqual(self.project.pool.imports, 1)

    def test_says_why_an_overlay_was_not_placed(self):
        reply = bridge.handle(self.request(clip="/v/elsewhere.mp4"), lambda: self.resolve)
        self.assertIn("isn't on the current timeline", reply["results"][0]["note"])
        reply = bridge.handle(self.request(overlay="/v/gone.mov"), lambda: self.resolve)
        self.assertIn("missing", reply["results"][0]["note"])

    def test_status_and_errors(self):
        reply = bridge.handle({"command": "status"}, lambda: self.resolve)
        self.assertEqual(reply["project"], "Race day")
        self.assertEqual(reply["timeline"], "Edit")

        def refuse():
            raise bridge.BridgeError("not_running", "Resolve didn't answer.")

        reply = bridge.handle({"command": "status"}, refuse)
        self.assertEqual(reply, {"ok": False, "error": "not_running",
                                 "message": "Resolve didn't answer."})
        self.assertEqual(bridge.handle({"command": "nope"})["error"], "bad_request")
        json.dumps(reply)

    def test_without_resolve_it_says_so(self):
        # No Resolve installed where the tests run: a clear error, not a crash.
        old = os.environ.get("RESOLVE_SCRIPT_API")
        os.environ["RESOLVE_SCRIPT_API"] = self.dir.name
        try:
            reply = bridge.handle({"command": "status"})
        finally:
            if old is None:
                os.environ.pop("RESOLVE_SCRIPT_API")
            else:
                os.environ["RESOLVE_SCRIPT_API"] = old
        self.assertFalse(reply["ok"])
        self.assertIn(reply["error"], ("not_installed", "not_running"))


if __name__ == "__main__":
    unittest.main()
