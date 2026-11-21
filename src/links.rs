use std::path::PathBuf;

use crate::dialect;
use crate::events::CHUNK;
use crate::ids::Key;
use crate::people::People;

/// Reads every link file and joins the IDs into persons.
pub fn collect(paths: &[PathBuf], threads: usize) -> std::io::Result<People> {
    let states = crate::scan::run(paths, threads, CHUNK, Vec::<(Key, Key)>::new, |state, line| {
        let mut fields: [&[u8]; 2] = [&[]; 2];
        if dialect::split(line, &mut fields) && !fields[0].is_empty() && !fields[1].is_empty() {
            state.push((Key::from_bytes(fields[0]), Key::from_bytes(fields[1])));
        }
    })?;
    Ok(People::build(states.into_iter().flatten()))
}
