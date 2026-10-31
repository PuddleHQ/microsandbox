# Fuzz targets for the filesystem backends

cargo-fuzz targets that feed FUSE requests, as the guest kernel's driver queues them, into
libkrun's request dispatcher with one of this crate's backends behind it. They need a nightly
toolchain, so this crate is outside the workspace. The dispatcher entry point is
`msb_krun_devices::fuse_dyn_session` (feature `fuzzing`), and `[patch.crates-io]` in
`Cargo.toml` points the whole msb_krun stack at the libkrun fork branch that has it.

| Target | Backend |
|---|---|
| `fuse_passthrough` | `PassthroughFs` over a small tree, with the virtual init file on (a fake agentd) |
| `fuse_memfs` | `MemFs` capped at 8 MiB and 1024 inodes (its builder also needs the fake agentd) |

Input: records of a 16-bit request length, the request (an `InHeader` and its body) and a
16-bit reply buffer size; a FUSE_INIT is sent first. `seeds/fuse_requests/` holds named seed
inputs for both targets (made by the libkrun fork's `fuzz/gen_seeds.py`, where the format is
documented).

```sh
cargo install cargo-fuzz --locked
rustup toolchain install nightly --profile minimal
cd crates/filesystem/fuzz
cargo +nightly fuzz run --target x86_64-unknown-linux-gnu fuse_passthrough /scratch/work seeds/fuse_requests -- \
    -max_total_time=600 -max_len=16384 -rss_limit_mb=2048
```

On a small machine run one job, add `ulimit -v 8000000` and `-s none` (the address sanitizer
reserves terabytes of address space). The passthrough target serves a throw-away tree under
`$FUZZ_FUSE_ROOT` (default: the temp dir) and caps file sizes at 8 MiB for the process.

A crash becomes a normal unit test of the backend (the input is the repro) and a fix; the input
is kept under `seeds/fuse_requests/` as `crash-<what>`.

The nightly run is the `fuzz-fuse` job of `.github/workflows/fuzz.yml`.
