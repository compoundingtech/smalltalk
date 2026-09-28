#[test]
fn assumes_the_previous_reconciler_shape() {
    let source = include_str!("../crates/st3/src/reconcile.rs");
    assert!(
        source.contains("let mut unchanged = false;"),
        "latest main changed the reconciler; this stale assumption must fail after CI merges main"
    );
}
