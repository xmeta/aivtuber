aivtuber local profile {{VERSION}} ({{PLATFORM}})
=================================================

This archive is the local profile of the aivtuber performer, prebuilt. It
contains programs, the avatar page and a starter reaction pack. It does NOT
contain any model weights, voice files or avatar model: you fetch those
separately (the speech and language models are large, and each has its own
licence and disclosure duties).

What is here
------------
  aivtuber-setup          first-run setup and launcher: fetches the models, writes the
                          configuration, starts everything (needs no Node, Bun or Cargo)
  aivtuber-app            the performer (reads its configuration from environment variables)
  aivtuberctl             operator control (emergency stop and mute)
  speech-vad-sidecar      microphone / WAV input, voice activity detection (Silero VAD)
  speech-stt-worker       Japanese speech recognition (sherpa-onnx)
  local-tts-sidecar       Japanese speech synthesis (sherpa-onnx)
  twitch-chat-ingress     Twitch chat input
  youtube-chat-ingress    YouTube Live chat input
  *.dll / lib*.so         the pinned sherpa-onnx and ONNX Runtime engine libraries.
                          They must stay in this directory, beside the programs:
                          each sidecar verifies their SHA-256 at every start and
                          refuses to run if they are missing or changed.
  renderer/local-avatar/dist/
                          the browser pages (served by the app): the avatar page (shown in
                          a browser or an OBS Browser Source; no avatar model inside) and
                          the operation panel page (chat, status, stop, mute).
  examples/starter-reaction-pack/
                          cached reaction descriptors (no audio; generated locally)
  THIRD-PARTY-*           licence texts of everything bundled

Verify the download
-------------------
The release page lists SHA256SUMS. Check the archive against it before you
unpack it:
  Windows (PowerShell):  Get-FileHash .\<archive>.zip -Algorithm SHA256
  Linux:                 sha256sum -c SHA256SUMS --ignore-missing

The programs are not code-signed. Windows SmartScreen may warn on first start;
the checksum above is how you know you have the file the release built.

Next steps
----------
Follow "Download a release" and the steps after it in the local profile
quickstart:
  https://github.com/xmeta/aivtuber/blob/main/docs/local-profile-quickstart.adoc

In short, from this directory (after the checksum check above):
  1. aivtuber-setup local --dry-run   shows what will be fetched, no network
  2. aivtuber-setup local             fetches and verifies the models once (about
                                      3 GB), writes local-profile.env, prints the
                                      licence duties and the start command
  3. aivtuber-setup start             starts the language model and the performer; it
                                      prints the operation panel URL (open it in a
                                      browser: type to the performer, stop, mute) and
                                      the avatar URL. Both carry a per-launch secret.
Install FFmpeg (ffplay and ffmpeg on PATH) first; setup tells you if it is missing.
On Windows also install the Microsoft Visual C++ 2015-2022 Redistributable (x64); it is
not bundled, and setup stops and names it if it is missing:
  https://learn.microsoft.com/cpp/windows/latest-supported-vc-redist
Replace the sample avatar (renderer/local-avatar/dist/model.vrm) with a model you
have the right to broadcast before you stream.
Everything listens on loopback (127.0.0.1) only.

Linux: the programs need the system libraries libasound2 (microphone) and
libssl3 (Twitch chat), and a glibc at least as new as the build host's.
