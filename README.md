# Relay

A Windows desktop app for a **plan → implement → verify → review** coding pipeline that runs entirely on your machine:

1. You type a short instruction ("add a retry with backoff to the HTTP client").
2. The **planner** — a local model with thinking on (or, optionally, Claude) — reads the project with read-only tools and designs the change as a spec at class-diagram level: the types and functions involved, their responsibilities, exact signatures, relationships and contracts, the design decisions with their rationale, the files to touch, edge cases, acceptance criteria, and the **commands that verify it** (build / test / run). The planner never writes code, pseudo-code or step-by-step logic.
3. You review and edit the spec (or turn on **Autopilot** to skip this; its *secure mode*, on by default, only runs allowlisted build/test commands, in a sandbox).
4. The **coder** — a local model with thinking off — writes all of the code, as SEARCH/REPLACE edits, into a **staging copy** of the project.
5. Relay **builds, tests and runs** the change in the staging copy. Failures go back to the coder with the condensed compiler / test output (*fix rounds*).
6. The planner **reviews** the diff and the verification results against the spec; if it finds problems, its follow-up spec goes back to the coder (*follow-up pass*), which is verified and reviewed again.
7. You review side-by-side diffs with the build/test log and accept or reject each file; only accepted files are written to the project.

Planner and coder are *roles*, each bound to a model profile. By default both run on Qwen3.6-35B-A3B (no model swaps, 64k context); give them different profiles and Relay swaps the model at each hand-off, since only one fits in 16 GB.

Built with Tauri 2 (Rust backend) + React/TypeScript + Monaco.

---

## Prerequisites

| What | Notes |
|---|---|
| Windows 11 x64 | WebView2 runtime (preinstalled on Windows 11). |
| NVIDIA GPU + recent driver | Tested on an RTX 5070 Ti (16 GB) with driver 617.42. Blackwell cards need a **CUDA 13.x** build of llama.cpp. |
| `llama-server.exe` (CUDA build) | From [ggml-org/llama.cpp releases](https://github.com/ggml-org/llama.cpp/releases): `llama-bXXXXX-bin-win-cuda-13.x-x64.zip` **plus** `cudart-llama-bin-win-cuda-13.x-x64.zip` extracted into the same folder. Tested with **b12360**. Relay can download and extract both for you (Settings → General → llama-server → *Download…*). |
| Build tools of your projects | Whatever the verification commands need on PATH: `cargo`, `npm`, `python`, `go`, `dotnet`, `cmake`, … Relay warns in the Spec pane when a detected tool is missing. |
| Claude Code CLI *(optional planner)* | Only if you set the planner to Claude: install [Claude Code](https://claude.com/claude-code) and run `claude` once in a terminal to sign in. Relay calls `claude -p` with your subscription. |
| Anthropic API key *(optional planner)* | Settings → Claude → Anthropic API. Stored in Windows Credential Manager (`Relay / anthropic-api-key`), never in a file. |
| Git *(optional)* | For the dirty-tree warning and checkpoint commits. |

Hardware used for the numbers below: RTX 5070 Ti 16 GB, Ryzen 9 5950X, 32 GB DDR4.

## Install

* **Installer:** `src-tauri/target/release/bundle/nsis/Relay_0.1.0_x64-setup.exe` (per-user install, no admin).
* **From source:** see [Development](#development).

On first start: Settings → General → set the **models folder** (default `%LOCALAPPDATA%\Relay\models`) and the **llama-server.exe** path (or let Relay auto-detect / download it). Pick a project folder with the 📁 button.

---

## Models

Relay ships three built-in profiles. All values below were verified on Hugging Face on 2026-11-13 and measured with llama.cpp b12360 on 2026-11-14 with nothing else using the GPU (Windows desktop ≈0.5 GB VRAM, included in the totals).

| Profile | Role | Repo → file | Size | Context | VRAM (total) | Speed |
|---|---|---|---|---|---|---|
| **Qwen3.8-27B** (dense) | best quality, default | [`unsloth/Qwen3.8-27B-GGUF`](https://huggingface.co/unsloth/Qwen3.8-27B-GGUF) → `Qwen3.8-27B-UD-IQ4_XS.gguf` | 14.35 GB | 32k | ≈15.8 GB | ≈120 tok/s (MTP drafting) |
| **Qwen3.6-35B-A3B** (MoE) | fast, long context | [`ggml-org/Qwen3.6-35B-A3B-GGUF`](https://huggingface.co/ggml-org/Qwen3.6-35B-A3B-GGUF) → `Qwen3.6-35B-A3B-Q4_K_M.gguf` | 20.54 GB | 64k | ≈15.0 GB | ≈86 tok/s |
| **gpt-oss-20b** | most reliable format-following, fallback | [`ggml-org/gpt-oss-20b-GGUF`](https://huggingface.co/ggml-org/gpt-oss-20b-GGUF) → `gpt-oss-20b-MXFP4.gguf` | 12.11 GB | 128k | ≈14.9 GB | ≈207 tok/s |

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
* **gpt-oss-20b: full 128k context** (native 131,072). KV ≈24 KiB/token, so 128k costs ≈3 GiB and still fits (≈14.9 GB total, ≈1.7 GB more than 64k, same speed).

### Recommended llama-server flags (16 GB card)

Relay starts the server itself; these are the profile defaults. Relay always appends `-m <gguf> --host 127.0.0.1 --port <port> -c <context limit> -np 1 --log-colors off` (`-np 1` matters: the default "auto" splits the context across several slots).

```bat
:: Qwen3.8-27B — fully on GPU, quantized KV cache, drafts with the model's own MTP head
llama-server -m Qwen3.8-27B-UD-IQ4_XS.gguf -c 32768 -np 1 -ngl all -fa on -ctk q8_0 -ctv q8_0 --spec-type draft-mtp,ngram-mod

:: Qwen3.6-35B-A3B — attention on GPU, experts of the first 15 layers in RAM, quantized KV cache
llama-server -m Qwen3.6-35B-A3B-Q4_K_M.gguf -c 65536 -np 1 -ngl all --n-cpu-moe 15 -fa on --load-mode none -ctk q8_0 -ctv q8_0 -ub 2048 -b 2048

:: gpt-oss-20b — fully on GPU (native MXFP4), full context
llama-server -m gpt-oss-20b-MXFP4.gguf -c 131072 -np 1 -ngl all -fa on
```

Measured with `cargo run --release --example bench` (see *Tests*): generation on the e2e retry spec, prompt processing on a ≈12k-token prompt, total VRAM incl. the desktop.

**Qwen3.8-27B: speculative decoding.** The unsloth GGUF includes the model's multi-token-prediction head (`blk.64.nextn.*`), so llama-server can draft tokens with it at almost no cost (`--spec-type draft-mtp`, ≈0.3 GB VRAM). `ngram-mod` additionally drafts from text the model already produced, which pays off when a retry or re-run repeats earlier output.

| `--spec-type` | Gen tok/s (run 1 / identical re-run) | Drafts accepted | VRAM |
|---|---|---|---|
| none (before 2026-11-14) | 49 / 49 | – | 15.5 GB |
| `ngram-simple` | 64 / 66 | 42–44% | 14.9 GB |
| `ngram-mod` | 54 / 88 | 23–36% | 14.9 GB |
| `draft-mtp` | 121 / 120 | 96–97% | 15.8 GB |
| **`draft-mtp,ngram-mod`** | **118 / 190** | 59–61% (mixed) | **15.8 GB** |

Edits were valid in every run. Temperature 0.3 instead of 0.7 changed nothing (117 tok/s). Longer drafts don't help: `--spec-draft-n-max 5` (default 3) gave the same first-run speed, and 7 no longer fit in VRAM (38 tok/s). This leaves ≈0.5 GB of VRAM headroom on a 16 GB card. **If Qwen3.8 suddenly runs at a fraction of its speed, another program is using VRAM:** close it, or remove `--spec-type …` from the profile's launch arguments (−0.3 GB). A Qwen3.8 GGUF without the MTP tensors needs `--spec-type ngram-mod` or nothing instead of `draft-mtp`.

N-gram drafting is **not** used for Qwen3.6-35B-A3B (first run 69 instead of 78 tok/s: with the experts in RAM, verifying rejected drafts costs more than it saves) or gpt-oss-20b (217 vs 207 tok/s, too small to bother).

**Qwen3.6-35B-A3B: `--n-cpu-moe`, KV type and batch size** (64k context):

| `--n-cpu-moe` | KV cache | `-ub`/`-b` | VRAM | Gen tok/s | Prompt tok/s |
|---|---|---|---|---|---|
| 12 | f16 | 512 | 15.9 GB — overcommits, spills to shared memory (2026-11-13) | ≈7 | – |
| 18 (before 2026-11-14) | f16 | 512 | 13.8 GB | 78 | 1,530 |
| 18 | f16 | 2048 | 14.2 GB | 78 | 3,660 |
| 16 | q8_0 | 512 | 14.1 GB | 82 | 1,670 |
| 14 | q8_0 | 512 | 15.0 GB | 88 | 1,830 |
| 16 | q8_0 | 2048 | 14.6 GB | 81 | 3,860 |
| **15** | **q8_0** | **2048** | **15.0 GB** | **86** | **3,920** |
| 14 | q8_0 | 2048 | 15.4 GB | 89 | 4,170 |

N = 15 is the fastest setting that keeps total VRAM at ≈15 GB; use 14 if nothing else ever runs on the GPU, 16–18 if something does.

**Two slots (`-np 2`) are not used.** Two per-file calls decoded at once gave 88 instead of 48 tok/s in total on Qwen3.8 (without MTP) and 301 instead of 191 on gpt-oss, but Qwen3.8 has no VRAM left for a second slot next to MTP (which alone gives 2.5×), and each slot would have to process the shared context itself instead of reusing the primed checkpoint.

`--load-mode none` (no mmap) is recommended by llama.cpp when tensors are overridden to the CPU. llama.cpp's own `--fit` (on by default when `-ngl`/`--n-cpu-moe` are not set) also works for this model and lands at ≈15.1 GB.

### Thinking / reasoning control (verified against the model cards and llama-server b12360)

| Model | What Relay sends | Notes |
|---|---|---|
| Qwen3.8 / Qwen3.6 | `chat_template_kwargs: {"enable_thinking": false}` | The official switch per the model cards. **`/no_think` is not used**: the Qwen3.6 card states the Qwen3 soft switch (`/think`, `/nothink`) is not supported, and the 3.8 card doesn't mention it. Any stray `<think>…</think>` is still stripped. |
| gpt-oss-20b | `reasoning_effort: "low"` (top-level) **and** `chat_template_kwargs: {"reasoning_effort": "low"}` | Reasoning can't be disabled. llama-server returns it in `reasoning_content` (separate from the answer); Relay also strips raw harmony markup (`<\|channel\|>analysis…`) if a server puts it in `content`. 2,048 tokens are reserved for it in the budget. |

**Planner role (thinking on):** Qwen3.x get `chat_template_kwargs: {"enable_thinking": true}`, gpt-oss gets `reasoning_effort` = the planner effort (default `high`). The reasoning streams in `reasoning_content` and is passed back between tool calls (`reasoning_content` on the assistant turn), so the model keeps its train of thought while it reads files. Planner sampling per profile (*Planner sampling* in the profile editor); for the Qwen built-ins it is the Qwen3.6 card's thinking mode for precise coding: `temperature 0.6, top_p 0.95, top_k 20, min_p 0, presence_penalty 0`. Up to 32,768 output tokens per planner request (the card's recommendation), clamped to the context left.

Sampling defaults (coder role): Qwen non-thinking mode `temperature 0.7, top_p 0.8, top_k 20, min_p 0` per the model card, but **`presence_penalty 0`** instead of the card's 1.5, because SEARCH blocks must reproduce existing code verbatim. gpt-oss: `temperature 1.0, top_p 1.0` (OpenAI's recommendation). All editable per profile.

---

## Using Relay

* **Toolbar:** project folder (+ git branch/dirty state), **model switcher** (unloaded / loading with progress / ready / error), VRAM in use, History, Log, Settings. Switching unloads the current model, waits for the process to exit, then loads the new one. Only one model is in memory at a time.
* **① Instruction:** type the instruction, tick files to give the planner as context, pick the planner model and press **Plan with** (`Ctrl+Enter`). The local planner gets the file tree, the ticked files and a *source overview* (small files in full, outlines of larger ones, up to ⅕ of its context or 12k tokens), and may call `read_file`, `list_files` and `search` (read-only, inside the project; Settings → Roles & loop → *Project tools*). With Claude as the planner, the CLI may read the project with Read/Glob/Grep instead. **Autopilot** (checkbox next to the button) starts implementing as soon as the spec is written; with secure mode (default) it reads *Autopilot (secure)*, see below.
* **② Spec:** the planner's output streams in (thinking, tool calls), then becomes an editable Markdown editor. Relay parses the `## Files` list: ✎ = will be edited, + = will be created, ◦ = read-only context (ticked files and any file the spec mentions are auto-included). The **context budget** shows prompt tokens + reserved output against the selected profile's context. Counts are *exact* once that profile's model is loaded (llama-server's `/tokenize`, cached per text; 2% safety margin), and estimated from text length otherwise (≈3.3 bytes/token, ~12% high on code; 5% margin); if the job doesn't fit, Relay warns and defaults to **one call per file** (each call gets the Summary, the Design, that file's `### path` section, Edge cases, Acceptance criteria and that file). You can force single/per-file mode.
  * **Context outlines:** large read-only context files (≥ 600 estimated tokens, in Python, JavaScript, TypeScript/TSX, Rust, Go, Java, C#, C or C++) are sent to the local model as an *outline*: the file with function and method bodies (and the inside of long string/array/dict literals) replaced by `… (N lines omitted)` markers, so imports, declarations, signatures, doc comments and docstrings remain, verbatim. On Relay's own sources that is 20–30% of the tokens. Their chips show an *outline* tag; click one to send that file in full for this session. Files being edited are always sent in full. Switch it off in Settings → *Context outlines*. To see what a file's outline looks like: `cargo run --example outline -- path\to\file.py`.
  * **Prompt cache:** llama-server keeps the previous prompt in its KV cache and only processes the part of a new prompt after the longest shared prefix. Relay therefore builds each local-model message as *read-only context → files to edit → spec*: per-file calls share the context, and re-running a job after editing only the spec reuses the files too. A retry continues the same conversation, so its whole first prompt comes from the cache. *Model calls* in the Result pane show each attempt's prompt tokens and how many came from the cache (`timings.cache_n`; for other OpenAI-compatible servers, `usage.prompt_tokens_details.cached_tokens` if they report it).
  * **Cache checkpoints:** all three built-in models are hybrid (Qwen3.x: Gated DeltaNet layers with full attention in every 4th layer) or use sliding-window attention (gpt-oss). llama-server can't rewind them to an arbitrary earlier position, only to a *checkpoint*, and it leaves one shortly before the end of each request's prompt. So before the calls of a per-file run, Relay sends the shared context as its own request (`/completion`, `n_predict: 0`, cut at the exact token boundary via `/apply-template` + `/tokenize`), and every later call goes back to that checkpoint instead of processing the context again. The run log shows it as *Primed the shared context*. Measured on all three models: the second call of a two-file run reused 2,467 of 3,501 prompt tokens with priming and 305 without.
  * **Prewarm** (Settings → *Prewarm*, off by default): while you review the spec, Relay evaluates everything before the spec (instructions, read-only context, files to edit) into the loaded model's cache, so the run only has to process the spec. It never loads a model, skips while a run is active, and editing only the spec doesn't trigger it again. Uses the GPU for a few seconds after you pick files or load a model. llama-server only (LM Studio and Ollama have no `/apply-template`).
  * **Verification:** the commands the run will execute, from the spec's `## Verification` section, or — without one — detected from the project (`Cargo.toml` → `cargo build` / `cargo test`, `package.json` → `npm run build` / `npm test` / `npm ci`, Python → `python -m compileall -q .` / pytest or unittest, `go.mod`, `.sln`/`.csproj`, `CMakeLists.txt`, `pom.xml`, Gradle wrapper). Editing that section is how you approve or change what runs.
* **③ Result:** live progress of the loop (steps, the running command's output, the planner's review as it streams), then the loop report (each step, every command's exit code and output) and per-file side-by-side diffs (Monaco) against the project as it was. The right side is editable before you accept. **Accept / Reject** per file or **Accept all**. If the project is a git repo with uncommitted changes, Relay warns before the first write and offers a **checkpoint commit**. Files changed on disk since the run read them are not overwritten without asking.
  * **Re-run with another model** sends the same spec to a different profile (switching models as needed); **Compare with** shows the two models' proposals diffed against each other, with speed/time/attempt stats.
  * The final **review** (PASS / NEEDS_FIXES) is shown with the report; a NEEDS_FIXES review that ran out of follow-up passes keeps its follow-up spec, which you can load with one click. Runs without the loop (verification and review off) can still be reviewed after accepting.
* **History:** every run is saved (instruction, spec, model profile + model file, raw local-model output including stripped reasoning, validation results, diffs, accept/reject decisions, timings, review). Reopen a job to look at it or re-run it.

### The loop: staging, verification, fix rounds, review

* **Staging copy:** `%LOCALAPPDATA%\Relay\work\<project>-<hash>\src`, a mirror of the project (`.gitignore` rules apply; `.git`, `node_modules` and caches are skipped; `node_modules` is linked with a junction). It is kept between runs: syncing copies only changed files and deletes removed ones, while build output in the mirror (e.g. `target\`) stays, so builds are incremental. Files a run wrote are restored from the project at the next sync, with a fresh timestamp so incremental builds notice. The project itself is only read until you accept files.
* **Verification:** the commands run in order (setup → build → test → run) with `cmd /C` in the staging copy: hidden console, no stdin, `CI=1`, `NO_COLOR=1`, the whole process tree in a job object that is killed on timeout or Cancel. Setup/build/test pass on exit code 0 within the build timeout (default 900 s). A **run** command passes if the program exits with 0 *or is still running at its timeout* (default 10 s; a game or server that started fine), and, with `expect "text"`, only if its output contains that text. GUI programs open their window for those seconds.
* **Fix rounds** (default up to 3): the coder gets the failing step's output — progress noise (`Compiling …`, `added N packages`) removed, the region from the first error line plus the end, ≤12k characters — and the current content of the files the output names (`--> src/main.rs:3:5`, `File "app.py", line 2`, `src/a.ts(3,5)`, …) and of the files changed so far, and answers with SEARCH/REPLACE blocks (`fix.md`). Same validation and automatic retry as the first pass. A failing setup step (e.g. `npm ci` without network) is not sent to the coder.
* **Review** (Settings → Roles & loop): the planner gets the original spec, the diff against the project, the changed files and the verification summary (`review.md`), and can read the staging copy with its tools. NEEDS_FIXES with a follow-up spec starts a follow-up pass (default at most 1): the coder implements it on top of the staging copy, then verification and review run again.
* **History** keeps every step, command output and the final review with the run.

### Autopilot secure mode

Without the spec review, nobody approves what a run executes: the planner's `## Verification` commands and the code the coder wrote (build scripts, tests, the program). Secure mode (Settings → Roles & loop → *Secure mode*, on by default) guards autopilot runs in two ways:

* **Allowlist.** Autopilot only starts implementing when every verification command is one program from a list of build and test tools, used for building or testing: `cargo build|check|test|run|clippy|fmt|…`, `npm ci|install|test|run|start` (installing only what the project lists, never `npm install <pkg>`), `pnpm`/`yarn` likewise, `npx tsc|vitest|jest|eslint|prettier|…` (no `-y`/`--package`), `node <script>` (no `-e`), `python -m pytest|unittest|compileall|mypy|ruff|…` or `python <script>.py` (no `-c`, no `pip`), `go build|test|vet|run`, `dotnet build|test|run|restore`, `cmake` (no `-E`/`-P`), `ctest`, `msbuild`, `make`, `ninja`, `mvn`, `gradle(w)`, `deno`, `bun`, `zig`, or a program inside the project (`target\debug\app.exe`). Shell operators (`& | < > ^ % !`), script hosts (`.ps1`, `.vbs`, …), paths outside the project and every other program are refused. The commands Relay detects itself are always on the list. If a command isn't, autopilot stops at the spec: the Spec pane marks the command and says why, and **Implement** runs it after your review. Runs started by autopilot are checked again in the backend before any model loads.
* **Sandbox.** The commands run with a restricted token (all privileges removed) at **Low integrity**, in a job with UI limits. Windows' mandatory integrity control then denies every write outside the folders Relay labels Low: the staging copy and `%LOCALAPPDATA%\Relay\sandbox\`. So a build script, test or program can't change or delete your project, your profile, other folders or registry keys (beyond `HKCU\Software\AppDataLow`). It also can't write through the `node_modules` junction into the real project, open or send messages to your other processes and windows, use the clipboard, change system or display settings, or log off. Tool caches that normally live in your profile are moved into the sandbox folder (`CARGO_HOME` with a copy of your Cargo `config.toml`, the npm, Yarn, pip and Python bytecode caches, Go, NuGet, the .NET CLI home, Gradle, Maven, Deno, Bun, and `TEMP`). The first sandboxed build therefore downloads its dependencies again. `CARGO_TARGET_DIR` is unset so build output stays in the staging copy. If the sandbox can't be set up, the run stops instead of running unsandboxed.
* **Not covered:** sandboxed processes can still **read** what your account can read, and use the **network** (dependency downloads need it). MSYS/Cygwin programs (Git for Windows' `bash`, `sh`, `whoami`, …) don't start at Low integrity, so scripts that call them fail in the sandbox. The diff still waits for you to accept it: secure mode never writes to the project.
* *Sandbox reviewed runs too* (off by default) also sandboxes runs whose spec you reviewed.

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

* **Duplicate** a built-in to experiment (e.g. a different quant, context or `--n-cpu-moe`), or **Add** a blank one. **Reset to default** restores a built-in; built-ins can't be deleted. When an update changes a built-in's default launch arguments or context limit, a built-in that still has the previous default there takes the new one; values you changed are kept.
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

* `expand.md` — instruction → spec (the planner, local or Claude). The planner acts as the designer: sections *Summary, Design, Files, Changes, Edge cases, Acceptance criteria, Verification*. **Design** is the class-diagram view (elements with responsibilities, attributes and exact signatures; relationships and dependency direction; contracts; 2–5 decisions with rationale and the rejected alternative; optionally one Mermaid `classDiagram`). It is sent with every per-file call, so separate calls agree on cross-file names and signatures. The per-file **Changes** say which elements a file defines and what they must guarantee, not how they work. No code, pseudo-code or algorithms: the Spec pane warns about a multi-line code block (Mermaid diagrams excepted) and about inline code that is an implementation rather than a name, signature or value (calls nested in calls, calls with arithmetic, `**`, arithmetic between names; a quantity like `max_retries + 1` is fine). **Verification** lists `- build: `command``, `- test: …`, `- setup: …`, `- run: `command` (timeout 10s, expect "text")`, or `- None.`; the commands Relay detected are offered as the default. Placeholders: `{{instruction}} {{model_name}} {{context_limit}} {{project_name}} {{file_tree}} {{context_files}} {{project_outline}} {{explore_note}} {{budget_guidance}} {{verification_defaults}}`. The budget guidance is computed from the coder profile's context limit so the planner writes tighter, self-contained per-file sections for smaller contexts.
  Everything above the `{{instruction}}` line (including its heading) is sent as the system prompt, the instruction section as the user message, so Claude's prompt cache covers the template, file tree and selected files across expansions of a project. Keep `{{instruction}}` near the end; anything after it is not cached. The Spec pane shows the tokens read from the cache.
* `implement-system.md` — the coder's system prompt (edit-format rules).
* `review.md` — review prompt. Placeholders: `{{spec}} {{diff}} {{files_after}} {{verification}}`.
* `fix.md` — the coder's fix round. Placeholders: `{{files}} {{spec}} {{failure}}`.

## Claude backends (optional planner)

Settings → Roles & loop → Planner → *Claude* uses these for the spec and the reviews; the coder is always local.

* **CLI (default):** `claude -p --output-format stream-json --verbose --include-partial-messages --no-session-persistence --model <id> --effort <level>`, prompt on **stdin** (Windows limits command lines to 32K chars), working directory = the project. Tools are disabled (`--tools ""`) unless *Explore project* is on (`--tools Read,Glob,Grep`). The stable part of the expansion prompt goes in with `--append-system-prompt-file`. Cancelling kills the whole CLI process tree.
* **API:** `POST https://api.anthropic.com/v1/messages` (`anthropic-version: 2026-10-21`), streaming, `thinking: {type: "adaptive"}`, `output_config.effort`, `cache_control` on the system block (the stable part of the expansion prompt), and server-side refusal fallbacks (`fallbacks: "default"`, beta `server-side-fallback-2026-07-01`). Default model `claude-opus-5-5`; effort default `high`.

## Data locations

`%LOCALAPPDATA%\Relay\`: `settings.json`, `profiles.json`, `history.db` (SQLite), `templates\`, `work\` (staging copies, safe to delete), `sandbox\` (tool caches and TEMP of sandboxed commands, safe to delete), `run\llama-server.pid`, `llama.cpp\` (if downloaded), `models\` (default models folder).

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
cargo run --example e2e -- fake      :: backend end-to-end against a fake llama-server (no GPU), incl. the planner/coder loop and secure autopilot
cargo run --example e2e -- claude    :: + real Claude CLI expansion and review
cargo run --example e2e -- real      :: real llama-server + the three real models (uses the GPU!)
cargo run --example e2e -- loop-real :: the loop on the real models: local planner on all three, Qwen3.6 secure autopilot loop, a forced cargo fix round (GPU!); specs + reports in %TEMP%elay-e2eeal-loop
cargo run --release --example bench  :: launch-argument variants of the three profiles, ≈30 min (GPU!); report in %TEMP%\relay-bench
```

Unit tests cover the SEARCH/REPLACE parser and applier, context budgeting, context outlines (all nine grammars), path safety (including junction escapes), per-profile reasoning suppression and stripping, the planner role's thinking parameters, tool-call streaming and the planner's read-only tools, spec parsing (including Verification), command detection, running commands (exit codes, timeouts, cancel; plain and sandboxed), the secure-mode allowlist (every detected command allowed, shell operators and other programs refused), the Low integrity sandbox (runs at Low, writes only the staging copy, no writes through a junction, sandboxed `cargo test`), output condensing and error-path extraction, the staging copy, prompt templates, SSE parsing, server arguments and error diagnosis, history, and resumable/verified downloads.

`tools/fake-llama-server` is a GPU-free stand-in for llama-server (same CLI flags, log lines, `/health`, `/tokenize` with decodable token ids, `/apply-template`, `/completion` with `n_predict: 0`, streaming `/v1/chat/completions` with timings, including a one-slot prompt cache reported as `cache_n`; `--fake-recurrent` makes the cache behave like a hybrid model's, reusable only up to a checkpoint). It can be scripted (`FAKE_LLM_SCRIPT=rules.json`, including tool calls and rules that apply only after a tool result) or synthesizes valid edits for any spec; `--fake-fail`, `--fake-reasoning`, `--fake-tps N`, `--fake-load-ms N` simulate failures, gpt-oss-style reasoning and speed.

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
  planner.rs              local planner: thinking + read-only tools loop, source overview
  workflow.rs             staging → implement → verify ⇄ fix → review → follow-up
  verify.rs               command detection, running (timeouts, job objects), output digest
  sandbox.rs              autopilot secure mode: command allowlist, Low integrity sandbox
  staging.rs              the staging copy of the project
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
* **Claude's spec contains code / looks like an older format** — you are probably running an old build. Settings shows `Relay <version> · <commit> <date>` in its header; compare it with `git log -1`. Rebuild with `npm run tauri build` (or run the new installer) after pulling changes. On first start a new build replaces prompt templates you never edited with the new defaults; edited ones are kept (Settings → Prompt templates → *Reset to default* restores it).
* **"Claude Code CLI not found"** — only matters with Claude as the planner: make sure `claude --version` works in a new terminal, then restart Relay.
* **Verification fails on something the change didn't cause** (a tool missing from PATH, a project that didn't build before) — check the command's output in the loop report; fix the environment or edit the spec's `## Verification` section (`- None.` skips verification for that spec). The staging copy can be opened from the report.
* **The planner thinks for a long time / runs out of output tokens** — lower *Planner effort* (gpt-oss), untick large context files, or turn off *Project tools*.
* **Validation keeps failing** — open *Model calls* in the Result pane to see each attempt's raw output and the exact mismatch; try per-file mode, a tighter spec, or gpt-oss-20b.
