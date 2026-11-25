// Ẩn cửa sổ console đen khi mở file .exe trên Windows
#![windows_subsystem = "windows"]

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use eframe::egui;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha1::{Digest, Sha1};

const API: &str = "https://api.netlify.com/api/v1";

#[derive(Serialize, Deserialize, Clone)]
struct SiteInfo {
    site_id: String,
    url: String,
}

#[derive(Serialize, Deserialize, Default)]
struct Config {
    #[serde(default)]
    token: String,
    // đường dẫn file/thư mục -> link đã tạo
    #[serde(default)]
    sites: HashMap<String, SiteInfo>,
}

#[derive(Deserialize)]
struct DeployResponse {
    id: String,
    #[serde(default)]
    required: Vec<String>,
}

struct JobResult {
    key: String,
    info: Option<SiteInfo>,
    error: Option<String>,
}

struct App {
    config: Config,
    config_path: PathBuf,
    selected: Option<PathBuf>,
    force_new: bool,
    busy: bool,
    status: String,
    last_url: Option<String>,
    rx: Option<mpsc::Receiver<JobResult>>,
}

// ---------- Lưu cấu hình ----------

fn config_path() -> PathBuf {
    let base = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    let dir = base.join("NetlifyShare");
    let _ = fs::create_dir_all(&dir);
    dir.join("config.json")
}

fn load_config(path: &Path) -> Config {
    fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

fn save_config(path: &Path, config: &Config) {
    if let Ok(text) = serde_json::to_string_pretty(config) {
        let _ = fs::write(path, text);
    }
}

fn key_of(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

// ---------- Tiện ích ----------

fn slug(path: &Path) -> String {
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut out = String::new();
    for c in stem.to_lowercase().replace('đ', "d").chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c);
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    let out: String = out.trim_matches('-').chars().take(40).collect();
    if out.is_empty() {
        "trang".to_string()
    } else {
        out
    }
}

fn unique_suffix() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{:x}", nanos % 0xff_ffff)
}

fn sha1_hex(data: &[u8]) -> String {
    let mut hasher = Sha1::new();
    hasher.update(data);
    format!("{:x}", hasher.finalize())
}

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(120))
        .build()
}

fn api_error(err: ureq::Error) -> String {
    match err {
        ureq::Error::Status(code, resp) => {
            let body = resp.into_string().unwrap_or_default();
            format!("Netlify trả về lỗi {code}: {body}")
        }
        other => format!("Lỗi kết nối: {other}"),
    }
}

// ---------- Netlify API ----------

fn create_site(token: &str, base: &str) -> Result<SiteInfo, String> {
    // Thêm đuôi ngẫu nhiên để tên site không trùng với site của người khác
    let name = format!("{base}-{}", unique_suffix());
    let resp = agent()
        .post(&format!("{API}/sites"))
        .set("Authorization", &format!("Bearer {token}"))
        .send_json(json!({ "name": name }))
        .map_err(api_error)?;
    let v: serde_json::Value = resp.into_json().map_err(|e| e.to_string())?;
    let site_id = v["id"].as_str().unwrap_or_default().to_string();
    let url = v["ssl_url"].as_str().unwrap_or_default().to_string();
    if site_id.is_empty() {
        return Err("Netlify không trả về thông tin site".to_string());
    }
    Ok(SiteInfo { site_id, url })
}

fn collect_files(root: &Path, dir: &Path, out: &mut Vec<(String, PathBuf)>) -> std::io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        let hidden = path
            .file_name()
            .map_or(false, |n| n.to_string_lossy().starts_with('.'));
        if hidden {
            continue; // bỏ qua file ẩn như .git, .DS_Store
        }
        if path.is_dir() {
            collect_files(root, &path, out)?;
        } else if path.is_file() {
            let rel = path
                .strip_prefix(root)
                .unwrap_or(path.as_path())
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("/");
            out.push((rel, path));
        }
    }
    Ok(())
}

fn deploy(token: &str, site_id: &str, files: &[(String, PathBuf)]) -> Result<(), String> {
    // Bước 1: tính SHA1 của từng file rồi gửi danh sách cho Netlify
    let mut digest = serde_json::Map::new();
    let mut by_hash: HashMap<String, (String, PathBuf)> = HashMap::new();
    for (rel, path) in files {
        let data = fs::read(path).map_err(|e| format!("Không đọc được {rel}: {e}"))?;
        let hash = sha1_hex(&data);
        digest.insert(format!("/{rel}"), json!(&hash));
        by_hash
            .entry(hash)
            .or_insert_with(|| (rel.clone(), path.clone()));
    }

    let resp = agent()
        .post(&format!("{API}/sites/{site_id}/deploys"))
        .set("Authorization", &format!("Bearer {token}"))
        .send_json(json!({ "files": digest }))
        .map_err(api_error)?;
    let dep: DeployResponse = resp.into_json().map_err(|e| e.to_string())?;

    // Bước 2: chỉ upload những file mà Netlify còn thiếu
    for hash in &dep.required {
        if let Some((rel, path)) = by_hash.get(hash) {
            let data = fs::read(path).map_err(|e| e.to_string())?;
            agent()
                .put(&format!("{API}/deploys/{}/files/{}", dep.id, rel))
                .set("Authorization", &format!("Bearer {token}"))
                .set("Content-Type", "application/octet-stream")
                .send_bytes(&data)
                .map_err(api_error)?;
        }
    }
    Ok(())
}

fn deploy_path(token: &str, site_id: &str, src: &Path) -> Result<(), String> {
    let files = if src.is_dir() {
        let mut v = Vec::new();
        collect_files(src, src, &mut v).map_err(|e| e.to_string())?;
        v
    } else {
        // File HTML đơn: đưa lên thành index.html
        vec![("index.html".to_string(), src.to_path_buf())]
    };
    if files.is_empty() {
        return Err("Thư mục không có file nào để đưa lên".to_string());
    }
    deploy(token, site_id, &files)
}

// Chạy trong luồng riêng để giao diện không bị đơ khi đang gửi lên mạng
fn run_job(token: String, key: String, src: PathBuf, existing: Option<SiteInfo>) -> JobResult {
    let info = match existing {
        Some(info) => info,
        None => match create_site(&token, &slug(&src)) {
            Ok(info) => info,
            Err(e) => {
                return JobResult {
                    key,
                    info: None,
                    error: Some(e),
                };
            }
        },
    };
    let error = deploy_path(&token, &info.site_id, &src).err();
    JobResult {
        key,
        info: Some(info),
        error,
    }
}

// ---------- Giao diện ----------

impl App {
    fn new(_cc: &eframe::CreationContext<'_>) -> Self {
        let config_path = config_path();
        let config = load_config(&config_path);
        Self {
            config,
            config_path,
            selected: None,
            force_new: false,
            busy: false,
            status: String::new(),
            last_url: None,
            rx: None,
        }
    }

    fn save(&self) {
        save_config(&self.config_path, &self.config);
    }

    fn start_share(&mut self, ctx: &egui::Context) {
        let Some(src) = self.selected.clone() else {
            return;
        };
        let key = key_of(&src);
        let existing = if self.force_new {
            None
        } else {
            self.config.sites.get(&key).cloned()
        };
        let token = self.config.token.trim().to_string();

        let (tx, rx) = mpsc::channel();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let result = run_job(token, key, src, existing);
            let _ = tx.send(result);
            ctx.request_repaint();
        });

        self.rx = Some(rx);
        self.busy = true;
        self.last_url = None;
        self.status = "Đang đưa lên Netlify, vui lòng đợi...".to_string();
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // 1. Nhận file/thư mục được kéo thả vào cửa sổ
        let dropped = ctx.input(|i| i.raw.dropped_files.iter().find_map(|f| f.path.clone()));
        if let Some(path) = dropped {
            if !self.busy {
                self.selected = Some(path);
                self.last_url = None;
                self.status.clear();
            }
        }

        // 2. Nhận kết quả từ luồng chạy nền
        let finished = self.rx.as_ref().and_then(|rx| rx.try_recv().ok());
        if let Some(res) = finished {
            self.rx = None;
            self.busy = false;
            if let Some(info) = res.info {
                self.last_url = Some(info.url.clone());
                self.config.sites.insert(res.key, info);
                self.save();
            }
            self.status = match res.error {
                Some(e) => format!("Lỗi: {e}"),
                None => "Xong! Có thể mất vài giây để link hiển thị nội dung mới.".to_string(),
            };
        }

        // 3. Chuẩn bị trạng thái nút bấm
        let has_link = self
            .selected
            .as_ref()
            .map_or(false, |p| self.config.sites.contains_key(&key_of(p)));
        let button_text = if has_link && !self.force_new {
            "Cập nhật link cũ"
        } else {
            "Tạo link chia sẻ"
        };
        let can_share =
            self.selected.is_some() && !self.config.token.trim().is_empty() && !self.busy;

        // 4. Vẽ giao diện
        let mut pick_file = false;
        let mut pick_folder = false;
        let mut start = false;
        let mut token_changed = false;

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("Netlify Share");
            ui.add_space(4.0);

            ui.horizontal(|ui| {
                ui.label("Netlify token:");
                let edit = egui::TextEdit::singleline(&mut self.config.token)
                    .password(true)
                    .desired_width(320.0);
                if ui.add(edit).changed() {
                    token_changed = true;
                }
            });
            if self.config.token.trim().is_empty() {
                ui.label(egui::RichText::new("Dán token Netlify vào ô trên để bắt đầu.").weak());
            }

            ui.add_space(8.0);
            egui::Frame::group(ui.style()).show(ui, |ui| {
                ui.set_min_height(60.0);
                match &self.selected {
                    Some(p) => {
                        ui.label(egui::RichText::new("Đã chọn:").weak());
                        ui.label(egui::RichText::new(p.display().to_string()).strong());
                    }
                    None => {
                        ui.label("Kéo thả file HTML hoặc thư mục vào đây");
                    }
                }
            });

            ui.add_space(4.0);
            ui.horizontal(|ui| {
                if ui.button("Chọn file HTML").clicked() {
                    pick_file = true;
                }
                if ui.button("Chọn thư mục").clicked() {
                    pick_folder = true;
                }
            });

            ui.add_space(8.0);
            ui.checkbox(&mut self.force_new, "Tạo link mới (không dùng link cũ)");
            let share_btn = egui::Button::new(button_text).min_size(egui::vec2(200.0, 34.0));
            if ui.add_enabled(can_share, share_btn).clicked() {
                start = true;
            }

            ui.add_space(6.0);
            if self.busy {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(self.status.as_str());
                });
            } else if !self.status.is_empty() {
                ui.label(self.status.as_str());
            }

            if let Some(url) = &self.last_url {
                ui.horizontal(|ui| {
                    ui.hyperlink(url.as_str());
                    if ui.button("Sao chép").clicked() {
                        ui.ctx().copy_text(url.clone());
                    }
                });
            }

            if !self.config.sites.is_empty() {
                ui.add_space(8.0);
                ui.separator();
                ui.label("Các link đã tạo:");
                let mut items: Vec<(&String, &SiteInfo)> = self.config.sites.iter().collect();
                items.sort_by(|a, b| a.0.cmp(b.0));
                egui::ScrollArea::vertical()
                    .max_height(160.0)
                    .show(ui, |ui| {
                        for (path, info) in items {
                            ui.hyperlink(info.url.as_str());
                            ui.label(egui::RichText::new(path.as_str()).small().weak());
                            ui.add_space(4.0);
                        }
                    });
            }
        });

        // 5. Xử lý các thao tác vừa bấm
        if token_changed {
            self.save();
        }
        if pick_file {
            if let Some(p) = rfd::FileDialog::new()
                .add_filter("HTML", &["html", "htm"])
                .pick_file()
            {
                self.selected = Some(p);
                self.last_url = None;
                self.status.clear();
            }
        }
        if pick_folder {
            if let Some(p) = rfd::FileDialog::new().pick_folder() {
                self.selected = Some(p);
                self.last_url = None;
                self.status.clear();
            }
        }
        if start {
            self.start_share(ctx);
        }
    }
}

fn main() -> Result<(), eframe::Error> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([540.0, 520.0])
            .with_min_inner_size([420.0, 400.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Netlify Share",
        options,
        Box::new(|cc| Ok(Box::new(App::new(cc)))),
    )
}
