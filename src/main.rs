#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod media;
mod net;
mod selftest;
mod store;
mod ui;
mod update;

use std::io::Write;
use std::sync::atomic::Ordering;

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let sub = args.get(1).map(|s| s.as_str());
    if matches!(sub, Some("bench" | "loopback" | "audiotest" | "selftest" | "shot")) {
        // Release builds have no console window: reuse the parent's one.
        unsafe {
            let _ = windows::Win32::System::Console::AttachConsole(windows::Win32::System::Console::ATTACH_PARENT_PROCESS);
        }
    }
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,iroh=warn,noq=warn,noq_proto=warn,wgpu=warn,eframe=warn".into()),
        )
        .init();
    match sub {
        Some("bench") => return bench(&args[2..]),
        Some("loopback") => return loopback(),
        Some("audiotest") => return audiotest(),
        Some("selftest") => return selftest::run(),
        Some("shot") => return shot(&args[2..]),
        _ => {}
    }
    // `--profile <nom>` runs an independent identity (handy to test two
    // instances on one machine).
    let profile = args.iter().position(|a| a == "--profile").and_then(|i| args.get(i + 1)).cloned();
    run_app(profile.as_deref())
}

fn run_app(profile: Option<&str>) -> anyhow::Result<()> {
    update::cleanup();
    let store = store::Store::open(profile)?;
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build()?;
    let handle = rt.handle().clone();
    let title = match profile {
        Some(p) => format!("MyStream ({p})"),
        None => "MyStream".to_string(),
    };
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title(&title)
            .with_inner_size([1100.0, 700.0])
            .with_min_inner_size([760.0, 480.0])
            .with_icon(app_icon()),
        ..Default::default()
    };
    eframe::run_native(
        &title,
        options,
        Box::new(move |cc| {
            ui::theme::install(&cc.egui_ctx);
            let ctx = cc.egui_ctx.clone();
            let (tx, rx) = std::sync::mpsc::channel();
            let node = handle.block_on(net::Node::start(store, tx, move || ctx.request_repaint()))?;
            Ok(Box::new(ui::App::new(node, handle.clone(), rx, cc.egui_ctx.clone())))
        }),
    )
    .map_err(|e| anyhow::anyhow!("{e}"))?;
    rt.shutdown_timeout(std::time::Duration::from_secs(1));
    Ok(())
}

fn app_icon() -> std::sync::Arc<eframe::egui::IconData> {
    let img = image::load_from_memory(include_bytes!("../assets/icon.png")).map(|i| i.to_rgba8());
    std::sync::Arc::new(match img {
        Ok(i) => eframe::egui::IconData { width: i.width(), height: i.height(), rgba: i.into_raw() },
        Err(_) => eframe::egui::IconData::default(),
    })
}

/// `mystream bench [secondes] [fps] [hauteur max] [kbps]`: capture the primary
/// screen, encode, write `bench.h264`, print stats.
fn bench(args: &[String]) -> anyhow::Result<()> {
    let secs: u64 = args.first().and_then(|s| s.parse().ok()).unwrap_or(20);
    let fps: u32 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(30);
    let max_h: u32 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(1080);
    let kbps: u32 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(6000);
    let sources = media::capture::list_sources();
    for s in &sources {
        println!("source: {}", s.name);
    }
    let src = sources.first().ok_or_else(|| anyhow::anyhow!("aucun écran"))?;
    let mut file = std::fs::File::create("bench.h264")?;
    let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
    let writer = std::thread::spawn(move || {
        for d in rx {
            let _ = file.write_all(&d);
        }
    });
    let (pipe, info) = media::pipeline::Pipeline::start(
        src.kind.clone(),
        media::pipeline::VideoSettings { fps, max_height: max_h, bitrate: kbps * 1000, always_encode: args.get(4).is_some_and(|a| a == "force") },
        move |f| {
            let _ = tx.send(f.data);
        },
    )?;
    println!("{}x{} via {}", info.width, info.height, info.encoder_name);
    for i in 0..secs {
        std::thread::sleep(std::time::Duration::from_secs(1));
        println!(
            "t={}s captured={} encoded={} {:.0} kb/s",
            i + 1,
            pipe.stats.captured.load(Ordering::Relaxed),
            pipe.stats.encoded.load(Ordering::Relaxed),
            pipe.stats.bytes.load(Ordering::Relaxed) as f64 * 8.0 / 1000.0 / (i + 1) as f64
        );
    }
    drop(pipe);
    let _ = writer.join();
    Ok(())
}

/// Capture -> encode -> decode in-process; saves `loopback.png`.
fn loopback() -> anyhow::Result<()> {
    let src = media::capture::list_sources().into_iter().next().ok_or_else(|| anyhow::anyhow!("aucun écran"))?;
    let (tx, rx) = std::sync::mpsc::channel::<media::encoder::EncodedFrame>();
    let (pipe, info) = media::pipeline::Pipeline::start(
        src.kind,
        media::pipeline::VideoSettings { fps: 30, max_height: 1080, bitrate: 6_000_000, always_encode: true },
        move |f| {
            let _ = tx.send(f);
        },
    )?;
    let (w, h) = (info.width, info.height);
    let dec = std::thread::spawn(move || -> anyhow::Result<(u32, Option<media::decoder::DecodedFrame>, bool)> {
        media::encoder::com_init();
        let mut d = media::decoder::Decoder::new()?;
        let mut n = 0;
        let mut last = None;
        let t0 = std::time::Instant::now();
        while let Ok(f) = rx.recv() {
            if let Some(img) = d.decode(&f.data, f.timestamp_us, w, h)? {
                n += 1;
                last = Some(img);
            }
            if t0.elapsed().as_secs() >= 5 {
                break;
            }
        }
        Ok((n, last, d.hardware()))
    });
    let (n, last, hw) = dec.join().unwrap()?;
    drop(pipe);
    println!("décodé {n} images en 5 s (matériel: {hw})");
    if let Some(img) = last {
        image::save_buffer("loopback.png", &img.rgba, img.width, img.height, image::ColorType::Rgba8)?;
        println!("loopback.png {}x{}", img.width, img.height);
    }
    Ok(())
}

/// Captures system audio for 5 s and plays it back through the player.
fn audiotest() -> anyhow::Result<()> {
    media::encoder::com_init();
    let player = std::sync::Arc::new(media::audio::AudioPlayer::start(100)?);
    let count = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
    let (p2, c2) = (player.clone(), count.clone());
    let cap = media::audio::AudioCapture::start(move |pkt| {
        c2.fetch_add(1, Ordering::Relaxed);
        p2.push(pkt);
    })?;
    std::thread::sleep(std::time::Duration::from_secs(5));
    drop(cap);
    println!("paquets opus: {} (≈{} attendus si du son joue)", count.load(Ordering::Relaxed), 5 * 50);
    Ok(())
}

/// `mystream shot <titre de fenêtre> <fichier.png>`: captures one window (dev tool).
fn shot(args: &[String]) -> anyhow::Result<()> {
    media::encoder::com_init();
    let title = args.first().map(|s| s.as_str()).unwrap_or("MyStream");
    let out = args.get(1).map(|s| s.as_str()).unwrap_or("shot.png");
    let hwnd = unsafe {
        windows::Win32::UI::WindowsAndMessaging::FindWindowW(None, &windows::core::HSTRING::from(title))?
    };
    let src = media::capture::Source { kind: media::capture::SourceKind::Window(hwnd.0 as isize), name: title.into() };
    let (w, h, rgba) = media::capture::grab_once(&src.kind)?;
    image::save_buffer(out, &rgba, w, h, image::ColorType::Rgba8)?;
    Ok(())
}
