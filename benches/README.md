# benches

Workloads, baselines and hardware profiles. A hardware profile is `profiles/<name>.toml`:

```toml
name = "reference-linux-x86"
cpu = "describe the CPU"
memory_gb = 64
storage = "NVMe, describe"
```

Baselines are stored per profile under `baselines/<profile>/`.

## Running

`cargo xtask bench` measures and compares against `benches/baselines/<profile>/baseline.json` (default
profile `local`, which is git-ignored because baselines are only meaningful on the machine that recorded
them). Reference baselines are committed under a named profile from `benches/profiles/` once they have
been recorded on that hardware. `--write-baseline` records, `--tolerance 0.15` sets the allowed slowdown.
