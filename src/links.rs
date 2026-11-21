use std::path::PathBuf;

use crate::events::CHUNK;
use crate::ids::Key;

/// Reads every link file into ID pairs, one list per scan thread. Malformed
/// lines and lines with an empty side are dropped.
pub fn collect(paths: &[PathBuf], threads: usize) -> std::io::Result<Vec<Vec<(Key, Key)>>> {
    crate::scan::run_fields::<2, _, _, _>(paths, threads, CHUNK, Vec::<(Key, Key)>::new, |state, fields| {
        if !fields[0].is_empty() && !fields[1].is_empty() {
            state.push((Key::from_bytes(fields[0]), Key::from_bytes(fields[1])));
        }
    })
}
