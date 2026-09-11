use std::collections::HashMap;
use comfy_table::{Attribute, Color};
use comfy_table::presets::UTF8_HORIZONTAL_ONLY;
use rustique_core::aliases::{ModID, ModVersion};
use rustique_core::api::client::ApiClient;
use crate::commands::sync::{get_sync_data};
use rustique_core::install_manager::{dependency_requirement, install_manager, resolve_installs, InstallRequest, ResolveFailure, ResolveOptions};
use rustique_core::rustique_errors::RustiqueError;
use rustique_core::utils::{extract_all_mods_metadata, gather_missing_dependencies, split_modid_version};
use tracing::{debug, info};
use rustique_core::config::config_manager::with_config;
use rustique_core::information_utils::{command_output, display_incompatible_mods_constraint, display_installation_results, display_table, notice};
use rustique_core::traits::ref_ext::PathRef;

// Report if trying install a mod that already exists
// Use -f to force an installation
pub async fn install_cmd(mod_dir: impl PathRef, mods_requested: Vec<ModID>, force: bool, ignore_dependencies: bool) -> Result<(), RustiqueError> {
    let mod_dir = mod_dir.as_ref();
    info!("install_cmd: {mods_requested:?}");
    
    display_table(vec![command_output("Installing..", mods_requested.join(", "))], Some(UTF8_HORIZONTAL_ONLY));
    
    // do this first as we need to strip the @ if it exists
    let mod_map: HashMap<ModID, Option<ModVersion>> = mods_requested.iter().map(split_modid_version).collect();
    
    
    // get sync data
    let sync_data = get_sync_data(mod_dir, true).await?;
    
    // only needed to explain a failure. resolve_installs applies the pins itself
    let config_pkgs = with_config(|c| c.pkg.clone()).await;

    let installed_mods = sync_data.rustique_sync.clone();

    // no point burning api calls and a download on something already sitting in the mod dir.
    // Typing modid@version means they know what they want, so let those through untouched
    let mut already_installed: Vec<String> = Vec::new();
    let mod_map: HashMap<ModID, Option<ModVersion>> = if force {
        mod_map
    } else {
        mod_map.into_iter().filter(|(mod_id, requested_version)| {
            if requested_version.is_none() && installed_mods.contains_key(mod_id) {
                already_installed.push(mod_id.clone());
                false
            } else {
                true
            }
        }).collect()
    };

    if !already_installed.is_empty() {
        notice(format!("Already installed, use -f to reinstall: [{}]", already_installed.join("], [")), Some(Color::Yellow), vec![Attribute::Bold]);
    }

    if mod_map.is_empty() {
        return Ok(());
    }

    let client = ApiClient::new();

    // -f means their own pins don't get a vote this run
    let options = ResolveOptions { apply_config_pins: !force, ..ResolveOptions::from_config().await };

    let requests: Vec<InstallRequest> = mod_map.iter()
        .map(|(mod_id, cli_version)| InstallRequest::new(mod_id.clone()).with_version(cli_version.clone()))
        .collect();

    let (mods_requested, failures) = resolve_installs(requests, &client, &options).await?;

    // resolve_installs hands back structured failures so we can say something better than it
    // could. It doesn't know a config pin was in play, we do
    let rows: Vec<String> = failures.iter().map(|failure| {
        let mod_id = failure.mod_id();

        let config_pin = config_pkgs.iter()
            .find(|package| package.mod_id.eq_ignore_ascii_case(mod_id))
            .and_then(|package| package.pinned_version.clone());

        match (matches!(failure, ResolveFailure::NoMatchingVersion(..)), mod_map.get(mod_id).cloned().flatten(), config_pin) {
            (true, Some(cli), Some(pin)) if !force => format!(
                "{mod_id}\npinned to {pin} in your config, which {cli} can't satisfy.\n\
                 Repin with [rustique config set -P <version> -w {mod_id}], clear it with \
                 [rustique config del -P {mod_id}], or use -f to override it just this once."
            ),
            _ => failure.to_row(),
        }
    }).collect();

    if !rows.is_empty() {
        display_incompatible_mods_constraint(rows, "Mods that could not be installed".into());
    }

    if mods_requested.is_empty() {
        // the table above already said which ids failed and why, no need to repeat it
        return Ok(());
    }

    info!("Mods requested {:?}", mods_requested);

    let mods_processed = install_manager(mod_dir, mods_requested, installed_mods, ignore_dependencies).await?;

    display_installation_results(mods_processed);

    Ok(())
}


/// mod_dir_for_req is where the mods_requested will be searched for
/// all dependencies will be installed to dep_install_path
pub async fn install_missing_deps<V: AsRef<[ModID]>>(mod_dir_for_req: impl PathRef, mods_requested: V, dep_install_path: impl PathRef) -> Result<(), RustiqueError> {
    let (mod_dir , mods_requested, dep_install_path) = (mod_dir_for_req.as_ref(), mods_requested.as_ref(), dep_install_path.as_ref());
    // get all installed mod info
    // retrieve all dependencies
    // send missing ones to install_manager()

    let installed_mods = extract_all_mods_metadata(mod_dir, true).await?;
    // both "what's missing" and the resolver seed get judged against where the deps actually
    // land, which isn't the same dir we searched for the requesting mods when modpacks call this.
    // silence the sync message because it happens too much during installation.
    let sync_data = get_sync_data(dep_install_path, true).await?.rustique_sync;

    let mods_map: HashMap<ModID, Option<ModVersion>> = mods_requested.iter().map(split_modid_version).collect();
    let mods_id_vec: Vec<ModID> = mods_map.keys().cloned().collect();
    
    info!("install_missing_deps: mods_id_vec: {:?}", mods_id_vec);

    // if there are reports of slowness is this section .values().par_bridge()...flat_map_iter() could be used to speed it up
    // this is prob not an issue even with a lot of mods as the data is all in memory at this point
    let missing = gather_missing_dependencies(&installed_mods, &mods_id_vec, &sync_data);

    let client = ApiClient::new();

    // whatever version the requesting mod asked for becomes the condition. resolve_installs
    // folds in the config pin and the game pin from there
    let requests: Vec<InstallRequest> = missing.into_iter()
        .map(|dep| InstallRequest::new(dep.mod_id).with_version(dependency_requirement(&dep.version_to_install)))
        .collect();

    let (missing_deps, failures) = resolve_installs(requests, &client, &ResolveOptions::from_config().await).await?;

    if !failures.is_empty() {
        display_incompatible_mods_constraint(
            failures.iter().map(ResolveFailure::to_row).collect(),
            "Could not resolve these dependencies".into(),
        );
    }

    if missing_deps.is_empty() {
        info!("No missing deps to download..");
        return Ok(())
    }

    debug!("deps: {:?}", missing_deps);

    let mods_processed = install_manager(dep_install_path, missing_deps, sync_data, false).await?;


    info!("mods_processed {:#?}", mods_processed);

    display_installation_results(mods_processed);

    Ok(())
}


