# ffmpeg-aimd

Native Rust video reduction for AiM SmartyCam MP4 files. It uses FFmpeg bindings for hardware encoding, preserves the embedded `aimd` telemetry track by atom-level MP4 splicing, and exports the telemetry to MoTeC LD/LDX sidecars.

## Requirements

- Rust 1.84+
- FFmpeg 8/9 development libraries discoverable through `pkg-config` or `FFMPEG_DIR`
- A working VAAPI render device for the default hardware path
- `motorsport-telemetry-rs` is pinned in `Cargo.toml` for reproducible builds

The utility refuses CPU fallback unless `--cpu` is supplied explicitly.

## Build

```sh
cargo build --release
```

## Usage

With no paths, the utility scans `~/Documents/upload` and writes to `reduced/`:

```sh
./target/release/ffmpeg-aimd
```

Reduce a file to 720p HEVC at 1800 kb/s while keeping the source FPS:

```sh
./target/release/ffmpeg-aimd \
  --res 720 \
  --bitrate 1800 \
  --output-dir ./reduced \
  input.MP4
```

Lower the frame rate as well:

```sh
./target/release/ffmpeg-aimd --fps 30 --res 720 --bitrate 1500 input.MP4
```

Trim a range. Times accept seconds, `MM:SS`, or `HH:MM:SS`:

```sh
./target/release/ffmpeg-aimd 00:10 02:30 input.MP4
```

Force CPU `libx264` only when hardware encoding is unavailable:

```sh
./target/release/ffmpeg-aimd --cpu input.MP4
```

Extract telemetry without transcoding. With no output argument, hidden sidecars are created beside the input:

```sh
./target/release/ffmpeg-aimd --extract-motec input.MP4
./target/release/ffmpeg-aimd --extract-motec input.MP4 output.ld
```

Use `--overwrite` to replace existing outputs.

## Outputs

For `input.MP4`, a reduced output named `input_720p.mp4` is created. Its companion files are:

```text
.input_720p.mp4.ld
.input_720p.mp4.ldx
```

The MP4 contains the transcoded video, copied audio, and the original AIMD track. FFmpeg does not understand AiM timed metadata itself, so the AIMD `trak` and its sample payload are copied and offset-patched after FFmpeg finishes. The MoTeC writer keeps AIMD sample values and counts; MoTeC LD requires integer, contiguous sample rates, so incompatible source timing is rounded and reported.

## Hardware selection

The encoder preference is:

1. `hevc_vaapi`
2. `h264_vaapi`

VAAPI render nodes under `/dev/dri/renderD*` are enumerated and tried in order. The selected encoder and device are printed before conversion. Progress reports decoded frames, percentage, elapsed time, and ETA.

## Verification

```sh
cargo fmt -- --check
cargo check
cargo test
```

The test suite includes an MP4 atom-splicing check. `omatrack-cli parse` can verify both the converted MP4 and generated `.ld` sidecar.

## License

MIT. See `LICENSE`.
