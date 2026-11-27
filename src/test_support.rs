/// Shared scoped warn-log capture subscriber for tests. `with_default`
/// installs a thread-local subscriber without mutating the process-global one.
#[derive(Clone, Default)]
pub(crate) struct WarnLog(pub(crate) std::sync::Arc<std::sync::Mutex<Vec<String>>>);

impl tracing::Subscriber for WarnLog {
    fn register_callsite(
        &self,
        _: &'static tracing::Metadata<'static>,
    ) -> tracing::subscriber::Interest {
        tracing::subscriber::Interest::sometimes()
    }

    fn enabled(&self, meta: &tracing::Metadata<'_>) -> bool {
        meta.level() <= &tracing::Level::WARN
    }

    fn event(&self, event: &tracing::Event<'_>) {
        struct Message(String);
        impl tracing::field::Visit for Message {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                if field.name() == "message" {
                    self.0 = format!("{value:?}");
                }
            }
        }
        let mut msg = Message(String::new());
        event.record(&mut msg);
        self.0.lock().unwrap().push(msg.0);
    }

    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
}

/// Serializes the one-time process-global subscriber install (the
/// `transport::http` capture test's `set_global_default`, which rebuilds
/// callsite interest) against every scoped `with_default` capture window, so
/// the global swap can never overlap an active scoped subscriber on another
/// thread. Test-only; production code never touches this.
pub(crate) static TRACING_SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Run `f` under a thread-local `WarnLog` subscriber, serialized against the
/// global-subscriber install (see [`TRACING_SERIAL`]). Drop-in replacement for
/// `tracing::subscriber::with_default(log, f)` in warn-capture tests.
pub(crate) fn with_warn_default<T>(log: WarnLog, f: impl FnOnce() -> T) -> T {
    let _serial = TRACING_SERIAL.lock().unwrap();
    tracing::subscriber::with_default(log, f)
}
