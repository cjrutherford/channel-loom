import unittest
from stream_agreement import parse_playlist_entries


class PlaylistTraceTest(unittest.TestCase):
    def test_duration_and_offset_timestamp_stay_with_each_segment(self):
        entries = parse_playlist_entries(
            '#EXTM3U\n#EXTINF:2.002,\n'
            '#EXT-X-PROGRAM-DATE-TIME:2026-01-01T01:00:00+01:00\nseg_a.ts\n'
            '#EXTINF:1.5,\n#EXT-X-PROGRAM-DATE-TIME:2026-01-01T00:00:02.002Z\nseg_b.ts\n'
        )
        self.assertEqual(entries, [
            {'segment': 'seg_a.ts', 'duration_seconds': 2.002, 'pdt_ms': 1767225600000.0},
            {'segment': 'seg_b.ts', 'duration_seconds': 1.5, 'pdt_ms': 1767225602002.0},
        ])

    def test_missing_tags_do_not_inherit_previous_entry(self):
        entries = parse_playlist_entries('#EXTINF:2,\n#EXT-X-PROGRAM-DATE-TIME:2026-01-01T00:00:00Z\na.ts\nb.ts\n')
        self.assertIsNone(entries[1]['duration_seconds'])
        self.assertIsNone(entries[1]['pdt_ms'])


if __name__ == '__main__':
    unittest.main()
