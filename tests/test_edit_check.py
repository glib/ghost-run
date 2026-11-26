"""scripts/edit_check.py: the timing maths of the test edit it writes for
Resolve, without making any media."""

import importlib.util
import json
import tempfile
import unittest
import xml.etree.ElementTree as ET
from pathlib import Path

SCRIPT = Path(__file__).resolve().parents[1] / "scripts/edit_check.py"
spec = importlib.util.spec_from_file_location("edit_check", SCRIPT)
ec = importlib.util.module_from_spec(spec)
spec.loader.exec_module(ec)


def frames(t):
    """An FCPXML time back to frames at 29.97."""
    if t == "0s":
        return 0
    num, den = t[:-1].split("/")
    n = int(num) * ec.FPS_NUM
    assert n % (int(den) * ec.FPS_DEN) == 0, t
    return n // (int(den) * ec.FPS_DEN)


class EditCheckTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.edit, self.assets = ec.build(Path(self.tmp.name), make_media=False)
        self.by_key = {a.key: a for a in self.assets}

    def tearDown(self):
        self.tmp.cleanup()

    def test_fcp_time(self):
        self.assertEqual(ec.fcp_time(0), "0s")
        self.assertEqual(ec.fcp_time(1), "1001/30000s")
        self.assertEqual(ec.tc_string(ec.RUN_TC), "10:00:00:00")
        self.assertEqual(ec.tc_string(ec.TIMELINE_START), "01:00:00:00")

    def test_records_and_transitions(self):
        rec = [(item[0], at) for item, at in self.edit.records()]
        self.assertEqual(rec, [("clip", 0), ("clip", 45), ("clip", 90), ("clip", 135),
                               ("transition", 337), ("clip", 345), ("clip", 975),
                               ("transition", 1170), ("gap", 1185)])
        self.assertEqual(self.edit.length(), 1215)

    def test_intro_lands_its_cut_frame_on_the_cut(self):
        intro = self.by_key["intro"]
        (_, src_in, _, record), = [x for x in self.edit.above if x[0] is intro]
        self.assertEqual(record + 94 - src_in, self.edit.record_of(self.by_key["talk"]))
        self.assertEqual(self.edit.parent_at(record), 0)  # starts over S1

    def test_fcpxml_connected_clips_use_the_parents_source_time(self):
        root = ET.fromstring(ec.fcpxml(self.edit, self.assets, "t").split("\n", 2)[2])
        spine = root.find("library/event/project/sequence/spine")
        clips = spine.findall("asset-clip")
        s1, run = clips[0], clips[4]
        intro = s1.find("asset-clip")
        # S1 is used from its frame 30; the intro starts 41 frames into the timeline.
        self.assertEqual(frames(intro.get("offset")), 30 + 41)
        overlay = run.find("asset-clip")
        self.assertEqual(overlay.get("offset"), run.get("start"))
        self.assertEqual(frames(run.get("start")), ec.RUN_TC + 60)
        self.assertEqual(frames(run.get("offset")), ec.TIMELINE_START + 345)
        fade = spine.findall("transition")[1]
        self.assertEqual(frames(fade.get("offset")), ec.TIMELINE_START + 1170)

    def test_otio_tracks_line_up(self):
        tl = json.loads(ec.otio(self.edit, "t"))
        v1, v2, a1 = tl["tracks"]["children"]
        self.assertEqual([c["OTIO_SCHEMA"] for c in v1["children"]],
                         ["Clip.1"] * 4 + ["Transition.1", "Clip.1", "Clip.1",
                                           "Transition.1", "Gap.1"])
        self.assertEqual([c["OTIO_SCHEMA"] for c in v2["children"]],
                         ["Gap.1", "Clip.1", "Gap.1", "Clip.1"])
        gap1, intro, gap2, overlay = v2["children"]
        self.assertEqual(gap1["source_range"]["duration"]["value"], 41)
        self.assertEqual(41 + 198 + gap2["source_range"]["duration"]["value"], 345)
        self.assertEqual(overlay["source_range"]["start_time"]["value"], ec.RUN_TC + 60)
        self.assertEqual(a1["kind"], "Audio")


if __name__ == "__main__":
    unittest.main()
