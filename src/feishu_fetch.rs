//! Fetch cloud documents as local editable copies and run authenticated CLI requests.
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
#[cfg(windows)]
use std::os::windows::process::CommandExt;

use anyhow::{Context, Result, anyhow, bail};
use quick_xml::events::{BytesStart, Event};
use quick_xml::{Reader, Writer};
use serde_json::Value;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum FetchProgress {
    FetchingDocument,
    DownloadingAssets { completed: usize, total: usize },
    ParsingDocument,
}

pub(crate) struct Fetched {
    pub doc: crate::model::Document,
    pub url: String,
    pub document_id: String,
    pub revision_id: i64,
    pub original_xml: String,
}

// Never include raw CLI output in errors: it may contain credentials or document content.
pub(crate) fn run(args: &[&str], input: Option<&str>, cwd: Option<&Path>) -> Result<Value> {
    #[cfg(windows)]
    let executable = {
        let paths = std::env::var_os("PATH").unwrap_or_default();
        std::env::split_paths(&paths).find_map(|dir| {
            let direct = dir.join("lark-cli.exe");
            let npm = dir.join("node_modules").join("@larksuite").join("cli").join("bin").join("lark-cli.exe");
            if direct.is_file() { Some(direct) } else if dir.join("lark-cli.cmd").is_file() && npm.is_file() { Some(npm) } else { None }
        }).unwrap_or_else(|| PathBuf::from("lark-cli.cmd"))
    };
    #[cfg(not(windows))]
    let executable = PathBuf::from("lark-cli");
    #[cfg(windows)]
    if executable.extension().is_some_and(|ext| ext.eq_ignore_ascii_case("cmd")) {
        for arg in args {
            if arg.chars().any(|c| matches!(c, '%' | '!' | '"' | '\r' | '\n' | '&' | '|' | '<' | '>' | '^')) {
                bail!("lark-cli 参数包含 Windows 命令脚本不支持的字符");
            }
        }
    }
    let mut cmd = Command::new(executable);
    #[cfg(windows)]
    cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
    cmd.args(args).stdin(if input.is_some() { Stdio::piped() } else { Stdio::null() })
        .env("LARKSUITE_CLI_NO_UPDATE_NOTIFIER", "1")
        .env("LARKSUITE_CLI_NO_SKILLS_NOTIFIER", "1")
        .env("LARK_CLI_NO_PROXY", "1")
        .env("LARKSUITE_CLI_PROXY_ENABLE", "false")
        .env("NO_PROXY", "*")
        .env("no_proxy", "*")
        .env_remove("HTTP_PROXY")
        .env_remove("http_proxy")
        .env_remove("HTTPS_PROXY")
        .env_remove("https_proxy")
        .env_remove("ALL_PROXY")
        .env_remove("all_proxy");
    if let Some(cwd) = cwd { cmd.current_dir(cwd); }
    let mut child = cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().context("无法启动 lark-cli，请确认已安装且位于 PATH")?;
    if let Some(input) = input { child.stdin.take().unwrap().write_all(input.as_bytes()).context("无法向 lark-cli 发送请求")?; }
    let output = child.wait_with_output().context("无法读取 lark-cli 响应")?;
    let data: Option<Value> = serde_json::from_slice(&output.stdout).ok()
        .or_else(|| serde_json::from_slice(&output.stderr).ok());
    if !output.status.success() || data.as_ref().and_then(|v| v.get("ok")).and_then(Value::as_bool) != Some(true) {
        let error = data.as_ref().and_then(|v| v.get("error"));
        let safe = |key: &str| error.and_then(|e| e.get(key)).and_then(Value::as_str)
            .filter(|s| !s.is_empty() && s.len() <= 80 && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-'));
        let kind = safe("type").unwrap_or("unknown");
        let subtype = safe("subtype").unwrap_or("unknown");
        let code = error.and_then(|e| e.get("code")).and_then(Value::as_i64);
        bail!("lark-cli {} 失败（退出码 {:?}，类型 {kind}，原因 {subtype}，代码 {code:?}）；请检查安装、用户授权及文档权限", args.get(1).unwrap_or(&"命令"), output.status.code());
    }
    Ok(data.unwrap())
}

fn download(token: &str, kind: &str, name: Option<&str>, assets: &Path) -> Result<String> {
    if token.is_empty() { bail!("飞书素材缺少 token"); }
    let id = uuid::Uuid::new_v4().to_string();
    let dir = assets.join("feishu").join(&id);
    std::fs::create_dir_all(&dir).context("无法创建飞书素材目录")?;
    // A generated file name avoids accepting paths from document content. Preserve a
    // safe attachment extension; without one the CLI detects it from Content-Type.
    let ext = name.and_then(|n| n.rsplit_once('.').map(|(_, ext)| ext))
        .filter(|ext| !ext.is_empty() && ext.len() <= 16 && ext.bytes().all(|b| b.is_ascii_alphanumeric()));
    let filename = ext.map_or_else(|| "asset".to_string(), |ext| format!("asset.{ext}"));
    let output = format!("./{filename}");
    let result = (|| -> Result<String> {
        run(&["docs", "+media-download", "--as", "user", "--token", token, "--type", kind, "--output", &output], None, Some(&dir))
            .context("下载飞书素材失败")?;
        let mut entries = std::fs::read_dir(&dir).context("无法读取下载的飞书素材")?;
        let entry = entries.next().ok_or_else(|| anyhow!("飞书素材下载未生成文件"))??;
        if entries.next().is_some() || !entry.file_type()?.is_file() {
            bail!("飞书素材下载结果不是单个文件");
        }
        let name = entry.file_name().into_string().map_err(|_| anyhow!("飞书素材文件名不是有效 Unicode"))?;
        Ok(format!("assets/feishu/{id}/{name}"))
    })();
    if result.is_err() { let _ = std::fs::remove_dir_all(&dir); }
    result
}

struct Asset {
    token: String,
    kind: &'static str,
    name: Option<String>,
}

enum LocalizedEvent<'a> {
    Element { tag: String, attrs: Vec<(String, String)>, empty: bool, asset: Option<usize> },
    Other(Event<'a>),
}

fn download_assets(
    tasks: &[Asset],
    progress: &mut impl FnMut(FetchProgress),
    download_one: impl Fn(&Asset) -> Result<String> + Sync,
) -> Result<Vec<String>> {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::mpsc;

    let total = tasks.len();
    if total == 0 { return Ok(Vec::new()); }
    progress(FetchProgress::DownloadingAssets { completed: 0, total });

    let next = AtomicUsize::new(0);
    let failed = AtomicBool::new(false);
    std::thread::scope(|scope| {
        let (tx, rx) = mpsc::channel();
        for _ in 0..total.min(10) {
            let tx = tx.clone();
            let next = &next;
            let failed = &failed;
            let download_one = &download_one;
            scope.spawn(move || {
                while !failed.load(Ordering::Relaxed) {
                    let index = next.fetch_add(1, Ordering::Relaxed);
                    if index >= total { break; }
                    let result = download_one(&tasks[index]);
                    if result.is_err() { failed.store(true, Ordering::Relaxed); }
                    if tx.send((index, result)).is_err() { break; }
                }
            });
        }
        drop(tx);
        let mut paths = vec![None; total];
        let mut completed = 0;
        let mut errors = Vec::new();
        for (index, result) in rx {
            match result {
                Ok(path) => {
                    paths[index] = Some(path);
                    completed += 1;
                    progress(FetchProgress::DownloadingAssets { completed, total });
                }
                Err(err) => errors.push(err),
            }
        }
        if errors.len() == 1 { return Err(errors.pop().unwrap()); }
        if !errors.is_empty() {
            bail!("多个飞书素材下载失败：{}", errors.iter().map(|err| format!("{err:#}")).collect::<Vec<_>>().join("；"));
        }
        Ok(paths.into_iter().map(|path| path.expect("each asset completed")).collect())
    })
}

fn localize(
    xml: &str,
    refs: &Value,
    progress: &mut impl FnMut(FetchProgress),
    download_one: impl Fn(&Asset) -> Result<String> + Sync,
) -> Result<String> {
    let mut reader = Reader::from_str(xml);
    let mut events = Vec::new();
    let mut tasks = Vec::new();
    loop {
        let event = reader.read_event().context("飞书返回的 DocxXML 无效")?;
        match event {
            event @ (Event::Start(_) | Event::Empty(_)) => {
                let empty = matches!(&event, Event::Empty(_));
                let e = match event { Event::Start(e) | Event::Empty(e) => e, _ => unreachable!() };
                let tag = std::str::from_utf8(e.name().as_ref())?.to_string();
                let mut attrs = Vec::<(String, String)>::new();
                for attr in e.attributes() {
                    let attr = attr.context("飞书 DocxXML 属性无效")?;
                    attrs.push((std::str::from_utf8(attr.key.as_ref())?.to_string(), attr.unescape_value()?.into_owned()));
                }
                if tag == "span" {
                    for (key, value) in &mut attrs {
                        let background = match key.as_str() { "text-color" => false, "background-color" => true, _ => continue };
                        if let Some(name) = crate::feishu_sync::canonical_color(value, background) { *value = name.into(); }
                    }
                }
                let get = |key: &str| attrs.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone());
                let mut asset = None;
                if matches!(tag.as_str(), "img" | "source" | "whiteboard") {
                    let reference = get("data-ref").and_then(|r| refs.get(&tag).and_then(|v| v.get(&r)));
                    let token = get("token").or_else(|| get("src").filter(|src| !src.contains("://") && !src.starts_with("data:")))
                        .or_else(|| reference.and_then(|v| v.get("token")).and_then(Value::as_str).map(str::to_string));
                    if let Some(token) = token {
                        asset = Some(tasks.len());
                        tasks.push(Asset {
                            token,
                            kind: if tag == "whiteboard" { "whiteboard" } else { "media" },
                            name: get("name"),
                        });
                    } else if tag != "whiteboard" {
                        // A URL cannot be handed to the local editor, and fetching arbitrary
                        // external URLs here would bypass the CLI's resource access policy.
                        bail!("飞书{tag}素材缺少可下载的 token，无法创建可用的本地副本");
                    }
                }
                events.push(LocalizedEvent::Element { tag, attrs, empty, asset });
            }
            Event::Eof => break,
            other => events.push(LocalizedEvent::Other(other)),
        }
    }
    let mut paths = download_assets(&tasks, progress, download_one)?;
    let mut writer = Writer::new(Vec::new());
    for event in events {
        match event {
            LocalizedEvent::Element { tag, mut attrs, empty, asset } => {
                if let Some(index) = asset {
                    attrs.retain(|(k, _)| !matches!(k.as_str(), "path" | "src" | "href" | "token"));
                    attrs.push(("path".into(), std::mem::take(&mut paths[index])));
                }
                let mut start = BytesStart::new(tag.as_str());
                for (key, value) in &attrs { start.push_attribute((key.as_str(), value.as_str())); }
                writer.write_event(if empty { Event::Empty(start) } else { Event::Start(start) })?;
            }
            LocalizedEvent::Other(event) => writer.write_event(event)?,
        }
    }
    String::from_utf8(writer.into_inner()).context("转换飞书 DocxXML 失败")
}

pub(crate) fn fetch(url: &str, mut progress: impl FnMut(FetchProgress)) -> Result<Fetched> {
    let url = url.trim();
    if !(url.starts_with("https://") && (url.contains("/docx/") || url.contains("/wiki/"))) {
        bail!("请输入飞书 /docx/ 或 /wiki/ HTTPS 链接");
    }
    progress(FetchProgress::FetchingDocument);
    let response = run(&["docs", "+fetch", "--doc", url, "--as", "user", "--doc-format", "xml", "--detail", "full", "--scope", "full"], None, None)?;
    let document = response.pointer("/data/document").ok_or_else(|| anyhow!("飞书响应缺少 document"))?;
    let original_xml = document.get("content").and_then(Value::as_str).filter(|s| !s.trim().is_empty())
        .ok_or_else(|| anyhow!("飞书响应缺少文档内容"))?.to_string();
    let document_id = document.get("document_id").and_then(Value::as_str).filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow!("飞书响应缺少 document_id"))?.to_string();
    let revision_id = document.get("revision_id").and_then(Value::as_i64)
        .ok_or_else(|| anyhow!("飞书响应缺少 revision_id"))?;
    let reference_map = document.get("reference_map").filter(|v| !v.is_null()).cloned().unwrap_or_else(|| serde_json::json!({}));
    if !reference_map.is_object() { bail!("飞书响应的 reference_map 格式无效"); }
    let assets: PathBuf = crate::store::Store::root_dir().join("assets");
    let localized = localize(&original_xml, &reference_map, &mut progress, |task| {
        download(&task.token, task.kind, task.name.as_deref(), &assets)
    })?;
    progress(FetchProgress::ParsingDocument);
    let mut doc = crate::docxml::parse(&localized).context("无法解析飞书 DocxXML")?;
    doc.ensure_sync_ids();
    Ok(Fetched { doc, url: url.to_string(), document_id, revision_id, original_xml })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn localizes_only_known_span_colors() {
        let xml = "<p id=\"b\"><span text-color=\"rgb(216,57,49)\" background-color=\"rgba(255,246,122,0.8)\">a</span><span text-color=\"rgb(1,2,3)\">b</span></p>";
        let mut progress = Vec::new();
        let localized = localize(xml, &serde_json::json!({}), &mut |p| progress.push(p), |_| unreachable!()).unwrap();
        assert!(progress.is_empty());
        let doc = crate::docxml::parse(&localized).unwrap();
        assert_eq!(doc.blocks[0].text.runs[0].marks.text_color.as_ref().unwrap().0, "red");
        assert_eq!(doc.blocks[0].text.runs[0].marks.bg_color.as_ref().unwrap().0, "light-yellow");
        assert_eq!(doc.blocks[0].text.runs[1].marks.text_color.as_ref().unwrap().0, "rgb(1,2,3)");
    }

    #[test]
    fn downloads_in_parallel_but_writes_paths_in_document_order() {
        use std::sync::{Barrier, Mutex, mpsc};
        use std::sync::atomic::{AtomicUsize, Ordering};

        let xml = "<doc>before<img token=\"0\" src=\"old\" href=\"old\" path=\"old\" name=\"a.png\"/><source data-ref=\"r\"/><whiteboard token=\"2\"/><img src=\"3\"/><img token=\"4\"/><source token=\"5\"/><img token=\"6\"/><img token=\"7\"/><img token=\"8\"/><img token=\"9\"/><img token=\"10\"/><img token=\"11\"/>after<whiteboard/></doc>";
        let refs = serde_json::json!({"source": {"r": {"token": "1"}}});
        let barrier = Barrier::new(11);
        let active = AtomicUsize::new(0);
        let maximum = AtomicUsize::new(0);
        let (senders, receivers): (Vec<_>, Vec<_>) = (0..12).map(|_| {
            let (tx, rx) = mpsc::channel::<()>();
            (tx, Mutex::new(rx))
        }).unzip();
        let (ack_tx, ack_rx) = mpsc::channel();
        let mut progress = Vec::new();

        let localized = std::thread::scope(|scope| {
            let controller_barrier = &barrier;
            scope.spawn(move || {
                controller_barrier.wait();
                for index in [9, 8, 7, 6, 5, 4, 3, 2, 1, 0, 11, 10] {
                    senders[index].send(()).unwrap();
                    ack_rx.recv().unwrap();
                }
            });
            localize(xml, &refs, &mut |event| {
                if let FetchProgress::DownloadingAssets { completed, .. } = event && completed > 0 {
                    ack_tx.send(()).unwrap();
                }
                progress.push(event);
            }, |asset| {
                let index: usize = asset.token.parse().unwrap();
                assert_eq!(asset.kind, if index == 2 { "whiteboard" } else { "media" });
                assert_eq!(asset.name.as_deref(), if index == 0 { Some("a.png") } else { None });
                let count = active.fetch_add(1, Ordering::SeqCst) + 1;
                maximum.fetch_max(count, Ordering::SeqCst);
                if index < 10 { barrier.wait(); }
                receivers[index].lock().unwrap().recv().unwrap();
                active.fetch_sub(1, Ordering::SeqCst);
                Ok(format!("assets/{index}"))
            }).unwrap()
        });
        assert_eq!(maximum.load(Ordering::SeqCst), 10);
        assert_eq!(progress, (0..=12).map(|completed| FetchProgress::DownloadingAssets { completed, total: 12 }).collect::<Vec<_>>());
        assert_eq!(localized, "<doc>before<img name=\"a.png\" path=\"assets/0\"/><source data-ref=\"r\" path=\"assets/1\"/><whiteboard path=\"assets/2\"/><img path=\"assets/3\"/><img path=\"assets/4\"/><source path=\"assets/5\"/><img path=\"assets/6\"/><img path=\"assets/7\"/><img path=\"assets/8\"/><img path=\"assets/9\"/><img path=\"assets/10\"/><img path=\"assets/11\"/>after<whiteboard/></doc>");
    }

    #[test]
    fn reports_all_download_failures_after_started_tasks_finish() {
        use std::sync::Barrier;

        let barrier = Barrier::new(4);
        let mut progress = Vec::new();
        let xml = "<doc><img token=\"0\"/><img token=\"1\"/><img token=\"2\"/><img token=\"3\"/></doc>";
        let error = localize(xml, &serde_json::json!({}), &mut |p| progress.push(p), |asset| {
            barrier.wait();
            match asset.token.as_str() {
                "0" => bail!("first failure"),
                "1" => bail!("second failure"),
                _ => Ok(format!("assets/{}", asset.token)),
            }
        }).unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("first failure"));
        assert!(message.contains("second failure"));
        assert_eq!(progress, (0..=2).map(|completed| FetchProgress::DownloadingAssets { completed, total: 4 }).collect::<Vec<_>>());
    }
}
