use super::*;
// The same print macro as main.rs: CLI output goes through st3's writer.
use st3::cli_println as println;

fn directory_is_empty(path: &Path) -> bool {
    fs::read_dir(path).is_ok_and(|mut entries| entries.next().is_none())
}

fn remove_uninstall_path(path: &Path, binary_aliases: &[&Path]) -> std::io::Result<()> {
    if binary_aliases.contains(&path) && !fractal::owns_alias(path) {
        return Ok(());
    }
    if path.is_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    }
}

pub(super) async fn run_uninstall(endpoint: &Endpoint, args: UninstallArgs) -> Result<()> {
    st3::service::preflight(!args.no_service)?;
    let config = Config::load_unvalidated(None)?;
    let client = Client::new(endpoint.clone());
    let config_dir = Config::default_path()
        .parent()
        .map(Path::to_path_buf)
        .context("the config path has no directory")?;
    let data_home = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")))
        .context("HOME is not set")?;
    let state_home = config
        .state_dir
        .parent()
        .map(Path::to_path_buf)
        .context("the state directory has no parent")?;
    let data_dir = data_home.join("st3");
    let instance = st3::instance::current();
    let manifest: Option<Value> = fs::read(data_dir.join("install.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok());
    let binaries = fractal::installed_binaries(
        manifest
            .as_ref()
            .and_then(|manifest| manifest["files"].as_array())
            .into_iter()
            .flatten()
            .filter_map(|path| path.as_str().map(PathBuf::from))
            .collect::<Vec<_>>(),
    );
    let instance_alias = instance
        .as_ref()
        .map(|instance| instance.bin_dir().join("fractal"));
    let mut binary_aliases = binaries
        .iter()
        .filter(|path| path.file_name() == Some(std::ffi::OsStr::new("fractal")))
        .map(PathBuf::as_path)
        .collect::<Vec<_>>();
    if let Some(alias) = instance_alias.as_deref() {
        binary_aliases.push(alias);
    }
    let work_root = config.work_root().ok();
    let st2_state = state_home.join("st2");
    let st2_only_hooks = fs::read_dir(&st2_state)
        .is_ok_and(|entries| entries.flatten().all(|entry| entry.file_name() == "hooks"));
    let mut paths = vec![
        config.state_dir.clone(),
        config_dir,
        data_dir,
        config.socket.clone(),
        config.client_gateway_socket.clone(),
    ];
    paths.extend(st3::startup::paths(&config.socket));
    paths.push(st3::api::unix_reservation_path(&config.socket));
    paths.push(st3::api::unix_reservation_path(&config.client_gateway_socket));
    if st2_only_hooks {
        paths.push(st2_state);
    }
    if !args.keep_binaries {
        paths.extend(binaries.iter().cloned());
    }
    // What the default install writes outside its own directories: st's Claude plugin
    // marketplace, and the copies of the st skill that seats install into harness directories.
    let skill_copies = st3::skill::untouched_installs();
    let plugin_files = st_drivers::claude_channel::st3_marketplace_root()
        .ok()
        .and_then(|root| root.ancestors().nth(2).map(Path::to_path_buf))
        .filter(|root| root.ends_with("st/plugins") && root.exists());
    paths.extend(skill_copies.iter().cloned());
    paths.extend(plugin_files);
    if let Some(instance) = &instance {
        // An instance owns exactly one directory, and every path above lies inside it. Removing
        // that directory is the whole uninstall, and nothing outside it is listed or touched.
        let root = instance.root().to_path_buf();
        anyhow::ensure!(
            paths.iter().all(|path| path.starts_with(&root)),
            "the instance would remove a path outside {}: {:?}",
            root.display(),
            paths.iter().filter(|path| !path.starts_with(&root)).collect::<Vec<_>>()
        );
        let work = work_root.clone().unwrap_or_else(|| instance.work_dir());
        paths = instance_removals(&root, &work);
    }
    paths.sort();
    paths.dedup();
    if args.dry_run {
        for path in &paths {
            println!("remove\t{}", path.display());
        }
        if let Some(work) = work_root.as_ref().filter(|work| !directory_is_empty(work)) {
            println!(
                "keep\t{}\tthe work folder: what your agents made there stays",
                work.display()
            );
        }
        if let Some(instance) = &instance {
            println!(
                "remove\tthe user services of instance {}, if installed",
                instance.name()
            );
        } else {
            println!("remove\tthe st3 user services, if installed");
            println!("remove\tst's Claude plugin and marketplace registration, if installed");
        }
        if instance.is_none() && manifest.is_none() && !args.keep_binaries {
            println!(
                "keep\tthe st3 executables: no release install manifest; remove them the way you installed them"
            );
        }
        return Ok(());
    }
    anyhow::ensure!(
        args.yes,
        "st uninstall erases this machine's st3 state, settings, and services; run it again with --yes, or --dry-run to list them"
    );
    let daemon_running = client.get::<Value>("/v1/health").await.is_ok();
    if let Some(file) = st3::config::FleetFile::load(&config.state_dir)?
        && file.removed.is_none()
    {
        if args.offline {
            fleet_leave(&client, &config, "person/uninstall", true, true, 0).await?;
        } else {
            anyhow::ensure!(
                daemon_running,
                "start st3 so this machine can leave its fleet first, or pass --offline"
            );
            let person = fleet_person(args.actor.clone(), &config)?;
            fleet_leave(&client, &config, &person, false, false, 600).await?;
        }
    }
    let local_only = config.fleet_id.is_none()
        && !config.state_dir.join("left-fleet.json").exists()
        && !config.state_dir.join("fleet").exists()
        && config.state_dir.join("claims.sqlite3").exists();
    anyhow::ensure!(
        !local_only || args.erase_local_graph,
        "this machine's graph exists nowhere else; pass --erase-local-graph to erase it"
    );
    if !args.no_service && services_installed()? {
        st3::service::stop_owned_runtimes(&config)?;
        st3::service::uninstall()?;
    } else if client.get::<Value>("/v1/health").await.is_ok()
        || st3::startup::read(&config.socket).is_some()
    {
        // Setup starts an instance's daemon without a service when the person declines one, so
        // nothing else would stop it. Only the process that answers on the instance's own socket
        // is stopped; the default install's daemon is still left for its owner to stop.
        let stopped = instance.is_some() && {
            st3::service::stop_owned_runtimes(&config)?;
            st3::service::stop_daemon_at(&config.socket, std::time::Duration::from_secs(20))?
        };
        anyhow::ensure!(
            stopped,
            "stop st3 up and st3 replication-worker, then run st uninstall --yes again"
        );
        println!("stopped\tthe daemon this instance started without a service");
    }
    if instance.is_none()
        && let Err(error) = st_drivers::claude_channel::remove_st3_channel(true)
        // No `claude` on this machine means there is no plugin registration to remove.
        && !error.chain().any(|cause| {
            cause
                .downcast_ref::<std::io::Error>()
                .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound)
        })
    {
        eprintln!("could not unregister st's Claude plugin: {error:#}");
    }
    for path in &paths {
        // Recheck ownership only for identified executable aliases, never state directories.
        let result = remove_uninstall_path(path, &binary_aliases);
        if let Err(error) = result
            && error.kind() != std::io::ErrorKind::NotFound
        {
            eprintln!("could not remove {}: {error}", path.display());
        }
    }
    // The work folder setup created goes only while nothing is in it; `remove_dir` refuses a
    // folder that holds anything.
    if let Some(work) = &work_root {
        let _ = fs::remove_dir(work.join("agents"));
        let _ = fs::remove_dir(work);
    }
    if let Some(instance) = &instance {
        let _ = fs::remove_dir(instance.bin_dir());
        // The directory that holds instances goes with the last one; `remove_dir` leaves it alone
        // while another instance, or anything else, is inside.
        let _ = fs::remove_dir(instance.root());
        if let Some(parent) = instance.root().parent() {
            let _ = fs::remove_dir(parent);
        }
    }
    if let Some(work) = work_root.as_ref().filter(|work| work.exists()) {
        println!(
            "kept\t{}\tthe work folder: what your agents made there stays; remove it yourself if you want it gone",
            work.display()
        );
    }
    let remaining = paths
        .iter()
        .filter(|path| path.exists())
        .collect::<Vec<_>>();
    for path in &remaining {
        println!("remains\t{}", path.display());
    }
    println!(
        "Nothing else to remove needs this user. If you installed the Claude Code policy, remove it as root: st3 claude-channel uninstall-policy"
    );
    anyhow::ensure!(remaining.is_empty(), "some st3 files remain");
    println!("uninstalled");
    Ok(())
}

#[cfg(test)]
mod instance_uninstall_tests {
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

    #[test]
    fn an_instance_uninstall_preserves_a_foreign_fractal_but_removes_its_owned_alias() {
        let root = tempfile::tempdir().unwrap();
        let bin = root.path().join("bin");
        fs::create_dir(&bin).unwrap();
        fs::write(bin.join("st3"), "ours").unwrap();
        let alias = bin.join("fractal");
        std::os::unix::fs::symlink("st3", &alias).unwrap();
        let work = root.path().join("work");
        assert_eq!(names(instance_removals(root.path(), &work)), ["fractal", "st3"]);
        fs::remove_file(&alias).unwrap();
        fs::write(&alias, "foreign").unwrap();
        assert_eq!(names(instance_removals(root.path(), &work)), ["st3"]);
    }

    #[test]
    fn a_state_directory_named_fractal_is_removed_without_alias_ownership_checks() {
        let root = tempfile::tempdir().unwrap();
        let bin = root.path().join("bin");
        fs::create_dir(&bin).unwrap();
        let alias = bin.join("fractal");
        fs::write(&alias, "foreign").unwrap();
        let binary_aliases = [alias.as_path()];
        for parent in ["state", "config", "data"] {
            let directory = root.path().join(parent).join("fractal");
            fs::create_dir_all(&directory).unwrap();
            fs::write(directory.join("owned-state"), "ours").unwrap();
            remove_uninstall_path(&directory, &binary_aliases).unwrap();
            assert!(!directory.exists());
        }
        remove_uninstall_path(&alias, &binary_aliases).unwrap();
        assert_eq!(fs::read_to_string(&alias).unwrap(), "foreign");
        fs::remove_file(&alias).unwrap();
        std::os::unix::fs::symlink("st3", &alias).unwrap();
        remove_uninstall_path(&alias, &binary_aliases).unwrap();
        assert!(fs::symlink_metadata(&alias).is_err());
    }
}
