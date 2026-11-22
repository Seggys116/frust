import math


def median_interval(samples, simultaneous=1):
    """Two-sided distribution-free 95% interval for a population median."""
    values = sorted(samples)
    if not values:
        return None
    n = len(values)
    tail = 0
    interval = None
    for index in range((n + 1) // 2):
        tail += math.comb(n, index)
        # Bound both binomial tails, with Bonferroni across simultaneous medians.
        if 40 * simultaneous * tail > 2**n:
            break
        interval = (values[index], values[n - index - 1])
    return interval


def interval_result(interval, parity):
    if interval is None:
        return "insufficient pairs"
    low, high = interval
    if high < parity:
        return "lower"
    if low > parity:
        return "higher"
    return "unresolved"


# ---------------------------------------------------------------------------------------------
# Paired ratios of CPU time, for qualification/make-charts.py and its tests. Standard library only.
#
# A pair is one fRust build and one official build of the same scenario taken back to back. Its
# value is the log of fRust CPU over official CPU, so that "x times as much" and "1/x times as much"
# are symmetric and a mean is a geometric mean of ratios. Shared hosts add noise to CPU time that is
# not Gaussian: most pairs sit on the true ratio and a few are thrown far off by a neighbour that
# slowed one build and not the other. The estimator is therefore a trimmed mean of the log ratios
# of each stratum (a workload), averaged with equal weight over the strata of a cell. Nothing is
# dropped from the data: the mean, the median and the extremes stay available beside it.
#
# Two intervals bracket the estimate and the verdict uses the wider of the two at every end:
#   - Yuen's analytic interval for a trimmed mean (winsorized variance, Satterthwaite degrees of
#     freedom over the strata);
#   - a stratified percentile bootstrap of the same estimator, seeded from the cell name so the
#     output is reproducible.
# Serial correlation between consecutive rounds, when it is clearly positive, widens both.
# "win" or "loss" also needs an exact sign test over every pair of the cell and a minimum number of
# pairs in every stratum; otherwise the verdict is "parity" (the data do not separate the toolchains)
# or "insufficient" (too few pairs to judge).
# ---------------------------------------------------------------------------------------------
import hashlib
import random

TRIM = 0.25
LEVEL = 0.95
BOOTSTRAP_REPS = 4000
SIGN_ALPHA = 0.05


def _betacf(a, b, x):
    """Continued fraction of the incomplete beta function (modified Lentz)."""
    tiny = 1e-300
    qab, qap, qam = a + b, a + 1.0, a - 1.0
    c, d = 1.0, 1.0 - qab * x / qap
    d = 1.0 / (d if abs(d) > tiny else tiny)
    h = d
    for m in range(1, 400):
        m2 = 2 * m
        step = m * (b - m) * x / ((qam + m2) * (a + m2))
        d = 1.0 + step * d
        d = 1.0 / (d if abs(d) > tiny else tiny)
        c = 1.0 + step / c
        c = c if abs(c) > tiny else tiny
        h *= d * c
        step = -(a + m) * (qab + m) * x / ((a + m2) * (qap + m2))
        d = 1.0 + step * d
        d = 1.0 / (d if abs(d) > tiny else tiny)
        c = 1.0 + step / c
        c = c if abs(c) > tiny else tiny
        delta = d * c
        h *= delta
        if abs(delta - 1.0) < 1e-15:
            break
    return h


def _regularized_beta(a, b, x):
    if x <= 0.0:
        return 0.0
    if x >= 1.0:
        return 1.0
    front = math.exp(math.lgamma(a + b) - math.lgamma(a) - math.lgamma(b) + a * math.log(x) + b * math.log1p(-x))
    if x < (a + 1.0) / (a + b + 2.0):
        return front * _betacf(a, b, x) / a
    return 1.0 - front * _betacf(b, a, 1.0 - x) / b


def t_cdf(value, df):
    """Cumulative distribution of Student's t with df degrees of freedom."""
    x = df / (df + value * value)
    tail = 0.5 * _regularized_beta(df / 2.0, 0.5, x)
    return 1.0 - tail if value > 0 else tail


def t_quantile(probability, df):
    """Inverse of t_cdf, by bisection."""
    if not 0.0 < probability < 1.0:
        raise ValueError("probability must be strictly between 0 and 1")
    if probability == 0.5:
        return 0.0
    if probability < 0.5:
        return -t_quantile(1.0 - probability, df)
    low, high = 0.0, 1.0
    while t_cdf(high, df) < probability:
        high *= 2.0
        if high > 1e12:
            break
    for _ in range(200):
        middle = (low + high) / 2.0
        if t_cdf(middle, df) < probability:
            low = middle
        else:
            high = middle
    return (low + high) / 2.0


def trimmed_mean(values, trim=TRIM):
    """Mean after dropping int(trim * n) values from each end; the plain mean when n is too small."""
    ordered = sorted(values)
    cut = int(trim * len(ordered))
    kept = ordered[cut:len(ordered) - cut]
    return math.fsum(kept) / len(kept)


def winsorized_variance(values, trim=TRIM):
    """Sample variance after pulling the int(trim * n) values at each end in to the nearest kept one."""
    ordered = sorted(values)
    n = len(ordered)
    cut = int(trim * n)
    if cut:
        ordered[:cut] = [ordered[cut]] * cut
        ordered[n - cut:] = [ordered[n - cut - 1]] * cut
    mean = math.fsum(ordered) / n
    return math.fsum((value - mean) ** 2 for value in ordered) / (n - 1)


def sign_test(values):
    """Exact two-sided sign test of a zero median. Returns (below, above, p); ties are not counted."""
    below = sum(1 for value in values if value < 0)
    above = sum(1 for value in values if value > 0)
    n = below + above
    if n == 0:
        return below, above, 1.0
    extreme = min(below, above)
    tail = sum(math.comb(n, k) for k in range(extreme + 1)) / 2.0 ** n
    return below, above, min(1.0, 2.0 * tail)


def average_ranks(series):
    """Ranks of a series in its own order, ties sharing the average rank; None below four values."""
    n = len(series)
    if n < 4:
        return None
    order = sorted(range(n), key=lambda index: series[index])
    ranks = [0.0] * n
    position = 0
    while position < n:
        end = position
        while end + 1 < n and series[order[end + 1]] == series[order[position]]:
            end += 1
        for index in range(position, end + 1):
            ranks[order[index]] = (position + end) / 2.0
        position = end + 1
    return ranks


def pooled_lag1(strata_series):
    """Lag-1 autocorrelation of ranks pooled over strata, with the number of lagged pairs, or (None, 0)."""
    numerator = denominator = 0.0
    count = 0
    for series in strata_series:
        ranks = average_ranks(series)
        if ranks is None:
            continue
        mean = math.fsum(ranks) / len(ranks)
        numerator += math.fsum((ranks[i] - mean) * (ranks[i + 1] - mean) for i in range(len(ranks) - 1))
        denominator += math.fsum((rank - mean) ** 2 for rank in ranks)
        count += len(ranks) - 1
    if not count or denominator == 0.0:
        return None, 0
    return numerator / denominator, count


def seed_for(key):
    return int(hashlib.sha256(str(key).encode()).hexdigest()[:16], 16)


def yuen_interval(strata, trim=TRIM, level=LEVEL):
    """Yuen interval of the mean over strata of each stratum's trimmed mean. strata is a list of lists of
    log ratios. Returns (estimate, standard error, degrees of freedom, low, high); the interval is the
    point itself when no stratum has two kept values."""
    count = len(strata)
    estimate = math.fsum(trimmed_mean(stratum, trim) for stratum in strata) / count
    parts, dfs = [], []
    for stratum in strata:
        n = len(stratum)
        if n < 4:
            return estimate, None, None, None, None
        kept = n - 2 * int(trim * n)
        scale = (1.0 - 2.0 * trim) ** 2
        parts.append(winsorized_variance(stratum, trim) / (scale * n))
        dfs.append(kept - 1)
    variance = math.fsum(parts) / count ** 2
    if variance <= 0.0:
        return estimate, 0.0, float(sum(dfs)), estimate, estimate
    denominator = math.fsum(part * part / df for part, df in zip(parts, dfs))
    df = (math.fsum(parts) ** 2) / denominator
    quantile = t_quantile(0.5 + level / 2.0, df)
    error = math.sqrt(variance)
    return estimate, error, df, estimate - quantile * error, estimate + quantile * error


def bootstrap_interval(strata, key, trim=TRIM, level=LEVEL, reps=BOOTSTRAP_REPS):
    """Stratified percentile bootstrap of the estimator; pairs are resampled within their stratum."""
    rng = random.Random(seed_for(key))
    sizes = [len(stratum) for stratum in strata]
    estimates = []
    for _ in range(reps):
        total = 0.0
        for stratum, n in zip(strata, sizes):
            total += trimmed_mean(rng.choices(stratum, k=n), trim)
        estimates.append(total / len(strata))
    estimates.sort()
    tail = (1.0 - level) / 2.0
    low = estimates[int(math.floor(tail * (reps - 1)))]
    high = estimates[int(math.ceil((1.0 - tail) * (reps - 1)))]
    return low, high


def required_pairs(estimate, error, df, current, level=LEVEL):
    """Pairs per stratum that would be needed, with the same spread, for the interval around the
    current estimate to exclude 0 (None when the estimate is at 0) and for it to be within plus or
    minus 1 percent. Both scale the current pairs per stratum, so they are estimates, not promises."""
    if error is None or df is None or error <= 0.0:
        return None, None
    quantile = t_quantile(0.5 + level / 2.0, df)
    scale = (quantile * error) ** 2
    exclude = None if abs(estimate) < 1e-9 else math.ceil(current * scale / estimate ** 2)
    one_percent = math.ceil(current * scale / math.log(1.01) ** 2)
    return exclude, one_percent


def analyze_cell(strata, key, min_pairs, trim=TRIM, level=LEVEL, reps=BOOTSTRAP_REPS, order=None):
    """Full analysis of one cell. strata maps a stratum name to its log ratios in round order. order
    optionally maps the stratum name to a parallel list: True when the official build ran first. The
    result's order_effect is the factor by which the ratio is higher when fRust goes second than when it
    goes first, halved (1.0 means the order does not matter; a balanced design cancels it in the estimate).

    Returns a dict. Ratios and bounds are ratios (exp of log values); verdict is one of win, loss,
    parity, insufficient."""
    names = sorted(strata)
    series = [list(strata[name]) for name in names]
    flat = [value for part in series for value in part]
    smallest = min(len(part) for part in series)
    result = dict(strata=names, pairs=len(flat), pairs_per_stratum={name: len(strata[name]) for name in names},
                  min_pairs=min_pairs)
    if smallest == 0:
        return dict(result, verdict="insufficient", reason="a stratum has no pairs")
    estimate, error, df, y_low, y_high = yuen_interval(series, trim, level)
    mean_log = math.fsum(math.fsum(part) / len(part) for part in series) / len(series)
    median_log = math.fsum(statistics_median(part) for part in series) / len(series)
    result.update(ratio=math.exp(estimate), mean_ratio=math.exp(mean_log), median_ratio=math.exp(median_log),
                  ratio_min=math.exp(min(flat)), ratio_max=math.exp(max(flat)))
    below, above, sign_p = sign_test(flat)
    result.update(faster=below, slower=above, sign_p=sign_p)
    if error is None:
        return dict(result, verdict="insufficient", reason=f"fewer than 4 pairs in a stratum (need {min_pairs})",
                    interval=None)
    b_low, b_high = bootstrap_interval(series, key, trim, level, reps)
    low, high = min(y_low, b_low), max(y_high, b_high)
    lag, lag_pairs = pooled_lag1(series)
    inflation = 1.0
    if lag is not None and lag_pairs and lag > 2.0 / math.sqrt(lag_pairs):
        inflation = (1.0 + lag) / (1.0 - min(lag, 0.9))
        widen = math.sqrt(inflation)
        low, high = estimate - widen * (estimate - low), estimate + widen * (high - estimate)
    exclude, one_percent = required_pairs(estimate, error, df, smallest, level)
    result.update(interval=(math.exp(low), math.exp(high)), yuen=(math.exp(y_low), math.exp(y_high)),
                  bootstrap=(math.exp(b_low), math.exp(b_high)), df=df, lag1=lag, serial_inflation=inflation,
                  pairs_needed_to_exclude_parity=exclude, pairs_needed_for_1pct=one_percent)
    if order:
        effects = []
        for name in names:
            flags = order.get(name) or []
            first = [v for v, official_first in zip(strata[name], flags) if official_first]
            second = [v for v, official_first in zip(strata[name], flags) if not official_first]
            if len(first) >= 2 and len(second) >= 2:
                effects.append((trimmed_mean(first, trim) - trimmed_mean(second, trim)) / 2.0)
        if effects:
            result["order_effect"] = math.exp(math.fsum(effects) / len(effects))
    if smallest < min_pairs:
        return dict(result, verdict="insufficient", reason=f"{smallest} pairs in the smallest stratum, minimum {min_pairs}")
    consistent_win = below > above and sign_p < SIGN_ALPHA
    consistent_loss = above > below and sign_p < SIGN_ALPHA
    if high < 0.0 and consistent_win:
        verdict = "win"
    elif low > 0.0 and consistent_loss:
        verdict = "loss"
    else:
        verdict = "parity"
    return dict(result, verdict=verdict)


def statistics_median(values):
    ordered = sorted(values)
    n = len(ordered)
    middle = n // 2
    return ordered[middle] if n % 2 else (ordered[middle - 1] + ordered[middle]) / 2.0
