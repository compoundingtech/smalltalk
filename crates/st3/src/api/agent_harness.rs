//! Pure public-card eligibility. All inputs must come from the same captured namespace.
use crate::model::CurrentHarnessView;

pub(crate) fn eligible(
    declared_provider: Option<&str>,
    runtime_incarnation: Option<&str>,
    observed: Option<CurrentHarnessView>,
) -> Option<CurrentHarnessView> {
    observed.filter(|harness| {
        runtime_incarnation == Some(harness.incarnation_id.as_str())
            && declared_provider.is_none_or(|provider| harness.driver.as_deref() == Some(provider))
    })
}

pub(crate) fn availability(
    mut observed: Option<CurrentHarnessView>,
    mailbox_fault: Option<&str>,
) -> Option<CurrentHarnessView> {
    if let Some(harness) = observed.as_mut()
        && let Some(reason) = mailbox_fault
        && matches!(harness.state.as_str(), "ready" | "idle" | "working")
        && harness.blocked_on.as_deref() != Some("human")
    {
        harness.state = "indeterminate".into();
        harness.reason = Some(reason.into());
        harness.blocked_on = Some("channel".into());
    }
    observed
}
