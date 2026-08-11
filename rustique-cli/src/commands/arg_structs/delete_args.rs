use clap::{Args, ValueEnum};

#[derive(Args, Debug)]
pub struct DeleteArgs {
    
    /// Specify one or more mods to delete. You can delete a specific version with modid@version.  
    #[arg(short, long, num_args = 1.., value_name = "MOD_ID")]
    pub mod_id: Vec<String>,
    
    /// Used with mod_id, if you set this, it will delete the mods in the backup dir
    #[arg(short = 'b', long, default_value = "false")]
    pub mod_backups: bool,

    /// Also remove the deleted mod's dependencies, as long as nothing else still needs them.
    /// Off by default because plenty of content mods are dependencies of other mods, and Rustique
    /// has no way to tell a library mod from one you installed because you wanted it
    #[arg(long, default_value = "false")]
    pub with_deps: bool,
    
    /// Deletes all specified; mods or backups.
    #[arg(short, long, value_name = "TYPE")]
    pub all: Option<DeleteArgAllVals>,
}


#[derive(ValueEnum, Debug, Clone)]
pub enum DeleteArgAllVals {
    Mods,
    Backups,
    Both,
}