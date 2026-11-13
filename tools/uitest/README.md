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
   node tools\uitest\uitest-real.mjs   C:\tmp\shots   :: real Claude + all three real models (GPU)
   ```

For the fake scenarios, point `llamaServerPath` at `tools\fake-llama-server\target\release\fake-llama-server.exe`
and give each profile a dummy `.gguf` file. Extra flags such as `--fake-tps 60 --fake-reasoning` go in the profile's
launch arguments.
