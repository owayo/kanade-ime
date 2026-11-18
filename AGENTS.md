# Development

コミットの件名・本文は日本語で書く。Conventional Commitsの接頭辞は維持する。

Reply in Japanese. Read `docs/architecture.md` and `docs/status.md` before changing boundaries.

- Keep `third_party/mozc` identical to its pinned upstream commit. Cargo must never run Bazel.
- All Mozc C++ dependencies belong in `mozc-worker/worker_main.cc`; generated protocol types stay private to `kanade-mozc`.
- Key processing is synchronous. A request is identified by connection epoch, context epoch and event sequence. Retries return the cached reply without invoking Mozc again.
- A frontend acknowledges document edits. Never persist Kanade learning before a successful acknowledgement. Do not replay an uncertain event after reconnecting.
- Password fields bypass the engine. Private and unknown contexts must disable persistent learning before sending a key to Mozc.
- No input text, key logs, prompts or document excerpts in diagnostic output or persistent storage.
- Candidate UI belongs in each native frontend. GPUI and optional analysis workers must not be on the synchronous key path.
- Use astro-sight refs for identifier searches; context while editing and impact/review after changes.
- Public source, examples and metadata must use generic names and paths; never include internal domains, account addresses or workstation paths.
- Validate using `cargo xtask check`; real-worker tests additionally need `cargo xtask build-worker` and `cargo run -p kanade-eval -- contract`.
- `make setup` and `make ci` are the shared local/CI entry points. Release installers target Windows x64 and Apple Silicon macOS only; see `docs/distribution.md`.
- Never qualify native installation or signing from compilation alone. The Windows MSI rehearsal runs only on disposable CI runners; macOS archive installers reuse the input-source handoff and preserve personal data.
