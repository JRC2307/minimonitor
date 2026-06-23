/// A sink for snapshots. The real HTTP-to-hub sink is intentionally deferred
/// until the Beszel build-vs-buy spike resolves (see the refactor spec §1).
#[allow(dead_code)]
pub trait Sink {
    fn send(&self, _snapshot_json: &str) {}
}

/// Default no-op sink: serve-only, no hub push.
#[allow(dead_code)]
pub struct NoopSink;
#[allow(dead_code)]
impl Sink for NoopSink {}
