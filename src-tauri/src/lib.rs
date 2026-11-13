pub mod app;
pub mod budget;
pub mod claude;
pub mod commands;
pub mod config;
pub mod download;
pub mod editformat;
pub mod events;
pub mod history;
pub mod llm;
pub mod outline;
pub mod pathsafe;
pub mod pipeline;
pub mod project;
pub mod reasoning;
pub mod server;
pub mod spec;
pub mod templates;
pub mod winproc;

use std::sync::Arc;
use tauri::{Emitter, Manager, RunEvent};

/// Forwards backend events to the webview.
struct TauriSink(tauri::AppHandle);

impl events::EventSink for TauriSink {
    fn emit_json(&self, event: &str, payload: serde_json::Value) {
        let _ = self.0.emit(event, payload);
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let app = tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
            let sink: events::Sink = Arc::new(TauriSink(app.handle().clone()));
            let core = Arc::new(app::AppCore::new(sink)?);
            app.manage(core);
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::get_app_state,
            commands::save_settings,
            commands::save_profiles,
            commands::reset_profile,
            commands::switch_profile,
            commands::stop_server,
            commands::server_status,
            commands::server_log,
            commands::gpu_memory,
            commands::test_profile,
            commands::test_claude,
            commands::list_files,
            commands::read_project_file,
            commands::git_status,
            commands::git_checkpoint,
            commands::expand,
            commands::parse_spec_cmd,
            commands::prepare_implement,
            commands::implement,
            commands::cancel_job,
            commands::accept_file,
            commands::reject_file,
            commands::review_run,
            commands::history_list,
            commands::history_get,
            commands::history_delete,
            commands::templates_list,
            commands::template_save,
            commands::template_reset,
            commands::set_api_key,
            commands::set_endpoint_key,
            commands::has_endpoint_key,
            commands::open_in_explorer,
            commands::path_exists,
            commands::model_file_status,
            commands::hf_list,
            commands::download_model,
            commands::verify_model,
            commands::llama_release,
            commands::download_llama,
        ])
        .build(tauri::generate_context!())
        .expect("error while building Relay");

    app.run(|handle, event| {
        if let RunEvent::Exit = event {
            // Unload the model before the process goes away (the job object is the backstop).
            if let Some(core) = handle.try_state::<Arc<app::AppCore>>() {
                tauri::async_runtime::block_on(core.shutdown());
            }
        }
    });
}
