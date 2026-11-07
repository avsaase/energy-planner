import numpy as np
import pytest

from backtest import DEFAULT_LEVELS, change_spread, fit_context, mean_of, seasonal_naive


def test_fit_context_pads_short_history_with_nan_in_front():
    context = fit_context(np.array([1.0, 2.0]), 4)
    assert context.dtype == np.float32
    assert np.isnan(context[:2]).all()
    assert list(context[2:]) == [1.0, 2.0]


def test_fit_context_keeps_most_recent_values():
    assert list(fit_context(np.arange(10.0), 3)) == [7.0, 8.0, 9.0]


def test_mean_of_averages_inner_levels_only():
    levels = [0.01, 0.05, 0.5, 0.95, 0.99]
    quantiles = np.array([[0.0], [10.0], [20.0], [30.0], [1000.0]])
    assert mean_of(levels, quantiles) == pytest.approx([20.0])


def test_seasonal_naive_uses_same_slot_of_previous_days():
    # 4 steps per day (6 h): yesterday [3, 4, 5, 6], the day before [1, 2, 3, 4].
    history = np.array([1.0, 2.0, 3.0, 4.0, 3.0, 4.0, 5.0, 6.0])
    q = seasonal_naive(history, 2, 360)
    assert q.shape == (len(DEFAULT_LEVELS), 2)
    assert mean_of(DEFAULT_LEVELS, q) == pytest.approx([2.0, 3.0])


def test_change_spread_needs_enough_history():
    assert change_spread(np.ones(30), 5, 0.9) == (0.0, 0.0)
    below, above = change_spread(np.tile([0.0, 1000.0], 200), 1, 0.9)
    assert below == pytest.approx(1000.0) and above == pytest.approx(1000.0)
