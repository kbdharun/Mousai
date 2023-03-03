use anyhow::Result;
use gtk::glib;

use std::fs;

const N_NAMED_DB: u32 = 2;
pub const SONG_LIST_DB_NAME: &str = "song_list";
pub const RECORDINGS_DB_NAME: &str = "saved_recordings";

/// Note: This must be only called once.
pub fn new_env() -> Result<heed::Env> {
    let path = glib::user_data_dir().join("mousai/db");
    fs::create_dir_all(&path)?;
    let env = unsafe {
        heed::EnvOpenOptions::new()
            .map_size(100 * 1024 * 1024) // 100 MiB
            .max_dbs(N_NAMED_DB)
            .flag(heed::Flags::MdbWriteMap)
            .flag(heed::Flags::MdbMapAsync)
            .open(&path)?
    };

    tracing::debug!(
        ?path,
        info = ?env.info(),
        real_disk_size = ?env.real_disk_size(),
        "Opened db env"
    );

    Ok(env)
}

/// Create a new env for tests with 1 max named db and a
/// path to a temporary directory.
#[cfg(test)]
pub fn new_test_env() -> (heed::Env, tempfile::TempDir) {
    let tempdir = tempfile::tempdir().unwrap();
    let env = unsafe {
        heed::EnvOpenOptions::new()
            .map_size(100 * 1024 * 1024) // 100 MiB
            .max_dbs(1)
            .flag(heed::Flags::MdbWriteMap)
            .flag(heed::Flags::MdbMapAsync)
            .open(&tempdir)
            .unwrap()
    };
    (env, tempdir)
}