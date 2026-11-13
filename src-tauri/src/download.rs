//! Resumable downloads with progress and SHA256 verification (Hugging Face GGUFs, llama.cpp).
//!
//! Relay never downloads without the UI first showing the file name and size and the user
//! confirming; these functions are only called after that confirmation.

use crate::events::{Sink, emit};
use anyhow::{Context, Result, anyhow, bail};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct HfFile {
    pub path: String,
    pub size: u64,
    pub sha256: Option<String>,
}

pub fn hf_url(repo: &str, file: &str) -> String {
    format!("https://huggingface.co/{repo}/resolve/main/{file}")
}

/// List `.gguf` files in a Hugging Face repo (sizes and SHA256 come from the LFS metadata).
pub async fn list_repo_ggufs(http: &reqwest::Client, repo: &str) -> Result<Vec<HfFile>> {
    let url = format!("https://huggingface.co/api/models/{}/tree/main?recursive=true", repo.trim());
    let resp = http
        .get(&url)
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .context("contacting huggingface.co")?;
    if !resp.status().is_success() {
        bail!("Hugging Face returned {} for {repo}", resp.status());
    }
    let v: serde_json::Value = resp.json().await?;
    let mut files: Vec<HfFile> = v
        .as_array()
        .ok_or_else(|| anyhow!("unexpected response from Hugging Face"))?
        .iter()
        .filter(|f| f["type"] == "file")
        .filter_map(|f| {
            let path = f["path"].as_str()?.to_string();
            if !path.to_lowercase().ends_with(".gguf") {
                return None;
            }
            Some(HfFile {
                path,
                size: f["size"].as_u64().unwrap_or(0),
                sha256: f["lfs"]["oid"].as_str().map(String::from),
            })
        })
        .collect();
    files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(files)
}

/// Look up one file's size and hash (used to fill/verify profile metadata before asking).
pub async fn hf_file_info(http: &reqwest::Client, repo: &str, file: &str) -> Result<HfFile> {
    list_repo_ggufs(http, repo)
        .await?
        .into_iter()
        .find(|f| f.path == file)
        .ok_or_else(|| anyhow!("`{file}` not found in {repo}"))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DownloadOutcome {
    pub path: String,
    pub bytes: u64,
    pub sha256: String,
    pub verified: bool,
    pub resumed_from: u64,
    pub cancelled: bool,
}

fn progress(sink: &Sink, id: &str, phase: &str, done: u64, total: u64, bps: f64) {
    emit(
        sink,
        "download-progress",
        &json!({ "id": id, "phase": phase, "done": done, "total": total, "bytesPerSec": bps }),
    );
}

async fn hash_existing(path: &Path, hasher: &mut Sha256, sink: &Sink, id: &str, total: u64) -> Result<u64> {
    let mut f = tokio::fs::File::open(path).await?;
    let mut buf = vec![0u8; 4 << 20];
    let mut n_total = 0u64;
    let mut last = Instant::now();
    loop {
        let n = f.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        n_total += n as u64;
        if last.elapsed() > Duration::from_millis(300) {
            progress(sink, id, "hashing-partial", n_total, total, 0.0);
            last = Instant::now();
        }
    }
    Ok(n_total)
}

/// Download `url` to `dest`, resuming from `dest.part` if present, verifying SHA256 if given.
#[allow(clippy::too_many_arguments)]
pub async fn download(
    http: &reqwest::Client,
    id: &str,
    url: &str,
    dest: &Path,
    expected_sha: Option<&str>,
    expected_size: Option<u64>,
    sink: &Sink,
    cancel: &CancellationToken,
) -> Result<DownloadOutcome> {
    if let Some(dir) = dest.parent() {
        tokio::fs::create_dir_all(dir)
            .await
            .with_context(|| format!("creating {}", dir.display()))?;
    }
    let part = PathBuf::from(format!("{}.part", dest.display()));
    let mut hasher = Sha256::new();
    let mut have = 0u64;
    if part.exists() {
        have = hash_existing(&part, &mut hasher, sink, id, expected_size.unwrap_or(0)).await?;
    }
    let resumed_from = have;
    let mut req = http.get(url);
    if have > 0 {
        req = req.header(reqwest::header::RANGE, format!("bytes={have}-"));
    }
    let resp = req.send().await.with_context(|| format!("requesting {url}"))?;
    let status = resp.status();
    if status.as_u16() == 416 && expected_size == Some(have) {
        // Already complete.
    } else if !status.is_success() {
        bail!("Download failed: HTTP {status} for {url}");
    }
    let partial = status.as_u16() == 206;
    let mut file = if partial {
        tokio::fs::OpenOptions::new().append(true).open(&part).await?
    } else {
        // Server ignored the range (or fresh start): restart from zero.
        hasher = Sha256::new();
        have = 0;
        tokio::fs::File::create(&part).await?
    };
    let total = if partial {
        have + resp.content_length().unwrap_or(0)
    } else {
        resp.content_length().or(expected_size).unwrap_or(0)
    };
    let mut stream = resp.bytes_stream();
    let started = Instant::now();
    let start_have = have;
    let mut last = Instant::now();
    let mut cancelled = false;
    if status.as_u16() != 416 {
        loop {
            let next = tokio::select! {
                n = stream.next() => n,
                _ = cancel.cancelled() => { cancelled = true; None }
            };
            let Some(chunk) = next else { break };
            let chunk = chunk.map_err(|e| anyhow!("Download interrupted (resume later): {e}"))?;
            file.write_all(&chunk).await?;
            hasher.update(&chunk);
            have += chunk.len() as u64;
            if last.elapsed() > Duration::from_millis(250) {
                let bps = (have - start_have) as f64 / started.elapsed().as_secs_f64().max(0.001);
                progress(sink, id, "downloading", have, total, bps);
                last = Instant::now();
            }
        }
    }
    file.flush().await?;
    drop(file);
    if cancelled {
        progress(sink, id, "cancelled", have, total, 0.0);
        return Ok(DownloadOutcome {
            path: part.to_string_lossy().into_owned(),
            bytes: have,
            sha256: String::new(),
            verified: false,
            resumed_from,
            cancelled: true,
        });
    }
    if let Some(sz) = expected_size.filter(|s| *s > 0)
        && have != sz
    {
        bail!("Download incomplete: got {have} of {sz} bytes. Start it again to resume.");
    }
    let digest = hex::encode(hasher.finalize());
    let verified = match expected_sha {
        Some(exp) if !exp.is_empty() => {
            if !digest.eq_ignore_ascii_case(exp) {
                let _ = tokio::fs::remove_file(&part).await;
                progress(sink, id, "failed", have, total, 0.0);
                bail!(
                    "SHA256 mismatch for {}: expected {exp}, got {digest}. The partial file was deleted.",
                    dest.display()
                );
            }
            true
        }
        _ => false,
    };
    tokio::fs::rename(&part, dest)
        .await
        .with_context(|| format!("moving download into place at {}", dest.display()))?;
    progress(sink, id, "done", have, total, 0.0);
    Ok(DownloadOutcome {
        path: dest.to_string_lossy().into_owned(),
        bytes: have,
        sha256: digest,
        verified,
        resumed_from,
        cancelled: false,
    })
}

/// Hash a file on disk (for "verify existing file").
pub async fn sha256_file(path: &Path, sink: &Sink, id: &str) -> Result<String> {
    let mut h = Sha256::new();
    let total = tokio::fs::metadata(path).await.map(|m| m.len()).unwrap_or(0);
    hash_existing(path, &mut h, sink, id, total).await?;
    Ok(hex::encode(h.finalize()))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LlamaRelease {
    pub tag: String,
    pub server_zip: (String, String, u64),
    pub cudart_zip: Option<(String, String, u64)>,
}

/// Find the newest llama.cpp release with Windows x64 CUDA 13 assets (name, url, size).
pub async fn latest_llama_release(http: &reqwest::Client) -> Result<LlamaRelease> {
    let v: serde_json::Value = http
        .get("https://api.github.com/repos/ggml-org/llama.cpp/releases?per_page=10")
        .header("User-Agent", "Relay-desktop")
        .header("Accept", "application/vnd.github+json")
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    for rel in v.as_array().into_iter().flatten() {
        let assets = rel["assets"].as_array().cloned().unwrap_or_default();
        let pick = |pred: &dyn Fn(&str) -> bool| {
            assets.iter().find(|a| pred(a["name"].as_str().unwrap_or(""))).map(|a| {
                (
                    a["name"].as_str().unwrap_or("").to_string(),
                    a["browser_download_url"].as_str().unwrap_or("").to_string(),
                    a["size"].as_u64().unwrap_or(0),
                )
            })
        };
        let server = pick(&|n: &str| n.starts_with("llama-") && n.contains("-bin-win-cuda-13") && n.ends_with("-x64.zip"));
        if let Some(server) = server {
            let cudart = pick(&|n: &str| n.starts_with("cudart-") && n.contains("win-cuda-13") && n.ends_with("-x64.zip"));
            return Ok(LlamaRelease {
                tag: rel["tag_name"].as_str().unwrap_or("").to_string(),
                server_zip: server,
                cudart_zip: cudart,
            });
        }
    }
    bail!("No llama.cpp release with a Windows CUDA 13 x64 build was found")
}

/// Extract a zip into `dir` (flat or nested), rejecting entries that escape `dir`.
pub fn extract_zip(zip_path: &Path, dir: &Path) -> Result<usize> {
    let f = std::fs::File::open(zip_path)?;
    let mut z = zip::ZipArchive::new(f)?;
    std::fs::create_dir_all(dir)?;
    let mut n = 0;
    for i in 0..z.len() {
        let mut entry = z.by_index(i)?;
        let Some(rel) = entry.enclosed_name() else { continue };
        let out = dir.join(rel);
        if entry.is_dir() {
            std::fs::create_dir_all(&out)?;
            continue;
        }
        if let Some(p) = out.parent() {
            std::fs::create_dir_all(p)?;
        }
        let mut w = std::fs::File::create(&out)?;
        std::io::copy(&mut entry, &mut w)?;
        n += 1;
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::events::NullSink;
    use std::io::{BufRead, BufReader, Write};
    use std::sync::Arc;

    /// Tiny HTTP server serving `data`, honouring `Range: bytes=N-` unless `ignore_range`.
    fn serve(data: Vec<u8>, ignore_range: bool) -> String {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        std::thread::spawn(move || {
            for s in l.incoming().flatten() {
                let mut r = BufReader::new(s.try_clone().unwrap());
                let mut start = 0usize;
                loop {
                    let mut line = String::new();
                    if r.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                        break;
                    }
                    if let Some(v) = line.to_ascii_lowercase().strip_prefix("range: bytes=") {
                        start = v.trim().trim_end_matches('-').parse().unwrap_or(0);
                    }
                }
                let mut s = s;
                if start > 0 && !ignore_range {
                    let body = &data[start..];
                    let _ = write!(
                        s,
                        "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes {}-{}/{}\r\nConnection: close\r\n\r\n",
                        body.len(),
                        start,
                        data.len() - 1,
                        data.len()
                    );
                    let _ = s.write_all(body);
                } else {
                    let _ = write!(s, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", data.len());
                    let _ = s.write_all(&data);
                }
            }
        });
        format!("http://{addr}/model.gguf")
    }

    fn payload() -> (Vec<u8>, String) {
        let data: Vec<u8> = (0..300_000u32).map(|i| (i * 31 % 251) as u8).collect();
        let sha = hex::encode(Sha256::digest(&data));
        (data, sha)
    }

    #[tokio::test]
    async fn downloads_resumes_and_verifies() {
        let (data, sha) = payload();
        let sink: Sink = Arc::new(NullSink);
        let http = reqwest::Client::new();
        let dir = tempfile::tempdir().unwrap();

        // Fresh download.
        let url = serve(data.clone(), false);
        let dest = dir.path().join("a.gguf");
        let out = download(
            &http,
            "t",
            &url,
            &dest,
            Some(&sha),
            Some(data.len() as u64),
            &sink,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(out.verified && out.resumed_from == 0);
        assert_eq!(std::fs::read(&dest).unwrap(), data);

        // Resume from a partial file.
        let dest = dir.path().join("b.gguf");
        std::fs::write(dir.path().join("b.gguf.part"), &data[..120_000]).unwrap();
        let out = download(
            &http,
            "t",
            &url,
            &dest,
            Some(&sha),
            Some(data.len() as u64),
            &sink,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(out.resumed_from, 120_000);
        assert!(out.verified);
        assert_eq!(std::fs::read(&dest).unwrap(), data);
        assert!(!dir.path().join("b.gguf.part").exists());

        // Server that ignores Range: restart from zero, still correct.
        let url2 = serve(data.clone(), true);
        let dest = dir.path().join("c.gguf");
        std::fs::write(dir.path().join("c.gguf.part"), &data[..50_000]).unwrap();
        let out = download(
            &http,
            "t",
            &url2,
            &dest,
            Some(&sha),
            Some(data.len() as u64),
            &sink,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(out.verified);
        assert_eq!(std::fs::read(&dest).unwrap(), data);

        // Wrong hash → error, partial removed, nothing at dest.
        let dest = dir.path().join("d.gguf");
        let err = download(
            &http,
            "t",
            &url,
            &dest,
            Some(&"0".repeat(64)),
            Some(data.len() as u64),
            &sink,
            &CancellationToken::new(),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("SHA256 mismatch"));
        assert!(!dest.exists() && !dir.path().join("d.gguf.part").exists());
    }

    #[test]
    fn hf_url_format() {
        assert_eq!(
            hf_url("unsloth/Qwen3.8-27B-GGUF", "Qwen3.8-27B-UD-IQ4_XS.gguf"),
            "https://huggingface.co/unsloth/Qwen3.8-27B-GGUF/resolve/main/Qwen3.8-27B-UD-IQ4_XS.gguf"
        );
    }
}
