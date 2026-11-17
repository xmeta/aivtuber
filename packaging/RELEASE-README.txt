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
  twitch-chat-ingress     Twitch chat input (read-only; the panel's stream chat
                          section starts it for you)
  youtube-chat-ingress    YouTube Live chat input (read-only; needs a YouTube Data API
                          key, entered in the panel)
  *.dll / lib*.so         the pinned sherpa-onnx and ONNX Runtime engine libraries.
                          They must stay in this directory, beside the programs:
                          each sidecar verifies their SHA-256 at every start and
                          refuses to run if they are missing or changed.
  renderer/local-avatar/dist/
                          the browser pages (served by the app): the avatar page (shown in
                          a browser or an OBS Browser Source; no avatar model inside) and
                          the operation panel page (chat, status, stop, mute).
  examples/persona/starter.ja.md
                          character sheet template (Japanese): copy it to local-persona.md
                          beside local-profile.env, edit it and restart; see below
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
                                      The terminal stays quiet (measurement lines and
                                      start-up settings are hidden; errors, refusals,
                                      licence notices and operator lines are always
                                      shown) and ends with a short Japanese/English
                                      guide. Add --verbose to see everything. (With
                                      Windows PowerShell 5.1, redirected output can
                                      garble the Japanese; the English stays readable.)
                                      The microphone is OFF until you run
                                      "aivtuber-setup start --voice-input on" once
                                      (remembered; --voice-input off turns it off),
                                      or turn it on in the operation panel's
                                      voice input card (also remembered; the same
                                      card, or "aivtuberctl voice-input off", turns
                                      it off at once).
                                      In the hybrid profile, voice input on sends the
                                      transcribed speech to the cloud model.
                                      Optional character sheet: copy
                                      examples/persona/starter.ja.md to
                                      local-persona.md beside local-profile.env, edit
                                      it (at most 3000 bytes, plain text) and start
                                      again. The model then speaks as that character;
                                      cached reactions, the starter audio and fallback
                                      lines do not follow it. The app prints one line
                                      saying whether it was loaded (never its text).
                                      In the hybrid profile the sheet is sent to the
                                      cloud model with each reply.
Install FFmpeg (ffplay and ffmpeg on PATH) first; setup tells you if it is missing.
On Windows also install the Microsoft Visual C++ 2015-2022 Redistributable (x64); it is
not bundled, and setup stops and names it if it is missing:
  https://learn.microsoft.com/cpp/windows/latest-supported-vc-redist
Choose your own avatar before you stream: in the operation panel's avatar section pick
a .vrm file, read the licence it declares, and confirm (no restart; the sample in
renderer/local-avatar/dist/model.vrm is a test model). Use a model you have the right
to broadcast.
Everything listens on loopback (127.0.0.1) only.

Linux: the programs need the system libraries libasound2 (microphone) and
libssl3 (Twitch chat), and a glibc at least as new as the build host's.
