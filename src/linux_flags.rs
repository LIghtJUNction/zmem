//! Linux open(2) ABI values shared by all filesystem safety checks.
//!
//! The released x86_64 and aarch64 targets use different directory/nofollow
//! bits, independently of GNU versus musl libc. Keep the ARM overrides beside
//! the asm-generic values instead of repeating architecture-specific literals.
//! Sources (Linux v6.8 and glibc 2.39):
//! https://github.com/torvalds/linux/blob/v6.8/arch/arm64/include/uapi/asm/fcntl.h
//! https://github.com/torvalds/linux/blob/v6.8/include/uapi/asm-generic/fcntl.h
//! https://github.com/bminor/glibc/blob/glibc-2.39/sysdeps/unix/sysv/linux/aarch64/bits/fcntl.h

#[cfg(any(target_arch = "aarch64", target_arch = "arm"))]
pub const O_DIRECTORY: i32 = 0o40000;
#[cfg(not(any(target_arch = "aarch64", target_arch = "arm")))]
pub const O_DIRECTORY: i32 = 0o200000;

#[cfg(any(target_arch = "aarch64", target_arch = "arm"))]
pub const O_NOFOLLOW: i32 = 0o100000;
#[cfg(not(any(target_arch = "aarch64", target_arch = "arm")))]
pub const O_NOFOLLOW: i32 = 0o400000;

pub const O_NONBLOCK: i32 = 0o4000;
pub const O_PATH: i32 = 0o10000000;

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs::{self, File, OpenOptions},
        io::Read,
        os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, symlink},
        path::PathBuf,
        sync::atomic::{AtomicU64, Ordering},
    };

    static SEQUENCE: AtomicU64 = AtomicU64::new(0);

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "zmem-linux-flags-{}-{}",
                std::process::id(),
                SEQUENCE.fetch_add(1, Ordering::Relaxed)
            ));
            fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
            File::create(path.join("file")).unwrap();
            Self(path)
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn directory_flag_opens_a_directory_and_rejects_a_regular_file() {
        let fixture = Fixture::new();
        let directory = OpenOptions::new()
            .read(true)
            .custom_flags(O_DIRECTORY | O_NOFOLLOW)
            .open(&fixture.0)
            .unwrap();
        assert!(directory.metadata().unwrap().is_dir());
        let error = OpenOptions::new()
            .read(true)
            .custom_flags(O_DIRECTORY)
            .open(fixture.0.join("file"))
            .unwrap_err();
        assert_eq!(error.raw_os_error(), Some(20), "must fail with ENOTDIR");
    }

    #[test]
    fn nofollow_flag_accepts_a_regular_file_but_rejects_final_symlinks() {
        let fixture = Fixture::new();
        OpenOptions::new()
            .read(true)
            .custom_flags(O_NOFOLLOW)
            .open(fixture.0.join("file"))
            .unwrap();
        symlink("file", fixture.0.join("link")).unwrap();
        let error = OpenOptions::new()
            .read(true)
            .custom_flags(O_NOFOLLOW)
            .open(fixture.0.join("link"))
            .unwrap_err();
        assert_eq!(error.raw_os_error(), Some(40), "must fail with ELOOP");
        symlink(&fixture.0, fixture.0.join("directory-link")).unwrap();
        assert!(
            OpenOptions::new()
                .read(true)
                .custom_flags(O_DIRECTORY | O_NOFOLLOW)
                .open(fixture.0.join("directory-link"))
                .is_err()
        );
    }

    #[test]
    fn path_nofollow_pins_the_final_inode_including_a_symlink_for_metadata_rejection() {
        let fixture = Fixture::new();
        let path = fixture.0.join("file");
        let mut file = OpenOptions::new()
            .read(true)
            .custom_flags(O_PATH | O_NOFOLLOW)
            .open(&path)
            .unwrap();
        let metadata = file.metadata().unwrap();
        let expected = fs::symlink_metadata(&path).unwrap();
        assert!(metadata.is_file());
        assert_eq!(
            (metadata.dev(), metadata.ino()),
            (expected.dev(), expected.ino())
        );
        assert_eq!(
            file.read(&mut [0_u8; 1]).unwrap_err().raw_os_error(),
            Some(9),
            "O_PATH must not permit data reads (EBADF)"
        );

        let path = fixture.0.join("link");
        symlink("file", &path).unwrap();
        let link = OpenOptions::new()
            .read(true)
            .custom_flags(O_PATH | O_NOFOLLOW)
            .open(&path)
            .unwrap();
        let metadata = link.metadata().unwrap();
        let expected = fs::symlink_metadata(&path).unwrap();
        assert!(metadata.file_type().is_symlink());
        assert!(!metadata.is_file());
        assert_eq!(
            (metadata.dev(), metadata.ino()),
            (expected.dev(), expected.ino())
        );
    }
}
