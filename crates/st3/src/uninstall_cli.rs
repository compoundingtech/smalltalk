use super::*;
// The same print macro as main.rs: CLI output goes through st3's writer.
use st3::cli_println as println;

fn directory_is_empty(path: &Path) -> bool {
    fs::read_dir(path).is_ok_and(|mut entries| entries.next().is_none())
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
    let binaries = manifest
        .as_ref()
        .and_then(|manifest| manifest["files"].as_array())
        .into_iter()
        .flatten()
        .filter_map(|path| path.as_str().map(PathBuf::from))
        .collect::<Vec<_>>();
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
        let result = if path.is_dir() {
            fs::remove_dir_all(path)
        } else {
            fs::remove_file(path)
        };
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
