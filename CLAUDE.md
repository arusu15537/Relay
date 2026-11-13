# Build "Relay": a Windows desktop app for a plan → expand → implement coding pipeline

## Goal
Build a Windows desktop application that runs this pipeline:

1. **I type a short instruction** (e.g. "add a retry with backoff to the HTTP client").
2. **Claude expands it** into a detailed implementation spec: which files to touch, function signatures, step-by-step changes, edge cases, and acceptance criteria. The spec must be explicit enough that a weaker model can follow it without guessing.
3. **I review and edit the spec** in the app before anything runs.
4. **The spec is sent to a local LLM** running on my machine. The local model writes the code changes.
5. **The app shows the proposed changes as a diff.** I accept or reject each file, and accepted changes are written to the target project folder.
6. *(Optional, toggle)* **Claude reviews the applied diff** against the spec and either flags problems or produces a follow-up spec to fix them.

## My environment
- Windows 11, RTX 5070 Ti (16 GB VRAM), Ryzen 9 5950X, 32 GB DDR4 RAM.
- Only **one local model fits in memory at a time**, so switching models means unloading one and loading the other.

## Local models: built-in profiles with a toggle
The app ships with three **model profiles** and a model switcher in the main toolbar (dropdown plus status: unloaded / loading / ready / error). Switching unloads the current model, loads the new one, and shows load progress. Each profile stores: display name, GGUF file path, Hugging Face source repo, launch arguments, context limit, sampling defaults, and how to suppress reasoning.

| Profile | Role | Suggested quant | Placement on 16 GB | Default context | Reasoning control |
|---|---|---|---|---|---|
| **Qwen3.8-27B** (dense) | Best quality, default | IQ4_XS (~15.7 GB) | Fully on GPU | 32k | Disable thinking: `chat_template_kwargs: {"enable_thinking": false}`, with `/no_think` as an alternative |
| **Qwen3.6-35B-A3B** (MoE) | Fast, long context | Q4_K_M (~20 GB) | Attention on GPU, experts in RAM via llama.cpp `--n-cpu-moe N` (tune N so VRAM stays under ~15 GB) | 64k | Same as Qwen3.8 |
| **gpt-oss-20b** | Most reliable format-following, fallback | Native MXFP4 (~12–13 GB) | Fully on GPU | 32k+ | Thinking can't be fully turned off: set reasoning effort to **low** (`reasoning_effort: "low"` via chat template kwargs or the request) and strip reasoning output from the result |

Requirements for the profile system:
- **Verify before hard-coding.** Check on Hugging Face the exact repo names, available quant files and file sizes for each model, plus the correct parameters for disabling or lowering thinking in current llama.cpp. My numbers above come from secondary sources and may be off. Tell me what you found.
- **Model files:** let me point each profile at a GGUF already on disk. Also offer an in-app download from the profile's Hugging Face repo, with progress, resume support, and a SHA256 check, but **always show me the file name and size and ask before downloading**. Store models in a configurable folder (default `%LOCALAPPDATA%\Relay\models`).
- **Server management:** the app bundles or locates `llama-server.exe` (a CUDA build). It starts the server for the selected profile on a local port, waits for its `/health` endpoint, stops it cleanly on switch or exit, and kills orphaned processes on startup. Show the server log in a collapsible panel.
- **External mode:** as an alternative to the managed server, a profile can point at an existing OpenAI-compatible endpoint (LM Studio, Ollama, or a llama-server I started myself). In that mode the app skips process management.
- **Custom profiles:** I can duplicate, edit, add and delete profiles in settings. The three built-ins can be reset to their defaults.
- **Per-run choice:** the model used is recorded with each run in history. A "re-run with another model" button re-sends the same spec to a different profile so I can compare results side by side.
- **Auto-fallback (toggle, off by default):** if the selected model's output fails edit-format validation twice, offer to retry the same spec with gpt-oss-20b.

## Tech stack
- **Tauri 2 + React + TypeScript** for the UI, with Rust for the backend (file I/O, process management, HTTP, downloads). If you have a strong reason to prefer .NET 8 WinUI 3 instead, tell me before you start.
- Package as a Windows installer (MSI or NSIS via Tauri's bundler).

## Claude backends for the expansion and review steps
Support both, selectable in settings:
- **Claude Code CLI (default):** run `claude -p "<prompt>" --output-format json` as a subprocess. This uses my existing Claude subscription. Detect whether `claude` is on PATH and show a clear error if it isn't.
- **Anthropic API:** use an API key stored with Windows Credential Manager (never in plain-text config). Default model `claude-opus-5-5`, configurable.
- The expansion prompt should take the **active local model's context limit** into account, so that Claude writes tighter specs, or splits the work into per-file specs, for smaller-context profiles.

## Core features
- **Project picker:** choose the target repo folder. Show a file tree, and let me tick which files to include as context. The app should also auto-include any files the spec names.
- **Expansion prompt:** keep the system prompt that turns my instruction into a spec in an editable template file. Have Claude output the spec in a fixed structure (Markdown with sections: Summary, Files, Changes per file, Edge cases, Acceptance criteria) so the app can parse the file list.
- **Context budgeting:** estimate tokens before sending to the local model, using the active profile's context limit. If the job doesn't fit, warn me. Offer to split it per file (one local-model call per file in the spec), and make that the default when the job is too large.
- **Edit format:** the local model must return changes as **search/replace blocks** (Aider-style `<<<<<<< SEARCH / ======= / >>>>>>> REPLACE` with a file path header), and whole-file content only for new files. Validate that every SEARCH block matches the file exactly. If one doesn't, send one automatic retry with the error, then surface the failure to me.
- **Streaming:** stream output from both Claude and the local model into the UI, with a Cancel button. Show tokens/sec for the local model so I can compare profiles.
- **Diff viewer:** side-by-side diff per file (e.g. Monaco diff editor), with Accept / Reject per file and Accept all.
- **Safety:** never write outside the selected project folder. If the folder is a git repo, warn me when the working tree is dirty before applying changes, and offer to create a checkpoint commit.
- **History:** save every run (instruction, spec, model profile, local-model output, diffs, accept/reject decisions, timing) to a local SQLite database, with a history panel to reopen and re-run past jobs.
- **Settings:** model profiles (see above), models folder, llama-server path, Claude backend choice and model ID, the review-step toggle, and the auto-fallback toggle. Include a "Test" button for each backend and each profile.

## UI layout
One main window with the model switcher in the toolbar, and three panes laid out left to right: **Instruction → Spec (editable) → Result (diff)**. Each pane has a status indicator (idle / running / done / error). Keep the design clean and functional. Support dark mode, following the Windows theme.

## How to work
1. Start by confirming the plan and the stack with me in a short message, along with what you verified about the model repos and thinking parameters. Don't write a long design doc.
2. Build in vertical slices and get each one working end to end before polishing:
   a. Managed llama-server for one profile (Qwen3.8-27B), a hard-coded spec, and parsing and applying the search/replace blocks.
   b. Profile system and model switching for all three models, including external mode.
   c. Claude CLI expansion step.
   d. Spec editing, diff viewer, accept/reject.
   e. History, compare-models re-run, settings, Anthropic API backend, review step, auto-fallback.
   f. Model downloads and the installer.
3. Write unit tests for the search/replace parser and applier, the context budgeting, the path-safety check, and the per-profile reasoning-suppression and reasoning-stripping logic.
4. After each slice, run the app and verify it actually works, including switching between all three models. Tell me plainly what you verified and what you couldn't (e.g. a model you couldn't download or load).
5. Put a README in the repo root covering setup: prerequisites, where to get each model, recommended llama-server flags per profile for a 16 GB card, and how to add a custom profile.