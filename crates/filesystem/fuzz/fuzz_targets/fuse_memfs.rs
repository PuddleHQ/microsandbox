#![no_main]

use std::sync::Arc;

use libfuzzer_sys::fuzz_target;
use microsandbox_filesystem::MemFs;

#[path = "common.rs"]
mod common;

fuzz_target!(|data: &[u8]| {
    common::use_fake_agentd();
    let fs = MemFs::builder()
        .capacity(8 << 20)
        .max_inodes(1024)
        .build()
        .expect("memfs");
    msb_krun_devices::fuse_dyn_session(Arc::new(fs), data);
});
