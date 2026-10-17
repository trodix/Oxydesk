# ADR 007 — Audio output (M6)

## Decision
- Port `AudioProvider::open(AudioParams) -> AudioSource` (core): the source is opened **in the format negotiated** with the
  client (16-bit PCM, 44.1 kHz or 48 kHz, stereo); `AudioSource::read_exact` blocks until a packet is full.
- Adapters (`crates/audio`): `ToneProvider` (440 Hz sine paced in real time, `--test-mode`) and `PulseCapture` (capture of the
  monitor of a PipeWire null sink `rdsktp`, created when the session starts and made the default).
- `PulseCapture` v1 goes through the PulseAudio API tools of `pipewire-pulse` (`pactl`, `parec`, package `pulseaudio-utils`)
  rather than a native libpipewire binding: simpler to verify. A native capture (the `pipewire` crate, already used for the
  screen) remains possible behind the same port.
- `rdsktp-rdp`: `SoundFactory` (IronRDP RDPSND, `with_sound_factory`); on negotiation, a thread reads 20 ms packets and sends
  them as `ServerEvent::Rdpsnd(Wave)`. PCM only; Opus not done (no proven client need).
- **Format order: 44.1 kHz first, then 48 kHz.** The server picks the first format both sides share. Measured with the stock
  Windows client (mstscax, Win11 26300): a 48 kHz stream is played at 44.1/48 of its pitch on a 44.1 kHz device (440 Hz heard as
  405 Hz), while a 44.1 kHz stream is exact (ADR 010).
- `--audio tone|pulse|off` (default: `tone` with `--test-mode` or a synthetic session, `pulse` in a KWin session).

## Findings
- The client-side `ironrdp-rdpsnd` sends the intersection of the formats in `HashSet` order: the `format_no` of the packets
  cannot be read back with several formats. The test client therefore offers only one (48 kHz). Real clients (FreeRDP, mstsc)
  do not have this defect.
- Unconsumed packets are stale: the audio scenario first drains the queue and relies on the reception instant.
- No shared clock path with the video: no lip-sync in M6.
- The stream of a closed connection must not feed the next one (one event channel for all connections): see ADR 010.

## Oracle
`audio_tone` scenario: ≥ 1 s of continuous PCM (received / real time > 0.9) with ≥ 50 % of the energy at 440 Hz (Goertzel).
Negative controls: 1 kHz tone (mock, unit test) and a KWin session with no playback (silence → failure, verified on the VM).

## Verified (Debian 13 VM)
`--session synthetic` and `--session kwin`: 7/7, `audio_tone` ratio 1.00, continuity ≈ 1.0 (kwin: testapp `--tone` → `paplay` →
sink `rdsktp` → `parec` → RDPSND). Two `login` runs on 2026-10-17 measured ratio 0.00 (silence): the VM launcher no longer started the test
application with `--tone`, a harness bug fixed with `VM_TONE=1` (ADR 010 §4 ter). After the fix the login run gives ratio 1.00.

## Manual test
xfreerdp 3.31.1 (`/sound:sys:pulse`) with Firefox ESR on YouTube in the KWin session: sound heard on the host.

## Not verified / next
Measured audio latency; audible KRDC; production VMware (are PipeWire/pipewire-pulse and `pulseaudio-utils` present?); Opus;
microphone input (RDPEAI); volume; resynchronisation after a burst.
