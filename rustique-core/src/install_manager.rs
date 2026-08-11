use crate::aliases::{DownloadURL, ModID, ModName, ModVersion, PinnedVersionInfo};
use crate::api::api_structs::{Mod, ModInfo};
use crate::api::client::{ApiClient};
use crate::api::download::download_requested_mods;
use crate::rustique_errors::RustiqueError;
use crate::utils::{combine_version_reqs, extract_zip_metadata, has_semver_operator, is_base_game_dep, split_modid_version};
use crate::version_management::{parse_pinned_version, parse_version};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::PathBuf;
use comfy_table::{Attribute, Color};
use futures::stream::{self, StreamExt};
use indicatif::MultiProgress;
use tracing::{debug, error, info};
use crate::config::config_manager::{with_config, Package};
use crate::consts::FILE_MODINFO_JSON;
use crate::information_utils::{display_incompatible_mods_constraint, notice};
use crate::sync_structs::ModSyncInfo;
use crate::traits::ref_ext::PathRef;

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
pub fn dependency_requirement(required_version: &str) -> Option<ModVersion> {
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

/// What a caller wants installed: an id, and optionally a condition the version has to meet.
#[derive(Debug, Clone)]
pub struct InstallRequest {
    pub mod_id: ModID,
    /// A VersionReq string. None means whatever the config pin and game pin allow
    pub version_req: Option<ModVersion>,
    /// Only tried when version_req matches nothing. Modpacks name an exact build and want the
    /// nearest patch of it when an author pulls that release
    pub fallback_req: Option<ModVersion>,
}

impl InstallRequest {
    pub fn new(mod_id: impl Into<ModID>) -> Self {
        Self { mod_id: mod_id.into(), version_req: None, fallback_req: None }
    }

    #[must_use]
    pub fn with_version(mut self, version_req: Option<ModVersion>) -> Self {
        self.version_req = version_req;
        self
    }

    #[must_use]
    pub fn with_fallback(mut self, fallback_req: Option<ModVersion>) -> Self {
        self.fallback_req = fallback_req;
        self
    }
}

/// Why a requested mod didn't make it into the install list. Kept structured rather than
/// pre-formatted so callers that know more can say more, install knows when a config pin is
/// the likely culprit and can tell the user how to change it.
#[derive(Debug, Clone)]
pub enum ResolveFailure {
    /// The api gave us nothing at all for this id
    NotFound(ModID),
    /// The mod exists, but nothing it publishes satisfies the conditions
    NoMatchingVersion(ModID, String),
}

impl ResolveFailure {
    pub fn mod_id(&self) -> &str {
        match self {
            Self::NotFound(mod_id) | Self::NoMatchingVersion(mod_id, _) => mod_id,
        }
    }

    /// "\<mod id\>\n\<why\>", the shape `display_incompatible_mods_constraint` renders.
    pub fn to_row(&self) -> String {
        match self {
            Self::NotFound(mod_id) =>
                format!("{mod_id}\nNot found on the mod site. It may have been renamed, removed, or made private."),
            Self::NoMatchingVersion(mod_id, reason) => format!("{mod_id}\n{reason}"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ResolveOptions {
    pub pinned_game_version: String,
    pub allow_unstable: bool,
    /// false when the user forced past their own pins, or for modpacks, which carry the exact
    /// versions they were built against and shouldn't be second guessed by a global pin
    pub apply_config_pins: bool,
}

impl ResolveOptions {
    /// The usual case, whatever the user has configured.
    pub async fn from_config() -> Self {
        let (pinned_game_version, allow_unstable) =
            with_config(|c| (c.pinned_game_version.clone(), c.allow_unstable)).await;

        Self { pinned_game_version, allow_unstable, apply_config_pins: true }
    }
}

/// ANDs the caller's condition together with the user's config pin. Either side may be absent.
fn merge_condition(requested: Option<&str>, config_pin: Option<&str>) -> Option<ModVersion> {
    match (requested, config_pin) {
        (Some(a), Some(b)) => Some(combine_version_reqs(a, b)),
        (Some(a), None) => Some(a.to_string()),
        (None, b) => b.map(str::to_string),
    }
}

/// The single road from "these mod ids" to "these downloads".
///
/// Base game filtering, the api lookup, config pin merging, version selection and failure
/// collection all happen here and only here. Everything that installs mods goes through this so
/// a fix in one place is a fix everywhere, which is exactly what wasn't true before.
///
/// Hands back what it could resolve plus why the rest didn't make it. Nothing is fatal, one bad
/// mod never stops the others.
pub async fn resolve_installs(
    requests: Vec<InstallRequest>,
    client: &ApiClient,
    options: &ResolveOptions,
) -> Result<(Vec<Install>, Vec<ResolveFailure>), RustiqueError> {
    // the base game isn't something anybody can download, asking the api for it just 404s
    let requests: Vec<InstallRequest> = requests
        .into_iter()
        .filter(|request| !is_base_game_dep(&request.mod_id))
        .collect();

    if requests.is_empty() {
        return Ok((Vec::new(), Vec::new()));
    }

    // several mods can ask for the same dependency at once, and at different versions. Merge
    // them so every condition has to hold. Left separate they each resolve on their own, both
    // land in the list, and which one actually gets installed comes down to hashmap ordering
    let mut merged: HashMap<ModID, InstallRequest> = HashMap::new();

    for request in requests {
        match merged.entry(request.mod_id.to_lowercase()) {
            std::collections::hash_map::Entry::Vacant(e) => {
                e.insert(request);
            }
            std::collections::hash_map::Entry::Occupied(mut e) => {
                let existing = e.get_mut();
                existing.version_req = merge_condition(existing.version_req.as_deref(), request.version_req.as_deref());
                existing.fallback_req = existing.fallback_req.take().or(request.fallback_req);
            }
        }
    }

    let requests: Vec<InstallRequest> = merged.into_values().collect();

    let pkgs = if options.apply_config_pins {
        with_config(|c| c.pkg.clone()).await
    } else {
        Vec::new()
    };

    let mod_ids: Vec<ModID> = requests.iter().map(|r| r.mod_id.clone()).collect();
    let api_results: HashMap<ModID, Mod> = client.fetch_mods_parallel(mod_ids).await?;

    let mut installs: Vec<Install> = Vec::with_capacity(requests.len());
    let mut failures: Vec<ResolveFailure> = Vec::new();

    for request in requests {
        let Some(api_mod) = api_results.get(&request.mod_id) else {
            failures.push(ResolveFailure::NotFound(request.mod_id));
            continue;
        };

        let config_pin = pkgs.iter()
            .find(|p| p.mod_id.eq_ignore_ascii_case(&request.mod_id))
            .and_then(|p| p.pinned_version.clone());

        let wanted = merge_condition(request.version_req.as_deref(), config_pin.as_deref());

        let resolved = match resolve_dep_version(api_mod, &request.mod_id, wanted, &options.pinned_game_version, options.allow_unstable) {
            Ok(pv) => Some(pv),
            Err(e) => {
                // the exact build they named may simply be gone, so try what they'll settle for
                let second_chance = request.fallback_req.as_deref().and_then(|fallback| {
                    let merged = merge_condition(Some(fallback), config_pin.as_deref());
                    resolve_dep_version(api_mod, &request.mod_id, merged, &options.pinned_game_version, options.allow_unstable).ok()
                });

                if second_chance.is_none() {
                    failures.push(ResolveFailure::NoMatchingVersion(request.mod_id.clone(), e.to_string()));
                }

                second_chance
            }
        };

        let Some((version, download_url, _, _)) = resolved else {
            continue;
        };

        installs.push(Install {
            mod_id: request.mod_id.to_lowercase(),
            mod_name: api_mod.mod_json.name.clone().unwrap_or_default(),
            version_to_install: version,
            download_url,
            current_file_path: None,
        });
    }

    Ok((installs, failures))
}

/// Resolves one mod against a single set of conditions. Pulled out so the fallback paths can
/// run the exact same resolution with a different condition.
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
    let (pkgs, pinned_game_version, allow_unstable, jobs) = with_config(|c| {
        (c.pkg.clone(), c.pinned_game_version.clone(), c.allow_unstable, c.jobs)
    }).await;

    let mut graph: DependencyGraph = HashMap::new();
    let mut queue: VecDeque<Install> = VecDeque::new();
    let mut all_installed: Vec<Installed> = Vec::new();
    // deps we couldn't get hold of, reported together at the end rather than one notice at a
    // time. A dependency going missing shouldn't stop everything else from installing
    let mut unresolved_deps: Vec<String> = Vec::new();

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

        let recently_installed = download_requested_mods(mod_dir, &mut batch, client, Some(mp), jobs)
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
                                .filter(|(dep_id, _)| !is_base_game_dep(dep_id))
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

                // key on lowercase, the graph does. Two mods spelling the same dep differently
                // would otherwise land in separate buckets and their requirements never compared
                new_deps.entry(dep_id.to_lowercase()).or_default().push(Requester {
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

            // the api gave us nothing for this id. It's usually a mod that got renamed, pulled,
            // or made private, and it dropped out here without a word before
            let Some(api_mod) = api_results.get(dep_id.as_str()) else {
                let asked_by = requesters.iter().map(|r| r.mod_id.as_str()).collect::<Vec<_>>().join(", ");
                unresolved_deps.push(format!("{dep_id}\nNot found on the mod site. Needed by: {asked_by}"));
                continue;
            };

            {
                let mod_name = api_mod.mod_json.name.clone().unwrap_or_default();
                // println!("Mod name {mod_name}");
                // the url alias IS the mod ID in MOST cases. Need a check for validity
                let mod_id = if let Some(mod_alias) = &api_mod.mod_json.url_alias {
                    mod_alias
                } else  {
                    &api_mod.mod_json.mod_id.clone().to_string()
                };

                let pkg = match pkgs.iter().find(|p| p.mod_id.eq_ignore_ascii_case(mod_id)) {
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

    if !unresolved_deps.is_empty() {
        display_incompatible_mods_constraint(unresolved_deps, "Dependencies that could not be installed".into());
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