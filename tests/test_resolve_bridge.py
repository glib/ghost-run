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
    def __init__(self, path, frames=None, timecode=None):
        self.path = path
        self.proxy = None
        self.props = {"Frames": frames, "Start TC": timecode}

    def LinkProxyMedia(self, path):
        self.proxy = path
        return True

    def ReplaceClip(self, path):
        self.path = path
        return True

    def GetClipProperty(self, name):
        return self.path if name == "File Path" else self.props.get(name)


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
    def __init__(self, tracks, name="Edit", audio=None):
        self.tracks = tracks  # list of lists of FakeItem, track 1 first
        self.audio = audio or []
        self.name = name

    def GetName(self):
        return self.name

    def GetStartFrame(self):
        return 86400

    def GetTrackCount(self, kind):
        return len(self.audio if kind == "audio" else self.tracks)

    def GetItemListInTrack(self, kind, index):
        return list((self.audio if kind == "audio" else self.tracks)[index - 1])

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

    def CreateEmptyTimeline(self, name):
        self.timeline = FakeTimeline([[]], name)
        return self.timeline

    def AppendToTimeline(self, infos):
        for info in infos:
            length = info["endFrame"] - info["startFrame"] + 1
            item = FakeItem(info["mediaPoolItem"], info["recordFrame"],
                            info["recordFrame"] + length, info["startFrame"])
            self.timeline.tracks[info["trackIndex"] - 1].append(item)
        return [object()] * len(infos)


class FakeProject:
    def __init__(self, timeline, name="Race day"):
        self.timeline = timeline
        self.pool = FakeMediaPool(timeline)
        self.name = name
        self.settings = {}

    def GetName(self):
        return self.name

    def SetSetting(self, key, value):
        self.settings[key] = value
        return True

    def SetCurrentTimeline(self, timeline):
        self.timeline = timeline
        return True

    def GetCurrentTimeline(self):
        return self.timeline

    def GetTimelineCount(self):
        return len(self.timelines())

    def GetTimelineByIndex(self, index):
        return self.timelines()[index - 1]

    def timelines(self):
        return [t for t in [self.timeline] + getattr(self, "others", []) if t is not None]

    def GetMediaPool(self):
        return self.pool


class FakeResolve:
    def __init__(self, project):
        self.project = project
        self.projects = {"Race day": project}
        self.saved = 0

    def GetProjectListInCurrentFolder(self):
        return list(self.projects)

    def CreateProject(self, name):
        if name in self.projects:
            return None
        self.project = FakeProject(None, name)
        self.projects[name] = self.project
        return self.project

    def SaveProject(self):
        self.saved += 1
        return True

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
        self.assertEqual(reply["results"][0]["note"], "its clip is missing")
        reply = bridge.handle(self.request(overlay="/v/gone.mov"), lambda: self.resolve)
        self.assertIn("missing", reply["results"][0]["note"])

    def test_a_clip_not_on_the_timeline_is_added_at_its_end_with_its_overlay(self):
        new = os.path.join(self.dir.name, "DJI_0002.MP4")
        proxy = os.path.join(self.dir.name, "DJI_0002.mov")
        for p in (new, proxy):
            Path(p).write_bytes(b"x")
        reply = bridge.handle(self.request(clip=new, proxy=proxy, first_frame=60, frames=200),
                              lambda: self.resolve)
        result = reply["results"][0]
        self.assertTrue(result["clip_added"], result)
        self.assertEqual((result["placed"], result["note"]), (1, None))
        v1, v2 = self.timeline.tracks
        added = v1[-1]
        # After the last item (which ends at 88000), from the overlay's first clip frame.
        self.assertEqual((added.start, added.end, added.left), (88000, 88200, 60))
        self.assertEqual(added.mpi.proxy, proxy)
        self.assertEqual([(i.start, i.end, i.left) for i in v2], [(88000, 88200, 0)])
        bins = [f.name for f in self.project.pool.root.subfolders]
        self.assertIn("Ghost Run clips", bins)

        # Again: the clip is there now, so nothing more is added.
        reply = bridge.handle(self.request(clip=new, proxy=proxy, first_frame=60, frames=200),
                              lambda: self.resolve)
        self.assertFalse(reply["results"][0]["clip_added"])
        self.assertEqual(reply["results"][0]["already"], 1)
        self.assertEqual(len(v1), 3)

    def test_a_clip_whose_timed_part_was_cut_is_added_again(self):
        # The clip is on the timeline, but only frames 0..19, before the timed part.
        self.timeline.tracks[0][0].end = 86420
        reply = bridge.handle(self.request(), lambda: self.resolve)
        self.assertTrue(reply["results"][0]["clip_added"])
        self.assertEqual(self.timeline.tracks[0][-1].start, 88000)

    def test_a_clip_already_in_the_media_pool_is_not_imported_again(self):
        new = os.path.join(self.dir.name, "DJI_0003.MP4")
        Path(new).write_bytes(b"x")
        pooled = FakeMediaPoolItem(new)
        self.project.pool.root.clips.append(pooled)
        reply = bridge.handle(self.request(clip=new), lambda: self.resolve)
        self.assertTrue(reply["results"][0]["clip_added"])
        self.assertIs(self.timeline.tracks[0][-1].mpi, pooled)
        # Only the overlay was imported.
        self.assertEqual(self.project.pool.imports, 1)

    def test_an_empty_timeline_starts_at_its_first_frame(self):
        self.timeline.tracks = [[]]
        reply = bridge.handle(self.request(), lambda: self.resolve)
        self.assertEqual(reply["results"][0]["placed"], 1)
        self.assertEqual(self.timeline.tracks[0][0].start, 86400)
        self.assertEqual(self.timeline.tracks[1][0].start, 86400)

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


class ReadTimelineTests(unittest.TestCase):
    def setUp(self):
        a = FakeMediaPoolItem("/edit/DJI_0001.MP4.mov")
        overlay = FakeMediaPoolItem("/edit/Overlays/DJI_0001.overlay.mov")
        b = FakeMediaPoolItem("/nas/DJI_0002.MP4")
        title = FakeItem(None, 0, 50, 0)  # a generator: no media
        self.timeline = FakeTimeline([
            [title, FakeItem(b, 250, 400, 10), FakeItem(a, 50, 250, 30)],
            [FakeItem(overlay, 50, 250, 0)],
        ], name="Final")
        self.project = FakeProject(self.timeline, name="Ridge laps")
        self.project.others = [FakeTimeline([], name="Rough")]
        self.resolve = FakeResolve(self.project)

    def read(self, **request):
        return bridge.handle(dict(command="read_timeline", **request), lambda: self.resolve)

    def test_lists_the_clips_in_timeline_order(self):
        reply = self.read()
        self.assertTrue(reply["ok"], reply)
        self.assertEqual((reply["project"], reply["timeline"]), ("Ridge laps", "Final"))
        self.assertEqual(
            [(i["path"], i["track"], i["record"], i["first_frame"], i["frames"])
             for i in reply["items"]],
            [("/edit/DJI_0001.MP4.mov", 1, 50, 30, 200),
             ("/edit/Overlays/DJI_0001.overlay.mov", 2, 50, 0, 200),
             ("/nas/DJI_0002.MP4", 1, 250, 10, 150)],
        )

    def test_a_named_timeline_in_the_open_project(self):
        reply = self.read(project="Ridge laps", timeline="Rough")
        self.assertEqual((reply["timeline"], reply["items"]), ("Rough", []))
        reply = self.read(project="Ridge laps", timeline="Gone")
        self.assertEqual(reply["error"], "no_timeline")

    def test_another_project_open_is_refused_not_switched(self):
        reply = self.read(project="Whistler", timeline="Final")
        self.assertEqual(reply["error"], "other_project")
        self.assertIn("Open the project Whistler", reply["message"])
        self.assertIs(self.resolve.project, self.project)


class NewProjectTests(unittest.TestCase):
    def setUp(self):
        self.dir = tempfile.TemporaryDirectory()
        self.resolve = FakeResolve(FakeProject(FakeTimeline([[]])))

    def tearDown(self):
        self.dir.cleanup()

    def file(self, name):
        path = os.path.join(self.dir.name, name)
        Path(path).write_bytes(b"x")
        return path

    def test_runs_go_back_to_back_with_overlays_above_and_proxies_linked(self):
        runs = [
            {"clip": self.file("a.MP4"), "proxy": self.file("a.mov"),
             "overlay": self.file("a.overlay.mov"), "first_frame": 30, "frames": 300},
            {"clip": self.file("b.MP4"), "proxy": None,
             "overlay": None, "first_frame": 0, "frames": 120},
            {"clip": "/v/gone.MP4", "proxy": None, "overlay": None,
             "first_frame": 0, "frames": 10},
        ]
        reply = bridge.handle({"command": "new_project", "name": "Race day", "fps": "59.94",
                               "width": 3840, "height": 2160, "runs": runs},
                              lambda: self.resolve)
        self.assertTrue(reply["ok"], reply)
        # "Race day" was taken.
        self.assertEqual(reply["project"], "Race day 2")
        self.assertEqual(reply["placed"], 2)
        self.assertEqual(reply["notes"], ["gone.MP4: the clip is missing"])
        project = self.resolve.projects["Race day 2"]
        self.assertEqual(project.settings["timelineFrameRate"], "59.94")
        self.assertEqual(project.settings["timelineResolutionWidth"], "3840")
        v1, v2 = project.timeline.tracks
        self.assertEqual([(i.start, i.end, i.left) for i in v1],
                         [(86400, 86700, 30), (86700, 86820, 0)])
        self.assertEqual([(i.start, i.end, i.left) for i in v2], [(86400, 86700, 0)])
        self.assertTrue(v1[0].mpi.proxy.endswith("a.mov"))
        self.assertEqual(self.resolve.saved, 1)

    def test_an_overlay_goes_where_its_run_is_in_a_whole_clip(self):
        runs = [
            # A whole 600-frame clip whose run's overlay starts at frame 200.
            {"clip": self.file("a.MP4"), "proxy": None, "overlay": self.file("a.overlay.mov"),
             "first_frame": 0, "frames": 600, "overlay_first_frame": 200,
             "overlay_frames": 300},
            # Used from frame 100 for 100 frames; its overlay starts at 50.
            {"clip": self.file("b.MP4"), "proxy": None, "overlay": self.file("b.overlay.mov"),
             "first_frame": 100, "frames": 100, "overlay_first_frame": 50,
             "overlay_frames": 120},
            # The run isn't in the part used.
            {"clip": self.file("c.MP4"), "proxy": None, "overlay": self.file("c.overlay.mov"),
             "first_frame": 0, "frames": 100, "overlay_first_frame": 400,
             "overlay_frames": 50},
        ]
        reply = bridge.handle({"command": "new_project", "name": "Clips", "runs": runs},
                              lambda: self.resolve)
        self.assertTrue(reply["ok"], reply)
        self.assertEqual(reply["placed"], 3)
        self.assertEqual(reply["notes"], ["c.MP4: its timed part isn't in the part used"])
        v1, v2 = self.resolve.projects["Clips"].timeline.tracks
        self.assertEqual([(i.start, i.end, i.left) for i in v1],
                         [(86400, 87000, 0), (87000, 87100, 100), (87100, 87200, 0)])
        # a: at 86400 + 200, all 300 frames; b: from the item's start, overlay
        # frame 50 onward for the 70 frames left of it.
        self.assertEqual([(i.start, i.end, i.left) for i in v2],
                         [(86600, 86900, 0), (87000, 87070, 50)])

    def test_captions_go_on_a_third_track_from_the_clips_start(self):
        runs = [
            {"clip": self.file("a.MP4"), "proxy": None, "first_frame": 0, "frames": 600,
             "caption": self.file("00-caption.mov"), "caption_frames": 120},
            {"clip": self.file("b.MP4"), "proxy": None, "first_frame": 50, "frames": 60,
             "caption": self.file("01-caption.mov"), "caption_frames": 120},
            {"clip": self.file("c.MP4"), "proxy": None, "first_frame": 0, "frames": 10,
             "caption": "/v/gone-caption.mov"},
        ]
        reply = bridge.handle({"command": "new_project", "name": "Drop", "runs": runs},
                              lambda: self.resolve)
        self.assertTrue(reply["ok"], reply)
        self.assertEqual(reply["notes"], ["c.MP4: its caption file is missing"])
        v1, v2, v3 = self.resolve.projects["Drop"].timeline.tracks
        self.assertEqual(v2, [])
        # Each from its item's start; never longer than the item.
        self.assertEqual([(i.start, i.end, i.left) for i in v3],
                         [(86400, 86520, 0), (87000, 87060, 0)])

    def test_overlay_spans(self):
        self.assertEqual(bridge.overlay_span(0, 600, 200, 300), (200, 0, 300))
        self.assertEqual(bridge.overlay_span(100, 100, 50, 120), (0, 50, 70))
        self.assertEqual(bridge.overlay_span(30, 300, 30, 300), (0, 0, 300))
        self.assertIsNone(bridge.overlay_span(0, 100, 100, 10))

    def test_names_never_clash(self):
        self.assertEqual(bridge.unused_name({"A", "A 2"}, "A"), "A 3")
        self.assertEqual(bridge.unused_name(set(), "A"), "A")


class RelinkTests(unittest.TestCase):
    def setUp(self):
        self.dir = tempfile.TemporaryDirectory()
        self.addCleanup(self.dir.cleanup)
        here = Path(self.dir.name)
        self.copy = here / "GX010123.MP4.mov"
        self.copy.write_bytes(b"copy")
        self.proxy = here / "Proxy" / "GX010123.MP4.mov"
        self.proxy.parent.mkdir()
        self.proxy.write_bytes(b"proxy")
        self.online = here / "music.wav"
        self.online.write_bytes(b"wav")
        self.gone = FakeMediaPoolItem("/old/clips/f-~47~58010123.~4d~50~34.mov", "1200",
                                      "11:00:00:00")
        self.unused = FakeMediaPoolItem("/old/clips/unused.MP4", "300", None)
        music = FakeMediaPoolItem(str(self.online))
        timeline = FakeTimeline(
            [[FakeItem(self.gone, 0, 100, 0), FakeItem(None, 100, 150, 0),
              FakeItem(self.gone, 150, 300, 400)]],
            name="Final", audio=[[FakeItem(music, 0, 300, 0)]],
        )
        self.project = FakeProject(timeline, name="Ridge laps")
        bin_ = FakeFolder("Footage")
        bin_.clips = [self.gone, self.unused]
        self.project.pool.root.subfolders.append(bin_)
        self.project.pool.root.clips.append(music)
        self.resolve = FakeResolve(self.project)

    def run_bridge(self, **request):
        return bridge.handle(request, lambda: self.resolve)

    def test_lists_the_open_timelines_files_or_the_whole_projects(self):
        reply = self.run_bridge(command="list_media")
        self.assertTrue(reply["ok"], reply)
        self.assertEqual((reply["project"], reply["timeline"]), ("Ridge laps", "Final"))
        self.assertEqual(reply["files"], [
            {"path": self.gone.path, "frames": 1200, "timecode": "11:00:00:00",
             "online": False},
            {"path": str(self.online), "frames": None, "timecode": None, "online": True},
        ])
        reply = self.run_bridge(command="list_media", whole_project=True)
        self.assertEqual([f["path"] for f in reply["files"]],
                         [str(self.online), self.gone.path, self.unused.path])

    def test_no_open_timeline(self):
        self.project.timeline = None
        reply = self.run_bridge(command="list_media")
        self.assertEqual(reply["error"], "no_timeline")
        self.assertTrue(self.run_bridge(command="list_media", whole_project=True)["ok"])

    def test_points_items_at_their_new_files_and_saves(self):
        old = self.gone.path
        reply = self.run_bridge(command="relink", project="Ridge laps", files=[
            {"path": old, "new": str(self.copy), "proxy": str(self.proxy)},
            {"path": self.unused.path, "new": "/nowhere/unused.MP4.mov", "proxy": None},
            {"path": "/old/clips/never.MP4", "new": str(self.copy), "proxy": None},
        ])
        self.assertTrue(reply["ok"], reply)
        self.assertEqual(reply["relinked"], 1)
        self.assertEqual(self.gone.path, str(self.copy))
        self.assertEqual(self.gone.proxy, str(self.proxy))
        self.assertEqual(self.unused.path, "/old/clips/unused.MP4")
        self.assertEqual(reply["notes"], [
            "unused.MP4: /nowhere/unused.MP4.mov isn't there",
            "never.MP4: not in the media pool",
        ])
        self.assertEqual(self.resolve.saved, 1)

    def test_another_project_open_is_refused(self):
        reply = self.run_bridge(command="relink", project="Whistler", files=[])
        self.assertEqual(reply["error"], "other_project")
        self.assertEqual(self.resolve.saved, 0)


class FakeProjectManager:
    """Projects in nested folders: {"name": project, "Folder/": {...}}."""

    def __init__(self, tree, current):
        self.tree = tree
        self.path = []
        self.current = current
        self.saved = 0
        self.pages = []

    def here(self):
        node = self.tree
        for part in self.path:
            node = node[part + "/"]
        return node

    def GetProjectListInCurrentFolder(self):
        return [k for k in self.here() if not k.endswith("/")]

    def GetFolderListInCurrentFolder(self):
        return [k[:-1] for k in self.here() if k.endswith("/")]

    def OpenFolder(self, name):
        if name + "/" not in self.here():
            return False
        self.path.append(name)
        return True

    def GotoParentFolder(self):
        self.path.pop()
        return True

    def GotoRootFolder(self):
        self.path = []
        return True

    def LoadProject(self, name):
        project = self.here().get(name)
        if project is not None:
            self.current = project
        return project

    def SaveProject(self):
        self.saved += 1
        return True

    def GetCurrentProject(self):
        return self.current

    # The Resolve object, too.
    def GetProjectManager(self):
        return self

    def OpenPage(self, page):
        self.pages.append(page)
        return True


class OpenTests(unittest.TestCase):
    def setUp(self):
        self.open_now = FakeProject(FakeTimeline([[]], name="Main"), name="Whistler")
        self.wanted = FakeProject(FakeTimeline([[]], name="Timeline 1"), name="Ridge laps")
        self.wanted.others = [FakeTimeline([[]], name="YouTube")]
        self.pm = FakeProjectManager(
            {"Whistler": self.open_now, "2025/": {"Old/": {"Ridge laps": self.wanted}}},
            self.open_now,
        )

    def open(self, **request):
        return bridge.handle(dict(command="open", **request), lambda: self.pm)

    def test_saves_the_open_project_then_opens_the_one_in_a_folder_at_its_timeline(self):
        reply = self.open(project="Ridge laps", timeline="YouTube")
        self.assertEqual(reply, {"ok": True, "project": "Ridge laps", "timeline": "YouTube",
                                 "notes": []})
        self.assertEqual(self.pm.saved, 1)
        self.assertIs(self.pm.current, self.wanted)
        self.assertEqual(self.pm.pages, ["edit"])

    def test_the_project_already_open_is_kept_and_only_the_timeline_changes(self):
        reply = self.open(project="Whistler", timeline="Nope")
        self.assertTrue(reply["ok"])
        self.assertEqual(reply["notes"], ["Whistler has no timeline called Nope"])
        self.assertEqual(self.pm.saved, 0)

    def test_a_project_not_there_is_refused_and_nothing_closed(self):
        reply = self.open(project="Elsewhere")
        self.assertEqual(reply["error"], "no_such_project")
        self.assertIs(self.pm.current, self.open_now)


if __name__ == "__main__":
    unittest.main()
