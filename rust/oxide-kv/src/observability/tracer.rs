//! Tracer hooks (P8 PR 8).
//!
//! Stub interface so future PRs can swap in a real OpenTelemetry
//! implementation without touching every call site. The default
//! [`NoopTracer`] discards every span — there's no runtime cost
//! beyond an `#[inline(always)]` early return.
//!
//! A future PR will replace this with
//! `opentelemetry::trace::TracerProvider` behind the same trait.

/// A trace span. The default implementation has no fields and no
/// methods beyond `drop`. Real OTel spans will hang attributes,
/// events, and status off this handle.
#[must_use = "spans must be kept alive until the traced operation completes"]
pub struct Span {
    _private: (),
}

impl Span {
    /// Construct a fresh noop span. Internal — callers should go
    /// through `Tracer::start_span`.
    fn new() -> Self {
        Self { _private: () }
    }

    /// Mark the span as failed. Noop in the default impl; a real
    /// OTel span will set its status to `Error`.
    pub fn record_error(&mut self, _err: &str) {}
}

impl Drop for Span {
    fn drop(&mut self) {}
}

/// Tracer interface. The default [`NoopTracer`] is a single
/// zero-sized type. Future PRs can add a `OtelTracer` that wraps
/// `opentelemetry::global::tracer("oxide-kv")`.
pub trait Tracer: Send + Sync + 'static {
    /// Start a span for the given operation name. The returned
    /// span should be held until the operation completes, then
    /// dropped (which closes the span).
    fn start_span(&self, name: &str) -> Span;
}

/// Default tracer — every call returns a fresh noop span.
#[derive(Default, Clone, Copy, Debug)]
pub struct NoopTracer;

impl Tracer for NoopTracer {
    #[inline(always)]
    fn start_span(&self, _name: &str) -> Span {
        Span::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn noop_tracer_returns_span() {
        let t = NoopTracer;
        let _span = t.start_span("raft.append_entries");
        // Drop on `_span` is a noop; we just confirm the API
        // compiles and runs.
    }

    #[test]
    fn span_record_error_is_safe() {
        let mut s = Span::new();
        s.record_error("simulated failure");
        // No assertions — the test only pins that the call site
        // doesn't panic.
    }

    #[test]
    fn trait_is_object_safe() {
        // Compile-time check: `dyn Tracer` must be usable. This
        // test is here so a future refactor that adds a generic
        // method on `Tracer` fails to compile rather than slipping
        // through and breaking `Arc<dyn Tracer>` call sites.
        let t: Box<dyn Tracer> = Box::new(NoopTracer);
        let _ = t.start_span("raft.become_leader");
    }
}
