use super::*;

#[tokio::test(start_paused = true)]
async fn the_reconciler_is_woken_once_when_account_limits_become_ready() {
    let store = Arc::new(Store::open_memory("node").unwrap());
    let notify = Arc::new(Notify::new());
    let task = tokio::spawn(catch_up_account_limits(store.clone(), notify.clone()));
    tokio::time::timeout(Duration::from_secs(5), notify.notified())
        .await
        .expect("the reconciler is woken when the limits projection is ready");
    assert!(store.account_limits_ready().unwrap());
    assert!(
        tokio::time::timeout(Duration::from_secs(300), notify.notified())
            .await
            .is_err(),
        "the wake happens once, not on every idle pass"
    );
    task.abort();
}
