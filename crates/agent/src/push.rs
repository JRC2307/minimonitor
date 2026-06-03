/// A sink for snapshots. The real HTTP-to-hub sink is intentionally deferred
/// until the Beszel build-vs-buy spike resolves (see the refactor spec §1).
pub trait Sink {
    fn send(&self, _snapshot_json: &str) {}
}

/// Default no-op sink: serve-only, no hub push.
pub struct NoopSink;
impl Sink for NoopSink {}
