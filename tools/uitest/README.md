# UI tests (WebView2 + Chrome DevTools Protocol)

These scripts drive the real Relay window: clicks, typing and screenshots go through CDP, so the
window can stay in the background (no screen capture, no mouse).

1. Start Relay in dev mode with remote debugging and an isolated data folder:

   ```powershell
   $env:RELAY_APP_DIR = "$env:TEMP\relay-uitest\appdata"      # seed settings.json / profiles.json here
   $env:WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS = "--remote-debugging-port=9223"
   npm run tauri dev
   ```

   Dev builds expose `window.__relay = { useStore, actions }` for the scripts (not present in release builds).

2. Run a scenario (screenshots go to the given folder):

   ```bat
   node tools\uitest\uitest-fake.mjs   C:\tmp\shots   :: fake llama-server profiles: spec → implement → diff → accept → re-run → compare → git checkpoint → switch
   node tools\uitest\uitest-claude.mjs C:\tmp\shots   :: real Claude CLI expansion + review (fake local model)
   node tools\uitest\uitest-prewarm.mjs C:\tmp\shots  :: exact token counts + prewarm (needs a `fake-recurrent` profile, see below)
   node tools\uitest\uitest-real.mjs   C:\tmp\shots   :: real Claude + all three real models (GPU)
   node tools\uitest\uitest-loop.mjs   C:\tmp\shots   :: local planner (tool call) → verification chips → build fails → fix → review → follow-up → PASS → accept; autopilot (secure mode pauses at the spec, then without secure mode)
   ```

`uitest-loop.mjs` scripts the fake server's answers: start Relay with `$env:FAKE_LLM_SCRIPT = "$env:TEMP\relay-uitest\fake-script.json"`
(the scenario writes its rules there and empties the file afterwards). It uses its own project in `%TEMP%\relay-uitest\loop-project`
and switches back to the previous project at the end. The other fake scenarios turn verification and the loop review off, so they
test the plain implement path.

For the fake scenarios, point `llamaServerPath` at `tools\fake-llama-server\target\release\fake-llama-server.exe`
and give each profile a dummy `.gguf` file. Extra flags such as `--fake-tps 60 --fake-reasoning` go in the profile's
launch arguments; `uitest-fake.mjs` expects `--spec-type draft-mtp,ngram-mod` in the Qwen3.8 profile (as in the built-in default), which makes the fake report draft stats. `uitest-prewarm.mjs` also needs a custom profile with id `fake-recurrent` whose launch arguments include
`--fake-recurrent`; it resets the demo project (`git reset --hard` to its first commit) before it runs.
