use super::*;

fn names(paths: Vec<PathBuf>) -> Vec<String> {
    let mut names = paths
        .into_iter()
        .map(|path| path.file_name().unwrap().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    names.sort();
    names
}

#[test]
fn an_instance_uninstall_removes_everything_but_the_work_folder() {
    let root = tempfile::tempdir().unwrap();
    for entry in ["bin", "state", "work/agents/site"] {
        fs::create_dir_all(root.path().join(entry)).unwrap();
    }
    fs::write(root.path().join("work/agents/site/notes.txt"), "mine").unwrap();
    let work = root.path().join("work");
    assert_eq!(names(instance_removals(root.path(), &work)), ["bin", "state"]);
}

#[test]
fn an_entry_that_contains_a_nested_work_folder_is_kept_and_the_others_go() {
    let root = tempfile::tempdir().unwrap();
    for entry in ["bin", "data/projects", "state"] {
        fs::create_dir_all(root.path().join(entry)).unwrap();
    }
    let work = root.path().join("data/projects");
    assert_eq!(names(instance_removals(root.path(), &work)), ["bin", "state"]);
}

#[test]
fn a_work_folder_outside_the_instance_removes_every_entry() {
    let root = tempfile::tempdir().unwrap();
    for entry in ["bin", "state", "work"] {
        fs::create_dir_all(root.path().join(entry)).unwrap();
    }
    let elsewhere = tempfile::tempdir().unwrap();
    assert_eq!(
        names(instance_removals(root.path(), elsewhere.path())),
        ["bin", "state", "work"]
    );
}
