use super::*;
use std::ffi::CStr;
use std::os::{fd::AsRawFd, unix::fs::MetadataExt};

fn set_attribute(file: &std::fs::File, name: &CStr, value: &[u8]) {
    assert_eq!(
        unsafe {
            libc::fsetxattr(
                file.as_raw_fd(),
                name.as_ptr(),
                value.as_ptr().cast(),
                value.len(),
                0,
            )
        },
        0,
        "{}",
        std::io::Error::last_os_error()
    );
}

fn attribute(file: &std::fs::File, name: &CStr) -> Option<Vec<u8>> {
    let mut value = vec![0; 1024];
    let read = unsafe {
        libc::fgetxattr(
            file.as_raw_fd(),
            name.as_ptr(),
            value.as_mut_ptr().cast(),
            value.len(),
        )
    };
    let Ok(read) = usize::try_from(read) else {
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ENODATA)
        );
        return None;
    };
    value.truncate(read);
    Some(value)
}

#[test]
fn config_metadata_preserves_ownership_and_acl_without_inheriting_extra_access() {
    let dir = std::env::temp_dir().join(format!("shepr-config-acl-{}", std::process::id()));
    std::fs::create_dir(&dir).expect("test precondition");
    // Linux UAPI posix_acl_xattr_header/entry, version 2, little-endian fields.
    // Owner rw, named user 65534 read, group none, mask read, other none.
    let mut acl = 2_u32.to_le_bytes().to_vec();
    for (tag, permissions, id) in [
        (1_u16, 6_u16, u32::MAX),
        (2, 4, 65534),
        (4, 0, u32::MAX),
        (16, 4, u32::MAX),
        (32, 0, u32::MAX),
    ] {
        acl.extend(tag.to_le_bytes());
        acl.extend(permissions.to_le_bytes());
        acl.extend(id.to_le_bytes());
    }
    for has_acl in [false, true] {
        let source = dir.join(format!("source-{has_acl}"));
        let target = dir.join(format!("target-{has_acl}"));
        std::fs::write(&source, b"old").expect("test precondition");
        let input = std::fs::File::open(&source).expect("test precondition");
        if unsafe { libc::geteuid() } == 0 {
            assert_eq!(unsafe { libc::fchown(input.as_raw_fd(), 1001, 1002) }, 0);
        }
        if has_acl {
            set_attribute(&input, c"system.posix_acl_access", &acl);
        }
        set_attribute(&input, c"user.shepr-test", b"preserve this attribute");
        let original = input.metadata().expect("test precondition");
        drop(create_config_temporary(&target, true).expect("test precondition"));
        let output = std::fs::File::open(&target).expect("test precondition");
        // Model a default ACL inherited from the destination's parent directory.
        set_attribute(&output, c"system.posix_acl_access", &acl);
        write_config_temporary(Some(&source), &target, b"new").expect("test precondition");
        let actual = output.metadata().expect("test precondition");
        assert_eq!(
            (actual.uid(), actual.gid(), actual.mode()),
            (original.uid(), original.gid(), original.mode())
        );
        assert_eq!(
            attribute(&output, c"system.posix_acl_access"),
            attribute(&input, c"system.posix_acl_access")
        );
        assert_eq!(
            attribute(&output, c"user.shepr-test"),
            Some(b"preserve this attribute".to_vec())
        );
        assert_eq!(std::fs::read(source).expect("test precondition"), b"old");
        assert_eq!(std::fs::read(target).expect("test precondition"), b"new");
    }
    std::fs::remove_dir_all(dir).expect("test precondition");
}
