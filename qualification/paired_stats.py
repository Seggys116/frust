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
