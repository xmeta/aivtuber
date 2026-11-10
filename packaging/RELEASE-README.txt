aivtuber local profile {{VERSION}} ({{PLATFORM}})
=================================================

This archive is the local profile of the aivtuber performer, prebuilt. It
contains programs, the avatar page and a starter reaction pack. It does NOT
contain any model weights, voice files or avatar model: you fetch those
separately (the speech and language models are large, and each has its own
licence and disclosure duties).

What is here
------------
  aivtuber-app            the performer (reads its configuration from environment variables)
  aivtuberctl             operator control (emergency stop and mute)
  speech-vad-sidecar      microphone / WAV input, voice activity detection (Silero VAD)
  speech-stt-worker       Japanese speech recognition (sherpa-onnx)
  local-tts-sidecar       Japanese speech synthesis (sherpa-onnx)
  twitch-chat-ingress     Twitch chat input
  *.dll / lib*.so         the pinned sherpa-onnx and ONNX Runtime engine libraries.
                          They must stay in this directory, beside the programs:
                          each sidecar verifies their SHA-256 at every start and
                          refuses to run if they are missing or changed.
  renderer/local-avatar/dist/
                          the browser page for the avatar (served by the app, shown in a
                          browser or an OBS Browser Source). No avatar model inside.
  examples/starter-reaction-pack/
                          cached reaction descriptors (no audio; generated locally)
  scripts/, renderer/local-avatar/smoke/fetch-model.mjs
                          the pinned, SHA-256-verifying fetch scripts for the models
                          (they need Node 20 or newer; nothing else)
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

In short: fetch the models once with the project's pinned fetch step, choose
an avatar model you have the right to broadcast, set the environment variables
from the quickstart with the paths of this directory, and start aivtuber-app.
Everything listens on loopback (127.0.0.1) only.

Linux: the programs need the system libraries libasound2 (microphone) and
libssl3 (Twitch chat), and a glibc at least as new as the build host's.
