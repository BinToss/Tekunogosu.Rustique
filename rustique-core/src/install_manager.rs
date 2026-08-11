use crate::aliases::{DownloadURL, ModID, ModName, ModVersion, PinnedVersionInfo};
use crate::api::api_structs::{Mod, ModInfo};
use crate::api::client::{ApiClient};
use crate::api::download::download_requested_mods;
use crate::rustique_errors::RustiqueError;
use crate::utils::{combine_version_reqs, extract_zip_metadata, has_semver_operator, split_modid_version};
use crate::version_management::{parse_pinned_version, parse_version};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::PathBuf;
use comfy_table::{Attribute, Color};
use futures::stream::{self, StreamExt};
use indicatif::MultiProgress;
use tracing::{debug, error, info};
use crate::config::config_manager::{with_config, Package};
use crate::consts::FILE_MODINFO_JSON;
use crate::information_utils::notice;
use crate::sync_structs::ModSyncInfo;
use crate::traits::ref_ext::PathRef;
use crate::traits::string_ext::StrLowerExt;

// install & update both will obtain the info needed to fill this struct
#[derive(Debug, Clone, Default)]
pub struct Install {
    pub mod_id: ModID,
    pub mod_name: ModName,
    // Used with version pinning, otherwise ignored
    pub version_to_install: ModVersion,
    // download url of the version_to_install
    pub download_url: DownloadURL,
    // will be None if this is to be a fresh install
    pub current_file_path: Option<PathBuf>,
}


#[derive(Debug, Clone)]
pub struct Installed {
    pub mod_id: ModID,
    pub mod_name: ModName,
    pub installed_file_path: Option<PathBuf>,
    // will be None if this was a fresh install and not an update
    pub old_file_path: Option<PathBuf>,
    pub install_version: ModVersion,
    pub success: bool,
}

impl Default for Installed {
    fn default() -> Self {
        Self::new()
    }
}

impl Installed {
    pub fn new() -> Self {
        Self {
            mod_id: String::new(),
            mod_name: String::new(),
            installed_file_path: None,
            old_file_path: None,
            install_version: String::new(),
            success: false,
        }
    }
}


#[derive(Debug, Clone)]
pub struct Requester {
    pub mod_id: ModID,
    pub required_version: ModVersion,
}

#[derive(Debug, Clone)]
pub struct ResolvedDep {
    pub mod_id: ModID,
    pub mod_name: ModName,
    pub version_to_install: ModVersion,
    pub download_url: DownloadURL,
    pub requesters: Vec<Requester>,
}

pub type DependencyGraph = HashMap<ModID, ResolvedDep>;

/// Turns a dependency's declared version into something we can resolve against.
///
/// Authors declare a dep like 2.3.1 meaning "this is what I built against". A different patch of
/// the same major.minor is compatible, a different minor usually isn't, so we ask for ~2.3.1.
/// If they already wrote an operator we take them at their word, and anything unparseable or a
/// wildcard means they never really pinned it so we leave it open.
fn dependency_requirement(required_version: &str) -> Option<ModVersion> {
    let required = required_version.trim();

    if required.is_empty() || required == "*" {
        return None;
    }

    if has_semver_operator(required) {
        return Some(required.to_string());
    }

    match parse_version(required) {
        Ok(v) => Some(format!("~{}.{}.{}", v.major, v.minor, v.patch)),
        Err(_) => {
            debug!("Can't parse dependency version {required}, leaving it unconstrained");
            None
        }
    }
}

/// Resolves one dependency against a single set of conditions. Pulled out so the conflict
/// fallback below can run the exact same resolution with a different condition.
fn resolve_dep_version(
    api_mod: &Mod,
    mod_id: &str,
    pinned_version: Option<ModVersion>,
    pinned_game_version: &str,
    allow_unstable: bool,
) -> Result<PinnedVersionInfo, RustiqueError> {
    parse_pinned_version(
        &api_mod.mod_json.releases,
        &Package { mod_id: mod_id.to_string(), pinned_version },
        pinned_game_version,
        allow_unstable,
    )
}

pub async fn resolve_dependencies(
    mod_dir: &std::path::Path,
    initial_mods: Vec<Install>,
    installed_mods: &BTreeMap<ModID, ModSyncInfo>,
    client: &ApiClient,
    mp: &MultiProgress,
    ignore_dependencies: bool,
) -> Result<(DependencyGraph, Vec<Installed>), RustiqueError> {
    // grab what we need up front. Holding the guard across every download and api call below
    // would block any writer for the whole run and deadlock us if one ever queues up
    let (pkgs, pinned_game_version, allow_unstable) = with_config(|c| {
        (c.pkg.clone(), c.pinned_game_version.clone(), c.allow_unstable)
    }).await;

    let mut graph: DependencyGraph = HashMap::new();
    let mut queue: VecDeque<Install> = VecDeque::new();
    let mut all_installed: Vec<Installed> = Vec::new();

    // seed the queue with what was actually asked for BEFORE seeding the installed mods.
    // These download no matter what, even if they are already installed, otherwise update
    // has nothing left to do by the time it gets here
    for install in initial_mods {
        let key = install.mod_id.to_lowercase();
        if let std::collections::hash_map::Entry::Vacant(e) = graph.entry(key) {
            e.insert(ResolvedDep {
                mod_id: install.mod_id.clone(),
                mod_name: install.mod_name.clone(),
                version_to_install: install.version_to_install.clone(),
                download_url: install.download_url.clone(),
                requesters: vec![Requester {
                    mod_id: String::from("user"),
                    required_version: install.version_to_install.clone(),
                }],
            });
            queue.push_back(install);
        }
    }

    // seed graph with everything else already installed so BFS skips them.
    // A requested mod claimed its key above, don't clobber the version we're installing
    for (mod_id, sync_info) in installed_mods {
        let (mod_id, _) = split_modid_version(mod_id);
        let key = mod_id.to_lowercase();
        if graph.contains_key(&key) {
            continue;
        }

        graph.insert(key, ResolvedDep {
            mod_id: mod_id.clone(),
            mod_name: sync_info.mod_name.clone(),
            version_to_install: sync_info.installed_version.clone(),
            download_url: String::new(),
            requesters: vec![Requester {
                mod_id: String::from("installed"),
                required_version: sync_info.installed_version.clone(),
            }],
        });
    }


    let concurrent_limit = num_cpus::get();

    while !queue.is_empty() {
        let mut batch: Vec<Install> = queue.drain(..).collect();

        let recently_installed = download_requested_mods(mod_dir, &mut batch, client, Some(mp))
            .await
            .unwrap_or_else(|err| {
            error!("Failed to install batch: {:?}", err);
            Vec::new()
        });
        all_installed.extend(recently_installed.clone());

        // -i means install exactly what was asked for, don't go looking for what it needs
        if ignore_dependencies {
            break;
        }

        // read modinfo.json from each downloaded mod to discover dependencies
        #[allow(clippy::redundant_closure)]
        let dep_maps: Vec<(ModID, HashMap<String, String>)> = stream::iter(recently_installed.iter())
            .map(|installed_mod| {
                async move {
                    let path = installed_mod.installed_file_path.clone()?;
                    match extract_zip_metadata::<ModInfo>(&path, FILE_MODINFO_JSON).await {
                        Ok(mod_info) => {
                            let deps: HashMap<_, _> = mod_info.dependencies
                                .into_iter()
                                .filter(|(dep_id, _)| {
                                    !dep_id.lower_eq("game")
                                        && !dep_id.lower_eq("creative")
                                        && !dep_id.lower_eq("survival")
                                })
                                .collect();
                            if deps.is_empty() { None } else { Some((installed_mod.mod_id.clone(), deps)) }
                        }
                        Err(err) => {
                            error!("Failed to extract zip metadata: {:?}", err);
                            None
                        }
                    }
                }
            })
            .buffer_unordered(concurrent_limit)
            .filter_map(|res| futures::future::ready(res))
            .collect()
            .await;

        // group every requirement under the dep it belongs to. Collecting straight into a map
        // keyed by dep would drop all but one of them, and which one survived came down to
        // whichever download happened to finish last
        // filter already-seen mods (those in graph) before collecting
        let mut new_deps: HashMap<ModID, Vec<Requester>> = HashMap::new();
        for (requester_id, deps) in dep_maps {
            for (dep_id, required_version) in deps {
                if graph.contains_key(&dep_id.to_lowercase()) {
                    continue;
                }

                new_deps.entry(dep_id).or_default().push(Requester {
                    mod_id: requester_id.clone(),
                    required_version,
                });
            }
        }

        if new_deps.is_empty() {
            continue;
        }

        let mod_ids: Vec<ModID> = new_deps.keys().cloned().collect();
        let api_results: HashMap<ModID, Mod> = client.fetch_mods_parallel(mod_ids).await?;

        for (dep_id, requesters) in &new_deps {
            let key = dep_id.to_lowercase();
            if graph.contains_key(&key) {
                continue;
            }

            if let Some(api_mod) = api_results.get(dep_id.as_str()) {
                let mod_name = api_mod.mod_json.name.clone().unwrap_or_default();
                // println!("Mod name {mod_name}");
                // the url alias IS the mod ID in MOST cases. Need a check for validity
                let mod_id = if let Some(mod_alias) = &api_mod.mod_json.url_alias {
                    mod_alias
                } else  {
                    &api_mod.mod_json.mod_id.clone().to_string()
                };

                let pkg = match pkgs.iter().find(|p| p.mod_id.eq(mod_id)) {
                    Some(p) => p.clone(),
                    _ => {Package::default()}
                };

                // every mod that asked for this dep gets a say. If one version keeps them all
                // happy we use that, and the user's config pin has to hold on top of it
                let combined = requesters.iter()
                    .filter_map(|r| dependency_requirement(&r.required_version))
                    .reduce(|acc, req| combine_version_reqs(&acc, &req));

                let wanted = match (&combined, &pkg.pinned_version) {
                    (Some(dep_req), Some(pin)) => Some(combine_version_reqs(dep_req, pin)),
                    (Some(dep_req), None) => Some(dep_req.clone()),
                    (None, pin) => pin.clone(),
                };

                info!("{dep_id}: {} requester(s), resolving against {wanted:?}", requesters.len());

                let resolved = match resolve_dep_version(api_mod, mod_id, wanted, pinned_game_version.as_str(), allow_unstable) {
                    Ok(pv) => Some(pv),
                    // nothing satisfies everyone. The game only ever loads the highest copy of a
                    // mod it finds, so fall back to that rather than installing one it will ignore
                    Err(e) => {
                        let highest = requesters.iter()
                            .filter_map(|r| parse_version(&r.required_version).ok().map(|v| (v, r)))
                            .max_by(|(a, _), (b, _)| a.cmp(b))
                            .map(|(_, r)| r);

                        match highest {
                            Some(winner) if requesters.len() > 1 => {
                                let fallback = match (dependency_requirement(&winner.required_version), &pkg.pinned_version) {
                                    (Some(dep_req), Some(pin)) => Some(combine_version_reqs(&dep_req, pin)),
                                    (Some(dep_req), None) => Some(dep_req),
                                    (None, pin) => pin.clone(),
                                };

                                match resolve_dep_version(api_mod, mod_id, fallback, pinned_game_version.as_str(), allow_unstable) {
                                    Ok(pv) => {
                                        let asked = requesters.iter()
                                            .map(|r| format!("{} wants {}", r.mod_id, r.required_version))
                                            .collect::<Vec<_>>()
                                            .join(", ");

                                        notice(
                                            format!("{dep_id} is requested by multiple mods: ({asked}). Installing highest version needed: {}.", winner.required_version),
                                            Some(Color::Yellow),
                                            vec![Attribute::Bold],
                                        );

                                        Some(pv)
                                    }
                                    Err(e) => {
                                        notice(format!("Unable to locate compatible versions for {dep_id} - {e}"), Some(Color::Red), vec![Attribute::Bold]);
                                        None
                                    }
                                }
                            }
                            _ => {
                                notice(format!("Unable to locate compatible versions for {dep_id} - {e}"), Some(Color::Red), vec![Attribute::Bold]);
                                None
                            }
                        }
                    }
                };

                let Some((version, url, _, _)) = resolved else {
                    continue;
                };

                info!("Resolving deps for {url} {version}");


                // let (version, url, _, _) = if let Some(mod_pkg) = pkg {
                //     // println!("Parse_pinned_version {:?}", mod_pkg);
                //     match parse_pinned_version(&api_mod.mod_json.releases, &mod_pkg.clone(), config.pinned_game_version.as_str(), config.allow_unstable) {
                //         Ok(pv) => pv,
                //         Err(e) => {
                //             notice(format!("Unable to locate compatible versions for {} -- {}", dep_id, e), Some(Color::Red), vec![Attribute::Bold]);
                //             continue;
                //         }
                //     }
                // } else {
                //     // println!("parse_latest_version");
                //     parse_latest_version(&api_mod.mod_json.releases)
                // };

                // println!("Trying to download {} from {}", version, url);

                // insert into graph BEFORE pushing to queue — this prevents a dep discovered
                // by two mods in the same batch from being queued twice
                graph.insert(key, ResolvedDep {
                    mod_id: dep_id.clone(),
                    mod_name: mod_name.clone(),
                    version_to_install: version.clone(),
                    download_url: url.clone(),
                    requesters: requesters.clone(),
                });

                queue.push_back(Install {
                    mod_id: dep_id.clone(),
                    mod_name,
                    version_to_install: version,
                    download_url: url,
                    current_file_path: None,
                });
            }
        }
    }

    Ok((graph, all_installed))
}

pub async fn install_manager(
    mod_dir: impl PathRef,
    mods_requested: Vec<Install>,
    installed_mods: BTreeMap<ModID, ModSyncInfo>,
    ignore_dependencies: bool) -> Result<Vec<Installed>, RustiqueError> {

    info!("Install manager called");

    let mod_dir = mod_dir.as_ref();
    let client = ApiClient::new();
    let mp = MultiProgress::new();

    let (_, mut mods_processed) = resolve_dependencies(mod_dir, mods_requested, &installed_mods, &client, &mp, ignore_dependencies).await?;

    mods_processed.sort_by(|a, b| a.mod_name.to_lowercase().cmp(&b.mod_name.to_lowercase()));

    Ok(mods_processed)
}