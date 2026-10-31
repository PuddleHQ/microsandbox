#![no_main]

use std::sync::Arc;

use libfuzzer_sys::fuzz_target;
use microsandbox_filesystem::PassthroughFs;

#[path = "common.rs"]
mod common;

fuzz_target!(|data: &[u8]| {
    common::use_fake_agentd();
    let tree = msb_krun_devices::served_tree();
    let fs = PassthroughFs::builder()
        .root_dir(tree.as_ref())
        .build()
        .expect("passthrough over the served tree");
    msb_krun_devices::fuse_dyn_session(Arc::new(fs), data);
});
