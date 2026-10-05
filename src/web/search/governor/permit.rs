//! One engine leg's held concurrency permit and its pacing gate (#63, #73).

use tokio::time::Instant;

use super::config::{pick_gap, resolve_gap};
use super::Governor;
use crate::web::search::types::SearchProvider;

/// A held per-engine concurrency permit spanning one entire leg, however
/// many wire requests it turns out to need (DDG pagination, Yahoo's
/// paginated pages). Returned by [`Governor::acquire`]; dropping it frees
/// the permit for the next queued leg on the same engine, once that
/// engine's own concurrency limit (#73) allows it.
pub(crate) struct EnginePermit<'g> {
    governor: &'g Governor,
    provider: SearchProvider,
    _permit: tokio::sync::SemaphorePermit<'g>,
}

/// Result of [`EnginePermit::pace`]: whether the leg may proceed to its
/// first wire request, or was skipped by a state that changed while this
/// leg's `EnginePermit` was still queued behind a sibling leg for the same
/// engine.
#[derive(Debug)]
pub(crate) enum PaceOutcome {
    /// Proceed: the gap has elapsed and the budget has been reserved.
    Proceed,
    /// A sibling leg suspended this engine while this one was queued.
    Suspended { remaining_secs: u64, reason: String },
    /// A sibling leg exhausted the per-run budget while this one was
    /// queued.
    Throttled,
}

impl<'g> EnginePermit<'g> {
    pub(super) fn new(
        governor: &'g Governor,
        provider: SearchProvider,
        permit: tokio::sync::SemaphorePermit<'g>,
    ) -> Self {
        Self {
            governor,
            provider,
            _permit: permit,
        }
    }

    /// Refresh `engines.json` under the cross-process lock (#103: another
    /// process may have suspended this engine, or sent its own request,
    /// since this one last looked), then recheck suspension and the
    /// per-run budget (closing the race where a sibling leg for the *same*
    /// engine settled -- and suspended the engine, or exhausted the budget
    /// -- while this leg was still queued behind it), then wait the
    /// jittered gap since the last request any process dispatched to this
    /// engine, then atomically reserve the budget and record *this* moment
    /// as the new last-request time, persisting both under the lock. At
    /// most `resolve_concurrency(provider)` tasks may be inside this method
    /// at once for a given provider (#73: one per held [`EnginePermit`])
    /// -- for an engine at concurrency 1 that is exactly one, so the
    /// checks below can never race in-process; for concurrency 2
    /// (Bing/Yahoo) two siblings can race here, bounded to at most
    /// `concurrency - 1` requests over budget or under-gapped before the
    /// next check catches up (see [`Governor::over_budget`]'s doc
    /// comment). Call before every wire request of a leg, including DDG's
    /// continuation POSTs (#104). Always [`PaceOutcome::Proceed`], with
    /// zero wait, for a hermetic `Governor`.
    pub(crate) async fn pace(&self) -> PaceOutcome {
        if self.governor.0.hermetic {
            return PaceOutcome::Proceed;
        }
        self.governor.refresh();
        if let Some((remaining_secs, reason)) = self.governor.suspended_remaining(self.provider) {
            return PaceOutcome::Suspended {
                remaining_secs,
                reason,
            };
        }
        if self.governor.over_budget(self.provider) {
            return PaceOutcome::Throttled;
        }
        let (gap_min, gap_max) = resolve_gap(self.provider);
        let gap = pick_gap(gap_min, gap_max);
        let last = {
            let views = self.governor.0.views.lock().expect("governor views lock");
            views.get(&self.provider).and_then(|v| v.last_request)
        };
        if let Some(last) = last {
            let elapsed = Instant::now().saturating_duration_since(last);
            if elapsed < gap {
                tokio::time::sleep(gap - elapsed).await;
            }
        }
        // Gap measured from request *start*, so consecutive starts are
        // always >= gap apart regardless of how long the request itself
        // takes. The budget increment happens here, not after the wire
        // request resolves, so a leg aborted mid-flight by the fan-out
        // deadline still counts against the cap and still advances the
        // pacing clock -- it did send a real request.
        self.governor.sync(|views| {
            let view = views.entry(self.provider).or_default();
            view.last_request = Some(Instant::now());
            view.request_count += 1;
        });
        PaceOutcome::Proceed
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
    use std::time::Duration;

    use super::*;
    use crate::web::search::governor::test_support::EnvGuard;

    #[tokio::test(start_paused = true)]
    async fn acquire_pace_respects_minimum_gap_same_engine() {
        let _guard = EnvGuard::set(&[
            ("SEARCH_PACE_MIN_MS", "2000"),
            ("SEARCH_PACE_MAX_MS", "2000"),
        ]);
        let governor = Governor::new(None);
        {
            let permit = governor.acquire(SearchProvider::DuckDuckGo).await;
            assert!(matches!(permit.pace().await, PaceOutcome::Proceed));
        }

        let start = tokio::time::Instant::now();
        let second = tokio::spawn({
            let governor = governor.clone();
            async move {
                let permit = governor.acquire(SearchProvider::DuckDuckGo).await;
                assert!(matches!(permit.pace().await, PaceOutcome::Proceed));
            }
        });
        // The second call must not resolve before the 2s gap elapses.
        tokio::time::advance(Duration::from_millis(1900)).await;
        assert!(
            !second.is_finished(),
            "second call resolved before the minimum gap"
        );
        tokio::time::advance(Duration::from_millis(200)).await;
        second.await.expect("second call completes");
        assert!(
            tokio::time::Instant::now().saturating_duration_since(start)
                >= Duration::from_millis(2000)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn acquire_different_engines_run_in_parallel() {
        // Proof of real overlap, not just "finishes within the window": an
        // in-flight counter observes both engines' dispatches concurrently
        // in the air at once, which a single shared queue (a same-engine
        // bug) could never produce.
        let _guard = EnvGuard::set(&[
            ("SEARCH_PACE_MIN_MS", "5000"),
            ("SEARCH_PACE_MAX_MS", "5000"),
        ]);
        let governor = Governor::new(None);
        let in_flight = std::sync::Arc::new(AtomicUsize::new(0));
        let max_seen = std::sync::Arc::new(AtomicUsize::new(0));
        let run_leg = |provider: SearchProvider| {
            let governor = governor.clone();
            let in_flight = in_flight.clone();
            let max_seen = max_seen.clone();
            async move {
                let permit = governor.acquire(provider).await;
                assert!(matches!(permit.pace().await, PaceOutcome::Proceed));
                let now = in_flight.fetch_add(1, AtomicOrdering::SeqCst) + 1;
                max_seen.fetch_max(now, AtomicOrdering::SeqCst);
                tokio::time::sleep(Duration::from_millis(10)).await;
                in_flight.fetch_sub(1, AtomicOrdering::SeqCst);
            }
        };
        let ddg = tokio::spawn(run_leg(SearchProvider::DuckDuckGo));
        let sp = tokio::spawn(run_leg(SearchProvider::Startpage));
        tokio::time::advance(Duration::from_millis(20)).await;
        ddg.await.expect("ddg leg completes");
        sp.await.expect("sp leg completes");
        assert_eq!(
            max_seen.load(AtomicOrdering::SeqCst),
            2,
            "both engines must have been in flight at the same instant"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn two_engines_with_different_gaps_pace_differently() {
        // #73 acceptance: "two engines with different gaps pace
        // differently". Pinned to fixed, non-overlapping gaps via env
        // override (rather than relying on the #73 defaults' random
        // draw, whose ranges overlap -- Bing 1000..2000ms vs Brave
        // 1500..4000ms -- and would make this assertion flaky whenever
        // Brave's draw happened to land at or below 2100ms) so a second
        // leg to each engine waits its own engine's minimum
        // deterministically, not the other's.
        let _guard = EnvGuard::set(&[
            ("SEARCH_PACE_BING_MIN_MS", "1000"),
            ("SEARCH_PACE_BING_MAX_MS", "1000"),
            ("SEARCH_PACE_BRAVE_MIN_MS", "4000"),
            ("SEARCH_PACE_BRAVE_MAX_MS", "4000"),
        ]);
        let governor = Governor::new(None);
        {
            let permit = governor.acquire(SearchProvider::Bing).await;
            assert!(matches!(permit.pace().await, PaceOutcome::Proceed));
        }
        {
            let permit = governor.acquire(SearchProvider::Brave).await;
            assert!(matches!(permit.pace().await, PaceOutcome::Proceed));
        }

        let bing_second = tokio::spawn({
            let governor = governor.clone();
            async move {
                let permit = governor.acquire(SearchProvider::Bing).await;
                assert!(matches!(permit.pace().await, PaceOutcome::Proceed));
            }
        });
        let brave_second = tokio::spawn({
            let governor = governor.clone();
            async move {
                let permit = governor.acquire(SearchProvider::Brave).await;
                assert!(matches!(permit.pace().await, PaceOutcome::Proceed));
            }
        });

        // Just past Bing's fixed 1000ms gap: Bing's second leg must have
        // resolved by now, but Brave's fixed 4000ms gap must not have.
        tokio::time::advance(Duration::from_millis(1100)).await;
        assert!(
            bing_second.is_finished(),
            "Bing's fixed 1000ms gap must have already elapsed"
        );
        assert!(
            !brave_second.is_finished(),
            "Brave's fixed 4000ms gap must not have elapsed yet -- \
             if both paced identically this would already be finished"
        );

        tokio::time::advance(Duration::from_millis(3000)).await;
        brave_second.await.expect("brave second leg completes");
    }

    #[tokio::test(start_paused = true)]
    async fn engine_allowed_2_in_flight_overlaps_engine_allowed_1_never_does() {
        // #73 acceptance: "an engine allowed 2 in flight overlaps, while
        // an engine allowed 1 never does". Bing defaults to concurrency 2
        // (measured: zero blocks on the live 2-in-flight probe); Brave
        // defaults to 1 (measured: blocked on that exact probe). Three
        // legs to each engine, holding their permit for an overlapping
        // window, prove the difference via a live in-flight counter --
        // not just "all three eventually finish".
        let _guard = EnvGuard::set(&[("SEARCH_PACE_MIN_MS", "0"), ("SEARCH_PACE_MAX_MS", "0")]);
        let governor = Governor::new(None);

        async fn max_in_flight(
            governor: &Governor,
            provider: SearchProvider,
            legs: usize,
        ) -> usize {
            let in_flight = std::sync::Arc::new(AtomicUsize::new(0));
            let max_seen = std::sync::Arc::new(AtomicUsize::new(0));
            let mut handles = Vec::new();
            for _ in 0..legs {
                let governor = governor.clone();
                let in_flight = in_flight.clone();
                let max_seen = max_seen.clone();
                handles.push(tokio::spawn(async move {
                    let permit = governor.acquire(provider).await;
                    assert!(matches!(permit.pace().await, PaceOutcome::Proceed));
                    let now = in_flight.fetch_add(1, AtomicOrdering::SeqCst) + 1;
                    max_seen.fetch_max(now, AtomicOrdering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    in_flight.fetch_sub(1, AtomicOrdering::SeqCst);
                }));
            }
            tokio::time::advance(Duration::from_millis(50)).await;
            for h in handles {
                h.await.expect("leg completes");
            }
            max_seen.load(AtomicOrdering::SeqCst)
        }

        let bing_max = max_in_flight(&governor, SearchProvider::Bing, 3).await;
        let brave_max = max_in_flight(&governor, SearchProvider::Brave, 3).await;
        assert_eq!(
            bing_max, 2,
            "Bing's #73 default concurrency is 2: 2 of the 3 legs must overlap"
        );
        assert_eq!(
            brave_max, 1,
            "Brave's #73 default concurrency is 1: legs must never overlap"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn over_budget_trips_after_the_configured_cap() {
        let _guard = EnvGuard::set(&[
            ("SEARCH_MAX_REQUESTS_PER_ENGINE", "2"),
            ("SEARCH_PACE_MIN_MS", "0"),
            ("SEARCH_PACE_MAX_MS", "0"),
        ]);
        let governor = Governor::new(None);
        assert!(!governor.over_budget(SearchProvider::DuckDuckGo));
        {
            let permit = governor.acquire(SearchProvider::DuckDuckGo).await;
            assert!(matches!(permit.pace().await, PaceOutcome::Proceed));
        }
        assert!(
            !governor.over_budget(SearchProvider::DuckDuckGo),
            "1 of 2 requests used"
        );
        {
            let permit = governor.acquire(SearchProvider::DuckDuckGo).await;
            assert!(matches!(permit.pace().await, PaceOutcome::Proceed));
        }
        assert!(
            governor.over_budget(SearchProvider::DuckDuckGo),
            "2 of 2 requests used, cap reached"
        );
        assert!(
            !governor.over_budget(SearchProvider::Startpage),
            "the cap is per engine, Startpage is untouched"
        );
    }
}
