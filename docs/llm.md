# Local reranking model

An optional local language model reorders the Mozc candidates of the focused segment, using the text before the cursor. It is off by default. The Windows and macOS installers include the pinned Tsumugi GGUF. Each user can import it from settings after accepting the model and local-processing terms. The engine qualifies it before reranking can be enabled.

Each importable file is registered together with a profile that fixes how it is loaded, prompted, scored and qualified. Registrations are compiled into Kanade. One model is installed at a time.

When conversion starts at the beginning of a field, the model can use the preceding segments of that conversion to choose the spelling of the final inflected word. The frontend can show the revised preedit until the user navigates, types or commits. The engine compares existing Mozc choices.

## Components

```mermaid
flowchart LR
  FE[native frontend] -- Key / Select / Ack --> ENG[engine key loop]
  FE -- CandidatePoll --> ENG
  ENG -- queue job / take result --> SVC[service thread]
  SVC -- bounded binary frames over private pipes --> W[kanade-llm-worker]
  W --> RT[llama.cpp, CPU only]
  UI[CLI / settings] -- Manage LLM_* --> ADM[administration]
  ADM --> MGR[management thread]
  MGR -- qualification --> W2[kanade-llm-worker]
```

| Part | Location | Role |
|---|---|---|
| Policy, records, service, qualification | `crates/kanade-llm` (default build) | No inference and no native code |
| Registry of models and profiles | `crates/kanade-llm/src/profile.rs`, `prompt.rs` | Compiled-in identities, formats, corpora and gates |
| Worker | `crates/kanade-llm/src/bin/worker.rs`, feature `llama` | The only process that loads the model |
| Engine integration | `crates/kanade-engine/src/refine.rs` | Queues jobs, answers polls, applies an order |
| Management | `crates/kanade-engine/src/server/admin.rs` | `LLM_STATUS`, `LLM_IMPORT`, `LLM_QUALIFY`, `LLM_REMOVE` |

The key loop copies a few short strings into a queue of two jobs and later looks a result up. Both operations take a lock that is only held for a few memory operations. Spawning, loading, hashing, timeouts and restarts happen on the service and management threads.

## Tsumugi model

Kanade accepts one local reranker, **Tsumugi 26.11.102**. Model versions use Japan time and the form `yy.mm.counter`. The counter starts at 100 each month and increases for each model release, independently of the IME version. Its F16 GGUF contains 28,058,112 parameters and occupies 66,507,360 bytes. For conversion with preceding text, the model combines its neural and character scores using weights fitted on source-confirmed Microsoft Japanese IME conversions. The engine, worker and settings use the same pinned identity. The Windows EXE installs the GGUF in the application’s `models` directory. The macOS DMG includes it under `Settings.app/Contents/Resources/models` inside Kanade.app. Model distribution terms are separate from the Kanade source license. [GGUF identity and qualification profile](../locks/llm.lock.toml)

A Windows x64 installation matched all 373 tokenizer references and ranked the expected candidate first in 78 of 85 context tests (91.76%). Every case received a score. Repeated runs agreed. CPU scoring took 17 ms at the median and 30 ms at the 95th percentile on the installation PC. The 85 examples exercise selected homophones. An empty Windows TSF text store displayed 車に乗ります and 雑誌に載ります after one press of Space. With それは、梅の already entered, the same test showed 木の下に立っている after one press of Space. The integration test cancels each conversion without recording a selection.

For an initial conversion with confirmed text before the caret, the worker also scores candidate spellings with a character model trained on Japanese text. It reads up to four preceding characters for each character it predicts. Tsumugi 26.11.102 weights neural log scores by 0.75 and character log scores by 3. The GGUF stores both weights and character probabilities. Focused-segment scoring, conversion in an empty field and the neural qualification suite use the original neural scores.

## Prompt and score

Tsumugi scores a replacement for the focused segment using the last 40 characters of left context, the composition reading in katakana and the complete displayed surface. It removes line breaks and maps ASCII spaces to ideographic spaces. Its 7,680-token vocabulary supports UTF-8 byte fallback.

```text
U+E080 context U+E081 KATAKANA-READING U+E082 surface END
```

The worker sums continuation log probabilities, including end token 259. It omits the context marker for empty context and adds no start token. Request fields reserve U+E080–U+E08F.

## Privacy and bounds

A job is queued only when all of the following hold. Otherwise nothing leaves the key loop.

- `llm` is enabled and a qualified model is being served.
- The context is `NORMAL` after the engine applies learning exclusions. Password input is refused earlier; private and unknown contexts, excluded applications and a disabled learning switch all yield no job.
- The frontend can read surrounding text. At the beginning of a field, a conversion of two to four segments can supply its own prefix.
- The window, and the text it may have been cut from, contain no `@`, `/`, `\`, `:`, control character or long credential-like token.
- The reading is 2–64 kana, and at least two Mozc choices of at most 32 characters are among the first nine.
- No text of the job contains a marker or reserved character of the served model's profile.
- The user has not picked a candidate in this conversion.

| Bound | Value |
|---|---|
| Candidates per job | 9 |
| Prompt / continuation tokens | 192 / 112; a longer request is refused, not truncated |
| Context size, threads | 320 tokens, 4 |
| Time budget inside the worker | 500 ms, enforced by the runtime's abort callback |
| Hard timeout in the parent | 900 ms, then the process is killed |
| Result lifetime | 700 ms from submission |
| Queue | 2 jobs; the oldest is dropped |
| Frame size | 64 KiB |
| Restarts | Delays of 1 s and 10 s; after three consecutive failures the service stops until it is activated again |
| Idle | The worker is stopped after ten minutes without a job |

The worker has a cleared environment and private pipes. On Unix its frame channel is duplicated before standard output and error are redirected to the null device; the runtime's log callback discards everything. On macOS it additionally denies itself network access, file writes and process creation before reading any request. Errors are fixed enumeration values. KV memory and the input batch are cleared after every scoring request, including failures. The in-memory result cache is keyed by a digest of the request together with the served file and profile, holds scores only and is emptied whenever the served model changes. Records on disk contain identities, counts and durations. The status reply contains no path.

## Candidate updates

The additions to `proto/kanade.proto` are `CandidatePoll`, `CandidateUpdate` and `CandidatePage.refinement_pending`.

1. A key reply sets `refinement_pending` when a job was queued for its page.
2. After acknowledging the event, the frontend sends `CandidatePoll` with the reply's revision and the page's candidate revision.
3. A candidate-only poll receives a `CandidateUpdate`. A focused native editor may also provide its next event sequence to accept a revised preedit. The editor acknowledges that edit through the same path as a key reply.
4. A new page replaces the displayed one. It has a new candidate revision and new handles; handles of the old page are refused as stale.

With preceding document text, the following rules govern the candidate list.

- Only Mozc choices among the first nine slots move. Learned, project, date and emoji choices keep their slots.
- The selected choice keeps its slot, so Enter commits the surface that was displayed.
- The source of every choice stays `MOZC`. Selection resolves the real upstream candidate again.
- The best alternative is annotated `文脈` when the model prefers it over the displayed one.
- Any key, selection, reset, close, setting or model change cancels the job. A result is applied only to the exact revision and candidate identities it was computed for, and only after the acknowledgement.
- An applied order stays while the same candidates are on screen. After a manual choice nothing is reordered until the conversion ends.
- A repeated poll or key event returns the applied page again without new work.

With confirmed preceding text, Kanade considers up to eight segments. The background poll reads up to 32 distinct Mozc choices per segment and tries up to two reading boundaries in a disposable session. The boundary probabilities were learned from Microsoft IME phrase analyses that agreed with the source spelling. A second cut can include a neighboring segment. For punctuated input, the engine compares a clause and preserves the surrounding text and punctuation.

The engine submits up to 96 full spellings. Tsumugi retains the first nine and uses its character model to shortlist up to six more for neural scoring. An original alternative needs a score lead of 5.0 (7.0 for a clause). An expanded alternative needs another 2.0 over the runner-up. A close challenger leaves the original comparison in effect. Manual choices, learned preferences and the spelling safeguards for names still take precedence.

At the beginning of a field, the engine opens a temporary Mozc session to obtain the last segment’s alternatives. It checks the segment readings against the live conversion and compares single-kanji spellings with the same kana ending. A clear score advantage can produce one whole-reading preview. The model does this after the original key reply has been acknowledged. Typing, navigating or changing focus supersedes the job. Pressing Enter commits the visible spelling. An automatic preview creates no dictionary entry.

## Installing a model

Import the supplied Tsumugi GGUF after reviewing the displayed model and local-processing terms. The engine qualifies the file through its CPU worker. Qualification records bind the file, vocabulary, prompt and worker digests.

```sh
kanade-ui models import /path/to/tsumugi.gguf --accept-license --accept-local-processing
kanade-ui models status
```

## Qualification

The worker must reproduce 373 independent tokenizer examples. On the 85 context-dependent ranking fixtures it must score every candidate, produce deterministic results, respect request bounds, choose at least 77 expected spellings and finish 95 percent of requests within 300 ms.

```sh
KANADE_LLM_MODEL=/path/to/tsumugi.gguf cargo test --release -p kanade-llm --features llama --test real_model -- --ignored --test-threads 1
```
