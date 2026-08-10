use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use comfy_table::{Attribute, CellAlignment, Color};
use comfy_table::presets::UTF8_BORDERS_ONLY;
use tokio::fs::ReadDir;
use tracing::{info, warn};
use rustique_core::aliases::{ModFileName, ModID, ModVersion};
use crate::commands::arg_structs::delete_args::DeleteArgAllVals;
use crate::commands::sync::get_sync_data;
use rustique_core::config::config_manager::get_config;
use rustique_core::consts::FILE_RUSTIQUE_SYNC;
use rustique_core::information_utils::{display_table, notice, CellData};
use rustique_core::symlink_manager::SymlinkManager;
use rustique_core::rustique_errors::RustiqueError;
use rustique_core::traits::ref_ext::PathRef;
use rustique_core::utils::{delete_file, extract_all_mods_metadata, split_modid_version};
use rustique_core::version_management::compare_versions;

pub async fn delete_all(mod_dir: impl PathRef, delete_type: &DeleteArgAllVals) -> Result<(), RustiqueError> {
    
    let config = get_config().read().await;
    
   
    // location_type: Mods looks at the folder specified by mod_dir
    // location_type: Backups looks at the backup dir in the config
    // location_type: Both does both

    let mut cleaned_mods: Vec<PathBuf> = Vec::new();
    
    if matches!(delete_type, DeleteArgAllVals::Mods) || matches!(delete_type, DeleteArgAllVals::Both) {
        // delete all mods in the mod_dir
        // collect paths for all in mod_dir
        // use delete_file on each 
        
        let mut mods = tokio::fs::read_dir(mod_dir).await?;
        iterate_and_delete(&mut mods, &mut cleaned_mods).await?;
    }
    
    if matches!(delete_type, DeleteArgAllVals::Backups) || matches!(delete_type, DeleteArgAllVals::Both) {
       
        let mut mods = tokio::fs::read_dir(Path::new(&config.backup_mods_dir)).await?;
        iterate_and_delete(&mut mods, &mut cleaned_mods).await?;
    }
    
    show_deleted(&format!("{cleaned_mods:?}"));
    
    Ok(())
}

pub async fn iterate_and_delete(mods: &mut ReadDir, result_vec: &mut Vec<PathBuf>) -> Result<(), RustiqueError> {
    
    while let Some(entry) = mods.next_entry().await.map_err(|e| RustiqueError::SimpleError(format!("Unable to iterate on dir: {e}")))? {
        //make sure the file is still there, just to be cautious 
        if entry.path().exists() {
            result_vec.push(entry.path());
            delete_file(entry.path()).await?;
        }
    }
    
    Ok(())
}

#[allow(dead_code)]
pub async fn iterate_and_move_zip(curr_items: &mut ReadDir, target_dir: impl PathRef, ignore_symlinks: bool) -> Result<(), RustiqueError> {
    while let Some(entry) = curr_items.next_entry().await.map_err(|e| RustiqueError::SimpleError(format!("Unable to iterate on dir: {e}")))? {
        if entry.path().exists() {
            let file_name = entry.file_name();
            // check if file_name is a .zip
            
            // if the file is not a zip OR ignore symlinks is true AND the entry is a symlink, skip it.
            if !file_name.to_string_lossy().ends_with(".zip") 
                || (ignore_symlinks && SymlinkManager::exists(entry.path())) {
                continue;
            }
            info!("Moving {file_name:?}");
            tokio::fs::rename(&entry.path(), target_dir.as_ref().join(&file_name))
                .await.map_err(|e| RustiqueError::SimpleError(format!("Unable to move file: {e}")))?;
        }
    }

    Ok(())
}

pub async fn delete_cmd(mod_dir: impl PathRef, mod_ids: Vec<ModID>, is_backup: bool) -> Result<(), RustiqueError> {
    
    let config = get_config().read().await;
   
    let mod_lookup: HashMap<ModID, Option<ModVersion>> = mod_ids.iter().map(split_modid_version).collect();
   
    info!("mod_lookup {:?}", mod_lookup);
    
    let mod_dir = if is_backup {
        Path::new(&config.backup_mods_dir)
    } else {
        mod_dir.as_ref()
    };
    
    // grab only the real mods in the m_dir, ignoring the symlinks (modpacks)
    let all_metadata  = extract_all_mods_metadata(mod_dir, true).await?;
    let mut processed_mods: Vec<(ModID, ModVersion, ModFileName)> = Vec::new();

    for (filename, modinfo) in &all_metadata {
        // modinfo.json ids come in whatever case the author felt like, mod_lookup is already lowercased
        let mod_id = modinfo.mod_id.to_lowercase();
        let Some(target_version) = mod_lookup.get(&mod_id) else {
            continue;
        };

        info!("target_version: {:?}", target_version);

        let installed_version = modinfo.version.clone().unwrap_or("0.0.0".into());

        // no version means every copy of this mod goes. split_modid_version already turned a
        // bare 1.2.3 into =1.2.3 so a plain version is an exact match, ranges still work
        let should_delete = match target_version {
            // a mod whose version won't parse just isn't a match, no reason to kill the whole command over it
            Some(required_version) => compare_versions(required_version, &installed_version).unwrap_or_else(|e| {
                warn!("Can't compare {} against {} for {}: {}", installed_version, required_version, mod_id, e);
                false
            }),
            None => true,
        };

        if should_delete {
            // one bad file shouldn't abort the whole command and leave the sync file unwritten
            match delete_file(mod_dir.join(filename)).await {
                Ok(()) => processed_mods.push((mod_id, installed_version, filename.clone())),
                Err(e) => warn!("Failed to delete {}: {}", filename, e),
            }
        } else {
            info!("Skipping {} {}, doesn't match {:?}", mod_id, installed_version, target_version);
        }
    }
    
    if !processed_mods.is_empty() {
        // get sync data and remove all the processed_mods from it. (this saves having to sync again)
        let mut sync_data = get_sync_data(&mod_dir, true).await?;
        let deleted_files: HashSet<&ModFileName> = processed_mods.iter().map(|(_, _, f)| f).collect();

        // only drop a mod from the sync file once every copy of it is gone. Deleting one version
        // out of two leaves the other one installed and it still belongs in there
        for (mod_id, _, _) in &processed_mods {
            let still_installed = all_metadata.iter().any(|(filename, modinfo)| {
                modinfo.mod_id.to_lowercase().eq(mod_id) && !deleted_files.contains(filename)
            });

            if !still_installed {
                sync_data.rustique_sync.remove(mod_id);
            }
        }

        // save the file from the passed mod_dir (which could be from the config file or the cli)
        sync_data.save(Path::new(mod_dir).join(FILE_RUSTIQUE_SYNC)).await?;
    }

    let removed = processed_mods.iter().map(|(mod_id, version, filename)| format!("{mod_id}@{version}:{filename}")).collect::<Vec<String>>().join("], [");

    show_deleted(&removed);

    // anything asked for that we didn't actually delete, either it isn't installed or the
    // version they asked for isn't the one sitting there
    let deleted_ids: HashSet<&ModID> = processed_mods.iter().map(|(id, _, _)| id).collect();
    let not_deleted: Vec<&ModID> = mod_lookup.keys().filter(|id| !deleted_ids.contains(id)).collect();

    if !not_deleted.is_empty() {
        notice(format!("Nothing deleted for: [{}]. Check the modid and version with [Rustique list]", not_deleted.iter().map(|id| id.as_str()).collect::<Vec<&str>>().join("], [")), Some(Color::Yellow), vec![Attribute::Bold]);
    }

    Ok(())
}

fn show_deleted(deleted_mods: &str) {
    display_table(
        vec![
            (
                CellData::new("Successfully deleted:".into(), Some(Color::Green), vec![], None),
                CellData::new(format!("[{deleted_mods}]"), Some(Color::Magenta), vec![], Some(CellAlignment::Right))
            )
        ],
        Some(UTF8_BORDERS_ONLY)
    );
}