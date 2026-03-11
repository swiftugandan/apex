use apex_core::domain::HookEvent;
use apex_core::ports::HookRegistry;

/// Dispatch a log event through hooks while always printing the fallback message.
/// The `ctx_fn` closure is only called if OnLog hooks actually exist, avoiding
/// unnecessary JSON allocation on the hot path.
pub async fn dispatch_log(
    hooks: Option<&dyn HookRegistry>,
    ctx_fn: impl FnOnce() -> serde_json::Value,
    fallback: &str,
) {
    eprintln!("{fallback}");
    if let Some(h) = hooks {
        if h.has_hooks_for(HookEvent::OnLog) {
            let ctx = ctx_fn();
            let _ = h.dispatch(HookEvent::OnLog, &ctx).await;
        }
    }
}
