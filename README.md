# Relay

A Windows desktop app for a **plan → expand → implement** coding pipeline:

1. You type a short instruction ("add a retry with backoff to the HTTP client").
2. **Claude** expands it into a detailed spec: files, signatures, step-by-step changes, edge cases, acceptance criteria. Claude only plans and describes — in words, names and signatures; it does not write the code.
3. You review and edit the spec.
4. A **local LLM** (llama.cpp on your GPU) writes all of the code, as SEARCH/REPLACE edits.
5. You review side-by-side diffs and accept or reject each file; accepted files are written to the project.
6. *(Optional)* Claude reviews the applied diff against the spec and can write a follow-up spec.

Built with Tauri 2 (Rust backend) + React/TypeScript + Monaco.

---

## Prerequisites

| What | Notes |
|---|---|
| Windows 11 x64 | WebView2 runtime (preinstalled on Windows 11). |
| NVIDIA GPU + recent driver | Tested on an RTX 5070 Ti (16 GB) with driver 617.42. Blackwell cards need a **CUDA 13.x** build of llama.cpp. |
| `llama-server.exe` (CUDA build) | From [ggml-org/llama.cpp releases](https://github.com/ggml-org/llama.cpp/releases): `llama-bXXXXX-bin-win-cuda-13.x-x64.zip` **plus** `cudart-llama-bin-win-cuda-13.x-x64.zip` extracted into the same folder. Tested with **b12360**. Relay can download and extract both for you (Settings → General → llama-server → *Download…*). |
| Claude Code CLI *(default backend)* | Install [Claude Code](https://claude.com/claude-code) and run `claude` once in a terminal to sign in. Relay calls `claude -p` with your subscription. |
| Anthropic API key *(alternative)* | Settings → Claude → Anthropic API. Stored in Windows Credential Manager (`Relay / anthropic-api-key`), never in a file. |
| Git *(optional)* | For the dirty-tree warning and checkpoint commits. |

Hardware used for the numbers below: RTX 5070 Ti 16 GB, Ryzen 9 5950X, 32 GB DDR4.

## Install

* **Installer:** `src-tauri/target/release/bundle/nsis/Relay_0.1.0_x64-setup.exe` (per-user install, no admin).
* **From source:** see [Development](#development).

On first start: Settings → General → set the **models folder** (default `%LOCALAPPDATA%\Relay\models`) and the **llama-server.exe** path (or let Relay auto-detect / download it). Pick a project folder with the 📁 button.

---

## Models

Relay ships three built-in profiles. All values below were verified on Hugging Face on 2026-11-13 and measured with llama.cpp b12360 with nothing else using the GPU (Windows desktop ≈0.6 GB VRAM).

| Profile | Role | Repo → file | Size | Context | VRAM (total) | Speed |
|---|---|---|---|---|---|---|
| **Qwen3.8-27B** (dense) | best quality, default | [`unsloth/Qwen3.8-27B-GGUF`](https://huggingface.co/unsloth/Qwen3.8-27B-GGUF) → `Qwen3.8-27B-UD-IQ4_XS.gguf` | 14.35 GB | 32k | ≈15.5 GB | ≈49 tok/s |
| **Qwen3.6-35B-A3B** (MoE) | fast, long context | [`ggml-org/Qwen3.6-35B-A3B-GGUF`](https://huggingface.co/ggml-org/Qwen3.6-35B-A3B-GGUF) → `Qwen3.6-35B-A3B-Q4_K_M.gguf` | 20.54 GB | 64k | ≈14.5 GB | ≈73 tok/s |
| **gpt-oss-20b** | most reliable format-following, fallback | [`ggml-org/gpt-oss-20b-GGUF`](https://huggingface.co/ggml-org/gpt-oss-20b-GGUF) → `gpt-oss-20b-MXFP4.gguf` | 12.11 GB | 64k | ≈13.9 GB | ≈205 tok/s |

SHA256 (from the HF LFS metadata; Relay verifies downloads against these):

```
4b194b34ecec99fdfb74caef84445ccd01c987de87d96ae8ca50dc434b4b0a38  Qwen3.8-27B-UD-IQ4_XS.gguf
f4ffe03c9139e6a905a5d346f77d9fabdd179367c3280dfd005d1e75ca5ed8c8  Qwen3.6-35B-A3B-Q4_K_M.gguf
27cd6c432c7672cb812a92f611cf3ba7bbc35928262bb1e1253ff4ee6ae35901  gpt-oss-20b-MXFP4.gguf
```

**Getting the files:** either point a profile at a GGUF you already have (Settings → Local models → *GGUF file → Browse…*), or click *Download…* next to the Hugging Face repo. Relay lists the repo's GGUF files, shows the exact file name and size and asks before downloading. Downloads go to the models folder as `<file>.part`, **resume** after interruptions, and are **SHA256-verified** before being renamed into place.

### Why these quants (differences from the original plan)

* **Qwen3.8-27B: unsloth `UD-IQ4_XS` (14.35 GB) instead of bartowski `IQ4_XS` (15.48 GB).** Qwen3.8-27B has 16 full-attention layers × 4 KV heads × 256 dims → 64 KiB/token of f16 KV cache = 2 GiB at 32k. bartowski's file plus that cache does not fit in 16 GB. The unsloth file with a **q8_0 KV cache** (1 GiB at 32k) does.
* **Qwen3.6-35B-A3B: ggml-org `Q4_K_M` (20.54 GB)**, the smallest Q4_K_M (unsloth's UD-Q4_K_M is 22.13 GB). Its KV cache is tiny (10 full-attention layers × 2 KV heads → 20 KiB/token).
* **gpt-oss-20b: 64k context** (native 131k). KV ≈24 KiB/token, so 64k costs ≈1.5 GiB.

### Recommended llama-server flags (16 GB card)

Relay starts the server itself; these are the profile defaults. Relay always appends `-m <gguf> --host 127.0.0.1 --port <port> -c <context limit> -np 1 --log-colors off` (`-np 1` matters: the default "auto" splits the context across several slots).

```bat
:: Qwen3.8-27B — fully on GPU, quantized KV cache
llama-server -m Qwen3.8-27B-UD-IQ4_XS.gguf -c 32768 -np 1 -ngl all -fa on -ctk q8_0 -ctv q8_0

:: Qwen3.6-35B-A3B — attention on GPU, experts of the first 18 layers in RAM
llama-server -m Qwen3.6-35B-A3B-Q4_K_M.gguf -c 65536 -np 1 -ngl all --n-cpu-moe 18 -fa on --load-mode none

:: gpt-oss-20b — fully on GPU (native MXFP4)
llama-server -m gpt-oss-20b-MXFP4.gguf -c 65536 -np 1 -ngl all -fa on
```

`--n-cpu-moe` sweep for Qwen3.6-35B-A3B (64k ctx, total VRAM incl. desktop):

| N | VRAM | tok/s |
|---|---|---|
| 12 | 15.9 GB — overcommits, spills to shared memory (≈7 tok/s) | ✗ |
| 16 | 15.4 GB | 77.7 |
| **18** | **14.5 GB** | **73.1** |
| 20 | 13.7 GB | 68.8 |

`--load-mode none` (no mmap) is recommended by llama.cpp when tensors are overridden to the CPU. llama.cpp's own `--fit` (on by default when `-ngl`/`--n-cpu-moe` are not set) also works for this model and lands at ≈15.1 GB.

### Thinking / reasoning control (verified against the model cards and llama-server b12360)

| Model | What Relay sends | Notes |
|---|---|---|
| Qwen3.8 / Qwen3.6 | `chat_template_kwargs: {"enable_thinking": false}` | The official switch per the model cards. **`/no_think` is not used**: the Qwen3.6 card states the Qwen3 soft switch (`/think`, `/nothink`) is not supported, and the 3.8 card doesn't mention it. Any stray `<think>…</think>` is still stripped. |
| gpt-oss-20b | `reasoning_effort: "low"` (top-level) **and** `chat_template_kwargs: {"reasoning_effort": "low"}` | Reasoning can't be disabled. llama-server returns it in `reasoning_content` (separate from the answer); Relay also strips raw harmony markup (`<\|channel\|>analysis…`) if a server puts it in `content`. 2,048 tokens are reserved for it in the budget. |

Sampling defaults: Qwen non-thinking mode `temperature 0.7, top_p 0.8, top_k 20, min_p 0` per the model card, but **`presence_penalty 0`** instead of the card's 1.5, because SEARCH blocks must reproduce existing code verbatim. gpt-oss: `temperature 1.0, top_p 1.0` (OpenAI's recommendation). All editable per profile.

---

## Using Relay

* **Toolbar:** project folder (+ git branch/dirty state), **model switcher** (unloaded / loading with progress / ready / error), VRAM in use, History, Log, Settings. Switching unloads the current model, waits for the process to exit, then loads the new one. Only one model is in memory at a time.
* **① Instruction:** type the instruction, tick files to give Claude as context (`Ctrl+Enter` expands). With the CLI backend, Claude may also read the project with read-only tools (Read/Glob/Grep) — toggle in Settings → Claude.
* **② Spec:** Claude's output streams in, then becomes an editable Markdown editor. Relay parses the `## Files` list: ✎ = will be edited, + = will be created, ◦ = read-only context (ticked files and any file the spec mentions are auto-included). The **context budget** shows prompt tokens + reserved output against the selected profile's context. Counts are *exact* once that profile's model is loaded (llama-server's `/tokenize`, cached per text; 2% safety margin), and estimated from text length otherwise (≈3.3 bytes/token, ~12% high on code; 5% margin); if the job doesn't fit, Relay warns and defaults to **one call per file** (each call gets the Summary, that file's `### path` section, Edge cases, Acceptance criteria and that file). You can force single/per-file mode.
  * **Context outlines:** large read-only context files (≥ 600 estimated tokens, in Python, JavaScript, TypeScript/TSX, Rust, Go, Java, C#, C or C++) are sent to the local model as an *outline*: the file with function and method bodies (and the inside of long string/array/dict literals) replaced by `… (N lines omitted)` markers, so imports, declarations, signatures, doc comments and docstrings remain, verbatim. On Relay's own sources that is 20–30% of the tokens. Their chips show an *outline* tag; click one to send that file in full for this session. Files being edited are always sent in full. Switch it off in Settings → *Context outlines*. To see what a file's outline looks like: `cargo run --example outline -- path\to\file.py`.
  * **Prompt cache:** llama-server keeps the previous prompt in its KV cache and only processes the part of a new prompt after the longest shared prefix. Relay therefore builds each local-model message as *read-only context → files to edit → spec*: per-file calls share the context, and re-running a job after editing only the spec reuses the files too. A retry continues the same conversation, so its whole first prompt comes from the cache. *Model calls* in the Result pane show each attempt's prompt tokens and how many came from the cache (`timings.cache_n`; for other OpenAI-compatible servers, `usage.prompt_tokens_details.cached_tokens` if they report it).
  * **Cache checkpoints:** all three built-in models are hybrid (Qwen3.x: Gated DeltaNet layers with full attention in every 4th layer) or use sliding-window attention (gpt-oss). llama-server can't rewind them to an arbitrary earlier position, only to a *checkpoint*, and it leaves one shortly before the end of each request's prompt. So before the calls of a per-file run, Relay sends the shared context as its own request (`/completion`, `n_predict: 0`, cut at the exact token boundary via `/apply-template` + `/tokenize`), and every later call goes back to that checkpoint instead of processing the context again. The run log shows it as *Primed the shared context*.
  * **Prewarm** (Settings → *Prewarm*, off by default): while you review the spec, Relay evaluates everything before the spec (instructions, read-only context, files to edit) into the loaded model's cache, so the run only has to process the spec. It never loads a model, skips while a run is active, and editing only the spec doesn't trigger it again. Uses the GPU for a few seconds after you pick files or load a model. llama-server only (LM Studio and Ollama have no `/apply-template`).
* **③ Result:** live output with tokens/sec, then per-file side-by-side diffs (Monaco). The right side is editable before you accept. **Accept / Reject** per file or **Accept all**. If the project is a git repo with uncommitted changes, Relay warns before the first write and offers a **checkpoint commit**. Files changed on disk since the run read them are not overwritten without asking.
  * **Re-run with another model** sends the same spec to a different profile (switching models as needed); **Compare with** shows the two models' proposals diffed against each other, with speed/time/attempt stats.
  * **Review applied diff with Claude** (or automatically, if enabled) returns PASS / NEEDS_FIXES plus an optional follow-up spec you can load with one click.
* **History:** every run is saved (instruction, spec, model profile + model file, raw local-model output including stripped reasoning, validation results, diffs, accept/reject decisions, timings, review). Reopen a job to look at it or re-run it.

### The edit format and validation

The local model must answer only with blocks like:

```
path/to/file.py
<<<<<<< SEARCH
exact existing lines
=======
replacement lines
>>>>>>> REPLACE
```

An empty SEARCH creates a new file (whole content in REPLACE). Every SEARCH must match the current file **exactly** (only CRLF/LF is normalised) and **uniquely**. If anything fails — unmatched or ambiguous SEARCH, a missing target file, a format error, output cut off at `max_tokens` — Relay sends **one automatic retry** listing each failure, the closest matching lines from the file, and the current file content; successful blocks are kept. If it still fails, the failures are shown and, with **auto-fallback** on (off by default), Relay offers to re-run the spec with gpt-oss-20b.

### Path safety

Nothing is read or written outside the selected project folder: absolute paths, drive letters, UNC paths, `..`, alternate data streams, reserved device names (`CON`, `NUL`, …), trailing dots/spaces and anything under `.git` are rejected, and symlinks/junctions are resolved so they can't point outside the project.

---

## Custom profiles

Settings → **Local models**:

* **Duplicate** a built-in to experiment (e.g. a different quant, context or `--n-cpu-moe`), or **Add** a blank one. **Reset to default** restores a built-in; built-ins can't be deleted.
* A **managed** profile needs a GGUF (explicit path, or `<models folder>/<file in repo>`), launch arguments, context limit, max output tokens, sampling, and reasoning control:
  * *Disable thinking* → `enable_thinking: false` (Qwen3.x and other templates with that switch),
  * *Reasoning effort* → `reasoning_effort` (gpt-oss, or Qwen3.8 with thinking on),
  * *None*.
* An **external** profile points at an OpenAI-compatible endpoint you run yourself — Relay doesn't start or stop anything (and never kills that process):
  * LM Studio: `http://localhost:1234/v1`, model = the identifier shown in LM Studio
  * Ollama: `http://localhost:11434/v1`, model = e.g. `qwen3.6:35b-a3b`
  * your own llama-server: `http://127.0.0.1:8080/v1`
  * optional API key (stored in Credential Manager).
* **Test profile** loads the model and runs a tiny edit task: reports tok/s, how much reasoning was stripped, and whether the edit format validated.

Profiles live in `%LOCALAPPDATA%\Relay\profiles.json`.

## Prompt templates

Settings → **Prompt templates** (files in `%LOCALAPPDATA%\Relay\templates`):

* `expand.md` — instruction → spec (Claude). It tells Claude to describe changes without writing code (inline names, signatures and literal values only); a spec containing multi-line code blocks gets a warning in the Spec pane. Placeholders: `{{instruction}} {{model_name}} {{context_limit}} {{project_name}} {{file_tree}} {{context_files}} {{explore_note}} {{budget_guidance}}`. The budget guidance is computed from the active profile's context limit so Claude writes tighter, self-contained per-file sections for smaller contexts.
* `implement-system.md` — the local model's system prompt (edit-format rules).
* `review.md` — review prompt. Placeholders: `{{spec}} {{diff}} {{files_after}}`.

## Claude backends

* **CLI (default):** `claude -p --output-format stream-json --verbose --include-partial-messages --no-session-persistence --model <id> --effort <level>`, prompt on **stdin** (Windows limits command lines to 32K chars), working directory = the project. Tools are disabled (`--tools ""`) unless *Explore project* is on (`--tools Read,Glob,Grep`). Cancelling kills the whole CLI process tree.
* **API:** `POST https://api.anthropic.com/v1/messages` (`anthropic-version: 2026-10-21`), streaming, `thinking: {type: "adaptive"}`, `output_config.effort`, and server-side refusal fallbacks (`fallbacks: "default"`, beta `server-side-fallback-2026-07-01`). Default model `claude-opus-5-5`; effort default `high`.

## Data locations

`%LOCALAPPDATA%\Relay\`: `settings.json`, `profiles.json`, `history.db` (SQLite), `templates\`, `run\llama-server.pid`, `llama.cpp\` (if downloaded), `models\` (default models folder).

## Process management

* llama-server runs hidden, is assigned to a Windows **job object with kill-on-close** (it dies with Relay even on a crash), and its PID is recorded. On startup Relay kills an orphan only if the recorded PID still has the recorded executable path — a llama-server you started yourself is never touched.
* Readiness = `GET /health` returns 200 (503 while loading). Load progress is estimated from llama-server's log phases and previous load times.
* Stop = Ctrl+C (release builds), then kill after 4 s; Relay waits for the process to exit before loading the next model.
* Errors are diagnosed from the log: e.g. *CUDA error / out of memory* → "not enough free VRAM — close GPU-heavy apps (games…), lower context, smaller quant, raise --n-cpu-moe".

---

## Development

Requirements: Node 22+, Rust (MSVC toolchain), Visual Studio Build Tools (C++), WebView2.

```bat
npm install
npm run tauri dev          :: run the app
npm run tauri build        :: release build + NSIS installer
```

### Tests

```bat
cd src-tauri
cargo test --lib                     :: unit tests
cargo run --example e2e -- fake      :: backend end-to-end against a fake llama-server (no GPU)
cargo run --example e2e -- claude    :: + real Claude CLI expansion and review
cargo run --example e2e -- real      :: real llama-server + the three real models (uses the GPU!)
```

Unit tests cover the SEARCH/REPLACE parser and applier, context budgeting, context outlines (all nine grammars), path safety (including junction escapes), per-profile reasoning suppression and stripping, spec parsing, prompt templates, SSE parsing, server arguments and error diagnosis, history, and resumable/verified downloads.

`tools/fake-llama-server` is a GPU-free stand-in for llama-server (same CLI flags, log lines, `/health`, `/tokenize` with decodable token ids, `/apply-template`, `/completion` with `n_predict: 0`, streaming `/v1/chat/completions` with timings, including a one-slot prompt cache reported as `cache_n`; `--fake-recurrent` makes the cache behave like a hybrid model's, reusable only up to a checkpoint). It can be scripted (`FAKE_LLM_SCRIPT=rules.json`) or synthesizes valid edits for any spec; `--fake-fail`, `--fake-reasoning`, `--fake-tps N`, `--fake-load-ms N` simulate failures, gpt-oss-style reasoning and speed.

```bat
cd tools\fake-llama-server && cargo build --release
```

UI scenarios that click through the real window over the WebView2 DevTools protocol live in `tools/uitest/` (see its README).

### Results on the reference machine (2026-11-13)

Instruction *"add a retry with backoff to the HTTP client"* on a small Python project, spec written by Claude Opus 5.5 via the CLI (36 s, ≈$0.13), implemented by each profile:

| Profile | Valid edits | Attempts | Speed | Time |
|---|---|---|---|---|
| Qwen3.8-27B | ✓ | 1 | 36–46 tok/s | 13–16 s |
| Qwen3.6-35B-A3B | ✓ | 1 | 68 tok/s | 12 s |
| gpt-oss-20b | ✓ in 3/3 runs | 1, 2, 1 | 188 tok/s | 6–11 s |

These runs predate the no-code rule for specs (the specs still contained code), so they measure copying more than implementing; re-measure with prose-only specs. The first gpt-oss run failed (a SEARCH started with one blank line too many, and the retry "thought out loud" inside its answer). That led to two changes: the edit-format prompt now requires SEARCH to start and end on non-blank lines, and the retry message names near-misses precisely ("your SEARCH starts with 3 blank lines but the file has 2 before line 5", "matches only if indentation is ignored"). Matching itself stays exact. The Qwen3.8-27B run on the hand-written spec in `examples/e2e.rs` produced code that passes the spec's acceptance checks when executed.

### Layout

```
src/                      React UI (Toolbar, Instruction/Spec/Result panes, Settings, History, Log)
src-tauri/src/
  editformat.rs           SEARCH/REPLACE parser + applier + retry message
  pathsafe.rs             project-folder confinement
  reasoning.rs            per-profile reasoning suppression + stripping
  budget.rs               single vs per-file plans (exact or estimated counts)
  tokens.rs               exact token counts via llama-server /tokenize (cached)
  prewarm.rs              prompt prefix priming (/apply-template + /tokenize + /completion)
  outline.rs              tree-sitter outlines of read-only context files
  spec.rs                 spec Markdown parsing, per-file specs
  pipeline.rs             implement step (prompting, streaming, validation, retry)
  server.rs, winproc.rs   llama-server lifecycle, job objects, orphan cleanup
  llm.rs                  OpenAI-compatible streaming client
  claude.rs               Claude CLI / API backends, expand + review prompts
  history.rs              SQLite history
  download.rs             Hugging Face / llama.cpp downloads (resume + SHA256)
  app.rs, commands.rs     application core + Tauri commands
  templates/              default prompt templates
tools/fake-llama-server/  test double
tools/uitest/             CDP-driven UI scenarios
```

## Troubleshooting

* **`CUDA error: shared object initialization failed` / out of memory / very slow generation** — another program holds VRAM (games, browsers with hardware acceleration). Check the VRAM readout in the toolbar; close the app or use a profile with a smaller footprint (raise `--n-cpu-moe`, lower the context).
* **"Claude Code CLI not found"** — make sure `claude --version` works in a new terminal, then restart Relay.
* **Validation keeps failing** — open *Model calls* in the Result pane to see each attempt's raw output and the exact mismatch; try per-file mode, a tighter spec, or gpt-oss-20b.
