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
