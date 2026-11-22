#!/usr/bin/env python3
"""Checks the paired statistics of qualification/paired_stats.py against known values and synthetic data.

Standard library only. Run with: python3 -I qualification/test_paired_stats.py
"""

import math
import pathlib
import random
import runpy
import unittest

STATS = runpy.run_path(pathlib.Path(__file__).with_name("paired_stats.py"))


def noisy_logs(rng, count, effect, spread=0.01, contaminated=0.2, wild=0.25):
    """Log ratios: most pairs on the true effect, a share thrown off by a neighbour on one side."""
    return [effect + rng.gauss(0.0, wild if rng.random() < contaminated else spread) for _ in range(count)]


class TDistribution(unittest.TestCase):
    def test_two_sided_95_quantiles(self):
        known = {1: 12.706205, 2: 4.302653, 5: 2.570582, 10: 2.228139, 19: 2.093024, 30: 2.042272, 120: 1.979930}
        for df, expected in known.items():
            self.assertAlmostEqual(STATS["t_quantile"](0.975, df), expected, places=5, msg=f"df={df}")

    def test_large_df_approaches_the_normal(self):
        self.assertAlmostEqual(STATS["t_quantile"](0.975, 1e6), 1.959964, places=4)

    def test_cdf_is_symmetric_and_inverts(self):
        for df in (3, 7, 25):
            for value in (0.3, 1.1, 2.5):
                total = STATS["t_cdf"](value, df) + STATS["t_cdf"](-value, df)
                self.assertAlmostEqual(total, 1.0, places=12)
                self.assertAlmostEqual(STATS["t_quantile"](STATS["t_cdf"](value, df), df), value, places=8)


class Estimators(unittest.TestCase):
    def test_trimmed_mean_drops_a_quarter_from_each_end(self):
        self.assertAlmostEqual(STATS["trimmed_mean"](list(range(1, 11))), 5.5)
        # An outlier at either end does not move the estimate.
        self.assertAlmostEqual(STATS["trimmed_mean"]([1, 2, 3, 4, 5, 6, 7, 1000]), 4.5)
        self.assertAlmostEqual(STATS["trimmed_mean"]([-1000, 2, 3, 4, 5, 6, 7, 8]), 4.5)

    def test_trimmed_mean_of_few_values_is_the_mean(self):
        self.assertAlmostEqual(STATS["trimmed_mean"]([1.0, 5.0, 9.0]), 5.0)

    def test_winsorized_variance_hand_computed(self):
        # 1..10 winsorized at 25% is 3 3 3 4 5 6 7 8 8 8: mean 5.5, sum of squares 42.5, over 9.
        self.assertAlmostEqual(STATS["winsorized_variance"](list(range(1, 11))), 42.5 / 9.0)

    def test_yuen_interval_hand_computed(self):
        estimate, error, df, low, high = STATS["yuen_interval"]([list(range(1, 11))])
        self.assertAlmostEqual(estimate, 5.5)
        self.assertAlmostEqual(error, math.sqrt(42.5 / 9.0 / (0.25 * 10)))
        self.assertAlmostEqual(df, 5.0)
        half = 2.570582 * error
        self.assertAlmostEqual(low, 5.5 - half, places=4)
        self.assertAlmostEqual(high, 5.5 + half, places=4)

    def test_yuen_over_strata_uses_satterthwaite(self):
        first, second = list(range(1, 11)), [2 * value for value in range(1, 11)]
        estimate, error, df, low, high = STATS["yuen_interval"]([first, second])
        self.assertAlmostEqual(estimate, (5.5 + 11.0) / 2.0)
        parts = [42.5 / 9.0 / 2.5, 4 * 42.5 / 9.0 / 2.5]
        self.assertAlmostEqual(error, math.sqrt(sum(parts)) / 2.0)
        self.assertAlmostEqual(df, sum(parts) ** 2 / sum(part ** 2 / 5.0 for part in parts))
        self.assertLess(low, estimate)
        self.assertGreater(high, estimate)

    def test_sign_test_is_exact(self):
        below, above, p = STATS["sign_test"]([-1.0] * 9 + [1.0])
        self.assertEqual((below, above), (9, 1))
        self.assertAlmostEqual(p, 2.0 * (1 + 10) / 1024.0)
        self.assertEqual(STATS["sign_test"]([0.0, 0.0])[2], 1.0)
        self.assertAlmostEqual(STATS["sign_test"]([-1.0] * 5 + [1.0] * 5)[2], 1.0)

    def test_average_ranks_share_ties(self):
        self.assertEqual(STATS["average_ranks"]([3.0, 1.0, 1.0, 2.0]), [3.0, 0.5, 0.5, 2.0])

    def test_lag1_detects_a_trend_and_not_noise(self):
        trend, _ = STATS["pooled_lag1"]([[float(i) for i in range(30)]])
        self.assertGreater(trend, 0.8)
        rng = random.Random(7)
        noise, count = STATS["pooled_lag1"]([[rng.random() for _ in range(400)]])
        self.assertLess(abs(noise), 0.15)
        self.assertEqual(count, 399)

    def test_bootstrap_is_reproducible_and_brackets_the_estimate(self):
        values = noisy_logs(random.Random(3), 24, -0.03)
        first = STATS["bootstrap_interval"]([values], "cell", reps=500)
        second = STATS["bootstrap_interval"]([values], "cell", reps=500)
        other = STATS["bootstrap_interval"]([values], "another cell", reps=500)
        self.assertEqual(first, second)
        self.assertNotEqual(first, other)
        estimate = STATS["trimmed_mean"](values)
        self.assertLess(first[0], estimate)
        self.assertGreater(first[1], estimate)


class RequiredPairs(unittest.TestCase):
    def test_scales_with_the_square_of_the_ratio_of_error_to_effect(self):
        # error 0.01, df large, effect 0.02: the interval half-width is 1.96 * 0.01, so 20 pairs already clear
        # it by 1.02; a quarter of the effect needs 16 times the pairs.
        exclude, one_percent = STATS["required_pairs"](-0.02, 0.01, 1e9, 20)
        self.assertEqual(exclude, math.ceil(20 * (1.959964 * 0.01) ** 2 / 0.02 ** 2))
        quarter, _ = STATS["required_pairs"](-0.005, 0.01, 1e9, 20)
        self.assertAlmostEqual(quarter / exclude, 16.0, delta=1.0)
        self.assertEqual(one_percent, math.ceil(20 * (1.959964 * 0.01) ** 2 / math.log(1.01) ** 2))

    def test_no_estimate_at_zero(self):
        self.assertIsNone(STATS["required_pairs"](0.0, 0.01, 30.0, 20)[0])


class Verdicts(unittest.TestCase):
    analyze = staticmethod(STATS["analyze_cell"])

    def cell(self, effects, key="synthetic", min_pairs=10, count=20, seed=1, **noise):
        rng = random.Random(seed)
        strata = {f"w{i}": noisy_logs(rng, count, effect, **noise) for i, effect in enumerate(effects)}
        return self.analyze(strata, key, min_pairs, reps=400)

    def test_clear_win(self):
        result = self.cell([-0.10] * 4)
        self.assertEqual(result["verdict"], "win")
        self.assertLess(result["interval"][1], 1.0)
        self.assertAlmostEqual(result["ratio"], math.exp(-0.10), delta=0.01)

    def test_clear_loss(self):
        result = self.cell([0.10] * 4)
        self.assertEqual(result["verdict"], "loss")
        self.assertGreater(result["interval"][0], 1.0)

    def test_noise_only_is_parity(self):
        self.assertEqual(self.cell([0.0] * 4)["verdict"], "parity")

    def test_too_few_pairs_is_insufficient_even_with_a_big_effect(self):
        result = self.cell([-0.3] * 4, count=6, min_pairs=10)
        self.assertEqual(result["verdict"], "insufficient")
        self.assertIn("minimum 10", result["reason"])
        self.assertEqual(self.cell([-0.3], count=3)["verdict"], "insufficient")

    def test_an_empty_stratum_is_insufficient(self):
        result = self.analyze({"a": [], "b": [-0.1] * 20}, "empty", 10, reps=50)
        self.assertEqual(result["verdict"], "insufficient")

    def test_a_win_in_one_workload_cannot_carry_a_loss_in_the_others_to_a_win(self):
        # One workload at 0.6 times the CPU and three at 1.04 times: the pooled estimate is not below parity.
        result = self.cell([math.log(0.6), math.log(1.04), math.log(1.04), math.log(1.04)])
        self.assertNotEqual(result["verdict"], "win")

    def test_same_input_gives_the_same_result(self):
        self.assertEqual(self.cell([-0.04] * 3, seed=5), self.cell([-0.04] * 3, seed=5))

    def test_false_win_rate_on_equal_toolchains_is_small(self):
        wins = sum(self.cell([0.0] * 4, key=f"null-{seed}", seed=seed)["verdict"] == "win" for seed in range(120))
        losses = sum(self.cell([0.0] * 4, key=f"null-{seed}", seed=seed)["verdict"] == "loss" for seed in range(120))
        self.assertLessEqual(wins, 6)
        self.assertLessEqual(losses, 6)

    def test_power_to_see_a_four_percent_win_over_a_noisy_host(self):
        wins = sum(self.cell([-0.04] * 4, key=f"power-{seed}", seed=seed, contaminated=0.3, wild=0.15)["verdict"] == "win"
                   for seed in range(60))
        self.assertGreaterEqual(wins, 54)

    def test_serial_correlation_widens_the_interval(self):
        rng = random.Random(11)
        drift = [-0.02 + 0.002 * i + rng.gauss(0, 0.002) for i in range(30)]
        correlated = self.analyze({"a": drift}, "drift", 10, reps=300)
        shuffled = list(drift)
        random.Random(2).shuffle(shuffled)
        independent = self.analyze({"a": shuffled}, "drift", 10, reps=300)
        self.assertGreater(correlated["serial_inflation"], 1.0)
        self.assertEqual(independent["serial_inflation"], 1.0)
        width = lambda result: result["interval"][1] - result["interval"][0]
        self.assertGreater(width(correlated), width(independent))

    def test_order_effect_is_measured(self):
        # Whoever runs second looks 5 percent slower: the half-difference of the two orders is that effect.
        rng = random.Random(4)
        flags = [index % 2 == 0 for index in range(40)]
        # official first means fRust second: the ratio carries +5 percent; the reverse carries -5 percent.
        logs = [0.0488 * (1 if first else -1) + rng.gauss(0, 0.002) for first in flags]
        result = self.analyze({"a": logs}, "order", 10, reps=100, order={"a": flags})
        self.assertAlmostEqual(result["order_effect"], math.exp(0.0488), delta=0.003)
        self.assertAlmostEqual(result["ratio"], 1.0, delta=0.01)
        self.assertEqual(result["verdict"], "parity")

    def test_pairs_needed_reports_the_gap(self):
        weak = self.cell([-0.001] * 4, count=20, contaminated=0.0, spread=0.03, seed=3)
        self.assertEqual(weak["verdict"], "parity")
        self.assertGreater(weak["pairs_needed_to_exclude_parity"], 20)
        # The interval is already narrower than plus or minus 1 percent, so fewer pairs would do for that.
        self.assertLess(weak["pairs_needed_for_1pct"], 20)
        wide = self.cell([-0.001] * 4, count=20, contaminated=0.0, spread=0.12, seed=3)
        self.assertGreater(wide["pairs_needed_for_1pct"], 20)


if __name__ == "__main__":
    unittest.main()
