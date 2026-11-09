//! `mystream selftest`: end-to-end test with two identities on this machine,
//! going through the real iroh network stack (discovery, hole punching or
//! relay): friend request, acceptance, sharing, watch request, video frames,
//! screenshot.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};

use crate::net::{Audience, Node, UiEvent};
use crate::store::Store;

fn fresh_store(name: &str) -> Result<Store> {
    let base = std::env::var_os("APPDATA").map(std::path::PathBuf::from).unwrap_or_default();
    let dir = base.join("MyStream").join("profiles").join(name);
    let _ = std::fs::remove_dir_all(&dir);
    let store = Store::open(Some(name))?;
    store.update(|d| {
        d.name = name.to_string();
        d.settings.screenshots_dir = dir.join("captures");
    });
    Ok(store)
}

async fn wait_for(what: &str, secs: u64, mut f: impl FnMut() -> bool) -> Result<()> {
    let start = Instant::now();
    while !f() {
        if start.elapsed() > Duration::from_secs(secs) {
            bail!("échec : {what}");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    println!("  ✔ {what} ({:.1}s)", start.elapsed().as_secs_f32());
    Ok(())
}

pub fn run() -> Result<()> {
    crate::media::encoder::com_init();
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    rt.block_on(async {
        let (tx_a, rx_a) = std::sync::mpsc::channel();
        let (tx_b, rx_b) = std::sync::mpsc::channel();
        let a = Node::start(fresh_store("selftest-alice")?, tx_a, || {}).await?;
        let b = Node::start(fresh_store("selftest-bob")?, tx_b, || {}).await?;
        println!("alice = {}\nbob   = {}", a.id().fmt_short(), b.id().fmt_short());

        // Alice auto-accepts watch requests; events are logged.
        let a2 = a.clone();
        std::thread::spawn(move || {
            for e in rx_a {
                match e {
                    UiEvent::WatchRequest { req_id, name, .. } => {
                        println!("  [alice] {name} demande à regarder -> accepté");
                        a2.answer_watch(req_id, true);
                    }
                    UiEvent::Toast(t) => println!("  [alice] {t}"),
                    UiEvent::Screenshot { from, path } => println!("  [alice] capture de {from}: {}", path.display()),
                    UiEvent::HostFailed(e) => println!("  [alice] {e}"),
                }
            }
        });
        let shot = Arc::new(std::sync::Mutex::new(None));
        let shot2 = shot.clone();
        std::thread::spawn(move || {
            for e in rx_b {
                match e {
                    UiEvent::Toast(t) => println!("  [bob] {t}"),
                    UiEvent::Screenshot { from, path } => {
                        println!("  [bob] capture reçue de {from}: {}", path.display());
                        *shot2.lock().unwrap() = Some(path);
                    }
                    _ => {}
                }
            }
        });

        println!("1. réseau");
        let t = Instant::now();
        tokio::time::timeout(Duration::from_secs(20), async { tokio::join!(a.endpoint.online(), b.endpoint.online()) }).await?;
        println!("  ✔ en ligne ({:.1}s)", t.elapsed().as_secs_f32());

        println!("2. amis (bob colle le code d'alice)");
        b.clone().send_friend_request(a.id()).await;
        let bz = b.id().to_z32();
        wait_for("alice reçoit la demande", 20, || a.store.lock().incoming.iter().any(|r| r.id == bz)).await?;
        a.clone().accept_friend(b.id()).await;
        let az = a.id().to_z32();
        wait_for("bob voit alice comme amie", 20, || b.store.lock().friend(&az).is_some()).await?;
        println!("  noms: alice voit « {} », bob voit « {} »", a.store.lock().display_name(&bz), b.store.lock().display_name(&az));

        println!("3. alice partage son écran principal");
        let src = crate::media::capture::list_sources().into_iter().next().ok_or_else(|| anyhow::anyhow!("pas d'écran"))?;
        let a3 = a.clone();
        tokio::task::spawn_blocking(move || a3.start_host(&src, Audience::AllFriends)).await??;
        b.clone().refresh_peer(a.id()).await;
        wait_for("bob voit le partage d'alice", 20, || {
            b.live.lock().unwrap().peers.get(&az).is_some_and(|p| p.stream.is_some())
        })
        .await?;

        println!("4. bob regarde");
        let watch = tokio::spawn(b.clone().watch(a.id()));
        wait_for("bob reçoit des images", 30, || b.stats.received_frames.load(Ordering::Relaxed) > 0).await?;
        wait_for("alice voit bob comme spectateur", 5, || {
            a.live.lock().unwrap().hosting.as_ref().is_some_and(|h| h.viewers.contains(&bz))
        })
        .await?;
        let f0 = b.stats.received_frames.load(Ordering::Relaxed);
        let bytes0 = b.stats.received_bytes.load(Ordering::Relaxed);
        tokio::time::sleep(Duration::from_secs(5)).await;
        let frames = b.stats.received_frames.load(Ordering::Relaxed) - f0;
        let kbps = (b.stats.received_bytes.load(Ordering::Relaxed) - bytes0) * 8 / 5 / 1000;
        println!("  {frames} images décodées en 5 s, {kbps} kb/s (écran peu animé = peu d'images, c'est normal)");
        if let Some(f) = b.frame.lock().unwrap().as_ref() {
            println!("  dernière image {}x{}", f.width, f.height);
        }
        let path = a.endpoint.remote_info(b.id()).await.map(|i| format!("{i:?}")).unwrap_or_default();
        let active: Vec<&str> = path.split("TransportAddrInfo").filter(|p| p.contains("Active")).collect();
        println!("  chemins actifs: {active:?}");

        println!("5. capture envoyée par alice");
        let a4 = a.clone();
        let (w, h, rgba) = tokio::task::spawn_blocking(move || a4.host_screenshot().unwrap()).await??;
        let mut png = std::io::Cursor::new(Vec::new());
        image::RgbaImage::from_raw(w, h, rgba).unwrap().write_to(&mut png, image::ImageFormat::Png)?;
        a.clone().send_screenshot(Audience::AllFriends, Arc::new(png.into_inner()), "test.png".into()).await;
        wait_for("bob reçoit la capture", 20, || shot.lock().unwrap().is_some()).await?;

        println!("6. alice arrête");
        let a5 = a.clone();
        tokio::task::spawn_blocking(move || a5.stop_host()).await?;
        tokio::time::timeout(Duration::from_secs(10), watch).await??;
        wait_for("bob n'est plus en visionnage", 5, || b.live.lock().unwrap().watching.is_none()).await?;

        println!("\nTOUT EST OK ✅");
        a.endpoint.close().await;
        b.endpoint.close().await;
        Ok(())
    })
}
