use std::sync::Once;

/// The virtual init file is the agentd payload; any 64-bit x86-64 ELF header passes validation.
pub fn use_fake_agentd() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let mut elf = vec![0_u8; 4096];
        elf[..6].copy_from_slice(b"\x7fELF\x02\x01");
        elf[16..18].copy_from_slice(&2_u16.to_le_bytes());
        let machine: u16 = if cfg!(target_arch = "aarch64") {
            183
        } else {
            62
        };
        elf[18..20].copy_from_slice(&machine.to_le_bytes());
        let path = std::env::temp_dir().join(format!("fuzz-agentd-{}", std::process::id()));
        std::fs::write(&path, &elf).expect("write the fake agentd");
        // SAFETY: runs once, before any other thread of this process reads the environment.
        unsafe { std::env::set_var("MSB_AGENTD_PATH", &path) };
        microsandbox_filesystem::agentd::initialize_agentd_payload().expect("fake agentd");
        let _ = std::fs::remove_file(path);
    });
}
