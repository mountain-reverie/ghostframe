# ghostframe-lib benches

Three Criterion / iai-callgrind suites covering the M3 §12 codec-comparison
requirements and the §9.4 latency/throughput tracking obligations.

| Bench | Tool | Requires |
|---|---|---|
| `codec_latency` | Criterion | `--features gpu-bench` for H.264; otherwise no-op |
| `pipeline_throughput` | Criterion | nothing (CPU only) |
| `codec_callgrind` | iai-callgrind | `valgrind` installed |

## Running

```
# Quick smoke (CI-friendly)
cargo bench -p ghostframe-lib --bench pipeline_throughput -- --test
cargo bench -p ghostframe-lib --bench codec_latency        -- --test

# Full numbers (local, with GPU + valgrind)
cargo bench -p ghostframe-lib --features gpu-bench
```

## Adding an M3 codec

1. Implement `BenchEncoder` for the new codec in `benches/codec_latency.rs`.
2. Add it to `run_codecs()`. Wrap with `Lz4Wrapper` to measure the LZ4
   break-even §12 calls for.
3. The same `ContentClass` fixtures and BenchmarkId namespace are reused;
   no harness changes needed.
