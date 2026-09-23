# ghostframe-client-gpu

GPU decode and dmabuf export for the native client.

## Tests

| Target | Needs a GPU | Runs in CI |
|---|---|---|
| `--lib` | no | yes |
| `--test coalesce` | no | yes |
| `--test dirty` | no | yes |
| `--test cdf53_passes` | no | yes |
| `--test gpu_export` | yes | no |
| `--test gpu_pipelines` | yes | no |
| `--test gpu_oracle` | yes | no |
| `ghostframe-e2e --test native_client` | yes (also Docker) | no |
| `ghostframe-e2e --test native_client -- --ignored native_client_converges_at_production_scale_under_loss` | yes (also Docker + VKMS) | no |

Run everything locally with `cargo test -p ghostframe-client-gpu`.

The GPU targets are excluded from CI by not being named in
`.github/workflows/e2e.yml`, never by `#[ignore]`. CI exempts itself; a
developer must always see these run.

`ghostframe-e2e --test native_client` lives in a different crate (it is
the M1 acceptance test: `ghostframe-client-native` against a live server
over a real headscale-backed tailnet), but needs the same GPU this crate's
own `gpu_*` targets do, plus Docker for the headscale + server containers.
Same rule applies: run it locally, it is deliberately not named in any CI
workflow.

The same file's `native_client_converges_at_production_scale_under_loss`
is Task 20: the native client at production scale (1920x1080 = 2040 tiles)
on a `tc netem`-shaped, lossy link, polling `Client::cdf53_coverage()`
until the static scene either converges or the 90s budget runs out. It is
`#[ignore]`d (several minutes, needs VKMS at 1920x1080 specifically, not
just any GPU) so it takes an explicit `--ignored <name>` to run.
