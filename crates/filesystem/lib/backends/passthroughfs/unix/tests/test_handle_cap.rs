//! A guest that opens files without closing them hits `EMFILE` at the configured handle limit
//! instead of using up the sandbox process's descriptors. Driven through the real FUSE handlers.

use super::*;

/// Linux `EMFILE`.
const EMFILE: i32 = 24;

const LIMIT: usize = 64;

fn capped_fs() -> TestSandbox {
    TestSandbox::with_config(|mut cfg| {
        cfg.max_open_handles = LIMIT;
        cfg
    })
}

#[test]
fn opens_flooding_past_the_limit_get_emfile_and_hold_no_more_handles() {
    let sb = capped_fs();
    let (entry, first) = sb.fuse_create_root("a").unwrap();
    let mut handles = vec![first];
    for _ in 1..LIMIT {
        handles.push(sb.fuse_open(entry.inode, libc::O_RDONLY as u32).unwrap());
    }
    assert_eq!(sb.fs.handles.read().unwrap().len(), LIMIT);

    // The flood: far more opens than the limit.
    for _ in 0..(LIMIT * 20) {
        let err = sb
            .fuse_open(entry.inode, libc::O_RDONLY as u32)
            .unwrap_err();
        assert_eq!(err.raw_os_error(), Some(EMFILE));
    }
    // Refused opens left no handle (and so no descriptor) behind, and the other open paths refuse too.
    assert_eq!(sb.fs.handles.read().unwrap().len(), LIMIT);
    assert_eq!(
        sb.fuse_create_root("b").err().unwrap().raw_os_error(),
        Some(EMFILE)
    );
    assert!(
        !sb.root.join("b").exists(),
        "a refused create must not leave the file"
    );
    assert_eq!(
        sb.fuse_opendir(ROOT_INODE).unwrap_err().raw_os_error(),
        Some(EMFILE)
    );

    // Closing a handle makes room again.
    sb.fs
        .release(
            sb.ctx(),
            entry.inode,
            0,
            handles.pop().unwrap(),
            false,
            false,
            None,
        )
        .unwrap();
    sb.fuse_open(entry.inode, libc::O_RDONLY as u32).unwrap();
}

#[test]
fn the_default_limit_leaves_normal_use_alone() {
    let sb = TestSandbox::new();
    let (entry, _) = sb.fuse_create_root("a").unwrap();
    for _ in 0..2000 {
        sb.fuse_open(entry.inode, libc::O_RDONLY as u32).unwrap();
    }
}
