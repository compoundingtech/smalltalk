#[test]
fn runtime_start_upgrade_requires_its_assigned_storage_version() {
    assert_eq!(
        st_storage_version::STORAGE_VERSION,
        20,
        "If another incompatible upgrade is queued first, adopt its shared crate and advance this upgrade, its assertions, docs and release transition together"
    );
}
