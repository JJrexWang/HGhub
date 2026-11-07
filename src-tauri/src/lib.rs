mod api;
mod download;
mod lines;
mod media;
mod net;
mod session;

use api::Api;
use download::DownloadManager;
use tauri::Manager;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .manage(Api::new())
        .manage(DownloadManager::new())
        .setup(|app| {
            if let Ok(dir) = app.path().app_data_dir() {
                let _ = std::fs::create_dir_all(&dir);
                // 上次选择的线路（设置 → 线路）在启动时恢复
                net::bind_base(&dir);
                let state: tauri::State<Api> = app.state();
                state.session.bind(dir.clone());
                // 下载索引（downloads/downloads.json）在启动时恢复
                let dl: tauri::State<DownloadManager> = app.state();
                dl.bind(app.handle().clone(), dir);
            }
            Ok(())
        })
        .register_asynchronous_uri_scheme_protocol("hg", |ctx, request, responder| {
            let app = ctx.app_handle().clone();
            let uri = request.uri().clone();
            let path = uri.path().to_string();
            let mut query = uri.query().unwrap_or("").to_string();
            // 自定义协议基址随平台不同，统一定义在 media::LOCAL_ORIGIN
            // （Windows/Android 是 http://<scheme>.localhost，macOS/iOS/Linux 是 <scheme>://localhost）。
            // 它同时是 m3u8 分片/key 改写用的前缀，必须与前端 src/api.ts 的 LOCAL 一致。
            if path.starts_with("/local") {
                // 本地文件供流要支持 Range：请求头里的 Range 透传给 download::handle_local（query 参数 r）
                if let Some(r) = request.headers().get(tauri::http::header::RANGE) {
                    if let Ok(s) = r.to_str() {
                        if !query.is_empty() {
                            query.push('&');
                        }
                        query.push_str("r=");
                        query.push_str(s);
                    }
                }
            }
            tauri::async_runtime::spawn(async move {
                let resp = if path.starts_with("/local") {
                    let dl: tauri::State<DownloadManager> = app.state();
                    download::handle_local(&dl, &path, &query, media::LOCAL_ORIGIN).await
                } else {
                    media::handle(&path, &query, media::LOCAL_ORIGIN).await
                };
                responder.respond(resp);
            });
        })
        .invoke_handler(tauri::generate_handler![
            api::app_boot,
            api::debug_log,
            api::net_diag,
            api::line_state,
            api::line_set,
            api::line_mirrors,
            api::line_probe,
            api::check_update,
            api::open_url,
            api::api_login,
            api::api_register,
            api::api_logout,
            api::api_me,
            api::api_feed,
            api::api_category,
            api::api_rank,
            api::api_search,
            api::cache_clear,
            api::api_posts,
            api::api_post,
            api::api_post_like,
            api::api_post_collect,
            api::api_post_comments,
            api::api_video,
            api::api_play,
            api::api_comments,
            api::api_comment_send,
            api::api_like,
            api::api_collect,
            api::api_follow,
            download::dl_enqueue,
            download::dl_enqueue_series,
            download::dl_list,
            download::dl_pause,
            download::dl_resume,
            download::dl_cancel,
            download::dl_remove,
            download::dl_remove_work,
            download::dl_stats,
            download::dl_open_dir,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
