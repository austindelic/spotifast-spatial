# macOS spatial audio feasibility probe

This diagnostic tests the public AVFoundation output path proposed in
[issue #678](https://github.com/crmne/spotifast/issues/678). Production Spotifast
playback is unchanged. The probe generates quiet stereo tones; it does not
connect to Spotify, read credentials, or capture system audio.

Native AirPods spatialization needs an Apple-silicon Mac with macOS 12.3 or
later and compatible AirPods. Select the AirPods as the system output before
starting. Quit the installed Spotifast app first: the diagnostic deliberately
uses its bundle identity so macOS can associate playback with Spotifast.

## Build and check

Run from the repository root. The standalone manifest and lockfile keep the
prototype dependencies out of the app. Its output stays under the repository's
managed target directory.

```sh
CARGO_TARGET_DIR=target/spatial-audio-probe mise exec -- cargo test --locked --manifest-path examples/spatial-audio-probe/Cargo.toml
CARGO_TARGET_DIR=target/spatial-audio-probe mise exec -- cargo clippy --locked --manifest-path examples/spatial-audio-probe/Cargo.toml --all-targets -- -D warnings
CARGO_TARGET_DIR=target/spatial-audio-probe mise exec -- cargo build --locked --manifest-path examples/spatial-audio-probe/Cargo.toml
mise exec -- target/spatial-audio-probe/debug/spatial-audio-probe --check
```

`--check` constructs and enqueues owned PCM at playback rate zero. It makes no
sound and cannot verify native controls, spatialization, or head tracking.

## Run the listening test

The bundle must be used for this test. Running the diagnostic through
`cargo run` does not provide Spotifast's application identity.

```sh
mkdir -p .cache/spatial-audio-probe
mise exec -- bash packaging/macos/bundle.sh target/spatial-audio-probe/debug/spatial-audio-probe .cache/spatial-audio-probe/Spotifast.app 0.12.0
open -n "$PWD/.cache/spatial-audio-probe/Spotifast.app" --args --seconds 180 --log "$PWD/.cache/spatial-audio-probe/listening.log"
```

The app plays for three minutes and exits. Change `--seconds` to any whole
number from 1 to 600. There is no diagnostic window. Use the AirPods menu or
Control Center to compare Off, Fixed, and Head Tracked. Keep the system volume
comfortable and move your head during the Head Tracked test.

The log records the OS, architecture, bundle identity, renderer failures, and
missed scheduling deadlines. It appends a new session on each run. Successful
enqueueing and a completed run do not establish that spatialization is active.

Record these results in [RESULTS.md](RESULTS.md):

- Mac model, macOS version, AirPods model, and firmware version.
- Whether the native controls recognize Spotifast and expose all three modes.
- Whether Off/Fixed produce an audible difference and head tracking works.
- Whether the run finishes without underruns or renderer errors.
- Maintainer scope acceptance, with a link to the decision in issue #678.

Keep production integration gated until the hardware test passes and the
maintainer accepts the scope. Device switching, route recovery, EQ, and the
production transport tests belong to that integration, not this prototype.

The probe runs Apple calls serially on its main thread, schedules 20 ms chunks
at most 40 ms ahead, and installs no asynchronous data callbacks. This is a
hardware feasibility test, not a production output implementation or a CPU
benchmark.

Remove `.cache/spatial-audio-probe` after recording the hardware result. Do not
move the diagnostic into Applications or use it as the regular Spotify client.
