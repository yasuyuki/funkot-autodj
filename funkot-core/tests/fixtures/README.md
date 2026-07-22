# Analysis CI fixtures

`golden.json` holds synth recipes + tolerances for downbeat / section tests.
WAV files are optional (gitignored); tests synthesize in memory from `synth`.

```sh
./dev.sh cargo run -p funkot-cli --release -- --gen-test-fixtures funkot-core/tests/fixtures
./dev.sh cargo test -p funkot-core --release --test analysis_golden
```
