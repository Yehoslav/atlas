//! Free space on the volume that holds the data directory.

use std::{io, path::Path};

/// Free and total bytes of a filesystem, as `statvfs` reports them.
///
/// Free is what an unprivileged writer may use (`f_bavail`), not the blocks
/// reserved for root: Atlas runs as its own user, and reporting space it cannot
/// write to would be worse than reporting none.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DiskSpace {
    pub(crate) free_bytes: u64,
    pub(crate) total_bytes: u64,
}

/// Ask the filesystem how much room the data directory has.
///
/// This is one syscall that reads cached superblock counters; it does not touch
/// the disk. The answer is advisory — another process can take the space
/// between the check and the write — so the configured floor is fail-fast, not
/// a reservation. ENOSPC during a write remains a handled error.
pub(crate) fn space(path: &Path) -> io::Result<DiskSpace> {
    let stat = rustix::fs::statvfs(path)?;
    // `f_frsize` is the fragment size the block counts are expressed in;
    // POSIX allows it to be zero, in which case `f_bsize` is the fallback.
    let block_size = if stat.f_frsize > 0 {
        stat.f_frsize
    } else {
        stat.f_bsize
    };
    Ok(DiskSpace {
        free_bytes: bytes(stat.f_bavail, block_size),
        total_bytes: bytes(stat.f_blocks, block_size),
    })
}

fn bytes(blocks: u64, block_size: u64) -> u64 {
    u64::try_from(u128::from(blocks) * u128::from(block_size)).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_plausible_space_for_an_existing_directory() {
        let directory = tempfile::tempdir().unwrap();
        let space = space(directory.path()).unwrap();
        assert!(space.total_bytes > 0);
        assert!(space.free_bytes <= space.total_bytes);
    }

    #[test]
    fn a_missing_directory_is_an_error() {
        assert!(space(Path::new("/nonexistent/atlas/data")).is_err());
    }

    #[test]
    fn byte_math_saturates_instead_of_wrapping() {
        assert_eq!(bytes(u64::MAX, 2), u64::MAX);
        assert_eq!(bytes(3, 4096), 12_288);
    }
}
