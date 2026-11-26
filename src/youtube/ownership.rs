//! Exclusive media ownership tracking for the local playback arbiter.
//!
//! The router combines explicit start commands with the activity watch fed by
//! the two player bridges. Keeping this pure makes it testable without a
//! runtime and prevents a failed/stopped source from being forgotten while a
//! retry is still possible.

use crate::model::Source;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Active {
    source: Source,
    remote: bool,
}

/// Which player currently owns the display and which one may be retried.
#[derive(Debug, Clone, Default)]
pub struct Ownership {
    active: Option<Active>,
    last: Option<Source>,
}

impl Ownership {
    /// Record an explicit start before the child reports activity.
    pub fn starting(&mut self, source: Source, remote: bool) {
        self.active = Some(Active { source, remote });
        self.last = Some(source);
    }

    /// Apply an activity-watch observation.
    ///
    /// A concrete source becomes active even if no start was recorded (for
    /// example after a player's own retry), while preserving the remote flag
    /// of the same source. Idle clears the owner but keeps `last` so the
    /// failed source can still receive an explicit Retry.
    pub fn observe(&mut self, current: Option<Source>) {
        match current {
            Some(source) => {
                let remote = self
                    .active
                    .filter(|active| active.source == source)
                    .is_some_and(|active| active.remote);
                self.active = Some(Active { source, remote });
                self.last = Some(source);
            }
            None => self.active = None,
        }
    }

    /// Record a stop or explicit source release.
    pub fn stopped(&mut self) {
        self.active = None;
    }

    /// Current owner, if any.
    #[must_use]
    pub fn active(&self) -> Option<Source> {
        self.active.map(|active| active.source)
    }

    /// Whether `source` is the current owner.
    #[must_use]
    pub fn active_is(&self, source: Source) -> bool {
        self.active() == Some(source)
    }

    /// Whether the current owner was started by a remote command.
    #[must_use]
    pub fn remote(&self) -> bool {
        self.active.is_some_and(|active| active.remote)
    }

    /// Source that an explicit Retry should target.
    ///
    /// The active source wins; after a failure cleared activity, the last
    /// source is retained so a YouTube failure cannot be retried through the
    /// Jellyfin controller (or the reverse).
    #[must_use]
    pub fn retry_source(&self) -> Option<Source> {
        self.active().or(self.last)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn activity_some_sets_the_active_source_and_retry_target() {
        let mut ownership = Ownership::default();
        assert_eq!(ownership.retry_source(), None);
        ownership.observe(Some(Source::YouTube));
        assert_eq!(ownership.active(), Some(Source::YouTube));
        assert_eq!(ownership.retry_source(), Some(Source::YouTube));
        ownership.observe(Some(Source::Jellyfin));
        assert_eq!(ownership.active(), Some(Source::Jellyfin));
        assert_eq!(ownership.retry_source(), Some(Source::Jellyfin));
    }

    #[test]
    fn a_failed_youtube_playback_retries_youtube_not_jellyfin() {
        let mut ownership = Ownership::default();
        ownership.starting(Source::YouTube, true);
        assert!(ownership.remote());
        ownership.observe(Some(Source::YouTube));
        assert!(ownership.remote(), "remote origin survives activity");
        ownership.observe(None);
        assert_eq!(ownership.active(), None);
        assert_eq!(ownership.retry_source(), Some(Source::YouTube));
        assert!(!ownership.remote());
    }

    #[test]
    fn a_failed_jellyfin_playback_retries_jellyfin() {
        let mut ownership = Ownership::default();
        ownership.starting(Source::Jellyfin, false);
        ownership.observe(None);
        assert_eq!(ownership.active(), None);
        assert_eq!(ownership.retry_source(), Some(Source::Jellyfin));
    }

    #[test]
    fn starting_a_new_source_replaces_the_previous_owner() {
        let mut ownership = Ownership::default();
        ownership.starting(Source::Jellyfin, false);
        ownership.observe(Some(Source::Jellyfin));
        ownership.starting(Source::YouTube, false);
        assert!(ownership.active_is(Source::YouTube));
        assert!(!ownership.active_is(Source::Jellyfin));
        ownership.stopped();
        assert_eq!(ownership.active(), None);
        assert_eq!(ownership.retry_source(), Some(Source::YouTube));
    }
}
