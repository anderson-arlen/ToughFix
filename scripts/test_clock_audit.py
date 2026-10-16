"""Numerical and data-selection checks for the optional research scorer."""
import gzip
import hashlib
import importlib.util
from datetime import datetime, timedelta
from pathlib import Path
import tempfile
import unittest

import numpy as np

spec = importlib.util.spec_from_file_location('clock_audit', Path(__file__).with_name('score-clock-audit.py'))
audit = importlib.util.module_from_spec(spec)
spec.loader.exec_module(audit)


class ClockAuditTests(unittest.TestCase):
    def test_clock_reference_and_quantization_are_scored_at_correct_epochs(self):
        model = dict(degree=1, coefficients=[1e-4 + 10800e-12, 1e-12, 0.])
        coefficients = audit.encoded_coefficients(model)
        times = np.arange(0, 14 * audit.DAY, 900.)
        truth = 1e-4 + times * 1e-12
        score = audit.score(times, truth, coefficients, 0., 0., 14 * audit.DAY)
        self.assertLess(score['max_ns'], 1e-5)
        truth[times >= 7 * audit.DAY] += 200e-9
        score = audit.score(times, truth, coefficients, 0., 0., 14 * audit.DAY)
        self.assertAlmostEqual(score['rms_ns'], 200 / np.sqrt(2), places=6)
        self.assertAlmostEqual(score['max_ns'], 200, places=6)

    def test_prediction_flags_and_changed_products_cannot_enter_truth(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            name = 'IGS0OPSRAP_19800060000_01D_15M_ORB.SP3.gz'
            lines = ['#dP1980 01 06', '%c cc GPS']
            for index in range(96):
                instant = datetime(1980, 1, 6) + timedelta(minutes=15*index)
                lines.append(instant.strftime('*  %Y %m %d %H %M 00.00000000'))
                if index > 1:
                    continue
                row = list(f'PG01{22000.:14.6f}{10000.:14.6f}{10000.:14.6f}{100.:14.6f}'.ljust(80))
                if index == 1:
                    row[75] = 'P'
                lines.append(''.join(row))
            body = gzip.compress(('\n'.join(lines)+'\n').encode())
            (root/name).write_bytes(body)
            frozen = dict(dataset_start_gps=0., dataset_end_exclusive_gps=86400.,
                          sources=[dict(file=name, sha256=hashlib.sha256(body).hexdigest())])
            _, values = audit.load_clocks(root, frozen)
            self.assertAlmostEqual(values[0, 0], 100e-6)
            self.assertTrue(np.isnan(values[1, 0]))
            (root/name).write_bytes(body + b'x')
            with self.assertRaisesRegex(ValueError, 'Changed input'):
                audit.load_clocks(root, frozen)

    def test_sparse_truth_is_counted_as_missing_instead_of_a_good_forecast(self):
        times = np.arange(0, audit.DAY, 900.)
        truth = np.zeros(len(times))
        truth[20:30] = np.nan
        self.assertIsNone(audit.score(times, truth, [0., 0., 0.], 0., 0., audit.DAY))


if __name__ == '__main__':
    unittest.main()
